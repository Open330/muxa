//! Daemon-owned, disposable location read model. Reconciler observations are
//! reused by default; explicit refresh and startup share one demand scan.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{watch, Mutex};

use crate::agent_locator::{DirectoryIndex, LocationFreshness, LocationResults};
use crate::ask::AskStore;
use crate::backend::PaneObservation;
use crate::{HostKind, SharedBackend, SharedStore, TopologyInput, TopologySnapshot};

pub const CAPABILITY: &str = "agent_directory_v1";
const DEFAULT_MAX_AGE: Duration = Duration::from_secs(2);

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyntheticBucket {
    pub count: usize,
    pub examples: Vec<String>,
}

#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyntheticDiagnostics {
    pub inventory_age_ms: Option<u64>,
    pub max_inventory_age_ms: u64,
    pub inventory_invalidated: bool,
    pub unbound_hooks: SyntheticBucket,
    pub unobserved_backend: SyntheticBucket,
    pub unseen_panes: SyntheticBucket,
    pub unknown: SyntheticBucket,
}

pub struct AgentDirectory {
    state: Mutex<Option<CachedDirectory>>,
    inventory: watch::Sender<Option<Arc<ObservedInventory>>>,
    max_inventory_age: Duration,
    invalidated_at: std::sync::Mutex<Option<Instant>>,
    demand_generation: std::sync::atomic::AtomicU64,
}

struct ObservedInventory {
    inputs: Vec<TopologyInput>,
    at: Instant,
    observed_at: time::OffsetDateTime,
    warnings: Vec<String>,
    complete_kinds: Vec<HostKind>,
}

struct CachedDirectory {
    inventory: Option<Arc<ObservedInventory>>,
    index: Option<Arc<DirectoryIndex>>,
    missing: HashSet<(String, String, Option<String>)>,
    store_changes: watch::Receiver<()>,
}

impl Default for AgentDirectory {
    fn default() -> Self {
        Self::with_max_age(DEFAULT_MAX_AGE)
    }
}

