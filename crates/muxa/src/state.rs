//! In-memory agent registry.
//!
//! Events flow in, `Agent` rows are updated. No persistence — a daemon
//! restart drops state, and adapters re-announce on the next event.
//!
//! Concurrency: a single `tokio::sync::RwLock` guards the registry. This is
//! fine at the event rates we expect (tens/sec peak); revisit if profiling
//! shows contention.
//!
//! State-change fanout: the store owns a `tokio::sync::broadcast` channel
//! that emits a `Transition` on every `state`-field change. This is an
//! **in-process** signal only — it is not exposed over IPC — and is used
//! by the daemon's desktop-notifier task to wake users when an agent moves
//! into `WaitingInput` or `Error`.

use crate::event::{AgentEvent, AgentId, AgentKind, AgentState, NotificationLevel};
use crate::history::{HistoryEntry, HistoryOptions, PromptHistory};
use crate::tmux::PaneInfo;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::sync::{broadcast, RwLock};

/// Prefix used by `muxa sync` / startup discovery for the `session_id` of a
/// synthesized `Started` event. The store recognizes this prefix to keep
/// dedup honest: a real hook event arriving for the same `(kind, pane)`
/// replaces the synthetic placeholder rather than racing it.
///
/// Kept here (not in the runtime crate) so the no-I/O store layer can dedup
/// without taking a cross-crate dependency on the discovery module.
pub const SYNTHETIC_SESSION_PREFIX: &str = "synthetic-";

fn is_synthetic(session_id: &str) -> bool {
    session_id.starts_with(SYNTHETIC_SESSION_PREFIX)
}

/// Capacity of the in-process state-transition broadcast. Slow subscribers
/// that lag past this see `RecvError::Lagged` and should resync via
/// `Store::snapshot` — the notifier task logs and continues; the dashboard
/// SSE handler emits an `event: lagged` so its client can do the same.
///
/// Sized for the dashboard case: a long-running SSE connection over a
/// brief burst (e.g. a refactor touching many panes at once) should not
/// see lag on a healthy network. 256 is ~4× the in-process notifier's
/// previous capacity; bump again if profiling shows lag on real traffic.
const TRANSITION_CHANNEL_CAPACITY: usize = 256;

/// Capacity of the in-process prompt broadcast. Sized identically to the
/// transition channel — sinks subscribe here to receive every
/// `PromptSubmitted` event without having to reverse-engineer prompts
/// from `Transition` payloads. Slow subscribers see `RecvError::Lagged`
/// and should log + continue (same pattern as the notifier).
const PROMPT_CHANNEL_CAPACITY: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub kind: AgentKind,
    pub session_id: String,
    pub pane: Option<String>,
    pub cwd: Option<String>,
    pub state: AgentState,
    pub last_prompt: Option<String>,
    /// Last assistant response captured for this agent. Populated by the
    /// `TurnStopped` ingest path when the adapter could read the
    /// transcript; remains `None` for adapters that don't expose response
    /// text (e.g., Codex/Gemini today). Optional so the field is purely
    /// additive on the wire and in the UI.
    pub last_response: Option<String>,
    pub last_notification: Option<String>,
    pub model: Option<String>,
    pub context_used_pct: Option<f32>,
    pub cost_usd: Option<f64>,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub last_activity_at: OffsetDateTime,
}

impl Agent {
    fn new(
        kind: AgentKind,
        session_id: String,
        pane: Option<String>,
        cwd: Option<String>,
        at: OffsetDateTime,
    ) -> Self {
        Self {
            kind,
            session_id,
            pane,
            cwd,
            state: AgentState::Starting,
            last_prompt: None,
            last_response: None,
            last_notification: None,
            model: None,
            context_used_pct: None,
            cost_usd: None,
            started_at: at,
            last_activity_at: at,
        }
    }
}

/// In-process notification emitted when an agent's `state` field changes.
///
/// Not part of the IPC protocol — consumers must live in the daemon
/// process. `agent` is the post-transition snapshot, suitable for rendering
/// UI (desktop notification body, log line, etc.) without racing further
/// mutations.
#[derive(Debug, Clone, Serialize)]
pub struct Transition {
    pub from: AgentState,
    pub to: AgentState,
    pub agent: Agent,
}

