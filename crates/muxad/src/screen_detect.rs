//! Screen-manifest fallback detection task.
//!
//! For agent CLIs muxa has **no hooks** for (cursor-agent, amp, copilot, aider,
//! goose, …), this background task infers state from the pane's screen: every
//! `[screen_detect] interval_secs` it finds panes whose foreground command
//! matches a manifest, captures each, matches the manifest's regex rules against
//! the visible tail, and — on a state CHANGE — ingests SYNTHETIC muxa events
//! exactly the way [`crate::herdr_bridge`] does for herdr's detection. The pure
//! manifest/classifier core lives in [`muxa::screen`]; the synthetic-row
//! machinery (hook-authoritative precedence, row liveness, event building) is
//! shared with the herdr bridge via [`crate::synthetic`].
//!
//! ## Precedence — hooks > herdr bridge > screen detection
//!
//! Screen rows are synthetic, so:
//!
//! * A real hook `Started`/tool/prompt event on the pane evicts the screen row
//!   the instant it fires (`Store::apply`'s synthetic-eviction pass).
//! * Before capturing OR applying, the task checks
//!   [`synthetic::occupant_is_authoritative`]: if a live non-synthetic row owns
//!   the pane, the pane is skipped entirely — no capture, no update.
//! * **herdr hosts are skipped wholesale**: herdr's own detection + the herdr
//!   bridge already cover those panes, so a herdr backend is never a screen
//!   candidate (see [`detectable_backends`]).
//!
//! ## Cost / robustness
//!
//! Candidate discovery is one `list_panes` per backend per tick (`spawn_blocking`);
//! when no pane matches a manifest, zero captures run. Each capture is a
//! `spawn_blocking` `capture-pane` bounded by tmux's own 1s command timeout. A
//! tick is skipped if the previous one is still running (interval with
//! `MissedTickBehavior::Skip`, and each tick is fully awaited before the next).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use muxa::screen::{AgentManifest, ManifestSet, ScreenState};
use muxa::tmux::PaneInfo;
use muxa::{AgentId, AgentKind, Config, HostKind, PaneBackend, SharedBackend, SharedStore};
use time::OffsetDateTime;
use tokio::sync::broadcast;
use tokio::task::spawn_blocking;

use crate::synthetic::{self, SyntheticState};

/// How many lines from the bottom of a capture the classifier sees. The active
/// spinner/prompt/approval UI lives near the bottom; bounding the slice also
/// bounds regex work.
const CAPTURE_TAIL_LINES: usize = 40;

/// The backends screen detection may run on: **non-herdr** (herdr panes are
/// covered by the herdr bridge) and able to **capture panes**
/// (`caps().capture_pane`). Zellij without its plugin reports
/// `capture_pane = false` and drops out here.
fn detectable_backends(backends: &[SharedBackend]) -> Vec<SharedBackend> {
    backends
        .iter()
        .filter(|b| b.kind() != HostKind::Herdr && b.caps().capture_pane)
        .cloned()
        .collect()
}

/// Map the screen classifier's state onto the shared synthetic vocabulary.
fn to_synthetic(state: ScreenState) -> SyntheticState {
    match state {
        ScreenState::Working => SyntheticState::Working,
        ScreenState::Blocked => SyntheticState::Blocked,
        ScreenState::Idle => SyntheticState::Idle,
    }
}

/// Build the synthetic [`AgentId`] for a screen-detected pane. Uses the SAME
/// [`muxa::synthetic_session_id`] convention discovery mints, so a discovery
/// placeholder and this screen row collapse onto one registry key and share the
/// hook-eviction precedence. `kind` is always `Unknown` — the manifest name
/// rides in the row's `model` field (set by the heartbeat the shared builder
/// appends), not the kind.
fn synthetic_id(pane: &PaneInfo) -> AgentId {
    AgentId {
        kind: AgentKind::Unknown,
        session_id: muxa::synthetic_session_id(pane),
        surface: None,
        pane: Some(pane.pane_id.clone()),
        tmux_socket: pane.socket.clone(),
        cwd: None,
    }
}

