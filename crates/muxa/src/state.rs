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

use crate::event::{
    AgentEvent, AgentId, AgentKind, AgentState, NotificationLevel, RateLimitScope, RateLimitSource,
};
use crate::history::{HistoryEntry, HistoryOptions, PromptHistory};
use crate::metrics::Metrics;
use crate::tmux::PaneInfo;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use tokio::sync::{broadcast, Notify, RwLock};

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
    /// 5-hour rate-limit window utilization (0–100), refreshed by
    /// statusline Heartbeats. Optional because adapters that don't
    /// surface limit data (Codex/Gemini today) leave it unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_5h_pct: Option<f32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub rate_limit_5h_resets_at: Option<OffsetDateTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_7d_pct: Option<f32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub rate_limit_7d_resets_at: Option<OffsetDateTime>,
    /// Set by a `RateLimited` event — the user has been told they're
    /// capped until this timestamp. Cleared on the next `Started` for
    /// the same row (a fresh session means the wall has been cleared)
    /// or rendered as elapsed by the watch UI once `now > resets_at`.
    /// `None` when no limit hit has been observed, or when the source
    /// (e.g., `StopFailure` 429) didn't carry a reset time.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub rate_limited_until: Option<OffsetDateTime>,
    /// Which window (`five_hour` / `seven_day`) the most recent
    /// `RateLimited` event named, when known. Drives the watch UI label
    /// so a 7-day cap doesn't get rendered as a 5-hour cap and vice
    /// versa. Cleared together with `rate_limited_until`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_scope: Option<RateLimitScope>,
    /// Which signal pinned the current cap, when one is active.
    /// Distinguishes *soft* caps (statusline saturation — auto-clears
    /// when the next heartbeat reports utilization back below 100) from
    /// *hard* caps (`StopFailure` 429 / transcript synthetic — stay
    /// pinned until the next `Started`). Without this distinction a
    /// long-running session that hit the cap and then the window rolled
    /// over would keep its row glowing red forever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_source: Option<RateLimitSource>,
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
            rate_limit_5h_pct: None,
            rate_limit_5h_resets_at: None,
            rate_limit_7d_pct: None,
            rate_limit_7d_resets_at: None,
            rate_limited_until: None,
            rate_limit_scope: None,
            rate_limit_source: None,
            started_at: at,
            last_activity_at: at,
        }
    }
}

/// In-process notification emitted when an agent's `state` field changes.
///
/// `agent` is the post-transition snapshot, suitable for rendering
/// UI (desktop notification body, log line, status row) without
/// racing further mutations.
///
/// Both in-process consumers (sinks, notifier) and IPC subscribers
/// (`muxa watch`) receive the same payload — the type is
/// `Serialize + Deserialize` so the daemon can stream it as
/// newline-delimited JSON over the unix socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// Edge-triggered "registry changed" signal. `Store::apply` (and any
    /// other mutator) calls `notify_one()` after dropping the agents lock;
    /// the snapshotter task wakes, debounces, then writes the registry to
    /// disk. Cheap atomic on the hot path — disk I/O never touches it.
    /// Subscribers that don't care can simply never call `notified()`.
    dirty: Arc<Notify>,
    /// Lock-free runtime counters surfaced via `/api/metrics`. Cloning
    /// is cheap (`Arc`-based); the dashboard's `AppState` shares the
    /// same instance so SSE subscriber bumps and event-apply bumps land
    /// in the same atomics.
    metrics: Metrics,
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
            dirty: Arc::new(Notify::new()),
            metrics: Metrics::new(),
        }
    }

    /// Borrow the runtime metrics handle. Daemon hands this off to the
    /// dashboard `AppState` so the `/api/metrics` endpoint can read the
    /// same atomics that `Store::apply` (and the snapshotter, etc.)
    /// bump. Cloning the returned value is cheap.
    #[must_use]
    pub fn metrics(&self) -> Metrics {
        self.metrics.clone()
    }

    /// `Arc`-wrapped variant of [`Self::with_history`] mirroring
    /// [`Self::shared`].
    pub fn shared_with_history(history: Arc<PromptHistory>) -> SharedStore {
        Arc::new(Self::with_history(history))
    }

    /// Install an initial set of agents — used on daemon startup to
    /// rehydrate the registry from a previous run's snapshot.
    ///
    /// Upsert by `session_id`: existing entries are overwritten. The daemon
    /// startup path calls this exactly once on an empty store, so the
    /// upsert behavior is moot in practice; callers driving multiple
    /// hydrate passes should de-dup their inputs first. The reconciler
    /// will reap any panes that no longer exist on the next pass, so
    /// loading a stale snapshot is forgiving.
    pub async fn hydrate(&self, initial: Vec<Agent>) {
        let mut agents = self.agents.write().await;
        for a in initial {
            agents.insert(a.session_id.clone(), a);
        }
    }

    /// Seed agents that are missing from the registry but have leftovers
    /// in the prompt-history file. Used on startup, after `hydrate`, to
    /// rebuild rich rows for panes that the previous run never managed
    /// to snapshot — typically because the daemon died before its first
    /// debounce window — but for which we *do* have a recent prompt on
    /// disk.
    ///
    /// Inserts only when the candidate's `session_id` isn't already in
    /// the registry, so a hydrated state.json always wins over a
    /// possibly-staler history reconstruction. Returns the number of
    /// agents actually inserted.
    pub async fn seed_if_absent(&self, candidates: Vec<Agent>) -> usize {
        let mut inserted = 0usize;
        let mut agents = self.agents.write().await;
        for a in candidates {
            if !agents.contains_key(&a.session_id) {
                agents.insert(a.session_id.clone(), a);
                inserted += 1;
            }
        }
        if inserted > 0 {
            drop(agents);
            self.dirty.notify_one();
        }
        inserted
    }

    /// Handle to the dirty-signal Notify. Snapshotter tasks subscribe via
    /// `dirty().notified().await` to learn when the registry has mutated
    /// without polling. Every mutator in this module calls `notify_one()`
    /// after releasing the agents lock; redundant signals are coalesced by
    /// `Notify`'s saturating-to-1 semantics.
    pub fn dirty(&self) -> Arc<Notify> {
        self.dirty.clone()
    }
}