/// In-process record emitted whenever a `PromptSubmitted` event lands.
///
/// Sibling to [`Transition`] — sinks subscribe via
/// [`Store::subscribe_prompts`] and forward these records to external
/// systems (e.g. `oh-my-prompt`'s ingestion API). The `model` field is a
/// best-effort snapshot from the post-apply agent row at the time of the
/// prompt — `None` when no Heartbeat has populated it yet.
#[derive(Debug, Clone)]
pub struct PromptRecord {
    pub id: AgentId,
    pub prompt: String,
    pub at: OffsetDateTime,
    pub model: Option<String>,
}

pub struct Store {
    agents: RwLock<HashMap<String, Agent>>,
    transitions: broadcast::Sender<Transition>,
    prompts: broadcast::Sender<PromptRecord>,
    /// Disk-backed audit log of every prompt. Lives separately from
    /// `agents` so reaping/GC of the live registry doesn't take prompt
    /// history with it. Tests and consumers that want a no-op history use
    /// [`PromptHistory::in_memory_only`].
    history: Arc<PromptHistory>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("agents", &self.agents)
            .field("history_path", &self.history.options().path)
            .finish_non_exhaustive()
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::with_history(PromptHistory::in_memory_only(HistoryOptions::default()))
    }
}

impl Store {
    /// Build a store wired to a specific [`PromptHistory`]. Daemon uses
    /// this to plug in a disk-backed instance; tests use the default
    /// `Store::default()` / `Store::shared()` which wire an in-memory-only
    /// history.
    pub fn with_history(history: Arc<PromptHistory>) -> Self {
        let (tx, _) = broadcast::channel(TRANSITION_CHANNEL_CAPACITY);
        let (prompts_tx, _) = broadcast::channel(PROMPT_CHANNEL_CAPACITY);
        Self {
            agents: RwLock::default(),
            transitions: tx,
            prompts: prompts_tx,
            history,
        }
    }

    /// `Arc`-wrapped variant of [`Self::with_history`] mirroring
    /// [`Self::shared`].
    pub fn shared_with_history(history: Arc<PromptHistory>) -> SharedStore {
        Arc::new(Self::with_history(history))
    }
}

pub type SharedStore = Arc<Store>;

/// Apply one event's mutations to a single agent row, returning side
/// effects for the caller to fire after dropping the agents write lock.
///
/// Pulled out of [`Store::apply`] so the state-transition switch isn't
/// buried inside the lock-management dance — easier to read, clippy-clean
/// (`apply` stays under the line-count threshold), and one focused place
/// to look when adding a new event variant.
fn mutate_for_event(
    agent: &mut Agent,
    ev: &AgentEvent,
    id: &AgentId,
    at: OffsetDateTime,
) -> (Option<PromptRecord>, Option<HistoryEntry>) {
    let mut prompt_record: Option<PromptRecord> = None;
    let mut history_entry: Option<HistoryEntry> = None;

    match ev {
        AgentEvent::Started { .. } => {
            agent.state = AgentState::Idle;
        }
        AgentEvent::PromptSubmitted { prompt, .. } => {
            agent.last_prompt = Some(prompt.clone());
            agent.state = AgentState::Working;
            prompt_record = Some(PromptRecord {
                id: id.clone(),
                prompt: prompt.clone(),
                at,
                model: agent.model.clone(),
            });
            if let Some(pane) = agent.pane.clone() {
                history_entry = Some(HistoryEntry::new(
                    agent.kind,
                    agent.session_id.clone(),
                    pane,
                    prompt.clone(),
                    at,
                    agent.model.clone(),
                ));
            }
        }
        AgentEvent::ToolStarted { .. } => {
            agent.state = AgentState::Working;
        }
        AgentEvent::ToolCompleted { .. } => { /* state unchanged */ }
        AgentEvent::NotificationFired { level, message, .. } => {
            agent.last_notification = Some(message.clone());
            match level {
                NotificationLevel::NeedsInput => agent.state = AgentState::WaitingInput,
                NotificationLevel::Error => agent.state = AgentState::Error,
                NotificationLevel::Info | NotificationLevel::Warning => {}
            }
        }
        AgentEvent::TurnStopped { response, .. } => {
            if let Some(text) = response {
                agent.last_response = Some(text.clone());
            }
            if agent.state != AgentState::Error {
                agent.state = AgentState::Idle;
            }
        }
        AgentEvent::SessionEnded { .. } => {
            agent.state = AgentState::Stopped;
        }
        AgentEvent::Heartbeat {
            model,
            context_used_pct,
            cost_usd,
            ..
        } => {
            if let Some(m) = model {
                agent.model = Some(m.clone());
            }
            if let Some(p) = context_used_pct {
                agent.context_used_pct = Some(*p);
            }
            if let Some(c) = cost_usd {
                agent.cost_usd = Some(*c);
            }
        }
    }

    (prompt_record, history_entry)
}