/// The periodic screen detector. Owns the loaded manifests and the per-pane
/// last-classified-state map that makes ingestion state-change-only.
struct ScreenDetector {
    backends: Vec<SharedBackend>,
    store: SharedStore,
    manifests: ManifestSet,
    interval: Duration,
    /// pane id → last classified state. A capture that classifies to the same
    /// state is a no-op; a classification of `None` (unknown screen) leaves the
    /// entry untouched — the "unknown transitions are dropped" rule.
    last_state: HashMap<String, ScreenState>,
    /// Panes we currently own a synthetic row for. When a tracked pane stops
    /// being a candidate (its foreground command changed away from the agent),
    /// its synthetic row is stopped so it doesn't freeze.
    tracked: HashSet<String>,
}

impl ScreenDetector {
    /// The reconnect-free main loop: tick on the interval, drain on shutdown.
    async fn run(mut self, mut shutdown_rx: broadcast::Receiver<()>) {
        let mut tick = tokio::time::interval(self.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.tick().await; // consume the immediate first tick
        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    tracing::debug!("screen detection shutting down");
                    return;
                }
                _ = tick.tick() => {
                    let _ = self.run_tick().await;
                }
            }
        }
    }

    /// One detection pass. Returns the number of state CHANGES ingested (for
    /// tests). Steps: gather candidate panes, stop rows for panes that dropped
    /// out of candidacy, then capture/classify/ingest each candidate a live
    /// hook doesn't own.
    async fn run_tick(&mut self) -> usize {
        let candidates = self.gather_candidates().await;
        let current: HashSet<String> = candidates
            .iter()
            .map(|(_, p, _)| p.pane_id.clone())
            .collect();

        // Panes we tracked that are no longer candidates: the agent's foreground
        // command changed (it exited, or a different program took over). Stop the
        // synthetic row so it doesn't freeze at its last state.
        let dropped: Vec<String> = self
            .tracked
            .iter()
            .filter(|p| !current.contains(*p))
            .cloned()
            .collect();
        for pane_id in dropped {
            synthetic::stop_synthetic_row(&self.store, &pane_id).await;
            self.tracked.remove(&pane_id);
        }
        // Drop the last-classified state for every pane that fell out of
        // candidacy, tracked or not. Attention-refined panes are deliberately
        // never `tracked` (their row is real and not ours to stop), so pruning
        // off `tracked` alone would leak an entry per agy pane ever seen.
        self.last_state
            .retain(|pane_id, _| current.contains(pane_id));

        let mut changes = 0;
        for (backend_idx, pane, manifest) in candidates {
            if self.process_candidate(backend_idx, &pane, &manifest).await {
                changes += 1;
            }
        }
        changes
    }

    /// List panes across every detectable backend and keep the ones whose
    /// foreground command matches a manifest. Returns
    /// `(backend_index, pane, manifest)` — the manifest that selected the pane
    /// is carried forward (cloned; its `RegexSet`s are `Arc`-backed, so the
    /// clone is cheap) so [`process_candidate`](Self::process_candidate) need
    /// not re-resolve it. The `list_panes` calls (blocking tmux shell-outs) run
    /// inside one `spawn_blocking`.
    async fn gather_candidates(&self) -> Vec<(usize, PaneInfo, AgentManifest)> {
        // What the registry believes occupies each pane, built once per tick.
        // This is the stronger of the two selectors: an npm-installed codex
        // reports `node` as its foreground command, which names no manifest,
        // so command-only selection skipped those panes entirely and their
        // startup gate was never seen. Stopped rows are tombstones, not
        // occupants.
        let by_pane: HashMap<String, AgentKind> = self
            .store
            .snapshot()
            .await
            .into_iter()
            .filter(|a| a.state != muxa::AgentState::Stopped)
            .filter_map(|a| a.pane.clone().map(|pane| (pane, a.kind)))
            .collect();

        let backends = self.backends.clone();
        // `discover_from_panes` walks the process tree for wrapper panes, which
        // is the only way to identify an agent that has NO registry row yet —
        // exactly the startup gate, since the row is minted by the first hook
        // and the gate paints before any hook exists. Blocking (/proc), so it
        // rides along inside the same `spawn_blocking` as `list_panes`.
        let panes: Vec<(usize, PaneInfo, Option<AgentKind>)> = spawn_blocking(move || {
            let mut out = Vec::new();
            for (i, backend) in backends.iter().enumerate() {
                let listed = backend.list_panes();
                let discovered: HashMap<String, AgentKind> =
                    muxa::discovery::discover_from_panes(&listed)
                        .into_iter()
                        .map(|d| (d.pane.pane_id, d.kind))
                        .collect();
                for pane in listed {
                    let kind = discovered.get(&pane.pane_id).copied();
                    out.push((i, pane, kind));
                }
            }
            out
        })
        .await
        .unwrap_or_default();

        panes
            .into_iter()
            .filter_map(|(i, p, discovered)| {
                let direct = self.manifests.manifest_for_command(&p.current_command);
                // The registry knows which agent muxa put on the pane; the
                // foreground command only knows which process is in front, and
                // for an npm install that is the `node` shim. So prefer the
                // registry — but only while the command still looks like an
                // agent host. A pane that fell back to a shell has lost its
                // agent, and must drop out of candidacy even though a row may
                // still (briefly) point at it.
                let registry = (direct.is_some()
                    || muxa::discovery::is_wrapper_command(&p.current_command))
                .then(|| by_pane.get(&p.pane_id).copied())
                .flatten();
                let by_kind = registry
                    .or(discovered)
                    .and_then(AgentKind::screen_manifest_name)
                    .and_then(|name| self.manifests.manifest_for_name(name));
                by_kind.or(direct).map(|m| (i, p, m.clone()))
            })
            .collect()
    }

    /// Capture, classify, and (on a state change) ingest one candidate pane.
    /// Returns `true` iff a state change was ingested. Skips panes a live hook
    /// owns (no capture) and classifications that don't move the state.
    /// `manifest` is the one `gather_candidates` already resolved for this
    /// pane's command, carried forward so there is no second lookup.
    async fn process_candidate(
        &mut self,
        backend_idx: usize,
        pane: &PaneInfo,
        manifest: &AgentManifest,
    ) -> bool {
        let pane_id = pane.pane_id.clone();

        let occupants = self.store.by_pane(&pane_id).await;
        let ownership = synthetic::pane_ownership(&occupants, OffsetDateTime::now_utc());
        if matches!(ownership, synthetic::PaneOwnership::Hooked) {
            // Hook-authoritative: a live real row owns the pane and its hooks
            // report every state — don't even capture. Forget any tracking so
            // a later stop-sweep doesn't touch the real row.
            self.tracked.remove(&pane_id);
            self.last_state.remove(&pane_id);
            return false;
        }

        let Some(raw) = self.capture(backend_idx, &pane_id).await else {
            return false;
        };
        let prepared = muxa::screen::prepare_capture(&raw, CAPTURE_TAIL_LINES);

        // `None` = unknown screen → keep previous state, no change.
        let Some(state) = manifest.classify(&prepared) else {
            return false;
        };

        if self.last_state.get(&pane_id) == Some(&state) {
            return false; // no change since last capture
        }
        self.last_state.insert(pane_id.clone(), state);

        if let synthetic::PaneOwnership::AttentionBlind {
            id,
            state: owner_state,
        } = ownership
        {
            // The owning agent's hooks cannot report attention, so this pane
            // gets the attention signal ONLY — applied to the real row, and
            // never tracked: `tracked` drives the stop-sweep, and a real row is
            // not ours to stop.
            let events = synthetic::attention_refinement_events(
                id,
                owner_state,
                to_synthetic(state),
                &manifest.name,
                None,
                OffsetDateTime::now_utc(),
            );
            if events.is_empty() {
                return false;
            }
            for ev in &events {
                self.store.apply(ev).await;
            }
            tracing::debug!(
                pane = %pane_id,
                agent = %manifest.name,
                ?state,
                "screen detection refined an attention-blind hook row",
            );
            return true;
        }

        let id = synthetic_id(pane);
        let events = synthetic::state_events(
            id,
            to_synthetic(state),
            &manifest.name,
            None,
            OffsetDateTime::now_utc(),
        );
        synthetic::apply_if_unowned(&self.store, &pane_id, &events).await;
        self.tracked.insert(pane_id);
        true
    }

    /// Capture one pane on the given backend, off the async runtime.
    async fn capture(&self, backend_idx: usize, pane_id: &str) -> Option<String> {
        let backend = self.backends[backend_idx].clone();
        let pane_id = pane_id.to_owned();
        spawn_blocking(move || backend.capture_pane(&pane_id))
            .await
            .ok()
            .flatten()
    }
}