pub type SharedStore = Arc<Store>;

/// Apply one event's mutations to a single agent row, returning side
/// effects for the caller to fire after dropping the agents write lock.
///
/// Pulled out of [`Store::apply`] so the state-transition switch isn't
/// buried inside the lock-management dance. Heavier event handlers are
/// further factored into per-variant helpers ([`apply_heartbeat`],
/// [`apply_rate_limited`]) to keep the dispatch table scannable —
/// adding a new variant means adding one match arm here plus an
/// optional helper.
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
            // A fresh session means whatever wall the previous turn ran
            // into has been cleared (or the user is starting over) —
            // drop the active-cap marker so the watch row stops glowing
            // red. Rolling utilization fields stay so the new session
            // can keep counting against the same window.
            agent.rate_limited_until = None;
            agent.rate_limit_scope = None;
            agent.rate_limit_source = None;
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
            // A successful turn (response captured) is empirical proof
            // that the cap isn't blocking right now — clear any active
            // cap, hard or soft, and lift the row out of Error. Without
            // this the recovery path for a hard cap would be a session
            // restart, which `continue` after a transient 429 doesn't
            // produce. `response.is_none()` doesn't qualify: the
            // adapter may simply have failed to read the transcript,
            // including the case where the turn itself was rate-limited.
            if response.is_some() {
                agent.rate_limit_scope = None;
                agent.rate_limited_until = None;
                agent.rate_limit_source = None;
                agent.state = AgentState::Idle;
            } else if agent.state != AgentState::Error {
                agent.state = AgentState::Idle;
            }
        }
        AgentEvent::SessionEnded { .. } => {
            agent.state = AgentState::Stopped;
        }
        AgentEvent::Heartbeat { .. } => apply_heartbeat(agent, ev),
        AgentEvent::RateLimited { .. } => apply_rate_limited(agent, ev),
    }

    (prompt_record, history_entry)
}

/// Copy the model/cost/limit fields from a `Heartbeat` onto the agent
/// row. Each field is independently optional — adapters may emit some
/// without others, and absent fields preserve the row's prior value.
///
/// Saturation side effect: when either statusline window's utilization
/// hits 100, mark the agent as currently capped from
/// [`RateLimitSource::Statusline`] (a *soft* cap). The 5-hour window
/// wins the scope label when both are saturated — it's the shorter
/// wall the user is most likely watching. State is intentionally not
/// flipped to `Error` here: the LIMITS column's red ⛔ badge is a
/// stronger visual than crowding the State column too, and Heartbeat
/// changing `state` would break the convention that only discrete
/// lifecycle events drive state transitions.
///
/// Desaturation side effect: when neither window is saturated AND the
/// active cap is soft, clear it. Hard caps (`StopFailure` /
/// `Transcript`) are left intact and only `Started` clears them — a
/// 429-confirmed cap still holds even if Claude Code's percentage
/// reading later drops.
fn apply_heartbeat(agent: &mut Agent, ev: &AgentEvent) {
    let AgentEvent::Heartbeat {
        model,
        context_used_pct,
        cost_usd,
        rate_limit_5h_pct,
        rate_limit_5h_resets_at,
        rate_limit_7d_pct,
        rate_limit_7d_resets_at,
        ..
    } = ev
    else {
        return;
    };
    if let Some(m) = model {
        agent.model = Some(m.clone());
    }
    if let Some(p) = context_used_pct {
        agent.context_used_pct = Some(*p);
    }
    if let Some(c) = cost_usd {
        agent.cost_usd = Some(*c);
    }
    if let Some(p) = rate_limit_5h_pct {
        agent.rate_limit_5h_pct = Some(*p);
    }
    if let Some(t) = rate_limit_5h_resets_at {
        agent.rate_limit_5h_resets_at = Some(*t);
    }
    if let Some(p) = rate_limit_7d_pct {
        agent.rate_limit_7d_pct = Some(*p);
    }
    if let Some(t) = rate_limit_7d_resets_at {
        agent.rate_limit_7d_resets_at = Some(*t);
    }

    let five_hour_saturated = rate_limit_5h_pct.is_some_and(|p| p >= 100.0);
    let seven_day_saturated = rate_limit_7d_pct.is_some_and(|p| p >= 100.0);
    if five_hour_saturated || seven_day_saturated {
        let (scope, until) = if five_hour_saturated {
            (RateLimitScope::FiveHour, *rate_limit_5h_resets_at)
        } else {
            (RateLimitScope::SevenDay, *rate_limit_7d_resets_at)
        };
        // Saturation always picks a specific scope, so a plain assignment
        // can never regress an existing scope here.
        agent.rate_limit_scope = Some(scope);
        if until.is_some() {
            agent.rate_limited_until = until;
        }
        // Don't downgrade a hard cap (StopFailure / Transcript) to the
        // softer Statusline source — `Started` is the one and only
        // signal that should clear those, even if the percentage on
        // this heartbeat happens to confirm them.
        if !is_hard_source(agent.rate_limit_source) {
            agent.rate_limit_source = Some(RateLimitSource::Statusline);
        }
        tracing::debug!(
            source = ?RateLimitSource::Statusline,
            scope = ?scope,
            resets_at = ?until,
            "statusline saturation marked agent as rate-limited",
        );
    } else if agent.rate_limit_source == Some(RateLimitSource::Statusline) {
        // Soft cap auto-clears the moment the percentage drops back
        // below saturation — the only path that prevents a long-running
        // session from glowing red forever after the window rolls over.
        agent.rate_limit_scope = None;
        agent.rate_limited_until = None;
        agent.rate_limit_source = None;
        tracing::debug!("statusline desaturation cleared soft rate-limit cap");
    }
}