/// Reconcile pane occupants for an incoming `Started` event.
///
/// Returns `false` when the event should be dropped (re-running `muxa sync`
/// against a pane that's already represented). Otherwise the map has been
/// updated to make room for the new agent:
///
/// * Synthetic placeholders are only allowed when no other record — real or
///   synthetic, alive or stopped — already owns the pane. Once a real
///   session has touched a pane, its identity is sticky: even after it
///   becomes `Stopped` we still prefer to surface the real history rather
///   than have a sync pass overwrite it with a generic placeholder.
/// * Real `Started` events drop any synthetic placeholders for the same
///   pane outright, since the real session is now authoritative.
/// * Other active sessions sharing the pane are flipped to `Stopped` (the
///   user launched a fresh agent in the same pane and the previous session
///   never emitted `SessionEnd`). Stopped predecessors are left alone here;
///   the periodic reconciler collapses them later.
fn reconcile_pane_for_started(
    agents: &mut HashMap<String, Agent>,
    incoming_session: &str,
    pane: &str,
    at: OffsetDateTime,
) -> bool {
    if is_synthetic(incoming_session) {
        // Reject the placeholder if any *other* session — real or synthetic,
        // any state — already owns this pane. Re-syncing the same synthetic
        // session id is still allowed because the entry-or-insert path that
        // follows is a no-op upsert in that case.
        let already_owned = agents
            .values()
            .any(|a| a.session_id != incoming_session && a.pane.as_deref() == Some(pane));
        if already_owned {
            return false;
        }
    } else {
        // Real Started — drop synthetic placeholders for this pane outright.
        agents.retain(|_, a| !(a.pane.as_deref() == Some(pane) && is_synthetic(&a.session_id)));
    }

    for other in agents.values_mut() {
        if other.session_id != incoming_session
            && other.pane.as_deref() == Some(pane)
            && other.state != AgentState::Stopped
        {
            other.state = AgentState::Stopped;
            other.last_activity_at = at;
        }
    }
    true
}

impl Store {
    pub fn shared() -> SharedStore {
        Arc::new(Self::default())
    }

    /// Subscribe to in-process state transitions.
    ///
    /// Returns a fresh receiver; each subscriber has an independent cursor.
    /// Callers should handle `broadcast::error::RecvError::Lagged` by
    /// resyncing from `snapshot()` rather than treating it as fatal.
    pub fn subscribe(&self) -> broadcast::Receiver<Transition> {
        self.transitions.subscribe()
    }

    /// Subscribe to in-process prompt events.
    ///
    /// One [`PromptRecord`] is broadcast per `PromptSubmitted` event the
    /// store applies. Independent of the state-transition channel so
    /// downstream sinks see every prompt — even prompts that don't change
    /// the agent's state field. Same `Lagged` semantics as
    /// [`Self::subscribe`].
    pub fn subscribe_prompts(&self) -> broadcast::Receiver<PromptRecord> {
        self.prompts.subscribe()
    }

    pub async fn apply(&self, ev: &AgentEvent) {
        let mut agents = self.agents.write().await;
        let id = ev.id();
        let at = ev.at();

        if matches!(ev, AgentEvent::Started { .. }) {
            if let Some(pane) = id.pane.as_deref() {
                if !reconcile_pane_for_started(&mut agents, &id.session_id, pane, at) {
                    return;
                }
            }
        }

        let agent = agents.entry(id.session_id.clone()).or_insert_with(|| {
            Agent::new(
                id.kind,
                id.session_id.clone(),
                id.pane.clone(),
                id.cwd.clone(),
                at,
            )
        });

        // Keep identity fields fresh — adapters may re-send with more info.
        if agent.pane.is_none() {
            agent.pane.clone_from(&id.pane);
        }
        if agent.cwd.is_none() {
            agent.cwd.clone_from(&id.cwd);
        }
        agent.last_activity_at = at;

        let prev_state = agent.state;
        let (prompt_record, history_entry) = mutate_for_event(agent, ev, id, at);

        if agent.state != prev_state {
            let transition = Transition {
                from: prev_state,
                to: agent.state,
                agent: agent.clone(),
            };
            // `send` errors only when there are zero subscribers — that's
            // the common case (notifier disabled) and not worth logging.
            let _ = self.transitions.send(transition);
        }

        if let Some(record) = prompt_record {
            // Same "errors only when no subscribers" semantics as the
            // transitions channel — sinks are opt-in, so a no-subscriber
            // state is the steady-state default.
            let _ = self.prompts.send(record);
        }

        // Drop the agents write guard before touching history so the two
        // locks are never held in nested order — keeps the lock-acquisition
        // graph linear and the deadlock surface zero.
        drop(agents);
        if let Some(entry) = history_entry {
            self.history.append(entry).await;
        }
    }