/// Spawn the screen-detection task, but only when it can do useful work: the
/// feature is enabled, at least one detectable (non-herdr, capture-capable)
/// backend exists, and at least one manifest loaded. Returns the join handle so
/// the daemon can drain it on shutdown; `None` otherwise.
pub fn spawn_screen_detect_task(
    cfg: &Config,
    backends: &[SharedBackend],
    store: SharedStore,
    shutdown_tx: &broadcast::Sender<()>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !cfg.screen_detect.enabled {
        tracing::info!("screen detection disabled by config");
        return None;
    }
    let detectable = detectable_backends(backends);
    if detectable.is_empty() {
        tracing::debug!("screen detection: no non-herdr capture-capable backend; not spawning");
        return None;
    }
    let manifests = muxa::screen::load_manifests();
    if manifests.is_empty() {
        tracing::debug!("screen detection: no manifests loaded; not spawning");
        return None;
    }
    let interval = Duration::from_secs(cfg.screen_detect.interval_secs.max(1));
    tracing::info!(
        interval_secs = interval.as_secs(),
        manifests = manifests.len(),
        agents = ?manifests.names().collect::<Vec<_>>(),
        hosts = ?detectable.iter().map(muxa::PaneBackend::kind).collect::<Vec<_>>(),
        "screen detection enabled",
    );
    let detector = ScreenDetector {
        backends: detectable,
        store,
        manifests,
        interval,
        last_state: HashMap::new(),
        tracked: HashSet::new(),
    };
    Some(tokio::spawn(detector.run(shutdown_tx.subscribe())))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use muxa::backend::{BackendCaps, PaneObservation};
    use muxa::event::{AgentEvent, AgentId};
    use muxa::state::Store;
    use muxa::AgentState;
    use time::macros::datetime;

    use super::*;

    const AT: OffsetDateTime = datetime!(2026-07-20 12:00:00 UTC);

    /// A fake backend: a fixed pane inventory plus a pane-id → capture-text map
    /// that tests mutate between ticks to simulate the screen changing.
    struct FakeBackend {
        kind: HostKind,
        caps: BackendCaps,
        panes: Vec<PaneInfo>,
        captures: Arc<Mutex<HashMap<String, String>>>,
    }

    impl FakeBackend {
        fn tmux(panes: Vec<PaneInfo>, captures: Arc<Mutex<HashMap<String, String>>>) -> Self {
            Self {
                kind: HostKind::Tmux,
                caps: BackendCaps::default(),
                panes,
                captures,
            }
        }
    }

    impl PaneBackend for FakeBackend {
        fn kind(&self) -> HostKind {
            self.kind
        }
        fn list_panes(&self) -> Vec<PaneInfo> {
            self.panes.clone()
        }
        fn observe_panes(&self) -> PaneObservation {
            PaneObservation::complete(self.panes.clone())
        }
        fn resolve_pane(&self, id: &str) -> Option<PaneInfo> {
            self.panes.iter().find(|p| p.pane_id == id).cloned()
        }
        fn capture_pane(&self, pane_id: &str) -> Option<String> {
            self.captures.lock().unwrap().get(pane_id).cloned()
        }
        fn pane_pid_map(&self) -> HashMap<u32, String> {
            HashMap::new()
        }
        fn current_pane(&self) -> Option<String> {
            None
        }
        fn focus_pane(&self, _: &str) -> bool {
            true
        }
        fn caps(&self) -> BackendCaps {
            self.caps
        }
    }

    fn pane(pane_id: &str, command: &str) -> PaneInfo {
        PaneInfo {
            socket: Some("default".into()),
            pane_id: pane_id.into(),
            session_id: String::new(),
            session: "s".into(),
            window_id: String::new(),
            window_name: String::new(),
            window_index: "0".into(),
            pane_index: "0".into(),
            tty: String::new(),
            current_command: command.into(),
            title: String::new(),
            pane_pid: 0,
            current_path: String::new(),
        }
    }

    fn detector(backends: Vec<SharedBackend>, store: SharedStore) -> ScreenDetector {
        ScreenDetector {
            backends,
            store,
            manifests: muxa::screen::load_manifests(),
            interval: Duration::from_secs(3),
            last_state: HashMap::new(),
            tracked: HashSet::new(),
        }
    }

    #[test]
    fn detectable_backends_excludes_herdr_and_non_capturing() {
        let caps_no_capture = BackendCaps {
            capture_pane: false,
            ..BackendCaps::default()
        };
        let captures = Arc::new(Mutex::new(HashMap::new()));
        let tmux: SharedBackend = Arc::new(FakeBackend::tmux(vec![], captures.clone()));
        let herdr: SharedBackend = Arc::new(FakeBackend {
            kind: HostKind::Herdr,
            caps: BackendCaps::default(),
            panes: vec![],
            captures: captures.clone(),
        });
        let zellij_no_cap: SharedBackend = Arc::new(FakeBackend {
            kind: HostKind::Zellij,
            caps: caps_no_capture,
            panes: vec![],
            captures,
        });
        let out = detectable_backends(&[tmux, herdr, zellij_no_cap]);
        assert_eq!(
            out.len(),
            1,
            "only the capture-capable tmux backend remains"
        );
        assert_eq!(out[0].kind(), HostKind::Tmux);
    }

    /// The regression this pairs with the `STARTUP_ATTENTION_WINDOW` carve-out:
    /// an npm-installed codex runs as `node`, so command-based manifest
    /// selection never even considered the pane, and its startup gate — the one
    /// screen codex's own hooks structurally cannot report, because it paints
    /// before the session exists — showed up in muxa as a plain `idle` agent.
    ///
    /// Both halves have to hold for this to pass: the kind must select the
    /// manifest, and the fresh hooked row must still be refinable.
    #[tokio::test]
    async fn codex_startup_gate_is_detected_on_a_pane_running_as_node() {
        let captures = Arc::new(Mutex::new(HashMap::new()));
        captures.lock().unwrap().insert(
            "%1".into(),
            "  Do you trust this folder?\n› 1. Yes, continue\n  2. No, quit\n".into(),
        );
        // `node`, not `codex` — exactly what an npm install reports.
        let backend: SharedBackend = Arc::new(FakeBackend::tmux(
            vec![pane("%1", "node")],
            captures.clone(),
        ));
        let store = Store::shared();

        // A live codex row muxa just launched: Idle, because no hook has fired.
        let launched_at = OffsetDateTime::now_utc();
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::Codex,
                    session_id: "sess-1".into(),
                    surface: None,
                    pane: Some("%1".into()),
                    tmux_socket: None,
                    cwd: None,
                },
                at: launched_at,
            })
            .await;
        assert_eq!(store.by_pane("%1").await[0].state, AgentState::Idle);

        let mut det = detector(vec![backend], store.clone());
        assert_eq!(det.run_tick().await, 1, "the gate must be detected");

        let rows = store.by_pane("%1").await;
        assert_eq!(rows.len(), 1, "refinement targets the REAL row, adds none");
        assert_eq!(rows[0].kind, AgentKind::Codex);
        assert_eq!(
            rows[0].state,
            AgentState::WaitingInput,
            "a pane sitting on the trust gate is waiting on the operator",
        );
        assert!(
            !det.tracked.contains("%1"),
            "a real row is not ours to stop-sweep",
        );
    }

    #[tokio::test]
    async fn ingests_only_on_state_change() {
        let captures = Arc::new(Mutex::new(HashMap::new()));
        captures
            .lock()
            .unwrap()
            .insert("%1".into(), "⠋ Thinking\nesc to interrupt".into());
        let backend: SharedBackend = Arc::new(FakeBackend::tmux(
            vec![pane("%1", "cursor-agent")],
            captures.clone(),
        ));
        let store = Store::shared();
        let mut det = detector(vec![backend], store.clone());

        // First tick: a fresh Working row is created.
        assert_eq!(
            det.run_tick().await,
            1,
            "first working classification ingests"
        );
        let rows = store.by_pane("%1").await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, AgentState::Working);
        assert_eq!(rows[0].kind, AgentKind::Unknown);
        assert_eq!(rows[0].model.as_deref(), Some("cursor"));
        assert!(rows[0].session_id.starts_with("synthetic-"));

        // Second tick, same screen: no change ingested.
        assert_eq!(det.run_tick().await, 0, "unchanged screen is a no-op");

        // Screen flips to an approval prompt: one change.
        captures.lock().unwrap().insert(
            "%1".into(),
            "Do you want to allow this command? [y/n]".into(),
        );
        assert_eq!(det.run_tick().await, 1, "blocked classification ingests");
        assert_eq!(store.by_pane("%1").await[0].state, AgentState::WaitingInput);

        // Unknown screen: state is KEPT (previous WaitingInput), no change.
        captures
            .lock()
            .unwrap()
            .insert("%1".into(), "ordinary log output with no markers".into());
        assert_eq!(
            det.run_tick().await,
            0,
            "unknown screen keeps previous state"
        );
        assert_eq!(
            store.by_pane("%1").await[0].state,
            AgentState::WaitingInput,
            "state unchanged by an unrecognized screen",
        );
    }

    #[tokio::test]
    async fn skips_pane_owned_by_a_live_hook_row() {
        let captures = Arc::new(Mutex::new(HashMap::new()));
        captures
            .lock()
            .unwrap()
            .insert("%1".into(), "⠋ Thinking".into());
        let backend: SharedBackend = Arc::new(FakeBackend::tmux(
            vec![pane("%1", "cursor-agent")],
            captures,
        ));
        let store = Store::shared();
        // A real hook row claims the pane first.
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "real".into(),
                    surface: None,
                    pane: Some("%1".into()),
                    tmux_socket: Some("default".into()),
                    cwd: None,
                },
                at: AT,
            })
            .await;
        let mut det = detector(vec![backend], store.clone());
        assert_eq!(det.run_tick().await, 0, "hook-owned pane is skipped");
        let rows = store.by_pane("%1").await;
        assert_eq!(rows.len(), 1, "no synthetic row added");
        assert_eq!(rows[0].session_id, "real");
    }

    /// A hook-owned agy pane is the one exception to "hooks own the pane": agy
    /// has no permission hook, so screen inference supplies `WaitingInput` and
    /// nothing else. Driven through the real detector loop with the bundled
    /// `agy` manifest and captures taken from a live agy 1.1.17 pane.
    #[tokio::test]
    async fn refines_attention_on_a_hook_owned_agy_pane() {
        let captures = Arc::new(Mutex::new(HashMap::new()));
        captures.lock().unwrap().insert(
            "%1".into(),
            "⣷  Running command...\n>\nesc to cancel".into(),
        );
        let backend: SharedBackend =
            Arc::new(FakeBackend::tmux(vec![pane("%1", "agy")], captures.clone()));

        let store = Store::shared();
        let real = AgentId {
            kind: AgentKind::Antigravity,
            session_id: "conv-1".into(),
            surface: None,
            pane: Some("%1".into()),
            tmux_socket: Some("default".into()),
            cwd: None,
        };
        // Hooks put the row in Working and stamp the model, as they would.
        store
            .apply(&AgentEvent::Started {
                id: real.clone(),
                at: AT,
            })
            .await;
        store
            .apply(&AgentEvent::Heartbeat {
                id: real.clone(),
                model: Some("gemini-3.7-flash-high".into()),
                context_used_pct: None,
                cost_usd: None,
                rate_limit_5h_pct: None,
                rate_limit_5h_resets_at: None,
                rate_limit_7d_pct: None,
                rate_limit_7d_resets_at: None,
                at: AT,
            })
            .await;
        store
            .apply(&AgentEvent::ToolStarted {
                id: real.clone(),
                tool: "run_command".into(),
                subagent: None,
                at: AT,
            })
            .await;

        let mut det = detector(vec![backend], store.clone());

        // A working screen adds nothing — hooks already own that transition.
        assert_eq!(det.run_tick().await, 0);
        assert_eq!(store.by_pane("%1").await[0].state, AgentState::Working);

        // agy puts its permission widget up. Nothing in its hook stream can
        // say so, so this tick is the only way the row learns it.
        captures.lock().unwrap().insert(
            "%1".into(),
            "Run command?\n> Yes, and always allow for commands that start with 'echo'\n  No, deny"
                .into(),
        );
        assert_eq!(det.run_tick().await, 1);
        let rows = store.by_pane("%1").await;
        assert_eq!(rows.len(), 1, "refinement must not mint a second row");
        assert_eq!(rows[0].session_id, "conv-1");
        assert_eq!(rows[0].state, AgentState::WaitingInput);
        assert_eq!(
            rows[0].model.as_deref(),
            Some("gemini-3.7-flash-high"),
            "the hook-supplied model must survive",
        );

        // The operator approves; agy's PostToolUse fires and releases the row
        // without any help from the screen.
        store
            .apply(&AgentEvent::ToolCompleted {
                id: real,
                tool: "run_command".into(),
                success: true,
                at: AT,
            })
            .await;
        assert_eq!(store.by_pane("%1").await[0].state, AgentState::Working);
    }

    /// The safety valve: if the hook stream stops arriving while the row sits
    /// at `WaitingInput`, an idle screen releases it rather than freezing it.
    #[tokio::test]
    async fn idle_screen_releases_a_stuck_agy_wait() {
        let captures = Arc::new(Mutex::new(HashMap::new()));
        captures.lock().unwrap().insert(
            "%1".into(),
            "Allow access to this file?\n> Yes, allow access\n  No, deny access".into(),
        );
        let backend: SharedBackend =
            Arc::new(FakeBackend::tmux(vec![pane("%1", "agy")], captures.clone()));
        let store = Store::shared();
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::Antigravity,
                    session_id: "conv-1".into(),
                    surface: None,
                    pane: Some("%1".into()),
                    tmux_socket: Some("default".into()),
                    cwd: None,
                },
                at: AT,
            })
            .await;

        let mut det = detector(vec![backend], store.clone());
        assert_eq!(det.run_tick().await, 1);
        assert_eq!(store.by_pane("%1").await[0].state, AgentState::WaitingInput);

        // No further hook events ever arrive; the prompt is gone from screen.
        captures
            .lock()
            .unwrap()
            .insert("%1".into(), "? for shortcuts".into());
        assert_eq!(det.run_tick().await, 1);
        assert_eq!(store.by_pane("%1").await[0].state, AgentState::Idle);
    }

    /// A hook-owned agy pane must never be `tracked`: `tracked` drives the
    /// stop-sweep, and driving a REAL row to `Stopped` because its foreground
    /// command changed is not screen detection's call.
    #[tokio::test]
    async fn a_refined_pane_is_never_stopped_by_the_sweep() {
        let captures = Arc::new(Mutex::new(HashMap::new()));
        captures.lock().unwrap().insert(
            "%1".into(),
            "Run command?\n> Yes, and always allow\n  No, deny".into(),
        );
        let panes = Arc::new(Mutex::new(vec![pane("%1", "agy")]));
        let backend: SharedBackend = Arc::new(FakeBackend::tmux(
            panes.lock().unwrap().clone(),
            captures.clone(),
        ));
        let store = Store::shared();
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::Antigravity,
                    session_id: "conv-1".into(),
                    surface: None,
                    pane: Some("%1".into()),
                    tmux_socket: Some("default".into()),
                    cwd: None,
                },
                at: AT,
            })
            .await;
        let mut det = detector(vec![backend], store.clone());
        assert_eq!(det.run_tick().await, 1);
        assert!(
            !det.tracked.contains("%1"),
            "a real row is not ours to track or stop",
        );

        // The pane drops out of candidacy: the sweep must leave the real row be.
        let empty: SharedBackend = Arc::new(FakeBackend::tmux(
            vec![pane("%1", "bash")],
            Arc::new(Mutex::new(HashMap::new())),
        ));
        det.backends = vec![empty];
        det.run_tick().await;
        let rows = store.by_pane("%1").await;
        assert_eq!(rows[0].session_id, "conv-1");
        assert_ne!(
            rows[0].state,
            AgentState::Stopped,
            "the sweep must not stop a real row",
        );
        assert!(
            !det.last_state.contains_key("%1"),
            "last_state is pruned for refined panes too, not just tracked ones",
        );
    }

    #[tokio::test]
    async fn non_agent_panes_are_not_candidates() {
        let captures = Arc::new(Mutex::new(HashMap::new()));
        // A plain shell pane — no manifest matches "bash".
        let backend: SharedBackend =
            Arc::new(FakeBackend::tmux(vec![pane("%1", "bash")], captures));
        let store = Store::shared();
        let mut det = detector(vec![backend], store.clone());
        assert_eq!(det.run_tick().await, 0);
        assert!(
            store.by_pane("%1").await.is_empty(),
            "no row for a shell pane"
        );
    }

    #[tokio::test]
    async fn agent_leaving_stops_the_synthetic_row() {
        let captures = Arc::new(Mutex::new(HashMap::new()));
        captures
            .lock()
            .unwrap()
            .insert("%1".into(), "⠋ Thinking".into());
        // Tick 1 with the agent present.
        let store = Store::shared();
        let backend_present: SharedBackend = Arc::new(FakeBackend::tmux(
            vec![pane("%1", "cursor-agent")],
            captures.clone(),
        ));
        let mut det = detector(vec![backend_present], store.clone());
        assert_eq!(det.run_tick().await, 1);
        assert_eq!(store.by_pane("%1").await[0].state, AgentState::Working);

        // Tick 2: the pane's foreground command is now a shell (agent exited),
        // so it's no longer a candidate. The synthetic row must be stopped.
        let backend_gone: SharedBackend =
            Arc::new(FakeBackend::tmux(vec![pane("%1", "bash")], captures));
        det.backends = vec![backend_gone];
        assert_eq!(det.run_tick().await, 0);
        assert_eq!(
            store.by_pane("%1").await[0].state,
            AgentState::Stopped,
            "row driven to Stopped when the agent leaves the pane",
        );
    }
}