/// True for sources that should persist until the next `Started` —
/// `StopFailure` is a confirmed upstream 429 and `Transcript` is the
/// fallback path that observed Claude Code's own synthetic message.
/// Both are stronger evidence than statusline saturation alone, which
/// is just a reading off Claude Code's local counter.
fn is_hard_source(s: Option<RateLimitSource>) -> bool {
    matches!(
        s,
        Some(RateLimitSource::StopFailure | RateLimitSource::Transcript)
    )
}

/// Mark the agent as currently capped. `resets_at = None` means the
/// source (e.g., `StopFailure` 429) didn't carry a reset time — keep
/// any prior value learned from a richer source rather than blanking it.
fn apply_rate_limited(agent: &mut Agent, ev: &AgentEvent) {
    let AgentEvent::RateLimited {
        scope,
        source,
        resets_at,
        message,
        ..
    } = ev
    else {
        return;
    };
    // Don't let an `Unknown` scope from a coarse source (StopFailure 429)
    // clobber a richer scope already learned from statusline / transcript
    // — that would make the watch badge regress from `5h …` to a bare
    // `rate limited` label.
    let regressing_scope = matches!(scope, RateLimitScope::Unknown)
        && matches!(
            agent.rate_limit_scope,
            Some(RateLimitScope::FiveHour | RateLimitScope::SevenDay)
        );
    if !regressing_scope {
        agent.rate_limit_scope = Some(*scope);
    }
    // Source precedence: hard signals (StopFailure / Transcript) must
    // not be downgraded to soft (Statusline) by a later, weaker event.
    let new_is_hard = matches!(
        source,
        RateLimitSource::StopFailure | RateLimitSource::Transcript
    );
    if !is_hard_source(agent.rate_limit_source) || new_is_hard {
        agent.rate_limit_source = Some(*source);
    }
    if resets_at.is_some() {
        agent.rate_limited_until = *resets_at;
    }
    if let Some(m) = message {
        agent.last_notification = Some(m.clone());
    }
    tracing::debug!(
        source = ?source,
        scope = ?scope,
        resets_at = ?resets_at,
        "rate_limited event applied",
    );
    agent.state = AgentState::Error;
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

    #[tracing::instrument(level = "debug", skip(self, ev), fields(event_type))]
    pub async fn apply(&self, ev: &AgentEvent) {
        // Wall-clock start of the apply, used for the elapsed-time
        // emit at the bottom. Cheap monotonic clock read; no syscall on
        // Linux, just a `vDSO` call.
        let t = Instant::now();
        // Bump the events-received counter as early as possible so a
        // panic mid-apply still reflects in the metric. Lock-free atomic
        // add — invisible on the hot path.
        self.metrics.record_event();
        let mut agents = self.agents.write().await;
        let id = ev.id();
        let at = ev.at();
        // Tag the span with a stable string identifier for the variant
        // so trace consumers can group without leaking large fields
        // (prompts, response bodies). `tracing::Span::current` is cheap
        // when the span is disabled because the macro short-circuits.
        let event_type = match ev {
            AgentEvent::Started { .. } => "started",
            AgentEvent::PromptSubmitted { .. } => "prompt_submitted",
            AgentEvent::ToolStarted { .. } => "tool_started",
            AgentEvent::ToolCompleted { .. } => "tool_completed",
            AgentEvent::NotificationFired { .. } => "notification_fired",
            AgentEvent::TurnStopped { .. } => "turn_stopped",
            AgentEvent::SessionEnded { .. } => "session_ended",
            AgentEvent::Heartbeat { .. } => "heartbeat",
            AgentEvent::RateLimited { .. } => "rate_limited",
        };
        tracing::Span::current().record("event_type", event_type);

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

        // Wake the snapshotter (if any). Lock-free atomic; no I/O on the
        // hot path — disk write happens off-path after the writer's
        // debounce window. Saturates to 1 pending wakeup so a burst of
        // events coalesces into one disk write.
        self.dirty.notify_one();

        // Emit a structured per-apply timing line. `debug!` is filtered
        // out by the default subscriber level (`info`), so this costs
        // nothing in production unless an operator opts in via
        // `RUST_LOG=muxa=debug`. Field syntax keeps every value
        // structured so log scrapers can pivot without parsing.
        tracing::debug!(
            elapsed_us = u64::try_from(t.elapsed().as_micros()).unwrap_or(u64::MAX),
            session_id = %id.session_id,
            event_type,
            "store.apply",
        );
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
        let removed = {
            let mut agents = self.agents.write().await;
            let before = agents.len();
            agents.retain(|_, a| a.state != AgentState::Stopped || a.last_activity_at >= cutoff);
            before - agents.len()
        };
        if removed > 0 {
            self.dirty.notify_one();
        }
        removed
    }

    /// Auto-downgrade agents in `from` to `Idle` if they've been
    /// sitting in that state past `threshold` since their
    /// `last_activity_at`. Returns the number of agents flipped.
    ///
    /// Used by the reconciler to recover rows that a missed hook
    /// would otherwise leave stuck:
    /// - `from = Working` covers a missed `Stop`/`TurnStopped`
    ///   (Claude/Codex/Gemini)
    /// - `from = WaitingInput` covers Codex's permission-grant gap:
    ///   `permission_request` flips the row to `WaitingInput`, the
    ///   user grants permission, Codex resumes — but Codex never
    ///   fires another hook to flip the state back, so the row
    ///   stays yellow indefinitely.
    ///
    /// Every flip emits a synthetic `Transition` so IPC subscribers
    /// (`muxa watch`) see the correction live.
    ///
    /// Off when `threshold == Duration::ZERO` to preserve the
    /// "state changes only on explicit events" guarantee. The
    /// reconciler turns each variant on independently via the
    /// `stuck_working_timeout_secs` / `stuck_waiting_timeout_secs`
    /// config keys.
    pub async fn mark_stuck_idle_from(&self, from: AgentState, threshold: Duration) -> usize {
        if threshold.is_zero() {
            return 0;
        }
        let cutoff = OffsetDateTime::now_utc() - threshold;
        let mut agents = self.agents.write().await;
        let mut flipped = 0_usize;
        for agent in agents.values_mut() {
            if agent.state != from {
                continue;
            }
            if agent.last_activity_at > cutoff {
                continue;
            }
            let prev = agent.state;
            agent.state = AgentState::Idle;
            flipped += 1;
            // Broadcast for IPC subscribers and in-process sinks.
            // Identical shape to the broadcast in `apply` so consumers
            // can't tell a sweep from a real event — they just see an
            // Idle row.
            let _ = self.transitions.send(Transition {
                from: prev,
                to: agent.state,
                agent: agent.clone(),
            });
        }
        flipped
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

        drop(agents);
        if !report.is_noop() {
            self.dirty.notify_one();
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

    /// End-to-end composition: walk the full rate-limit lifecycle the
    /// way the daemon would see it from the adapters, and verify the
    /// agent row mirrors the user's actual situation at each step.
    ///
    /// This catches the class of bug the original PR review flagged —
    /// where each layer's unit test passed but a `StopFailure`-only
    /// signal failed to mark the agent capped. The renderer's
    /// `is_currently_capped` rule is mirrored here as a local helper so
    /// the test fails on logic regressions in either crate.
    #[tokio::test]
    async fn rate_limit_lifecycle_end_to_end() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-29 10:00:00 UTC);
        let t1 = datetime!(2026-04-29 10:30:00 UTC);
        let t2 = datetime!(2026-04-29 11:00:00 UTC);
        let t3 = datetime!(2026-04-29 14:00:00 UTC);
        let resets_at = datetime!(2026-04-29 15:00:00 UTC);

        // Mirror the renderer's "currently capped" rule.
        let is_capped = |a: &Agent, now: OffsetDateTime| -> bool {
            a.rate_limit_scope.is_some() && a.rate_limited_until.is_none_or(|until| until > now)
        };

        // 1. Session starts; no rate-limit data yet.
        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: t0,
            })
            .await;
        let a = store.by_session("s").await.unwrap();
        assert!(!is_capped(&a, t0));

        // 2. Statusline heartbeat: 84% utilization on 5h, well under
        //    saturation. Row tracks the percentage but stays uncapped.
        store
            .apply(&AgentEvent::Heartbeat {
                id: id("s"),
                model: Some("Sonnet".into()),
                context_used_pct: None,
                cost_usd: None,
                rate_limit_5h_pct: Some(84.0),
                rate_limit_5h_resets_at: Some(resets_at),
                rate_limit_7d_pct: Some(31.0),
                rate_limit_7d_resets_at: None,
                at: t1,
            })
            .await;
        let a = store.by_session("s").await.unwrap();
        assert!(!is_capped(&a, t1));
        assert_eq!(a.rate_limit_5h_pct, Some(84.0));

        // 3. StopFailure 429 lands — coarse signal with no reset on the
        //    wire. Row must mark capped, scope must NOT regress to
        //    Unknown if a richer scope lands later, and `rate_limited_until`
        //    stays as the prior reset time learned from the heartbeat.
        store
            .apply(&AgentEvent::RateLimited {
                id: id("s"),
                scope: RateLimitScope::Unknown,
                source: RateLimitSource::StopFailure,
                resets_at: None,
                message: Some("429 Too Many Requests".into()),
                at: t2,
            })
            .await;
        let a = store.by_session("s").await.unwrap();
        assert!(is_capped(&a, t2), "StopFailure 429 must mark agent capped");
        assert_eq!(a.state, AgentState::Error);

        // 4. A subsequent statusline-derived event (richer scope) lands
        //    — must upgrade Unknown → FiveHour, not regress.
        store
            .apply(&AgentEvent::RateLimited {
                id: id("s"),
                scope: RateLimitScope::FiveHour,
                source: RateLimitSource::Statusline,
                resets_at: Some(resets_at),
                message: None,
                at: t2,
            })
            .await;
        let a = store.by_session("s").await.unwrap();
        assert_eq!(a.rate_limit_scope, Some(RateLimitScope::FiveHour));
        assert_eq!(a.rate_limited_until, Some(resets_at));

        // 5. Coarse Unknown signal arrives again — must NOT clobber the
        //    specific scope we'd learned.
        store
            .apply(&AgentEvent::RateLimited {
                id: id("s"),
                scope: RateLimitScope::Unknown,
                source: RateLimitSource::StopFailure,
                resets_at: None,
                message: None,
                at: t2,
            })
            .await;
        let a = store.by_session("s").await.unwrap();
        assert_eq!(
            a.rate_limit_scope,
            Some(RateLimitScope::FiveHour),
            "Unknown must not regress a specific scope"
        );

        // 6. Wall-clock crosses the reset time — renderer rule says no
        //    longer capped even though the daemon hasn't cleared yet.
        let after_reset = resets_at + time::Duration::seconds(1);
        let a = store.by_session("s").await.unwrap();
        assert!(!is_capped(&a, after_reset));

        // 7. User starts a fresh session in the same row — daemon
        //    clears the active-cap markers on Started.
        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: t3,
            })
            .await;
        let a = store.by_session("s").await.unwrap();
        assert_eq!(a.rate_limit_scope, None);
        assert_eq!(a.rate_limited_until, None);
        assert_eq!(a.rate_limit_source, None);
        assert!(!is_capped(&a, t3));
    }

    /// Statusline saturation (≥100% on either window) must mark the
    /// agent capped without requiring a separate `RateLimited` event —
    /// that's the soft path that uses `RateLimitSource::Statusline`.
    #[tokio::test]
    async fn heartbeat_saturation_marks_agent_capped() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-29 10:00:00 UTC);
        let resets_at = datetime!(2026-04-29 15:00:00 UTC);

        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: t0,
            })
            .await;
        store
            .apply(&AgentEvent::Heartbeat {
                id: id("s"),
                model: None,
                context_used_pct: None,
                cost_usd: None,
                rate_limit_5h_pct: Some(100.0),
                rate_limit_5h_resets_at: Some(resets_at),
                rate_limit_7d_pct: Some(35.0),
                rate_limit_7d_resets_at: None,
                at: t0,
            })
            .await;

        let a = store.by_session("s").await.unwrap();
        assert_eq!(a.rate_limit_scope, Some(RateLimitScope::FiveHour));
        assert_eq!(a.rate_limited_until, Some(resets_at));
        assert_eq!(a.rate_limit_source, Some(RateLimitSource::Statusline));
        // Saturation does NOT flip state — that would break the
        // convention that only discrete lifecycle events drive state.
        // The watch row glows red via the LIMITS column gate, not via
        // a redundant State column flip.
        assert_eq!(a.state, AgentState::Idle);
    }

    /// Regression for the second-round P0: a long-running session that
    /// hit a *soft* cap (statusline saturation) and then the window
    /// rolled over must stop showing red. Without auto-clear the row
    /// would glow forever until the user happened to start a fresh
    /// session.
    #[tokio::test]
    async fn heartbeat_desaturation_clears_soft_cap() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-29 10:00:00 UTC);
        let t1 = datetime!(2026-04-29 15:01:00 UTC);
        let old_reset = datetime!(2026-04-29 15:00:00 UTC);
        let new_reset = datetime!(2026-04-29 20:00:00 UTC);

        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: t0,
            })
            .await;

        // Saturate.
        store
            .apply(&AgentEvent::Heartbeat {
                id: id("s"),
                model: None,
                context_used_pct: None,
                cost_usd: None,
                rate_limit_5h_pct: Some(100.0),
                rate_limit_5h_resets_at: Some(old_reset),
                rate_limit_7d_pct: Some(35.0),
                rate_limit_7d_resets_at: None,
                at: t0,
            })
            .await;
        let a = store.by_session("s").await.unwrap();
        assert_eq!(a.rate_limit_source, Some(RateLimitSource::Statusline));

        // Window rolls over; next heartbeat reports utilisation back below 100.
        store
            .apply(&AgentEvent::Heartbeat {
                id: id("s"),
                model: None,
                context_used_pct: None,
                cost_usd: None,
                rate_limit_5h_pct: Some(50.0),
                rate_limit_5h_resets_at: Some(new_reset),
                rate_limit_7d_pct: Some(35.0),
                rate_limit_7d_resets_at: None,
                at: t1,
            })
            .await;
        let a = store.by_session("s").await.unwrap();
        assert_eq!(a.rate_limit_scope, None, "soft cap must auto-clear");
        assert_eq!(a.rate_limited_until, None);
        assert_eq!(a.rate_limit_source, None);
        assert_eq!(a.rate_limit_5h_pct, Some(50.0), "rolling pct still tracked");
    }

    /// A *hard* cap (`StopFailure` 429 or transcript-derived) must
    /// persist across a desaturation heartbeat — only `Started` clears
    /// it. Without this the user would see the cap silently disappear
    /// the next time Claude Code's local percentage reading dropped,
    /// even though the upstream API hasn't unlocked them.
    #[tokio::test]
    async fn hard_cap_persists_across_desaturation_heartbeat() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-29 10:00:00 UTC);
        let t1 = datetime!(2026-04-29 10:30:00 UTC);

        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: t0,
            })
            .await;
        // Hard cap from StopFailure 429 — no reset on the wire, scope Unknown.
        store
            .apply(&AgentEvent::RateLimited {
                id: id("s"),
                scope: RateLimitScope::Unknown,
                source: RateLimitSource::StopFailure,
                resets_at: None,
                message: Some("429".into()),
                at: t0,
            })
            .await;
        let a = store.by_session("s").await.unwrap();
        assert_eq!(a.rate_limit_source, Some(RateLimitSource::StopFailure));

        // Heartbeat reports 50% — soft signal says no cap, but the
        // hard one is still authoritative.
        store
            .apply(&AgentEvent::Heartbeat {
                id: id("s"),
                model: None,
                context_used_pct: None,
                cost_usd: None,
                rate_limit_5h_pct: Some(50.0),
                rate_limit_5h_resets_at: None,
                rate_limit_7d_pct: None,
                rate_limit_7d_resets_at: None,
                at: t1,
            })
            .await;
        let a = store.by_session("s").await.unwrap();
        assert_eq!(
            a.rate_limit_source,
            Some(RateLimitSource::StopFailure),
            "hard source must not be cleared by desaturation",
        );
        assert!(a.rate_limit_scope.is_some(), "hard cap scope must persist");
    }

    /// Saturation arriving on top of an already-active hard cap must
    /// not downgrade the source from hard to soft — otherwise the very
    /// next desaturation would auto-clear a confirmed 429.
    #[tokio::test]
    async fn saturation_does_not_downgrade_hard_source_to_soft() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-29 10:00:00 UTC);

        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: t0,
            })
            .await;
        store
            .apply(&AgentEvent::RateLimited {
                id: id("s"),
                scope: RateLimitScope::FiveHour,
                source: RateLimitSource::Transcript,
                resets_at: None,
                message: None,
                at: t0,
            })
            .await;
        // Saturating heartbeat lands.
        store
            .apply(&AgentEvent::Heartbeat {
                id: id("s"),
                model: None,
                context_used_pct: None,
                cost_usd: None,
                rate_limit_5h_pct: Some(100.0),
                rate_limit_5h_resets_at: None,
                rate_limit_7d_pct: None,
                rate_limit_7d_resets_at: None,
                at: t0,
            })
            .await;
        let a = store.by_session("s").await.unwrap();
        assert_eq!(
            a.rate_limit_source,
            Some(RateLimitSource::Transcript),
            "hard source must not be overwritten by saturation",
        );
    }

    /// Round-3 P1 regression: a successful turn after a hard cap must
    /// clear it. Without this the recovery path for a transient
    /// `StopFailure` 429 was a full session restart — typing
    /// "continue" and getting a real response would leave the row
    /// stuck red despite empirical evidence the cap was gone.
    #[tokio::test]
    async fn successful_turn_clears_hard_cap() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-29 10:00:00 UTC);
        let t1 = datetime!(2026-04-29 10:15:00 UTC);

        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: t0,
            })
            .await;
        // Hard cap from a transient 429.
        store
            .apply(&AgentEvent::RateLimited {
                id: id("s"),
                scope: RateLimitScope::Unknown,
                source: RateLimitSource::StopFailure,
                resets_at: None,
                message: Some("429".into()),
                at: t0,
            })
            .await;
        let a = store.by_session("s").await.unwrap();
        assert_eq!(a.state, AgentState::Error);
        assert_eq!(a.rate_limit_source, Some(RateLimitSource::StopFailure));

        // User retries; Claude Code serves a real response. Stop hook
        // emits TurnStopped with the captured assistant text.
        store
            .apply(&AgentEvent::TurnStopped {
                id: id("s"),
                response: Some("recovered".into()),
                at: t1,
            })
            .await;
        let a = store.by_session("s").await.unwrap();
        assert_eq!(a.state, AgentState::Idle, "successful turn must lift Error");
        assert_eq!(a.rate_limit_scope, None);
        assert_eq!(a.rate_limited_until, None);
        assert_eq!(a.rate_limit_source, None);
        assert_eq!(a.last_response.as_deref(), Some("recovered"));
    }

    /// `TurnStopped` *without* a response means the adapter couldn't
    /// read the transcript — that includes the case where the turn
    /// itself was rate-limited and the synthetic message replaced the
    /// expected assistant text. We must NOT auto-clear in that case.
    #[tokio::test]
    async fn empty_turn_stopped_does_not_clear_hard_cap() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-29 10:00:00 UTC);

        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: t0,
            })
            .await;
        store
            .apply(&AgentEvent::RateLimited {
                id: id("s"),
                scope: RateLimitScope::Unknown,
                source: RateLimitSource::StopFailure,
                resets_at: None,
                message: None,
                at: t0,
            })
            .await;
        store
            .apply(&AgentEvent::TurnStopped {
                id: id("s"),
                response: None,
                at: t0,
            })
            .await;
        let a = store.by_session("s").await.unwrap();
        assert_eq!(
            a.rate_limit_source,
            Some(RateLimitSource::StopFailure),
            "empty turn must not clear an active hard cap",
        );
        assert_eq!(a.state, AgentState::Error);
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
    async fn mark_stuck_idle_flips_old_working_to_idle() {
        let store = Store::shared();
        // Drive an agent into Working with a stale last_activity_at
        // so it crosses the timeout cutoff.
        let stale_at = OffsetDateTime::now_utc() - time::Duration::hours(1);
        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: stale_at,
            })
            .await;
        store
            .apply(&AgentEvent::PromptSubmitted {
                id: id("s"),
                prompt: "long-running".into(),
                at: stale_at,
            })
            .await;
        assert_eq!(
            store.by_session("s").await.unwrap().state,
            AgentState::Working
        );

        // Subscribe before the sweep so we can observe the broadcast.
        let mut rx = store.subscribe();
        let flipped = store
            .mark_stuck_idle_from(AgentState::Working, Duration::from_secs(60))
            .await;
        assert_eq!(flipped, 1);
        assert_eq!(store.by_session("s").await.unwrap().state, AgentState::Idle);

        // Sweep emits a Transition matching the synthesized flip.
        let t = rx.try_recv().expect("transition broadcast");
        assert_eq!(t.from, AgentState::Working);
        assert_eq!(t.to, AgentState::Idle);
    }

    #[tokio::test]
    async fn mark_stuck_idle_flips_old_waiting_input_to_idle() {
        // Codex permission-grant case: row gets pinned to
        // WaitingInput by `permission_request`, user grants and
        // Codex resumes without firing another hook. The sweep
        // recovers the row after `threshold` of inactivity.
        let store = Store::shared();
        let stale_at = OffsetDateTime::now_utc() - time::Duration::hours(1);
        store
            .apply(&AgentEvent::Started {
                id: id("c"),
                at: stale_at,
            })
            .await;
        store
            .apply(&AgentEvent::NotificationFired {
                id: id("c"),
                level: NotificationLevel::NeedsInput,
                message: "codex permission: shell".into(),
                at: stale_at,
            })
            .await;
        assert_eq!(
            store.by_session("c").await.unwrap().state,
            AgentState::WaitingInput
        );

        let mut rx = store.subscribe();
        let flipped = store
            .mark_stuck_idle_from(AgentState::WaitingInput, Duration::from_secs(60))
            .await;
        assert_eq!(flipped, 1);
        assert_eq!(store.by_session("c").await.unwrap().state, AgentState::Idle);
        let t = rx.try_recv().expect("transition broadcast");
        assert_eq!(t.from, AgentState::WaitingInput);
        assert_eq!(t.to, AgentState::Idle);
    }

    #[tokio::test]
    async fn mark_stuck_idle_only_sweeps_target_state() {
        // Asking for the WaitingInput sweep does not touch a
        // Working row — the two timeouts must stay independent.
        let store = Store::shared();
        let stale_at = OffsetDateTime::now_utc() - time::Duration::hours(1);
        store
            .apply(&AgentEvent::Started {
                id: id("w"),
                at: stale_at,
            })
            .await;
        store
            .apply(&AgentEvent::PromptSubmitted {
                id: id("w"),
                prompt: "p".into(),
                at: stale_at,
            })
            .await;
        let flipped = store
            .mark_stuck_idle_from(AgentState::WaitingInput, Duration::from_secs(60))
            .await;
        assert_eq!(flipped, 0);
        assert_eq!(
            store.by_session("w").await.unwrap().state,
            AgentState::Working
        );
    }

    #[tokio::test]
    async fn mark_stuck_idle_skips_recent_working() {
        // An agent that just transitioned to Working should NOT be
        // flipped: real long-running tasks would be falsely marked
        // idle. The cutoff is `now - threshold`; recent activity
        // beats the cutoff and survives.
        let store = Store::shared();
        let now = OffsetDateTime::now_utc();
        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: now,
            })
            .await;
        store
            .apply(&AgentEvent::PromptSubmitted {
                id: id("s"),
                prompt: "fresh".into(),
                at: now,
            })
            .await;
        let flipped = store
            .mark_stuck_idle_from(AgentState::Working, Duration::from_secs(60))
            .await;
        assert_eq!(flipped, 0);
        assert_eq!(
            store.by_session("s").await.unwrap().state,
            AgentState::Working
        );
    }

    #[tokio::test]
    async fn mark_stuck_idle_zero_threshold_is_noop() {
        let store = Store::shared();
        let stale_at = OffsetDateTime::now_utc() - time::Duration::hours(2);
        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: stale_at,
            })
            .await;
        store
            .apply(&AgentEvent::PromptSubmitted {
                id: id("s"),
                prompt: "ancient".into(),
                at: stale_at,
            })
            .await;
        // Even a hours-stale agent stays Working when the sweep is
        // disabled (Duration::ZERO).
        let flipped = store
            .mark_stuck_idle_from(AgentState::Working, Duration::ZERO)
            .await;
        assert_eq!(flipped, 0);
        assert_eq!(
            store.by_session("s").await.unwrap().state,
            AgentState::Working
        );
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
                rate_limit_5h_pct: None,
                rate_limit_5h_resets_at: None,
                rate_limit_7d_pct: None,
                rate_limit_7d_resets_at: None,
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
                    rate_limit_5h_pct: None,
                    rate_limit_5h_resets_at: None,
                    rate_limit_7d_pct: None,
                    rate_limit_7d_resets_at: None,
                    rate_limited_until: None,
                    rate_limit_scope: None,
                    rate_limit_source: None,
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
                    rate_limit_5h_pct: None,
                    rate_limit_5h_resets_at: None,
                    rate_limit_7d_pct: None,
                    rate_limit_7d_resets_at: None,
                    rate_limited_until: None,
                    rate_limit_scope: None,
                    rate_limit_source: None,
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
                        rate_limit_5h_pct: None,
                        rate_limit_5h_resets_at: None,
                        rate_limit_7d_pct: None,
                        rate_limit_7d_resets_at: None,
                        rate_limited_until: None,
                        rate_limit_scope: None,
                        rate_limit_source: None,
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

    /// `seed_if_absent` is the entry point used by the daemon's
    /// `enrich_from_history` path: it must insert candidates whose
    /// `session_id` isn't yet in the registry, and skip any that are
    /// already present (so a `state.json` rehydrate always wins over a
    /// possibly-staler reconstruction from `prompts.ndjson`).
    #[tokio::test]
    async fn seed_if_absent_inserts_only_new_session_ids() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-28 00:00 UTC);
        // Pre-populate with session "live".
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "live".into(),
                    pane: Some("%1".into()),
                    cwd: None,
                },
                at: t0,
            })
            .await;

        // Two candidates: "live" already exists (must skip),
        // "fresh" is new (must insert).
        let mk = |sid: &str, pane: &str, prompt: &str| Agent {
            kind: AgentKind::ClaudeCode,
            session_id: sid.into(),
            pane: Some(pane.into()),
            cwd: None,
            state: AgentState::Idle,
            last_prompt: Some(prompt.into()),
            last_response: None,
            last_notification: None,
            model: None,
            context_used_pct: None,
            cost_usd: None,
            rate_limit_5h_pct: None,
            rate_limit_5h_resets_at: None,
            rate_limit_7d_pct: None,
            rate_limit_7d_resets_at: None,
            rate_limited_until: None,
            rate_limit_scope: None,
            rate_limit_source: None,
            started_at: t0,
            last_activity_at: t0,
        };
        let inserted = store
            .seed_if_absent(vec![
                mk("live", "%1", "should not overwrite"),
                mk("fresh", "%2", "history-derived"),
            ])
            .await;

        assert_eq!(inserted, 1, "only the new session_id should be inserted");
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 2);
        // The pre-existing "live" agent must keep its original
        // last_prompt (None from the Started event), proving seed
        // didn't clobber it.
        let live = snap.iter().find(|a| a.session_id == "live").unwrap();
        assert!(live.last_prompt.is_none());
        // The fresh seed must carry the candidate's last_prompt.
        let fresh = snap.iter().find(|a| a.session_id == "fresh").unwrap();
        assert_eq!(fresh.last_prompt.as_deref(), Some("history-derived"));
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