    pub async fn snapshot(&self) -> Vec<Agent> {
        self.agents.read().await.values().cloned().collect()
    }

    pub async fn by_pane(&self, pane: &str) -> Vec<Agent> {
        self.agents
            .read()
            .await
            .values()
            .filter(|a| a.pane.as_deref() == Some(pane))
            .cloned()
            .collect()
    }

    pub async fn by_session(&self, session_id: &str) -> Option<Agent> {
        self.agents.read().await.get(session_id).cloned()
    }

    /// Most-recent-first prompt history. `pane = None` returns prompts
    /// across all panes; `limit = 0` returns everything available. The
    /// store always has a [`PromptHistory`] (default: in-memory only), so
    /// this is safe to call regardless of `[history]` config.
    pub async fn recent_prompts(&self, pane: Option<&str>, limit: usize) -> Vec<HistoryEntry> {
        match pane {
            Some(p) => self.history.recent_for_pane(p, limit).await,
            None => self.history.recent_all(limit).await,
        }
    }

    /// Borrow the [`PromptHistory`] handle. The daemon uses this to drive
    /// the periodic compaction task without re-plumbing the registry.
    pub fn history(&self) -> &Arc<PromptHistory> {
        &self.history
    }

    /// Remove agents that ended more than `max_age` ago. Caller decides
    /// cadence; daemon runs this on a timer.
    pub async fn gc(&self, max_age: time::Duration) -> usize {
        let cutoff = OffsetDateTime::now_utc() - max_age;
        let mut agents = self.agents.write().await;
        let before = agents.len();
        agents.retain(|_, a| a.state != AgentState::Stopped || a.last_activity_at >= cutoff);
        before - agents.len()
    }

    /// Converge the registry against ground truth from tmux.
    ///
    /// This is the periodic control-loop pass — analogous to a Kubernetes
    /// controller's reconcile step. It consumes a snapshot of currently
    /// live tmux panes and drives the registry toward an invariant state:
    ///
    /// 1. **Reap stale**: any agent whose pane no longer exists in the
    ///    snapshot is dropped. Without this the registry accumulates
    ///    forever as users close panes.
    /// 2. **Demote synthetic**: synthetic placeholders for panes that also
    ///    have a real session are dropped — the real entry is always the
    ///    authoritative one.
    /// 3. **Collapse duplicates**: when more than one record points at the
    ///    same live pane, pick a single canonical winner and drop the rest.
    ///    Priority: real beats synthetic, alive beats stopped, then most
    ///    recent activity. The losers' history is already published on the
    ///    `prompts` and `transitions` broadcast channels for any sink that
    ///    needs to persist it — keeping them in the live registry just
    ///    pollutes the picker view.
    ///
    /// Idempotent and safe to run on a timer regardless of event traffic.
    /// Returns counts so callers can log non-trivial sweeps without spamming.
    pub async fn reconcile(&self, live_panes: &[PaneInfo]) -> ReconcileReport {
        let live: HashSet<&str> = live_panes.iter().map(|p| p.pane_id.as_str()).collect();
        let mut agents = self.agents.write().await;
        let mut report = ReconcileReport::default();

        // Sweep 1: drop agents whose pane is gone. Done as a single retain
        // pass to avoid building an intermediate Vec of doomed session ids.
        let before = agents.len();
        agents.retain(|_, a| match a.pane.as_deref() {
            Some(pane_id) => live.contains(pane_id),
            None => true, // paneless agents (rare) aren't governed by tmux
        });
        report.stale_panes_reaped = before - agents.len();

        // Sweeps 2 & 3: per-pane dedup. Group surviving agents by pane id.
        let mut by_pane: HashMap<&str, Vec<String>> = HashMap::new();
        for (sid, a) in agents.iter() {
            if let Some(p) = a.pane.as_deref() {
                by_pane.entry(p).or_default().push(sid.clone());
            }
        }

        let mut to_remove: Vec<(String, RemovalReason)> = Vec::new();
        for sids in by_pane.values() {
            if sids.len() <= 1 {
                continue;
            }
            // Build comparable rank tuples. Higher tuple = better.
            let mut ranked: Vec<(String, (u8, u8), OffsetDateTime, bool)> = sids
                .iter()
                .filter_map(|sid| {
                    let a = agents.get(sid)?;
                    let real = !is_synthetic(sid);
                    let alive = a.state != AgentState::Stopped;
                    Some((
                        sid.clone(),
                        (u8::from(real), u8::from(alive)),
                        a.last_activity_at,
                        real,
                    ))
                })
                .collect();
            // Sort so the canonical winner is at index 0:
            //   primary: (real, alive) tuple descending — (1,1) > (1,0) > (0,1) > (0,0)
            //   secondary: last_activity_at descending — most recent first
            ranked.sort_by(|a, b| match b.1.cmp(&a.1) {
                Ordering::Equal => b.2.cmp(&a.2),
                other => other,
            });
            for loser in ranked.into_iter().skip(1) {
                let reason = if loser.3 {
                    RemovalReason::DuplicateCollapsed
                } else {
                    RemovalReason::SyntheticDemoted
                };
                to_remove.push((loser.0, reason));
            }
        }

        for (sid, reason) in to_remove {
            agents.remove(&sid);
            match reason {
                RemovalReason::SyntheticDemoted => report.synthetic_demoted += 1,
                RemovalReason::DuplicateCollapsed => report.duplicates_collapsed += 1,
            }
        }

        report
    }
}