impl AgentDirectory {
    pub fn with_max_age(max_inventory_age: Duration) -> Self {
        Self {
            state: Mutex::new(None),
            inventory: watch::channel(None).0,
            max_inventory_age: max_inventory_age.max(Duration::from_millis(1)),
            invalidated_at: std::sync::Mutex::new(None),
            demand_generation: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Keep a floor rather than dropping the slot: an older in-flight
    /// reconcile must not undo a layout mutation's invalidation.
    pub fn invalidate(&self) {
        *self
            .invalidated_at
            .lock()
            .expect("inventory invalidation lock poisoned") = Some(Instant::now());
    }

    /// Called after reconciliation/rebinding, using the original observation
    /// time. A failed/partial inventory replaces the old one with a warning;
    /// it never silently extends the life of an earlier successful view.
    pub fn publish(
        &self,
        observations: Vec<(HostKind, PaneObservation)>,
        at: Instant,
        observed_at: time::OffsetDateTime,
    ) {
        let mut warnings = Vec::new();
        let mut complete_kinds = Vec::new();
        let inputs = observations.into_iter().map(|(host, observation)| {
            if observation.is_complete() { complete_kinds.push(host); }
            if !observation.is_complete() {
                warnings.push(format!("{host} pane inventory is incomplete; absence is not proof that an agent has no location."));
            }
            TopologyInput::new(host, observation.panes)
        }).collect();
        let next = Arc::new(ObservedInventory {
            inputs,
            at,
            observed_at,
            warnings,
            complete_kinds,
        });
        // A slow reconcile started before a demand refresh must not replace
        // that newer observation when it eventually finishes.
        self.inventory.send_if_modified(|slot| {
            if slot.as_ref().is_some_and(|old| old.at > at) {
                return false;
            }
            *slot = Some(next);
            true
        });
    }

    /// Reuse published evidence without starting a backend discovery scan.
    pub fn synthetic_diagnostics<'a>(
        &self,
        agents: impl Iterator<Item = &'a crate::Agent>,
        configured: &[HostKind],
    ) -> SyntheticDiagnostics {
        let inventory = self.inventory.borrow().clone();
        let floor = *self
            .invalidated_at
            .lock()
            .expect("inventory invalidation lock poisoned");
        let invalidated = inventory
            .as_ref()
            .is_some_and(|i| floor.is_some_and(|f| i.at < f));
        let valid = inventory
            .as_ref()
            .filter(|i| i.at.elapsed() < self.max_inventory_age && !invalidated);
        let mut result = SyntheticDiagnostics {
            inventory_age_ms: inventory
                .as_ref()
                .map(|i| u64::try_from(i.at.elapsed().as_millis()).unwrap_or(u64::MAX)),
            max_inventory_age_ms: u64::try_from(self.max_inventory_age.as_millis())
                .unwrap_or(u64::MAX),
            inventory_invalidated: invalidated,
            ..Default::default()
        };
        for agent in agents.filter(|a| {
            a.state != crate::AgentState::Stopped
                && a.session_id
                    .starts_with(crate::state::SYNTHETIC_SESSION_PREFIX)
        }) {
            let pane = agent.pane.as_deref();
            let host = pane.and_then(crate::backend::pane_id_host_kind);
            let bucket = match host {
                Some(host) if !configured.contains(&host) => &mut result.unobserved_backend,
                Some(host) => match valid.filter(|i| i.complete_kinds.contains(&host)) {
                    Some(i) => {
                        let candidates = i
                            .inputs
                            .iter()
                            .filter(|input| input.host == host)
                            .flat_map(|input| &input.panes)
                            .filter(|p| Some(p.pane_id.as_str()) == pane)
                            .collect::<Vec<_>>();
                        let present = if let Some(socket) = &agent.tmux_socket {
                            candidates.iter().any(|p| {
                                p.socket.as_ref().is_some_and(|observed| {
                                    crate::backend::pane_endpoints_match(pane, socket, observed)
                                })
                            })
                        } else {
                            candidates.len() == 1
                        };
                        if agent.tmux_socket.is_none() && candidates.len() > 1 {
                            &mut result.unknown
                        } else if present {
                            &mut result.unbound_hooks
                        } else {
                            &mut result.unseen_panes
                        }
                    }
                    None => &mut result.unknown,
                },
                None => &mut result.unknown,
            };
            bucket.count += 1;
            bucket.examples.push(
                agent
                    .tmux_socket
                    .as_deref()
                    .unwrap_or("?")
                    .chars()
                    .chain(std::iter::once(':'))
                    .chain(pane.unwrap_or("?").chars())
                    .take(120)
                    .collect(),
            );
            bucket.examples.sort_unstable();
            bucket.examples.dedup();
            bucket.examples.truncate(3);
        }
        result
    }

    async fn observe(&self, backends: &[SharedBackend]) -> Result<(), String> {
        let at = Instant::now();
        let observed_at = time::OffsetDateTime::now_utc();
        let handles: Vec<_> = backends
            .iter()
            .map(|backend| {
                let backend = Arc::clone(backend);
                (
                    backend.kind(),
                    tokio::task::spawn_blocking(move || backend.observe_panes()),
                )
            })
            .collect();
        let mut observations = Vec::new();
        for (host, handle) in handles {
            observations.push((
                host,
                handle
                    .await
                    .unwrap_or_else(|_| PaneObservation::incomplete(Vec::new())),
            ));
        }
        self.publish(observations, at, observed_at);
        self.demand_generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn find(
        &self,
        store: &SharedStore,
        _ask: &AskStore,
        backends: &[SharedBackend],
        query: &str,
        limit: usize,
        exclude: &[String],
        refresh: bool,
    ) -> Result<LocationResults, String> {
        crate::agent_locator::validate(query, limit)?;
        // A concurrent force-refresh joins the observation already in flight.
        // A later sequential refresh still requests a new observation.
        let requested_generation = self
            .demand_generation
            .load(std::sync::atomic::Ordering::SeqCst);
        // Serialize only refresh/build work; readers search an immutable Arc
        // after releasing the guard. Bursts coalesce into one lazy rebuild.
        let mut guard = self.state.lock().await;
        let state = guard.get_or_insert_with(|| CachedDirectory {
            inventory: None,
            index: None,
            missing: HashSet::new(),
            store_changes: store.subscribe_changes(),
        });
        let inventory = self.inventory.borrow().clone();
        let observed_since_request = self
            .demand_generation
            .load(std::sync::atomic::Ordering::SeqCst)
            != requested_generation;
        let floor = *self
            .invalidated_at
            .lock()
            .expect("inventory invalidation lock poisoned");
        let expired = (refresh && !observed_since_request)
            || inventory
                .as_ref()
                .is_some_and(|i| floor.is_some_and(|floor| i.at < floor))
            || inventory
                .as_ref()
                .is_none_or(|i| i.at.elapsed() >= self.max_inventory_age);
        if expired {
            self.observe(backends).await?;
        }
        let mut inventory = self
            .inventory
            .borrow()
            .clone()
            .expect("inventory observed above");
        let inventory_changed = state
            .inventory
            .as_ref()
            .is_none_or(|old| !Arc::ptr_eq(old, &inventory));
        let changed = state.store_changes.has_changed().unwrap_or(true);
        let rebuild = inventory_changed || changed || state.index.is_none();
        if rebuild {
            // Consume BEFORE reading sources: changes during await remain
            // visible to the following lookup, rather than being lost.
            state.store_changes.borrow_and_update();
            let agents = store.directory_snapshot().await;
            let missing = missing_locations(&agents, &inventory.inputs);
            if !expired && missing.iter().any(|key| !state.missing.contains(key)) {
                // A hook can bind a new/moved agent before the next reconcile
                // tick. Refresh once for each newly missing binding, even if
                // the inventory's age is otherwise acceptable.
                self.observe(backends).await?;
                inventory = self
                    .inventory
                    .borrow()
                    .clone()
                    .expect("inventory refreshed");
            }
            state.missing = missing_locations(&agents, &inventory.inputs);
            let inputs = inventory.inputs.clone();
            let observed_at = inventory.observed_at;
            let index = tokio::task::spawn_blocking(move || {
                DirectoryIndex::build(&TopologySnapshot::build(observed_at, inputs, agents))
            })
            .await
            .map_err(|error| error.to_string())?;
            state.index = Some(Arc::new(index));
            state.inventory = Some(Arc::clone(&inventory));
        }
        let index = Arc::clone(state.index.as_ref().expect("directory built above"));
        let warnings = inventory.warnings.clone();
        let freshness = LocationFreshness {
            inventory_observed_at: inventory.observed_at,
            inventory_age_ms: millis(inventory.at.elapsed()),
            max_inventory_age_ms: millis(self.max_inventory_age),
            reused_index: !rebuild,
        };
        drop(guard);
        let query = query.to_owned();
        let exclude = exclude.to_vec();
        let mut result = tokio::task::spawn_blocking(move || {
            index.find(&query, limit, &exclude).map_err(str::to_owned)
        })
        .await
        .map_err(|error| error.to_string())??;
        result.warnings = warnings;
        result.freshness = Some(freshness);
        if let Some(freshness) = &mut result.freshness {
            freshness.inventory_age_ms = millis(inventory.at.elapsed());
            if freshness.inventory_age_ms > freshness.max_inventory_age_ms {
                result.warnings.push("Inventory collection exceeded the age bound; verify the exact agent identity before acting.".into());
            }
        }
        Ok(result)
    }
}

fn missing_locations(
    agents: &[crate::Agent],
    inputs: &[TopologyInput],
) -> HashSet<(String, String, Option<String>)> {
    agents
        .iter()
        .filter(|agent| agent.state != crate::AgentState::Stopped)
        .filter_map(|agent| {
            let pane_id = agent.pane.as_ref()?;
            let found = inputs.iter().any(|input| {
                input.panes.iter().any(|pane| {
                    pane.pane_id == *pane_id
                        && agent.tmux_socket.as_deref().is_none_or(|socket| {
                            crate::backend::pane_endpoint_identity(Some(pane_id), socket)
                                == crate::PaneKey::from_pane(input.host, pane)
                                    .window
                                    .session
                                    .endpoint
                                    .socket
                        })
                })
            });
            (!found).then(|| {
                (
                    agent.session_id.clone(),
                    pane_id.clone(),
                    agent.tmux_socket.clone(),
                )
            })
        })
        .collect()
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_locator::tests::pane;
    use crate::backend::PaneBackend;
    use crate::event::{AgentEvent, AgentId};
    use crate::tmux::PaneInfo;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct Backend {
        panes: std::sync::Mutex<Vec<PaneInfo>>,
        calls: AtomicUsize,
        incomplete: AtomicBool,
        gate: std::sync::Mutex<Option<Arc<std::sync::Barrier>>>,
    }
    impl Backend {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                panes: std::sync::Mutex::new(vec![pane("test", "workspace", "task")]),
                calls: AtomicUsize::new(0),
                incomplete: AtomicBool::new(false),
                gate: std::sync::Mutex::new(None),
            })
        }
    }
    impl PaneBackend for Backend {
        fn kind(&self) -> HostKind {
            HostKind::Tmux
        }
        fn list_panes(&self) -> Vec<PaneInfo> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let gate = self.gate.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.wait();
            }
            self.panes.lock().unwrap().clone()
        }
        fn observe_panes(&self) -> PaneObservation {
            if self.incomplete.load(Ordering::SeqCst) {
                self.calls.fetch_add(1, Ordering::SeqCst);
                PaneObservation::incomplete(Vec::new())
            } else {
                PaneObservation::complete(self.list_panes())
            }
        }
        fn resolve_pane(&self, _: &str) -> Option<PaneInfo> {
            panic!("lookup must use inventory")
        }
        fn capture_pane(&self, _: &str) -> Option<String> {
            panic!("lookup must not capture")
        }
        fn pane_pid_map(&self) -> std::collections::HashMap<u32, String> {
            panic!("lookup must not scan processes")
        }
        fn current_pane(&self) -> Option<String> {
            None
        }
        fn focus_pane(&self, _: &str) -> bool {
            panic!("lookup must not navigate")
        }
    }
    async fn prompt(store: &SharedStore, pane_id: &str, text: &str) {
        store
            .apply(&AgentEvent::PromptSubmitted {
                id: AgentId {
                    kind: crate::AgentKind::ClaudeCode,
                    session_id: "session".into(),
                    surface: None,
                    pane: Some(pane_id.into()),
                    tmux_socket: Some("test".into()),
                    cwd: Some("/repo".into()),
                },
                prompt: text.into(),
                at: time::OffsetDateTime::now_utc(),
            })
            .await;
    }
    fn ask() -> Arc<AskStore> {
        AskStore::in_memory(crate::ask::AskOptions::default())
    }

    #[tokio::test]
    async fn synthetic_health_uses_daemon_scope_and_complete_fresh_endpoint_evidence() {
        let store = crate::Store::shared();
        prompt(&store, "%1", "private prompt").await;
        let mut agent = store.snapshot().await.remove(0);
        agent.session_id = format!("{}test:%1", crate::state::SYNTHETIC_SESSION_PREFIX);
        let directory = AgentDirectory::default();
        let pane = pane("test", "workspace", "task");
        let publish = |observation| {
            directory.publish(
                vec![(HostKind::Tmux, observation)],
                Instant::now(),
                time::OffsetDateTime::now_utc(),
            );
        };
        publish(PaneObservation::complete(vec![pane.clone()]));
        let diagnose = |configured: &[HostKind]| {
            directory.synthetic_diagnostics(std::iter::once(&agent), configured)
        };
        assert_eq!(diagnose(&[HostKind::Tmux]).unbound_hooks.count, 1);
        assert_eq!(
            diagnose(&[HostKind::Rmux]).unobserved_backend.count,
            1,
            "the CLI environment must not turn an unobserved tmux row into hook advice"
        );
        let mut other_socket = pane.clone();
        other_socket.socket = Some("other-server".into());
        publish(PaneObservation::complete(vec![other_socket]));
        assert_eq!(diagnose(&[HostKind::Tmux]).unseen_panes.count, 1);
        publish(PaneObservation::incomplete(vec![pane.clone()]));
        assert_eq!(diagnose(&[HostKind::Tmux]).unknown.count, 1);
        publish(PaneObservation::complete(vec![pane.clone()]));
        let mut socketless = agent.clone();
        socketless.tmux_socket = None;
        assert_eq!(
            directory
                .synthetic_diagnostics(std::iter::once(&socketless), &[HostKind::Tmux])
                .unbound_hooks
                .count,
            1
        );
        let mut duplicate = pane.clone();
        duplicate.socket = Some("second".into());
        publish(PaneObservation::complete(vec![pane.clone(), duplicate]));
        assert_eq!(
            directory
                .synthetic_diagnostics(std::iter::once(&socketless), &[HostKind::Tmux])
                .unknown
                .count,
            1
        );
        publish(PaneObservation::complete(vec![pane.clone()]));
        directory.invalidate();
        assert_eq!(diagnose(&[HostKind::Tmux]).unknown.count, 1);
        let stale = AgentDirectory::default();
        stale.publish(
            vec![(HostKind::Tmux, PaneObservation::complete(vec![pane]))],
            Instant::now().checked_sub(Duration::from_secs(30)).unwrap(),
            time::OffsetDateTime::now_utc(),
        );
        assert_eq!(
            stale
                .synthetic_diagnostics(std::iter::once(&agent), &[HostKind::Tmux])
                .unknown
                .count,
            1
        );
        assert_eq!(
            stale
                .synthetic_diagnostics(std::iter::once(&agent), &[HostKind::Rmux])
                .unobserved_backend
                .count,
            1
        );
    }

    #[tokio::test]
    async fn concurrent_clients_and_different_queries_share_one_inventory_and_index() {
        let directory = Arc::new(AgentDirectory::default());
        let store = crate::Store::shared();
        let backend = Backend::new();
        let ask = ask();
        prompt(&store, "%1", "한국어 작업").await;
        let mut jobs = Vec::new();
        for i in 0..50 {
            let (directory, store, backend, ask) = (
                directory.clone(),
                store.clone(),
                backend.clone(),
                ask.clone(),
            );
            jobs.push(tokio::spawn(async move {
                directory
                    .find(
                        &store,
                        &ask,
                        &[backend],
                        if i % 2 == 0 { "작업" } else { "task" },
                        10,
                        &[],
                        false,
                    )
                    .await
                    .unwrap()
            }));
        }
        let mut reused = 0;
        for job in jobs {
            let result = job.await.unwrap();
            assert_eq!(result.total, 1);
            reused += usize::from(result.freshness.unwrap().reused_index);
        }
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        assert_eq!(reused, 49);
    }

    #[tokio::test]
    async fn registry_changes_rebuild_documents_without_rescanning_and_new_panes_refresh_once() {
        let directory = AgentDirectory::default();
        let store = crate::Store::shared();
        let backend = Backend::new();
        let ask = ask();
        let backends: Vec<SharedBackend> = vec![backend.clone()];
        prompt(&store, "%1", "before").await;
        assert_eq!(
            directory
                .find(&store, &ask, &backends, "before", 10, &[], false)
                .await
                .unwrap()
                .total,
            1
        );
        prompt(&store, "%1", "after").await;
        assert_eq!(
            directory
                .find(&store, &ask, &backends, "after", 10, &[], false)
                .await
                .unwrap()
                .total,
            1
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        backend.panes.lock().unwrap()[0].pane_id = "%2".into();
        prompt(&store, "%2", "moved").await;
        let result = directory
            .find(&store, &ask, &backends, "moved", 10, &[], false)
            .await
            .unwrap();
        assert_eq!(result.matches[0].location.pane_id, "%2");
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        directory
            .find(&store, &ask, &backends, "moved", 10, &[], false)
            .await
            .unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn published_inventory_is_reused_and_force_expiry_and_failure_are_explicit() {
        let directory = AgentDirectory::default();
        let store = crate::Store::shared();
        let backend = Backend::new();
        let ask = ask();
        let backends: Vec<SharedBackend> = vec![backend.clone()];
        prompt(&store, "%1", "work").await;
        directory.publish(
            vec![(
                HostKind::Tmux,
                PaneObservation::complete(backend.panes.lock().unwrap().clone()),
            )],
            Instant::now(),
            time::OffsetDateTime::now_utc(),
        );
        assert_eq!(
            directory
                .find(&store, &ask, &backends, "work", 10, &[], false)
                .await
                .unwrap()
                .total,
            1
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        directory.publish(
            Vec::new(),
            Instant::now().checked_sub(Duration::from_secs(3)).unwrap(),
            time::OffsetDateTime::now_utc(),
        );
        let retained = directory
            .find(&store, &ask, &backends, "work", 10, &[], false)
            .await
            .unwrap();
        assert_eq!(
            retained.total, 1,
            "late older publish must not erase newer inventory"
        );
        assert_eq!(backend.calls.load(Ordering::SeqCst), 0);
        let directory = AgentDirectory::default();
        directory.publish(
            Vec::new(),
            Instant::now().checked_sub(Duration::from_secs(3)).unwrap(),
            time::OffsetDateTime::now_utc(),
        );
        directory
            .find(&store, &ask, &backends, "work", 10, &[], false)
            .await
            .unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        backend.incomplete.store(true, Ordering::SeqCst);
        let failed = directory
            .find(&store, &ask, &backends, "work", 10, &[], true)
            .await
            .unwrap();
        assert_eq!(failed.total, 0);
        assert_eq!(failed.unlocated_matches, 1);
        assert!(!failed.warnings.is_empty());
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    }
    #[tokio::test]
    async fn overlapping_forced_refreshes_share_one_demand_scan_but_sequential_refreshes_do_not() {
        let directory = Arc::new(AgentDirectory::default());
        let store = crate::Store::shared();
        let backend = Backend::new();
        let ask = ask();
        let backends: Vec<SharedBackend> = vec![backend.clone()];
        prompt(&store, "%1", "task").await;
        directory
            .find(&store, &ask, &backends, "task", 10, &[], false)
            .await
            .unwrap();
        let gate = Arc::new(std::sync::Barrier::new(2));
        *backend.gate.lock().unwrap() = Some(gate.clone());
        let arrivals = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..20 {
            let (directory, store, ask, backends, arrivals) = (
                directory.clone(),
                store.clone(),
                ask.clone(),
                backends.clone(),
                arrivals.clone(),
            );
            tasks.push(tokio::spawn(async move {
                arrivals.fetch_add(1, Ordering::SeqCst);
                directory
                    .find(&store, &ask, &backends, "task", 10, &[], true)
                    .await
                    .unwrap()
            }));
        }
        while arrivals.load(Ordering::SeqCst) < 20 {
            tokio::task::yield_now().await;
        }
        // A late reconcile publish must not count as a demand refresh.
        directory.publish(
            vec![(
                HostKind::Tmux,
                PaneObservation::complete(backend.panes.lock().unwrap().clone()),
            )],
            Instant::now()
                .checked_sub(Duration::from_millis(1))
                .unwrap(),
            time::OffsetDateTime::now_utc(),
        );
        gate.wait();
        for task in tasks {
            assert_eq!(task.await.unwrap().total, 1);
        }
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
        directory
            .find(&store, &ask, &backends, "task", 10, &[], true)
            .await
            .unwrap();
        assert_eq!(backend.calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn mutation_floor_survives_a_late_publish_and_refreshes_same_pane_window_changes() {
        let directory = AgentDirectory::default();
        let store = crate::Store::shared();
        let backend = Backend::new();
        let ask = ask();
        let backends: Vec<SharedBackend> = vec![backend.clone()];
        prompt(&store, "%1", "task").await;
        directory
            .find(&store, &ask, &backends, "task", 10, &[], false)
            .await
            .unwrap();
        let old_panes = backend.panes.lock().unwrap().clone();
        let old_at = Instant::now();
        backend.panes.lock().unwrap()[0].window_name = "renamed".into();
        backend.panes.lock().unwrap()[0].window_id = "@2".into();
        directory.invalidate();
        directory.publish(
            vec![(HostKind::Tmux, PaneObservation::complete(old_panes))],
            old_at,
            time::OffsetDateTime::now_utc(),
        );
        let fresh = directory
            .find(&store, &ask, &backends, "task", 10, &[], false)
            .await
            .unwrap();
        assert_eq!(fresh.matches[0].window, "renamed");
        assert_eq!(fresh.matches[0].location.window.window_id, "@2");
        assert_eq!(backend.calls.load(Ordering::SeqCst), 2);
    }
    #[tokio::test]
    async fn late_reconcile_publish_cannot_satisfy_a_queued_force_request() {
        let directory = Arc::new(AgentDirectory::default());
        let backend = Backend::new();
        let store = crate::Store::shared();
        let ask = ask();
        prompt(&store, "%1", "task").await;
        let lock = directory.state.lock().await;
        let task = {
            let directory = directory.clone();
            let backend = backend.clone();
            tokio::spawn(async move {
                directory
                    .find(&store, &ask, &[backend], "task", 10, &[], true)
                    .await
                    .unwrap()
            })
        };
        tokio::task::yield_now().await;
        directory.publish(
            Vec::new(),
            Instant::now().checked_sub(Duration::from_secs(1)).unwrap(),
            time::OffsetDateTime::now_utc(),
        );
        drop(lock);
        assert_eq!(task.await.unwrap().total, 1);
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    }
}