/// Outcome of one [`Store::reconcile`] pass. Each counter tracks one
/// invariant the reconciler enforces; together they describe how far the
/// registry had drifted from ground truth before the call.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Records dropped because their pane is no longer in tmux.
    pub stale_panes_reaped: usize,
    /// Synthetic placeholders dropped because a real entry owns the same pane.
    pub synthetic_demoted: usize,
    /// Older / duplicate real records collapsed onto the canonical row.
    pub duplicates_collapsed: usize,
}

impl ReconcileReport {
    /// Total number of agents removed in this pass.
    pub fn total_removed(&self) -> usize {
        self.stale_panes_reaped + self.synthetic_demoted + self.duplicates_collapsed
    }

    /// True when this pass changed the registry — handy for log-on-change
    /// patterns so quiet steady-state passes don't flood the log.
    pub fn is_noop(&self) -> bool {
        self.total_removed() == 0
    }
}

#[derive(Debug, Clone, Copy)]
enum RemovalReason {
    SyntheticDemoted,
    DuplicateCollapsed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, AgentId, AgentKind, NotificationLevel};
    use time::macros::datetime;

    fn id(session: &str) -> AgentId {
        AgentId {
            kind: AgentKind::ClaudeCode,
            session_id: session.into(),
            pane: Some("%1".into()),
            cwd: None,
        }
    }

    #[tokio::test]
    async fn lifecycle_transitions() {
        let store = Store::shared();
        let now = datetime!(2026-04-24 12:00:00 UTC);

        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: now,
            })
            .await;
        assert_eq!(store.by_session("s").await.unwrap().state, AgentState::Idle);

        store
            .apply(&AgentEvent::PromptSubmitted {
                id: id("s"),
                prompt: "hi".into(),
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("s").await.unwrap().state,
            AgentState::Working
        );

        store
            .apply(&AgentEvent::NotificationFired {
                id: id("s"),
                level: NotificationLevel::NeedsInput,
                message: "?".into(),
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("s").await.unwrap().state,
            AgentState::WaitingInput
        );

        store
            .apply(&AgentEvent::TurnStopped {
                id: id("s"),
                response: None,
                at: now,
            })
            .await;
        assert_eq!(store.by_session("s").await.unwrap().state, AgentState::Idle);

        store
            .apply(&AgentEvent::SessionEnded {
                id: id("s"),
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("s").await.unwrap().state,
            AgentState::Stopped
        );
    }

    #[tokio::test]
    async fn turn_stopped_with_response_sets_last_response() {
        let store = Store::shared();
        let now = datetime!(2026-04-24 12:00:00 UTC);

        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: now,
            })
            .await;
        store
            .apply(&AgentEvent::TurnStopped {
                id: id("s"),
                response: Some("the assistant said hi".into()),
                at: now,
            })
            .await;
        let agent = store.by_session("s").await.unwrap();
        assert_eq!(
            agent.last_response.as_deref(),
            Some("the assistant said hi")
        );
        // Idle transition still happens.
        assert_eq!(agent.state, AgentState::Idle);
    }

    #[tokio::test]
    async fn turn_stopped_without_response_preserves_prior_value() {
        let store = Store::shared();
        let now = datetime!(2026-04-24 12:00:00 UTC);

        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: now,
            })
            .await;
        store
            .apply(&AgentEvent::TurnStopped {
                id: id("s"),
                response: Some("first answer".into()),
                at: now,
            })
            .await;
        // A subsequent TurnStopped from an adapter that can't read
        // transcripts (Codex/Gemini) must not blank the previously
        // captured response.
        store
            .apply(&AgentEvent::TurnStopped {
                id: id("s"),
                response: None,
                at: now,
            })
            .await;
        let agent = store.by_session("s").await.unwrap();
        assert_eq!(agent.last_response.as_deref(), Some("first answer"));
    }

    #[tokio::test]
    async fn started_dedupes_previous_session_in_same_pane() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        let t1 = datetime!(2026-04-24 12:05:00 UTC);

        // First session starts on pane %1 and is happily working.
        store
            .apply(&AgentEvent::Started {
                id: id("first"),
                at: t0,
            })
            .await;
        store
            .apply(&AgentEvent::PromptSubmitted {
                id: id("first"),
                prompt: "do a thing".into(),
                at: t0,
            })
            .await;
        assert_eq!(
            store.by_session("first").await.unwrap().state,
            AgentState::Working
        );

        // A fresh session opens in the same pane — e.g. user closed the old
        // agent and launched a new one without the adapter ever seeing a
        // SessionEnded. The old row must flip to Stopped.
        store
            .apply(&AgentEvent::Started {
                id: id("second"),
                at: t1,
            })
            .await;

        assert_eq!(
            store.by_session("first").await.unwrap().state,
            AgentState::Stopped
        );
        assert_eq!(
            store.by_session("second").await.unwrap().state,
            AgentState::Idle
        );
    }

    #[tokio::test]
    async fn gc_removes_old_stopped_agents() {
        let store = Store::shared();
        let stale = OffsetDateTime::now_utc() - time::Duration::hours(2);
        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: stale,
            })
            .await;
        store
            .apply(&AgentEvent::SessionEnded {
                id: id("s"),
                at: stale,
            })
            .await;
        let removed = store.gc(time::Duration::hours(1)).await;
        assert_eq!(removed, 1);
        assert!(store.by_session("s").await.is_none());
    }

    #[tokio::test]
    async fn subscribe_receives_state_transition() {
        let store = Store::shared();
        let now = datetime!(2026-04-24 12:00:00 UTC);

        // Subscribe BEFORE applying; otherwise the event is missed.
        let mut rx = store.subscribe();

        // Seed the agent so its prior state is known (Starting -> Idle on
        // Started). Drain that transition so the assertion below targets
        // the one we care about.
        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: now,
            })
            .await;
        let first = rx.recv().await.unwrap();
        assert_eq!(first.from, AgentState::Starting);
        assert_eq!(first.to, AgentState::Idle);

        store
            .apply(&AgentEvent::PromptSubmitted {
                id: id("s"),
                prompt: "hello".into(),
                at: now,
            })
            .await;

        let t = rx.recv().await.unwrap();
        assert_eq!(t.from, AgentState::Idle);
        assert_eq!(t.to, AgentState::Working);
        assert_eq!(t.agent.session_id, "s");
        assert_eq!(t.agent.last_prompt.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn synthetic_started_idempotent_on_same_pane() {
        let store = Store::shared();
        let now = datetime!(2026-04-24 12:00:00 UTC);

        // First synthetic from `muxa sync` lands an Idle agent on %1.
        let synthetic = AgentId {
            kind: AgentKind::ClaudeCode,
            session_id: "synthetic-%1".into(),
            pane: Some("%1".into()),
            cwd: None,
        };
        store
            .apply(&AgentEvent::Started {
                id: synthetic.clone(),
                at: now,
            })
            .await;
        assert_eq!(store.snapshot().await.len(), 1);

        // Re-running discovery must not create a duplicate or wipe the
        // first entry's started_at — it's a no-op.
        let later = datetime!(2026-04-24 12:30:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: synthetic,
                at: later,
            })
            .await;
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].started_at, now);
    }

    #[tokio::test]
    async fn real_started_replaces_synthetic_on_same_pane() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        let t1 = datetime!(2026-04-24 12:01:00 UTC);

        // Discovery synthesizes a placeholder.
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "synthetic-%7".into(),
                    pane: Some("%7".into()),
                    cwd: None,
                },
                at: t0,
            })
            .await;

        // A real hook arrives — same pane, real session id. The synthetic
        // should be replaced (gone), leaving exactly one entry under the
        // canonical session id.
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "real-sess".into(),
                    pane: Some("%7".into()),
                    cwd: Some("/work".into()),
                },
                at: t1,
            })
            .await;

        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1, "synthetic should have been removed");
        assert_eq!(snap[0].session_id, "real-sess");
        assert_eq!(snap[0].cwd.as_deref(), Some("/work"));
        assert_eq!(snap[0].state, AgentState::Idle);
        assert!(store.by_session("synthetic-%7").await.is_none());
    }

    #[tokio::test]
    async fn synthetic_skipped_when_real_agent_present() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);

        // Real agent already known via a prior hook.
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "real-sess".into(),
                    pane: Some("%9".into()),
                    cwd: None,
                },
                at: t0,
            })
            .await;

        // `muxa sync` runs and tries to backfill the same pane.
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "synthetic-%9".into(),
                    pane: Some("%9".into()),
                    cwd: None,
                },
                at: t0,
            })
            .await;

        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].session_id, "real-sess");
    }

    #[tokio::test]
    async fn no_transition_when_state_unchanged() {
        let store = Store::shared();
        let now = datetime!(2026-04-24 12:00:00 UTC);

        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: now,
            })
            .await;

        let mut rx = store.subscribe();

        // Heartbeat updates metadata only — no state change, no broadcast.
        store
            .apply(&AgentEvent::Heartbeat {
                id: id("s"),
                model: Some("Opus".into()),
                context_used_pct: None,
                cost_usd: None,
                at: now,
            })
            .await;

        // A 50ms window is plenty — the send is synchronous-ish (tokio
        // broadcast send is non-blocking) and we're on a single runtime.
        let res = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(res.is_err(), "expected no transition, got {res:?}");
    }

    fn pane(id: &str) -> PaneInfo {
        PaneInfo {
            pane_id: id.into(),
            session: "s".into(),
            window_index: "0".into(),
            pane_index: "0".into(),
            tty: String::new(),
            current_command: "claude".into(),
            title: String::new(),
        }
    }

    /// Synthetic must lose to a real session even when the real session is
    /// already `Stopped`. Without this rule, a `muxa sync` pass after the
    /// real agent ended would re-introduce a synthetic placeholder for the
    /// same pane, producing a duplicate row in `muxa watch`.
    #[tokio::test]
    async fn synthetic_rejected_when_real_stopped_exists_for_same_pane() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);

        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "real".into(),
                    pane: Some("%5".into()),
                    cwd: None,
                },
                at: t0,
            })
            .await;
        store
            .apply(&AgentEvent::SessionEnded {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "real".into(),
                    pane: Some("%5".into()),
                    cwd: None,
                },
                at: t0,
            })
            .await;
        assert_eq!(
            store.by_session("real").await.unwrap().state,
            AgentState::Stopped
        );

        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "synthetic-%5".into(),
                    pane: Some("%5".into()),
                    cwd: None,
                },
                at: t0,
            })
            .await;

        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1, "synthetic must not coexist with real");
        assert_eq!(snap[0].session_id, "real");
    }

    #[tokio::test]
    async fn reconcile_reaps_agents_whose_pane_is_gone() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        for sid in ["a", "b", "c"] {
            store
                .apply(&AgentEvent::Started {
                    id: AgentId {
                        kind: AgentKind::ClaudeCode,
                        session_id: sid.into(),
                        pane: Some(format!("%{sid}")),
                        cwd: None,
                    },
                    at: t0,
                })
                .await;
        }
        // Only %a is still alive; %b and %c are gone.
        let live = vec![pane("%a")];

        let report = store.reconcile(&live).await;

        assert_eq!(report.stale_panes_reaped, 2);
        assert_eq!(report.synthetic_demoted, 0);
        assert_eq!(report.duplicates_collapsed, 0);
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].session_id, "a");
    }

    #[tokio::test]
    async fn reconcile_keeps_paneless_agents_untouched() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "no-pane".into(),
                    pane: None,
                    cwd: None,
                },
                at: t0,
            })
            .await;

        let report = store.reconcile(&[]).await;

        assert!(report.is_noop());
        assert!(store.by_session("no-pane").await.is_some());
    }

    /// When a pane has multiple records, reconcile must keep the canonical
    /// one (real > synthetic, alive > stopped, recent > old) and drop the
    /// rest. Demoted synthetics and collapsed real duplicates are reported
    /// separately so operators can tell the two pathologies apart.
    #[tokio::test]
    async fn reconcile_collapses_duplicates_with_correct_priority() {
        let store = Store::shared();
        let old = datetime!(2026-04-24 12:00:00 UTC);
        let mid = datetime!(2026-04-24 12:30:00 UTC);
        let new = datetime!(2026-04-24 13:00:00 UTC);

        // Pane %1: synthetic (Idle) + real-old (Stopped) + real-new (Working).
        // Expected winner: real-new (real beats synthetic, alive beats stopped).
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "synthetic-%1".into(),
                    pane: Some("%1".into()),
                    cwd: None,
                },
                at: old,
            })
            .await;
        // Force synthetic_started_idempotent guard out of the way: clear
        // synthetic's claim by directly inserting another agent via apply
        // — the existing reconcile logic would normally drop the synthetic
        // when a real Started arrives, so to set up this test we have to
        // bypass that path by forcibly inserting the duplicates after the
        // fact through the lower-level write lock.
        {
            let mut agents = store.agents.write().await;
            agents.insert(
                "real-old".into(),
                Agent {
                    kind: AgentKind::ClaudeCode,
                    session_id: "real-old".into(),
                    pane: Some("%1".into()),
                    cwd: None,
                    state: AgentState::Stopped,
                    last_prompt: None,
                    last_response: None,
                    last_notification: None,
                    model: None,
                    context_used_pct: None,
                    cost_usd: None,
                    started_at: mid,
                    last_activity_at: mid,
                },
            );
            agents.insert(
                "real-new".into(),
                Agent {
                    kind: AgentKind::ClaudeCode,
                    session_id: "real-new".into(),
                    pane: Some("%1".into()),
                    cwd: None,
                    state: AgentState::Working,
                    last_prompt: None,
                    last_response: None,
                    last_notification: None,
                    model: None,
                    context_used_pct: None,
                    cost_usd: None,
                    started_at: new,
                    last_activity_at: new,
                },
            );
        }
        assert_eq!(store.snapshot().await.len(), 3);

        let report = store.reconcile(&[pane("%1")]).await;

        assert_eq!(report.stale_panes_reaped, 0);
        assert_eq!(
            report.synthetic_demoted, 1,
            "synthetic-%1 should be dropped"
        );
        assert_eq!(
            report.duplicates_collapsed, 1,
            "real-old (Stopped) should be collapsed onto real-new"
        );
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].session_id, "real-new");
    }

    /// Two real records, both alive — newer wins.
    #[tokio::test]
    async fn reconcile_picks_most_recent_real_when_both_alive() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        let t1 = datetime!(2026-04-24 13:00:00 UTC);

        {
            let mut agents = store.agents.write().await;
            for (sid, at) in [("older", t0), ("newer", t1)] {
                agents.insert(
                    sid.into(),
                    Agent {
                        kind: AgentKind::ClaudeCode,
                        session_id: sid.into(),
                        pane: Some("%1".into()),
                        cwd: None,
                        state: AgentState::Working,
                        last_prompt: None,
                        last_response: None,
                        last_notification: None,
                        model: None,
                        context_used_pct: None,
                        cost_usd: None,
                        started_at: at,
                        last_activity_at: at,
                    },
                );
            }
        }

        let report = store.reconcile(&[pane("%1")]).await;
        assert_eq!(report.duplicates_collapsed, 1);
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].session_id, "newer");
    }

    #[tokio::test]
    async fn reconcile_is_idempotent_on_clean_state() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "lone".into(),
                    pane: Some("%1".into()),
                    cwd: None,
                },
                at: t0,
            })
            .await;

        let r1 = store.reconcile(&[pane("%1")]).await;
        let r2 = store.reconcile(&[pane("%1")]).await;
        assert!(r1.is_noop() && r2.is_noop());
        assert_eq!(store.snapshot().await.len(), 1);
    }
}
