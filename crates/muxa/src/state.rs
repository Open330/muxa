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

use crate::backend::{HostKind, PaneObservation};
use crate::event::{
    AgentEvent, AgentId, AgentKind, AgentState, NotificationLevel, RateLimitScope, RateLimitSource,
    SurfaceRef,
};
use crate::history::{HistoryEntry, HistoryOptions, PromptHistory};
use crate::metrics::Metrics;
use crate::process_tree::WorkloadSummary;
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
/// dedup honest: a real hook event arriving for the same `(kind, pane, socket)`
/// replaces the synthetic placeholder rather than racing it.
///
/// Kept here (not in the runtime crate) so the no-I/O store layer can dedup
/// without taking a cross-crate dependency on the discovery module.
pub const SYNTHETIC_SESSION_PREFIX: &str = "synthetic-";
const CLAUDE_IDLE_PROMPT_NOTIFICATION: &str = "Claude is waiting for your input";

fn is_synthetic(session_id: &str) -> bool {
    session_id.starts_with(SYNTHETIC_SESSION_PREFIX)
}

fn same_pane_identity(agent: &Agent, pane: &str, tmux_socket: Option<&str>) -> bool {
    if agent.pane.as_deref() != Some(pane) {
        return false;
    }
    match (agent.tmux_socket.as_deref(), tmux_socket) {
        (Some(left), Some(right)) => {
            crate::tmux::socket_short_name(left) == crate::tmux::socket_short_name(right)
        }
        (None, None) => true,
        (Some(_), None) | (None, Some(_)) => false,
    }
}

fn pane_is_live(
    agent: &Agent,
    panes_by_id: &HashMap<&str, Vec<&PaneInfo>>,
    observing_kind: HostKind,
) -> bool {
    if agent.pid.is_some() {
        return true;
    }
    // Cross-host reaping guard: an observation from one backend only governs
    // rows whose pane id belongs to that backend's host. A tmux-backend daemon
    // physically cannot see herdr/zellij panes (and vice versa), so a namespaced
    // id for *another* host missing from this snapshot is not death — it's just
    // out of view. Treat such rows as live so a tmux→herdr migration (both hosts
    // active) doesn't reap the other host's live rows every reconcile tick.
    // Unknown-shape ids (`None`) fall through to the active backend's normal
    // governance, exactly as before this guard existed.
    if let Some(pane_id) = agent.pane.as_deref() {
        if let Some(host) = crate::backend::pane_id_host_kind(pane_id) {
            if host != observing_kind {
                return true;
            }
        }
    }
    match agent.pane.as_deref() {
        Some(pane_id) => match (panes_by_id.get(pane_id), agent.tmux_socket.as_deref()) {
            (None, _) => false,
            (Some(candidates), Some(socket)) => candidates
                .iter()
                .any(|pane| pane.socket.as_deref().is_none_or(|value| value == socket)),
            (Some(_), None) => true,
        },
        None => true,
    }
}

fn normalize_hydrated_agent(mut agent: Agent) -> Agent {
    if is_legacy_claude_idle_prompt_wait(&agent) {
        agent.state = AgentState::Idle;
        agent.state_entered_at = agent.last_activity_at;
    }
    agent
}

fn is_legacy_claude_idle_prompt_wait(agent: &Agent) -> bool {
    agent.kind == AgentKind::ClaudeCode
        && agent.state == AgentState::WaitingInput
        && agent.last_notification.as_deref() == Some(CLAUDE_IDLE_PROMPT_NOTIFICATION)
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

/// Upper bound on tracked in-flight subagents per agent. A safety cap so a
/// missed `ToolCompleted` (crash, dropped hook) can't grow the list without
/// bound; real fan-out is far below this.
const MAX_SUBAGENTS: usize = 32;

/// A subagent (Claude `Task` child) currently running under an agent.
/// Presence means "in flight": entries are pushed on the Task `ToolStarted`
/// and removed on its matching `ToolCompleted`, so the list only ever holds
/// subagents that are still working.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subagent {
    /// Task `subagent_type`, e.g. `"Explore"`, `"general-purpose"`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub kind: AgentKind,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface: Option<SurfaceRef>,
    pub pane: Option<String>,
    /// Short name of the tmux server socket `pane` lives on (the socket
    /// file's basename, e.g. `default` or `amux`) — from the adapter's
    /// `$TMUX` at hook time, or backfilled by the reconciler's pane scan.
    /// Pane ids are only unique per server; wire consumers matching by pane
    /// use this to disambiguate. Optional and purely additive on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_socket: Option<String>,
    /// Name of the tmux session `pane` belongs to, backfilled by the
    /// reconciler's multi-socket pane scan each tick. Optional and purely
    /// additive on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_session: Option<String>,
    pub cwd: Option<String>,
    /// OS process id for pid-tracked rows (`AgentKind::Task`). When set,
    /// the reconciler governs this agent's liveness by checking whether the
    /// process is still alive instead of by tmux pane presence, and flips
    /// it to `Stopped` once the pid is gone. `None` for hook/pane agents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Best-effort summary of extra work spawned below this pane's primary
    /// agent process: shell commands, nested agent sessions, and helper
    /// processes. Refreshed by the reconciler from OS process-tree state.
    /// Empty on hosts/backends that cannot expose pane PIDs.
    #[serde(default, skip_serializing_if = "WorkloadSummary::is_empty")]
    pub workload: WorkloadSummary,
    /// Subagents (Claude `Task` children) currently in flight under this
    /// agent, newest last. Pushed on the Task `ToolStarted` and cleared on
    /// its `ToolCompleted` / at turn end. Empty for agents with none.
    /// Additive on the wire.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subagents: Vec<Subagent>,
    pub state: AgentState,
    pub last_prompt: Option<String>,
    /// Last assistant response captured for this agent. Populated by the
    /// `TurnStopped` ingest path when the adapter could read the
    /// transcript; remains `None` for adapters that don't expose response
    /// text (e.g., Codex/Gemini today). Optional so the field is purely
    /// additive on the wire and in the UI.
    pub last_response: Option<String>,
    /// Claude Code's session "recap" (`※ recap: …`), scraped from the
    /// transcript at turn end. The richest "what is this agent actually
    /// doing" signal muxa can get, but sparse — Claude only writes one
    /// when the user returns after being away — so it is never cleared by
    /// a turn that didn't produce one, and the UI falls back to
    /// [`Self::ai_title`] then [`Self::last_prompt`]. `None` for agents
    /// with no recap source (Codex/Gemini have no equivalent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recap: Option<String>,
    /// Claude Code's rolling short session title — the same string it puts
    /// in the tmux pane title. Rewritten far more often than a recap, so
    /// it's the practical steady-state summary. Additive on the wire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ai_title: Option<String>,
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
    /// Wall-clock at which the agent most recently entered its current
    /// `state`. Drives the watch UI's stuck-duration suffix so operators
    /// can spot forgotten `WaitingInput` / `WaitingChoice` rows. Old
    /// snapshots written before this field existed deserialize with the
    /// rehydrate timestamp — the duration restarts at restart, which is
    /// less misleading than a 1970-anchored multi-decade reading.
    #[serde(default = "default_state_entered_at", with = "time::serde::rfc3339")]
    pub state_entered_at: OffsetDateTime,
}

fn default_state_entered_at() -> OffsetDateTime {
    OffsetDateTime::now_utc()
}

/// Whether an OS process is still alive, used for pid-tracked task rows.
/// Dependency-free: a fast `/proc/<pid>` check on Linux (the primary
/// target), falling back to `kill -0` elsewhere (macOS/BSD have no
/// `/proc`). A live entry — including a zombie, which vanishes once reaped
/// — counts as alive.
fn pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

impl Agent {
    fn new(
        kind: AgentKind,
        session_id: String,
        surface: Option<SurfaceRef>,
        pane: Option<String>,
        cwd: Option<String>,
        at: OffsetDateTime,
    ) -> Self {
        Self {
            kind,
            session_id,
            surface,
            pane,
            tmux_socket: None,
            tmux_session: None,
            cwd,
            pid: None,
            workload: WorkloadSummary::default(),
            subagents: Vec::new(),
            state: AgentState::Starting,
            last_prompt: None,
            last_response: None,
            recap: None,
            ai_title: None,
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
            state_entered_at: at,
        }
    }

    /// Record a newly-spawned subagent (Claude `Task` child), capped at
    /// [`MAX_SUBAGENTS`] so a missed completion can't grow the list without
    /// bound. Cloned from the wire spec since the event is borrowed.
    fn record_subagent(&mut self, spec: &crate::event::SubagentSpec, at: OffsetDateTime) {
        if self.subagents.len() < MAX_SUBAGENTS {
            self.subagents.push(Subagent {
                kind: spec.kind.clone(),
                description: spec.description.clone(),
                started_at: at,
            });
        }
    }
}

/// In-process notification emitted when an agent's `state` field changes.
///
/// `agent` is the post-transition snapshot, suitable for rendering
/// UI (desktop notification body, log line, status row) without
/// racing further mutations.
///
/// `agent` is wrapped in [`Arc`] specifically because the
/// `tokio::sync::broadcast` channel clones the payload **once per
/// subscriber per `recv()`** — with the daemon's notifier + sinks +
/// every live `muxa watch` SSE/IPC subscriber, that fanout was
/// dominating `Store::apply` wall time at modest subscriber counts
/// (4–8). The Arc keeps the per-fanout cost a refcount bump instead
/// of an `Agent`-sized memcpy of the up-to-8 KB-of-`String` payload.
/// See `crates/muxa/benches/store_apply.rs` for the measurement.
///
/// Both in-process consumers (sinks, notifier) and IPC subscribers
/// (`muxa watch`) receive the same payload — the type is
/// `Serialize + Deserialize` so the daemon can stream it as
/// newline-delimited JSON over the unix socket. `Arc<T>` serializes
/// transparently as `T`, and on the deserializing side rebuilds a
/// fresh, single-strong `Arc<T>` — so the wire format is unchanged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    pub from: AgentState,
    pub to: AgentState,
    pub agent: Arc<Agent>,
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
    /// Registry of live agents, keyed by session id.
    ///
    /// **Invariant — never hold this write guard across an `.await`.** Every
    /// mutator (`apply`, `register_task`, `reconcile`, `update_workloads`, …)
    /// does only synchronous work under the guard and `drop`s it *before* any
    /// suspending call (disk I/O, `notify_one`, channel sends). Holding it
    /// across a suspension point would serialize — and, if that future stalls,
    /// deadlock — every reader (`snapshot`/`by_pane`/…), which in turn parks
    /// every IPC handler waiting on the store and pins its fd. The IPC layer
    /// bounds the blast radius (handler cap + timeouts), but this invariant is
    /// what keeps lock hold time in the microsecond range to begin with.
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
            let a = normalize_hydrated_agent(a);
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

    /// Insert (or replace) a pid-tracked background task row, surfacing an
    /// arbitrary process in `muxa status` / `muxa watch`. Used by the
    /// `Register` IPC (`muxa register`) and `muxa run` PTY spawns. The row
    /// starts `Working` and is flipped to `Stopped` by `reap_dead_pids`
    /// once its pid dies. `name` becomes the session id (and the displayed
    /// NAME); falls back to `task-<pid>` when empty.
    pub async fn register_task(
        &self,
        name: String,
        pid: Option<u32>,
        cwd: Option<String>,
        pane: Option<String>,
        command: Option<String>,
    ) -> Result<String, String> {
        let now = OffsetDateTime::now_utc();
        let base = if name.trim().is_empty() {
            format!("task-{}", pid.unwrap_or(0))
        } else {
            name
        };
        let mut agents = self.agents.write().await;
        // Resolve the registry key, handling name collisions:
        let key = match agents.get(&base) {
            // Never clobber a real (non-Task) agent row sharing this name.
            Some(existing) if existing.kind != AgentKind::Task => {
                return Err(format!(
                    "'{base}' already names a live {} agent; pick another --name",
                    existing.kind
                ));
            }
            // Same task re-registering (same pid) — idempotent update in place.
            Some(existing) if existing.pid == pid => base.clone(),
            // A different task already holds this name (e.g. two `muxa run`
            // of the same command). Disambiguate so both coexist.
            Some(_) => {
                if let Some(p) = pid {
                    format!("{base}#{p}")
                } else {
                    let mut n = 2;
                    while agents.contains_key(&format!("{base}#{n}")) {
                        n += 1;
                    }
                    format!("{base}#{n}")
                }
            }
            None => base.clone(),
        };
        let mut agent = Agent::new(AgentKind::Task, key.clone(), None, pane, cwd, now);
        agent.state = AgentState::Working;
        agent.pid = pid;
        agent.last_prompt = command;
        agents.insert(key.clone(), agent);
        drop(agents);
        self.dirty.notify_one();
        Ok(key)
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

fn event_touches_activity(agent: &Agent, ev: &AgentEvent) -> bool {
    match ev {
        AgentEvent::Heartbeat { .. } => false,
        AgentEvent::ToolCompleted { .. } => matches!(
            agent.state,
            AgentState::Working | AgentState::WaitingInput | AgentState::WaitingChoice
        ),
        AgentEvent::ToolStarted { .. } => agent.state != AgentState::Error,
        AgentEvent::Started { .. }
        | AgentEvent::PromptSubmitted { .. }
        | AgentEvent::NotificationFired { .. }
        | AgentEvent::TurnStopped { .. }
        | AgentEvent::SessionEnded { .. }
        | AgentEvent::RateLimited { .. } => true,
    }
}

/// Apply one event's mutations to a single agent row, returning side
/// effects for the caller to fire after dropping the agents write lock.
///
/// Pulled out of [`Store::apply`] so the state-transition switch isn't
/// buried inside the lock-management dance. Heavier event handlers are
/// further factored into per-variant helpers ([`apply_heartbeat`],
/// [`apply_rate_limited`]) to keep the dispatch table scannable —
/// adding a new variant means adding one match arm here plus an
/// optional helper.
#[allow(clippy::too_many_lines)] // dispatch table — see the doc comment above
fn mutate_for_event(
    agent: &mut Agent,
    ev: &AgentEvent,
    id: &AgentId,
    at: OffsetDateTime,
) -> (Option<PromptRecord>, Option<HistoryEntry>, bool) {
    let mut prompt_record: Option<PromptRecord> = None;
    let mut history_entry: Option<HistoryEntry> = None;
    let prev_state = agent.state;
    let touches_activity = event_touches_activity(agent, ev);

    match ev {
        AgentEvent::Started { .. } => {
            agent.state = AgentState::Idle;
            agent.subagents.clear();
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
            let history_key = agent
                .pane
                .clone()
                .or_else(|| agent.surface.as_ref().map(|s| s.id.clone()));
            if let Some(pane) = history_key {
                history_entry = Some(HistoryEntry::with_cwd(
                    agent.kind,
                    agent.session_id.clone(),
                    pane,
                    agent.cwd.clone(),
                    prompt.clone(),
                    at,
                    agent.model.clone(),
                ));
            }
        }
        AgentEvent::ToolStarted { subagent, .. } => {
            // ToolStarted always means "agent is actively doing work" —
            // covers the Idle → Working transition AND the
            // WaitingInput → Working recovery (e.g. Codex resuming
            // after a permission grant). Error stays Error so a
            // legitimate failure isn't silently masked by tool activity
            // — the next Stop or NotificationFired clears it.
            if agent.state != AgentState::Error {
                agent.state = AgentState::Working;
            }
            // A Claude `Task` call carries a subagent spec — record it as an
            // in-flight subagent; the matching `ToolCompleted` retires it.
            if let Some(spec) = subagent {
                agent.record_subagent(spec, at);
            }
        }
        AgentEvent::ToolCompleted { tool, .. } => {
            // Tool activity proves the agent isn't waiting on the user
            // any more. Specifically targets these cases:
            //   - Codex grants permission, runs the tool, the
            //     completion fires before any TurnStopped does.
            //   - Claude's `AskUserQuestion` / `ExitPlanMode` (routed
            //     through NotificationFired { NeedsChoice } in the
            //     adapter, landing the row in WaitingChoice) complete;
            //     the user picked an option.
            //   - A free-text Notification prompt cleared.
            // Other states are left alone — Working stays Working,
            // Idle stays Idle (a stray ToolCompleted with no
            // PromptSubmitted before it shouldn't fake activity),
            // Error is preserved.
            if matches!(
                agent.state,
                AgentState::WaitingInput | AgentState::WaitingChoice
            ) {
                agent.state = AgentState::Working;
            }
            // A completed `Task` retires the oldest in-flight subagent. We
            // match FIFO rather than by id — the hook stream carries no
            // per-call id, and parallel same-type subagents finish close
            // enough in practice.
            if tool == "Task" && !agent.subagents.is_empty() {
                agent.subagents.remove(0);
            }
        }
        AgentEvent::NotificationFired { level, message, .. } => {
            agent.last_notification = Some(message.clone());
            match level {
                NotificationLevel::NeedsInput => agent.state = AgentState::WaitingInput,
                NotificationLevel::NeedsChoice => agent.state = AgentState::WaitingChoice,
                NotificationLevel::Error => agent.state = AgentState::Error,
                NotificationLevel::Info | NotificationLevel::Warning => {}
            }
        }
        AgentEvent::TurnStopped {
            response,
            recap,
            ai_title,
            ..
        } => {
            if let Some(text) = response {
                agent.last_response = Some(text.clone());
            }
            // Session-summary signals ride along on turn end. Only
            // overwrite on `Some`: a recap is sparse (Claude writes one
            // only when the user returns after being away, and an old one
            // falls out of the transcript tail window), so a turn that
            // surfaced none must not erase the last good recap.
            if let Some(text) = recap {
                agent.recap = Some(text.clone());
            }
            if let Some(text) = ai_title {
                agent.ai_title = Some(text.clone());
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
            } else if matches!(
                agent.state,
                AgentState::WaitingInput | AgentState::WaitingChoice
            ) && !is_synthetic(&agent.session_id)
            {
                // Codex can emit `Stop` while a permission request is still
                // sitting in the terminal. A response-less stop is not proof
                // that the user-facing wait cleared; keep the row waiting
                // until a tool event, response, or explicit later state
                // transition says otherwise.
                //
                // REAL rows only. A SYNTHETIC detection row (screen inference /
                // the herdr bridge) that reports `idle`/`done` is reading the
                // pane's *current* screen — if the approval prompt is gone, the
                // wait genuinely cleared, so a synthetic response-less stop must
                // fall through to `Idle` rather than freezing on `WaitingInput`.
            } else if agent.state != AgentState::Error {
                agent.state = AgentState::Idle;
            }
            // Turn boundary: subagents have finished (their own
            // `ToolCompleted` fired first); clear defensively so a missed
            // completion can't leave phantom subagents on an idle row.
            agent.subagents.clear();
        }
        AgentEvent::SessionEnded { .. } => {
            agent.state = AgentState::Stopped;
            agent.subagents.clear();
        }
        AgentEvent::Heartbeat { .. } => apply_heartbeat(agent, ev),
        AgentEvent::RateLimited { .. } => apply_rate_limited(agent, ev),
    }

    // Catch-all: any event for a `Starting` agent demonstrates the
    // agent is alive — promote to `Idle` so the row stops painting
    // cyan. `Starting` is the default of `Agent::new()` (and so the
    // initial state of any agent created via `or_insert_with` in
    // `apply`); for events that don't carry an explicit state
    // transition (`Heartbeat`, `ToolCompleted` on a fresh row,
    // `RateLimited` arriving before `Started`) the agent would
    // otherwise stay `Starting` indefinitely. Synthetic discovery
    // placeholders that have *never* received a hook event are still
    // accurately `Starting` — the catch-all only fires once an
    // event lands.
    if agent.state == AgentState::Starting {
        agent.state = AgentState::Idle;
    }

    if prev_state != agent.state {
        agent.state_entered_at = at;
    }

    (prompt_record, history_entry, touches_activity)
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
/// `StopFailure` is a confirmed upstream 429, `Transcript` observed Claude
/// Code's own synthetic message, and `CodexRollout` saw codex stamp
/// `rate_limit_reached_type` on disk. All three are stronger evidence than
/// statusline saturation alone, which is just a reading off a local counter.
fn is_hard_source(s: Option<RateLimitSource>) -> bool {
    matches!(
        s,
        Some(
            RateLimitSource::StopFailure
                | RateLimitSource::Transcript
                | RateLimitSource::CodexRollout
        )
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
    // Source precedence: hard signals (StopFailure / Transcript /
    // CodexRollout) must not be downgraded to soft (Statusline) by a later,
    // weaker event.
    let new_is_hard = matches!(
        source,
        RateLimitSource::StopFailure | RateLimitSource::Transcript | RateLimitSource::CodexRollout
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
/// against a pane that's already represented by a live session).
/// Otherwise the map has been updated to make room for the new agent:
///
/// * Synthetic placeholders are rejected only when a *live* session
///   (real or synthetic, non-`Stopped`) already owns the pane. A
///   `Stopped` predecessor does **not** block the synthetic — that's
///   the exact "user restarted claude in the same pane" case `muxa
///   sync` is meant to recover from. The stale `Stopped` record is
///   evicted so the pane carries a single entry. When a real hook
///   later fires for the new session, the real Started event wins via
///   the branch below.
/// * Real `Started` events drop any synthetic placeholders for the same
///   pane outright, since the real session is now authoritative.
/// * Other active sessions sharing the pane are flipped to `Stopped` (the
///   user launched a fresh agent in the same pane and the previous session
///   never emitted `SessionEnd`). Stopped predecessors that aren't
///   evicted above are left alone here; the periodic reconciler
///   collapses them later.
fn reconcile_pane_for_started(
    agents: &mut HashMap<String, Agent>,
    incoming_session: &str,
    pane: &str,
    tmux_socket: Option<&str>,
    at: OffsetDateTime,
) -> bool {
    if is_synthetic(incoming_session) {
        let live_owner_exists = agents.values().any(|a| {
            a.session_id != incoming_session
                && same_pane_identity(a, pane, tmux_socket)
                && a.state != AgentState::Stopped
        });
        if live_owner_exists {
            return false;
        }
        // Evict the stale Stopped predecessor(s) — keeping them would
        // leave two rows for the same pane (one Stopped real, one
        // synthetic-Idle) and surface the older "stopped" timestamp in
        // `muxa status` even though the pane is alive again.
        agents.retain(|_, a| {
            !(same_pane_identity(a, pane, tmux_socket)
                && a.session_id != incoming_session
                && a.state == AgentState::Stopped)
        });
    } else {
        // Real Started — drop synthetic placeholders for this pane outright.
        agents.retain(|_, a| {
            !(same_pane_identity(a, pane, tmux_socket) && is_synthetic(&a.session_id))
        });
    }

    for other in agents.values_mut() {
        if other.session_id != incoming_session
            && same_pane_identity(other, pane, tmux_socket)
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

        if let Some(pane) = id.pane.as_deref() {
            if matches!(ev, AgentEvent::Started { .. }) {
                if !reconcile_pane_for_started(
                    &mut agents,
                    &id.session_id,
                    pane,
                    id.tmux_socket.as_deref(),
                    at,
                ) {
                    return;
                }
            } else if !is_synthetic(&id.session_id) {
                // A session may have started paneless when the agent hook
                // subprocess did not inherit TMUX_PANE. If a later prompt or
                // tool event recovers the pane through process ancestry, that
                // event is authoritative too: remove discovery's idle
                // placeholder immediately instead of keeping a duplicate
                // synthetic row until the next SessionStart.
                agents.retain(|_, agent| {
                    !(same_pane_identity(agent, pane, id.tmux_socket.as_deref())
                        && is_synthetic(&agent.session_id))
                });
            }
        }

        let agent = agents.entry(id.session_id.clone()).or_insert_with(|| {
            Agent::new(
                id.kind,
                id.session_id.clone(),
                id.surface.clone(),
                id.pane.clone(),
                id.cwd.clone(),
                at,
            )
        });

        // Keep identity fields fresh — adapters may re-send with more info.
        if agent.pane.is_none() {
            agent.pane.clone_from(&id.pane);
        }
        if agent.tmux_socket.is_none() {
            // `$TMUX` carries the socket *path*; store the short name so it
            // compares directly against the pane scan's socket tags.
            agent.tmux_socket = id
                .tmux_socket
                .as_deref()
                .map(crate::tmux::socket_short_name);
        }
        if agent.surface.is_none() {
            agent.surface.clone_from(&id.surface);
        }
        if agent.cwd.is_none() {
            agent.cwd.clone_from(&id.cwd);
        }
        let prev_state = agent.state;
        let (prompt_record, history_entry, touches_activity) = mutate_for_event(agent, ev, id, at);
        if touches_activity {
            agent.last_activity_at = at;
        }

        if agent.state != prev_state {
            // Wrap the post-transition snapshot in an `Arc` exactly once
            // here; the broadcast channel then bumps the refcount per
            // subscriber instead of memcpy-ing the whole `Agent` (which
            // can hold up to ~8 KB of `String` data on a busy session).
            let transition = Transition {
                from: prev_state,
                to: agent.state,
                agent: Arc::new(agent.clone()),
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

    pub async fn by_surface(&self, surface_id: &str) -> Vec<Agent> {
        self.agents
            .read()
            .await
            .values()
            .filter(|a| a.surface.as_ref().is_some_and(|s| s.id == surface_id))
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

    /// Immediately delete fully orphaned rows (no pane, surface, or pid)
    /// whose last activity predates `cutoff`. The on-demand analogue of the
    /// reconciler's `mark_stale_paneless_stopped` age-out — backs `muxa
    /// prune`, so a user can clear accumulated remote/detached ghost rows
    /// now instead of waiting for the 24h sweep plus the GC's Stopped-row
    /// TTL. Returns the number removed.
    ///
    /// A pane, surface, or pid each disqualifies a row (some other liveness
    /// path owns it), so this can never remove a live tmux pane agent, a
    /// muxa PTY session, or a pid-tracked task — only truly ownerless rows.
    pub async fn prune_orphans(&self, cutoff: OffsetDateTime) -> usize {
        let removed = {
            let mut agents = self.agents.write().await;
            let before = agents.len();
            agents.retain(|_, a| {
                let orphan = a.kind != AgentKind::Task
                    && a.pane.is_none()
                    && a.surface.is_none()
                    && a.pid.is_none();
                !(orphan && a.last_activity_at < cutoff)
            });
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
        let now = OffsetDateTime::now_utc();
        let cutoff = now - threshold;
        let mut agents = self.agents.write().await;
        let mut flipped = 0_usize;
        for agent in agents.values_mut() {
            // Background tasks have no attention states — they're Working
            // while alive and Stopped when dead (governed by pid liveness),
            // and never emit activity, so the stuck-idle sweep must not
            // demote a live task to Idle.
            if agent.kind == AgentKind::Task {
                continue;
            }
            if agent.state != from {
                continue;
            }
            if agent.last_activity_at > cutoff {
                continue;
            }
            let prev = agent.state;
            agent.state = AgentState::Idle;
            agent.state_entered_at = now;
            agent.last_activity_at = now;
            flipped += 1;
            // Broadcast for IPC subscribers and in-process sinks.
            // Identical shape to the broadcast in `apply` so consumers
            // can't tell a sweep from a real event — they just see an
            // Idle row. Same `Arc::new(agent.clone())` discipline as
            // `apply` — see the `Transition::agent` doc comment.
            let _ = self.transitions.send(Transition {
                from: prev,
                to: agent.state,
                agent: Arc::new(agent.clone()),
            });
        }
        if flipped > 0 {
            drop(agents);
            self.dirty.notify_one();
        }
        flipped
    }

    /// Flip pid-tracked rows (`AgentKind::Task`) whose process has exited to
    /// `Stopped`. Run every reconciler tick. Conservative by design: it only
    /// changes state, never deletes — the regular GC reaps `Stopped` rows
    /// after their inactivity TTL. Returns the number of rows flipped.
    pub async fn reap_dead_pids(&self) -> usize {
        let mut agents = self.agents.write().await;
        let mut flipped = 0_usize;
        for agent in agents.values_mut() {
            let Some(pid) = agent.pid else { continue };
            if agent.state == AgentState::Stopped {
                continue;
            }
            if pid_alive(pid) {
                continue;
            }
            let prev = agent.state;
            let now = OffsetDateTime::now_utc();
            agent.state = AgentState::Stopped;
            agent.state_entered_at = now;
            // Touch last_activity_at so the GC's Stopped-row TTL is measured
            // from when the task died, not from registration — otherwise a
            // task that ran longer than the TTL is evicted on the next sweep
            // instead of lingering as Stopped for the window.
            agent.last_activity_at = now;
            flipped += 1;
            // Same broadcast shape as `apply`/`mark_stuck_idle_from` so live
            // subscribers see the row go Stopped immediately.
            let _ = self.transitions.send(Transition {
                from: prev,
                to: agent.state,
                agent: Arc::new(agent.clone()),
            });
        }
        if flipped > 0 {
            drop(agents);
            self.dirty.notify_one();
        }
        flipped
    }

    /// Flip stale paneless agents to `Stopped` so the regular GC can reap
    /// them. Returns the number of rows flipped.
    ///
    /// This closes a liveness hole: an agent that is paneless **and** has no
    /// surface **and** no pid is governed by none of the other converge
    /// paths — `reconcile` reaps only pane-dead rows, `reap_dead_pids` reaps
    /// only pid-tracked rows, and `gc` deletes only `Stopped` rows. A codex
    /// session driven through a detached `app-server` / remote bridge fires
    /// hooks with no `TMUX_PANE` and an ancestry that terminates at launchd,
    /// so it lands paneless and never transitions to `Stopped` on its own —
    /// the row lingers forever and `muxa watch`'s `+N paneless` count creeps
    /// up. Flipping to `Stopped` (rather than deleting) keeps the shape
    /// consistent with `reap_dead_pids` and lets `gc`'s TTL make the final
    /// call.
    ///
    /// Conservative by construction:
    /// - `threshold == Duration::ZERO` disables the sweep (matching
    ///   `mark_stuck_idle_from`), so the historical "no age-based reaping"
    ///   behaviour is one config key away.
    /// - Only truly orphan rows qualify (`pane`, `surface`, and `pid` all
    ///   absent). A live-but-idle remote session is spared by a generous
    ///   default threshold; if it really is dead it stops emitting activity
    ///   and ages out.
    /// - `Task` rows are excluded explicitly (belt-and-suspenders on top of
    ///   the `pid` check — a registered task may momentarily carry no pid).
    pub async fn mark_stale_paneless_stopped(&self, threshold: Duration) -> usize {
        if threshold.is_zero() {
            return 0;
        }
        let cutoff = OffsetDateTime::now_utc() - threshold;
        let mut agents = self.agents.write().await;
        let mut flipped = 0_usize;
        for agent in agents.values_mut() {
            if agent.kind == AgentKind::Task {
                continue;
            }
            // Only orphan rows: a pane, a surface, or a pid each means some
            // other liveness path owns this row's lifecycle.
            if agent.pane.is_some() || agent.surface.is_some() || agent.pid.is_some() {
                continue;
            }
            if agent.state == AgentState::Stopped {
                continue;
            }
            if agent.last_activity_at > cutoff {
                continue;
            }
            let prev = agent.state;
            let now = OffsetDateTime::now_utc();
            agent.state = AgentState::Stopped;
            agent.state_entered_at = now;
            // Measure the GC's Stopped-row TTL from when we gave up on the
            // row, not from its last real activity — otherwise a row already
            // older than the TTL is deleted on the very next GC sweep instead
            // of lingering as `Stopped` for the intended window.
            agent.last_activity_at = now;
            flipped += 1;
            // Same broadcast shape as `apply`/`reap_dead_pids` so live
            // subscribers see the row go Stopped immediately.
            let _ = self.transitions.send(Transition {
                from: prev,
                to: agent.state,
                agent: Arc::new(agent.clone()),
            });
        }
        if flipped > 0 {
            drop(agents);
            self.dirty.notify_one();
        }
        flipped
    }

    /// Age out rows whose pane belongs to a host **no active observation
    /// governs**, flipping them to `Stopped` after `threshold` of inactivity
    /// so the regular GC can reap them. Returns the number flipped.
    ///
    /// This closes the flip side of the cross-host reaping guard. That guard
    /// (see [`pane_is_live`]) deliberately exempts a foreign-host row from
    /// *immediate* reaping — a tmux-backend daemon physically cannot see a
    /// `herdr:`/`zellij:` pane, so its absence from a tmux scan is not death.
    /// But a single-backend daemon never observes those panes at all, and the
    /// GC only evicts `Stopped` rows — so without an age-out a foreign-host
    /// row (e.g. a `herdr:` row left in `state.json` after the operator
    /// switched the daemon back to tmux) would ghost forever. Flipping to
    /// `Stopped` after the same inactivity window as
    /// [`Self::mark_stale_paneless_stopped`] lets a genuinely-live remote row
    /// keep itself alive by emitting activity, while a truly dead one ages out.
    ///
    /// `observing_kinds` is the set of hosts whose observation was *complete*
    /// this pass — the hosts that actually answered, not merely the ones in the
    /// backend set. A host that answers governs its own rows via reaping, so it
    /// must be spared here; a host that *can't* answer for longer than the
    /// inactivity window is, for our purposes, indistinguishable from a host
    /// outside the set — its rows are never reaped by any observation, so they
    /// must age out here or they ghost forever. Passing the complete-this-tick
    /// set (rather than every kind in the backend set) is what closes that gap:
    /// a chronically-incomplete host's stale rows age out, while a transiently
    /// incomplete tick is harmless because the threshold is the (24h-default)
    /// paneless window on `last_activity_at`, not a single tick. A row is a
    /// candidate only when its pane id classifies to a known host
    /// ([`crate::backend::pane_id_host_kind`]) that is *not* in this set.
    /// Same-host rows (governed by `reconcile`) and unclassifiable/paneless
    /// rows (governed by the normal reap / `mark_stale_paneless_stopped`
    /// paths) are left untouched.
    ///
    /// Conservative by construction, mirroring [`Self::mark_stale_paneless_stopped`]:
    /// `threshold == Duration::ZERO` disables the sweep; `Task` and
    /// pid-tracked rows are excluded (process liveness owns them); already
    /// `Stopped` rows are skipped.
    pub async fn mark_stale_cross_host_stopped(
        &self,
        observing_kinds: &[HostKind],
        threshold: Duration,
    ) -> usize {
        if threshold.is_zero() {
            return 0;
        }
        let cutoff = OffsetDateTime::now_utc() - threshold;
        let mut agents = self.agents.write().await;
        let mut flipped = 0_usize;
        for agent in agents.values_mut() {
            if agent.kind == AgentKind::Task || agent.pid.is_some() {
                continue;
            }
            if agent.state == AgentState::Stopped {
                continue;
            }
            // Only rows whose pane classifies to a host NOT currently
            // observed — those are the ones no live observation can reap.
            let Some(pane_id) = agent.pane.as_deref() else {
                continue;
            };
            let Some(host) = crate::backend::pane_id_host_kind(pane_id) else {
                continue;
            };
            if observing_kinds.contains(&host) {
                continue;
            }
            if agent.last_activity_at > cutoff {
                continue;
            }
            let prev = agent.state;
            let now = OffsetDateTime::now_utc();
            agent.state = AgentState::Stopped;
            agent.state_entered_at = now;
            // Measure the GC's Stopped-row TTL from when we gave up on the
            // row, not from its last real activity — same rationale as
            // `mark_stale_paneless_stopped`.
            agent.last_activity_at = now;
            flipped += 1;
            // Same broadcast shape as `apply`/`mark_stale_paneless_stopped`
            // so live subscribers see the row go Stopped immediately.
            let _ = self.transitions.send(Transition {
                from: prev,
                to: agent.state,
                agent: Arc::new(agent.clone()),
            });
        }
        if flipped > 0 {
            drop(agents);
            self.dirty.notify_one();
        }
        flipped
    }

    /// Refresh pane-backed workload summaries from the latest OS
    /// process-tree scan. This is metadata enrichment only: it must not
    /// touch `last_activity_at`, `state_entered_at`, or emit transitions,
    /// because a shell child appearing below an agent is not itself an
    /// agent lifecycle transition.
    ///
    /// `complete_kinds` is the set of hosts whose pane observation was
    /// *complete* this tick. Because the process-tree scan is store-global —
    /// it clears the workload of any pane absent from its map — a row is only
    /// governed (reset/updated) when its pane-id namespace classifies to a host
    /// in that set. A row on a host observed *incompletely* (or not at all)
    /// keeps its previous workload metadata rather than being wrongly reset to
    /// the default from a scan that never covered its host. Rows whose pane id
    /// doesn't classify to a known host (paneless / legacy) are governed as
    /// before — they never carry a scan workload anyway, so the outcome matches
    /// the pre-multi-host single-observation behavior byte-for-byte.
    pub async fn update_workloads(
        &self,
        by_pane: &HashMap<String, WorkloadSummary>,
        complete_kinds: &[HostKind],
    ) -> usize {
        let mut agents = self.agents.write().await;
        let mut changed = 0_usize;
        for agent in agents.values_mut() {
            if agent.pid.is_some() {
                continue;
            }
            // Skip rows namespaced to a host whose scan didn't run this tick.
            if let Some(pane_id) = agent.pane.as_deref() {
                if let Some(host) = crate::backend::pane_id_host_kind(pane_id) {
                    if !complete_kinds.contains(&host) {
                        continue;
                    }
                }
            }
            let next = agent
                .pane
                .as_ref()
                .and_then(|pane| by_pane.get(pane))
                .cloned()
                .unwrap_or_default();
            if agent.workload != next {
                agent.workload = next;
                changed += 1;
            }
        }
        if changed > 0 {
            drop(agents);
            self.dirty.notify_one();
        }
        changed
    }

    /// Backfills tmux session/socket names from the pane scan so agent rows
    /// carry them on the wire (clients join workspaces by session name).
    /// Deliberately silent — no state transition is broadcast for a name
    /// refresh. Ambiguous untagged rows (same pane id on several servers,
    /// agent without `$TMUX` info) prefer the default server.
    fn backfill_tmux_names(
        agents: &mut HashMap<String, Agent>,
        panes_by_id: &HashMap<&str, Vec<&PaneInfo>>,
    ) {
        for a in agents.values_mut() {
            if a.pid.is_some() {
                continue;
            }
            let Some(pane_id) = a.pane.as_deref() else {
                continue;
            };
            let Some(cands) = panes_by_id.get(pane_id) else {
                continue;
            };
            let chosen = match a.tmux_socket.as_deref() {
                Some(sock) => cands.iter().find(|p| p.socket.as_deref() == Some(sock)),
                None => match cands.len() {
                    1 => cands.first(),
                    _ => cands
                        .iter()
                        .find(|p| p.socket.as_deref() == Some("default"))
                        .or(cands.first()),
                },
            };
            if let Some(p) = chosen {
                a.tmux_session = Some(p.session.clone());
                if a.tmux_socket.is_none() {
                    a.tmux_socket.clone_from(&p.socket);
                }
            }
        }
    }

    /// Correlate paneless codex hook rows to the tmux pane they are actually
    /// running in, by working-directory match. Returns the number adopted.
    ///
    /// A `features.code_mode_host` codex runs its turns — and fires its hooks —
    /// from a shared, detached `app-server` (parent PID 1, no `TMUX_PANE`), so
    /// the real hook row lands paneless even when the session lives in a tmux
    /// pane. Meanwhile discovery has planted a *synthetic* codex placeholder on
    /// that pane. The two describe one session but never merge, because the
    /// hook never learned the pane. This bridges them: the synthetic row is the
    /// "codex runs in this pane" evidence, and the pane's `current_path` joined
    /// against the paneless row's `cwd` recovers the pairing. Once the real row
    /// adopts the pane, the dedup pass demotes the synthetic.
    ///
    /// Deliberately conservative — a wrong pane is worse than none:
    /// - Only unambiguous 1:1 pairings act (exactly one paneless row and one
    ///   candidate pane share a cwd). Any many-to-one is skipped.
    /// - A pane already owned by a *real* codex row is never stolen.
    /// - Rows/panes without a usable cwd/path contribute nothing.
    fn correlate_paneless_codex(
        agents: &mut HashMap<String, Agent>,
        panes_by_id: &HashMap<&str, Vec<&PaneInfo>>,
    ) -> usize {
        // A pane's cwd, resolved the same way `backfill_tmux_names` picks the
        // right candidate when a pane id repeats across tmux servers.
        let resolve_path = |pane_id: &str, socket: Option<&str>| -> Option<String> {
            let cands = panes_by_id.get(pane_id)?;
            let chosen = match socket {
                Some(sock) => cands.iter().find(|p| p.socket.as_deref() == Some(sock)),
                None => match cands.len() {
                    1 => cands.first(),
                    _ => cands
                        .iter()
                        .find(|p| p.socket.as_deref() == Some("default"))
                        .or(cands.first()),
                },
            }?;
            let path = chosen.current_path.trim();
            (!path.is_empty()).then(|| path.to_string())
        };

        // Panes already owned by a real codex row — off-limits.
        let real_codex_panes: HashSet<(String, Option<String>)> = agents
            .iter()
            .filter(|(sid, a)| a.kind == AgentKind::Codex && !is_synthetic(sid) && a.pane.is_some())
            .filter_map(|(_, a)| Some((a.pane.clone()?, a.tmux_socket.clone())))
            .collect();

        // cwd -> candidate codex panes (those carrying a synthetic codex row).
        let mut panes_by_cwd: HashMap<String, Vec<(String, Option<String>)>> = HashMap::new();
        for (sid, a) in agents.iter() {
            if a.kind != AgentKind::Codex || !is_synthetic(sid) {
                continue;
            }
            let Some(pane_id) = a.pane.clone() else {
                continue;
            };
            if real_codex_panes.contains(&(pane_id.clone(), a.tmux_socket.clone())) {
                continue;
            }
            if let Some(path) = resolve_path(&pane_id, a.tmux_socket.as_deref()) {
                panes_by_cwd
                    .entry(path)
                    .or_default()
                    .push((pane_id, a.tmux_socket.clone()));
            }
        }

        // cwd -> paneless real codex rows.
        let mut rows_by_cwd: HashMap<String, Vec<String>> = HashMap::new();
        for (sid, a) in agents.iter() {
            if a.kind != AgentKind::Codex
                || is_synthetic(sid)
                || a.pane.is_some()
                || a.pid.is_some()
            {
                continue;
            }
            let Some(cwd) = a.cwd.as_deref().map(str::trim).filter(|c| !c.is_empty()) else {
                continue;
            };
            rows_by_cwd
                .entry(cwd.to_string())
                .or_default()
                .push(sid.clone());
        }

        // Collect unambiguous adoptions first, then apply (avoids holding an
        // immutable borrow of `agents` across the mutable `get_mut`).
        let mut adoptions: Vec<(String, String, Option<String>)> = Vec::new();
        for (cwd, sids) in &rows_by_cwd {
            let Some(panes) = panes_by_cwd.get(cwd) else {
                continue;
            };
            if sids.len() != 1 || panes.len() != 1 {
                continue;
            }
            adoptions.push((sids[0].clone(), panes[0].0.clone(), panes[0].1.clone()));
        }

        let adopted = adoptions.len();
        for (sid, pane_id, socket) in adoptions {
            if let Some(a) = agents.get_mut(&sid) {
                a.pane = Some(pane_id);
                a.tmux_socket = socket;
            }
        }
        adopted
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
    pub async fn reconcile_observation(
        &self,
        observation: &PaneObservation,
        observing_kind: HostKind,
    ) -> ReconcileReport {
        if !observation.is_complete() {
            return ReconcileReport::default();
        }
        self.reconcile_hosted(&observation.panes, observing_kind)
            .await
    }

    /// Converge against a pane set already known to be complete, assuming the
    /// panes were observed by the **tmux** backend.
    ///
    /// Most runtime callers should use [`Self::reconcile_observation`], which
    /// threads the real observing [`HostKind`] through so the cross-host
    /// reaping guard can exempt other hosts' rows. This slice-based entry point
    /// remains for focused store tests and callers that obtain their complete
    /// pane inventory without a [`PaneBackend`](crate::backend::PaneBackend);
    /// every such caller today observes tmux, so it defaults to
    /// [`HostKind::Tmux`]. Tests exercising cross-host behavior call
    /// [`Self::reconcile_hosted`] directly.
    ///
    /// Composes the two halves the multi-host reconciler now runs separately —
    /// the paneless-codex correlation ([`Self::correlate_paneless_codex_union`])
    /// then the tmux reap/dedup pass — so a single-host caller (and every store
    /// test) sees the exact same adopt-then-demote-in-one-pass behavior as
    /// before the correlation was lifted out of [`Self::reconcile_hosted`].
    pub async fn reconcile(&self, live_panes: &[PaneInfo]) -> ReconcileReport {
        let paneless_correlated = self.correlate_paneless_codex_union(live_panes).await;
        let mut report = self.reconcile_hosted(live_panes, HostKind::Tmux).await;
        report.paneless_correlated = paneless_correlated;
        report
    }

    /// Adopt paneless codex hook rows onto the tmux/herdr pane they actually
    /// run in, by working-directory match, across the **union** of live panes
    /// from every complete observation this tick. Returns the number adopted.
    ///
    /// This is the multi-host-safe home of [`Self::correlate_paneless_codex`]:
    /// a `code_mode_host` codex fires its hooks from a shared, detached
    /// `app-server` (no `TMUX_PANE`), so its real row lands paneless while
    /// discovery plants a synthetic placeholder on its pane. Correlating over
    /// the union — rather than per host inside [`Self::reconcile_hosted`] — lets
    /// the many-to-one cwd ambiguity guard see candidate panes on *both* hosts,
    /// so it won't mis-adopt a row whose codex lives in a herdr pane sharing a
    /// cwd with a tmux pane. The reconciler calls this before its per-host
    /// reap/dedup passes so those passes still demote the redundant synthetic
    /// in the same tick.
    pub async fn correlate_paneless_codex_union(&self, live_panes: &[PaneInfo]) -> usize {
        let mut agents = self.agents.write().await;
        let mut panes_by_id: HashMap<&str, Vec<&PaneInfo>> = HashMap::new();
        for p in live_panes {
            panes_by_id.entry(p.pane_id.as_str()).or_default().push(p);
        }
        let adopted = Self::correlate_paneless_codex(&mut agents, &panes_by_id);
        if adopted > 0 {
            drop(agents);
            self.dirty.notify_one();
        }
        adopted
    }

    /// Converge against a complete pane set observed by `observing_kind`.
    ///
    /// The `observing_kind` is the host that produced `live_panes`. It gates
    /// the stale-pane reap so an observation only governs rows whose pane id
    /// belongs to that host (see [`pane_is_live`] and
    /// [`crate::backend::pane_id_host_kind`]).
    pub async fn reconcile_hosted(
        &self,
        live_panes: &[PaneInfo],
        observing_kind: HostKind,
    ) -> ReconcileReport {
        let mut agents = self.agents.write().await;
        let mut report = ReconcileReport::default();

        // Sweep 1: drop agents whose pane is gone. Done as a single retain
        // pass to avoid building an intermediate Vec of doomed session ids.
        // Candidate panes per pane id: the scan spans every tmux server
        // socket, and pane ids are only unique per server, so one id can
        // map to several panes. Liveness and the session-name backfill both
        // use the agent's `tmux_socket` (when known) to pick the right one.
        let mut panes_by_id: HashMap<&str, Vec<&PaneInfo>> = HashMap::new();
        for p in live_panes {
            panes_by_id.entry(p.pane_id.as_str()).or_default().push(p);
        }

        let reaped_at = OffsetDateTime::now_utc();
        let mut stale_transitions = Vec::new();
        let before = agents.len();
        agents.retain(|_, a| {
            let keep = pane_is_live(a, &panes_by_id, observing_kind);
            if !keep && a.state != AgentState::Stopped {
                let mut stopped = a.clone();
                let from = stopped.state;
                stopped.state = AgentState::Stopped;
                stopped.state_entered_at = reaped_at;
                stopped.last_activity_at = reaped_at;
                stale_transitions.push(Transition {
                    from,
                    to: AgentState::Stopped,
                    agent: Arc::new(stopped),
                });
            }
            keep
        });
        report.stale_panes_reaped = before - agents.len();

        // NOTE: paneless-codex correlation is NOT done here. In a multi-host
        // daemon each backend's observation reconciles separately, and a
        // per-host correlation would only see one host's panes — its
        // many-to-one cwd ambiguity guard couldn't tell that a cwd is also
        // claimed by a pane on the *other* host, so the tmux pass could adopt a
        // row whose codex actually lives in a herdr pane at the same cwd. The
        // correlation runs ONCE per tick over the union of complete
        // observations via [`Self::correlate_paneless_codex_union`], which the
        // reconciler invokes *before* the per-host passes so this reconcile's
        // dedup still demotes the now-redundant synthetic in the same tick.
        // The single-host [`Self::reconcile`] entry point composes the two so
        // its behavior is unchanged.
        Self::backfill_tmux_names(&mut agents, &panes_by_id);

        // Sweeps 2 & 3: per-pane dedup. Group surviving agents by pane id
        // AND server socket — pane ids repeat across tmux servers, and two
        // agents on same-numbered panes of different servers are different
        // agents, never duplicates. (The backfill above just stamped every
        // pane-bearing agent's socket, so the key is populated.)
        // pid-tracked rows (Task) are excluded entirely — they're governed
        // by process liveness, never by pane ownership, so a task that
        // carries a `--pane` must not be deduped against the pane's real
        // agent (that would delete one of them).
        let mut by_pane: HashMap<(&str, Option<&str>), Vec<String>> = HashMap::new();
        for (sid, a) in agents.iter() {
            if a.pid.is_some() {
                continue;
            }
            if let Some(p) = a.pane.as_deref() {
                by_pane
                    .entry((p, a.tmux_socket.as_deref()))
                    .or_default()
                    .push(sid.clone());
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
        for transition in stale_transitions {
            let _ = self.transitions.send(transition);
        }
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
    /// Paneless codex hook rows adopted onto a tmux pane via cwd match.
    pub paneless_correlated: usize,
}

impl ReconcileReport {
    /// Total number of agents removed in this pass.
    pub fn total_removed(&self) -> usize {
        self.stale_panes_reaped + self.synthetic_demoted + self.duplicates_collapsed
    }

    /// True when this pass changed the registry — handy for log-on-change
    /// patterns so quiet steady-state passes don't flood the log. Correlation
    /// mutates a row without removing one, so it counts too.
    pub fn is_noop(&self) -> bool {
        self.total_removed() == 0 && self.paneless_correlated == 0
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
            tmux_socket: None,
            kind: AgentKind::ClaudeCode,
            session_id: session.into(),
            surface: None,
            pane: Some("%1".into()),
            cwd: None,
        }
    }

    fn stored_agent(
        session: &str,
        kind: AgentKind,
        state: AgentState,
        last_notification: Option<&str>,
    ) -> Agent {
        let at = datetime!(2026-05-05 12:00:00 UTC);
        Agent {
            tmux_socket: None,
            tmux_session: None,
            kind,
            session_id: session.into(),
            surface: None,
            pid: None,
            workload: crate::WorkloadSummary::default(),
            subagents: Vec::new(),
            pane: Some("%1".into()),
            cwd: None,
            state,
            last_prompt: None,
            last_response: None,
            recap: None,
            ai_title: None,
            last_notification: last_notification.map(Into::into),
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
            state_entered_at: at,
        }
    }

    #[tokio::test]
    async fn register_task_inserts_working_pid_tracked_row() {
        let store = Store::shared();
        let sid = store
            .register_task(
                "game".into(),
                Some(4242),
                Some("/home/u/game".into()),
                None,
                Some("./play.sh".into()),
            )
            .await
            .unwrap();
        assert_eq!(sid, "game");
        let agent = store.by_session("game").await.unwrap();
        assert_eq!(agent.kind, AgentKind::Task);
        assert_eq!(agent.state, AgentState::Working);
        assert_eq!(agent.pid, Some(4242));
        assert_eq!(agent.last_prompt.as_deref(), Some("./play.sh"));
    }

    #[tokio::test]
    async fn register_task_empty_name_falls_back_to_task_pid() {
        let store = Store::shared();
        let sid = store
            .register_task(String::new(), Some(99), None, None, None)
            .await
            .unwrap();
        assert_eq!(sid, "task-99");
    }

    #[tokio::test]
    async fn register_task_refuses_to_clobber_real_agent() {
        let store = Store::shared();
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "claude-1".into(),
                    surface: None,
                    pane: Some("%1".into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;
        // A task registration that collides with the real agent's id is
        // rejected — the real row (kind + pane) must be preserved.
        let err = store
            .register_task("claude-1".into(), Some(123), None, None, None)
            .await;
        assert!(err.is_err());
        let agent = store.by_session("claude-1").await.unwrap();
        assert_eq!(agent.kind, AgentKind::ClaudeCode);
        assert_eq!(agent.pid, None);
    }

    #[tokio::test]
    async fn register_task_disambiguates_duplicate_names() {
        let store = Store::shared();
        // Two `muxa run sleep` with distinct pids must coexist, not clobber.
        let a = store
            .register_task("sleep".into(), Some(111), None, None, None)
            .await
            .unwrap();
        let b = store
            .register_task("sleep".into(), Some(222), None, None, None)
            .await
            .unwrap();
        assert_eq!(a, "sleep");
        assert_ne!(a, b);
        assert!(store.by_session(&a).await.is_some());
        assert!(store.by_session(&b).await.is_some());
        // Same pid re-registering is idempotent (same key).
        let a2 = store
            .register_task("sleep".into(), Some(111), None, None, None)
            .await
            .unwrap();
        assert_eq!(a2, "sleep");
    }

    #[tokio::test]
    async fn reap_dead_pids_stops_dead_keeps_alive() {
        let store = Store::shared();
        // A pid that cannot exist (max u32) is dead; our own pid is alive.
        store
            .register_task("dead".into(), Some(u32::MAX), None, None, None)
            .await
            .unwrap();
        store
            .register_task("alive".into(), Some(std::process::id()), None, None, None)
            .await
            .unwrap();
        let flipped = store.reap_dead_pids().await;
        assert_eq!(flipped, 1);
        let dead = store.by_session("dead").await.unwrap();
        assert_eq!(dead.state, AgentState::Stopped);
        // last_activity_at is refreshed so GC measures the TTL from death.
        assert!(dead.last_activity_at >= dead.started_at);
        assert_eq!(
            store.by_session("alive").await.unwrap().state,
            AgentState::Working
        );
        // Idempotent: a second pass flips nothing new.
        assert_eq!(store.reap_dead_pids().await, 0);
    }

    #[tokio::test]
    async fn mark_stale_paneless_stopped_reaps_only_orphan_stale_rows() {
        use crate::event::{SurfaceKind, SurfaceRef};
        let store = Store::shared();
        let old = datetime!(2026-04-24 12:00:00 UTC); // long before "now"
        let fresh = OffsetDateTime::now_utc();

        let started = |sid: &str, pane: Option<String>, surface: Option<SurfaceRef>, at| {
            AgentEvent::Started {
                id: AgentId {
                    tmux_socket: None,
                    kind: AgentKind::Codex,
                    session_id: sid.into(),
                    surface,
                    pane,
                    cwd: None,
                },
                at,
            }
        };

        // Orphan + stale → should flip.
        store.apply(&started("orphan", None, None, old)).await;
        // Orphan but fresh → spared by the age gate.
        store
            .apply(&started("orphan-fresh", None, None, fresh))
            .await;
        // Paneless but surface-tracked → not an orphan, spared.
        store
            .apply(&started(
                "surface",
                None,
                Some(SurfaceRef {
                    kind: SurfaceKind::Pty,
                    id: "s1".into(),
                }),
                old,
            ))
            .await;
        // Has a pane → governed by tmux liveness, spared.
        store
            .apply(&started("paned", Some("%1".into()), None, old))
            .await;

        let flipped = store
            .mark_stale_paneless_stopped(Duration::from_secs(86_400))
            .await;
        assert_eq!(flipped, 1, "only the stale orphan should flip");
        assert_eq!(
            store.by_session("orphan").await.unwrap().state,
            AgentState::Stopped
        );
        assert_ne!(
            store.by_session("orphan-fresh").await.unwrap().state,
            AgentState::Stopped
        );
        assert_ne!(
            store.by_session("surface").await.unwrap().state,
            AgentState::Stopped
        );
        assert_ne!(
            store.by_session("paned").await.unwrap().state,
            AgentState::Stopped
        );

        // Idempotent, and a zero threshold disables the sweep entirely.
        assert_eq!(
            store
                .mark_stale_paneless_stopped(Duration::from_secs(86_400))
                .await,
            0
        );
        store.apply(&started("orphan2", None, None, old)).await;
        assert_eq!(store.mark_stale_paneless_stopped(Duration::ZERO).await, 0);
        assert_ne!(
            store.by_session("orphan2").await.unwrap().state,
            AgentState::Stopped
        );
    }

    #[tokio::test]
    async fn mark_stale_cross_host_ages_out_only_foreign_host_stale_rows() {
        let store = Store::shared();
        let old = datetime!(2026-04-24 12:00:00 UTC); // long before "now"
        let fresh = OffsetDateTime::now_utc();

        let started = |sid: &str, pane: &str, at| AgentEvent::Started {
            id: AgentId {
                tmux_socket: None,
                kind: AgentKind::ClaudeCode,
                session_id: sid.into(),
                surface: None,
                pane: Some(pane.into()),
                cwd: None,
            },
            at,
        };

        // A tmux daemon observing tmux. A stale `herdr:` row is foreign and
        // unobservable ⇒ must age out. A fresh `herdr:` row is spared by the
        // age gate. A stale tmux `%N` row is same-host (governed by reconcile)
        // ⇒ untouched here.
        store.apply(&started("herdr-stale", "herdr:p1", old)).await;
        store
            .apply(&started("herdr-fresh", "herdr:p2", fresh))
            .await;
        store.apply(&started("tmux-stale", "%1", old)).await;

        let flipped = store
            .mark_stale_cross_host_stopped(&[HostKind::Tmux], Duration::from_secs(86_400))
            .await;
        assert_eq!(flipped, 1, "only the stale foreign-host row should flip");
        assert_eq!(
            store.by_session("herdr-stale").await.unwrap().state,
            AgentState::Stopped,
        );
        assert_ne!(
            store.by_session("herdr-fresh").await.unwrap().state,
            AgentState::Stopped,
            "a fresh foreign-host row keeps itself alive via activity",
        );
        assert_ne!(
            store.by_session("tmux-stale").await.unwrap().state,
            AgentState::Stopped,
            "same-host reaping is unchanged — reconcile governs it, not this sweep",
        );

        // Idempotent, and a zero threshold disables the sweep.
        assert_eq!(
            store
                .mark_stale_cross_host_stopped(&[HostKind::Tmux], Duration::from_secs(86_400))
                .await,
            0,
        );
        assert_eq!(
            store
                .mark_stale_cross_host_stopped(&[HostKind::Tmux], Duration::ZERO)
                .await,
            0,
        );
    }

    #[tokio::test]
    async fn mark_stale_cross_host_spares_rows_of_an_observed_host() {
        // A herdr daemon observing herdr must NOT age out its own `herdr:`
        // rows even when stale — reconcile governs same-host liveness.
        let store = Store::shared();
        let old = datetime!(2026-04-24 12:00:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "herdr-own".into(),
                    surface: None,
                    pane: Some("herdr:p9".into()),
                    cwd: None,
                },
                at: old,
            })
            .await;
        let flipped = store
            .mark_stale_cross_host_stopped(&[HostKind::Herdr], Duration::from_secs(86_400))
            .await;
        assert_eq!(flipped, 0);
        assert_ne!(
            store.by_session("herdr-own").await.unwrap().state,
            AgentState::Stopped,
        );
    }

    #[tokio::test]
    async fn prune_orphans_deletes_only_stale_ownerless_rows() {
        let store = Store::shared();
        let old = datetime!(2026-04-24 12:00:00 UTC);
        let fresh = OffsetDateTime::now_utc();
        let codex = |sid: &str, pane: Option<String>, at| AgentEvent::Started {
            id: AgentId {
                tmux_socket: None,
                kind: AgentKind::Codex,
                session_id: sid.into(),
                surface: None,
                pane,
                cwd: None,
            },
            at,
        };
        store.apply(&codex("ghost", None, old)).await; // orphan + stale → gone
        store.apply(&codex("live", None, fresh)).await; // orphan but fresh → kept
        store.apply(&codex("paned", Some("%1".into()), old)).await; // has pane → kept

        // Cutoff one hour ago: deletes the stale orphan, spares the fresh one.
        let cutoff = OffsetDateTime::now_utc() - Duration::from_secs(3600);
        let removed = store.prune_orphans(cutoff).await;
        assert_eq!(removed, 1);
        assert!(store.by_session("ghost").await.is_none());
        assert!(store.by_session("live").await.is_some());
        assert!(store.by_session("paned").await.is_some());
    }

    #[tokio::test]
    async fn reconcile_never_reaps_pid_tracked_rows() {
        let store = Store::shared();
        // Pid-tracked task carrying a pane id that is NOT in the live set.
        store
            .register_task(
                "task".into(),
                Some(std::process::id()),
                None,
                Some("%999".into()),
                None,
            )
            .await
            .unwrap();
        // Reconcile against an empty live-pane set — a normal pane agent
        // would be reaped, but the pid-tracked row must survive.
        let report = store.reconcile(&[]).await;
        assert_eq!(report.stale_panes_reaped, 0);
        assert!(store.by_session("task").await.is_some());
    }

    #[tokio::test]
    async fn update_workloads_refreshes_metadata_without_touching_activity() {
        let store = Store::shared();
        let at = datetime!(2026-05-10 12:00:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "agent".into(),
                    surface: None,
                    pane: Some("%1".into()),
                    cwd: None,
                },
                at,
            })
            .await;

        let mut by_pane = HashMap::new();
        by_pane.insert(
            "%1".to_string(),
            WorkloadSummary {
                primary_pid: Some(20),
                process_count: 2,
                shell_count: 1,
                subagent_count: 0,
                helper_count: 1,
                preview: Vec::new(),
            },
        );

        assert_eq!(store.update_workloads(&by_pane, &[HostKind::Tmux]).await, 1);
        let snap = store.snapshot().await;
        assert_eq!(snap[0].workload.shell_count, 1);
        assert_eq!(snap[0].workload.process_count, 2);
        assert_eq!(snap[0].last_activity_at, at);

        assert_eq!(store.update_workloads(&by_pane, &[HostKind::Tmux]).await, 0);
        assert_eq!(
            store
                .update_workloads(&HashMap::new(), &[HostKind::Tmux])
                .await,
            1
        );
        assert!(store.snapshot().await[0].workload.is_empty());
    }

    /// Fix 3: when only some hosts are observed complete this tick, the
    /// workload update must govern only the complete hosts' rows. A tmux row
    /// (complete) updates from the scan; a herdr row (incomplete this tick, so
    /// `Herdr` absent from `complete_kinds`) keeps its previous workload
    /// instead of being reset to the default by a scan that never covered herdr.
    #[tokio::test]
    async fn update_workloads_governs_only_complete_hosts() {
        let store = Store::shared();
        let at = datetime!(2026-05-10 12:00:00 UTC);
        let started = |sid: &str, pane: &str| AgentEvent::Started {
            id: AgentId {
                tmux_socket: None,
                kind: AgentKind::ClaudeCode,
                session_id: sid.into(),
                surface: None,
                pane: Some(pane.into()),
                cwd: None,
            },
            at,
        };
        store.apply(&started("tmux-row", "%1")).await;
        store.apply(&started("herdr-row", "herdr:p1")).await;

        let wl = |n: u16| WorkloadSummary {
            primary_pid: Some(20),
            process_count: n,
            shell_count: 1,
            subagent_count: 0,
            helper_count: 0,
            preview: Vec::new(),
        };

        // Seed both rows with a non-default workload (as a prior complete tick
        // would have), so a wrongful reset is observable.
        let mut seed = HashMap::new();
        seed.insert("%1".to_string(), wl(2));
        seed.insert("herdr:p1".to_string(), wl(3));
        assert_eq!(
            store
                .update_workloads(&seed, &[HostKind::Tmux, HostKind::Herdr])
                .await,
            2
        );

        // Now only tmux is complete; the scan map covers only tmux panes.
        let mut tmux_scan = HashMap::new();
        tmux_scan.insert("%1".to_string(), wl(5));
        let changed = store.update_workloads(&tmux_scan, &[HostKind::Tmux]).await;

        assert_eq!(changed, 1, "only the tmux row's workload changes");
        let snap = store.snapshot().await;
        let tmux = snap.iter().find(|a| a.session_id == "tmux-row").unwrap();
        let herdr = snap.iter().find(|a| a.session_id == "herdr-row").unwrap();
        assert_eq!(tmux.workload.process_count, 5, "tmux row updated");
        assert_eq!(
            herdr.workload.process_count, 3,
            "herdr row (incomplete this tick) keeps its previous workload",
        );
    }

    #[tokio::test]
    async fn reconcile_keeps_task_and_agent_sharing_a_pane() {
        // Regression: a Task that carries a `--pane` must NOT be deduped
        // against the real agent owning that pane — both must survive.
        let store = Store::shared();
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "agent".into(),
                    surface: None,
                    pane: Some("%1".into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;
        store
            .register_task(
                "task".into(),
                Some(std::process::id()),
                None,
                Some("%1".into()),
                None,
            )
            .await
            .unwrap();
        let pane = PaneInfo {
            socket: None,
            pane_id: "%1".into(),
            session: "s".into(),
            window_index: "0".into(),
            pane_index: "0".into(),
            tty: String::new(),
            current_command: "claude".into(),
            title: String::new(),
            pane_pid: 0,
            current_path: String::new(),
        };
        store.reconcile(std::slice::from_ref(&pane)).await;
        assert!(store.by_session("agent").await.is_some());
        assert!(store.by_session("task").await.is_some());
    }

    #[tokio::test]
    async fn stuck_idle_sweep_never_demotes_tasks() {
        let store = Store::shared();
        store
            .register_task("task".into(), Some(std::process::id()), None, None, None)
            .await
            .unwrap();
        // A 1ns threshold makes everything "stuck"; the task must stay Working.
        let flipped = store
            .mark_stuck_idle_from(AgentState::Working, std::time::Duration::from_nanos(1))
            .await;
        assert_eq!(flipped, 0);
        assert_eq!(
            store.by_session("task").await.unwrap().state,
            AgentState::Working
        );
    }

    #[tokio::test]
    async fn hydrate_downgrades_legacy_claude_idle_prompt_waiting_input() {
        let store = Store::shared();
        store
            .hydrate(vec![stored_agent(
                "legacy-idle",
                AgentKind::ClaudeCode,
                AgentState::WaitingInput,
                Some(CLAUDE_IDLE_PROMPT_NOTIFICATION),
            )])
            .await;

        let agent = store.by_session("legacy-idle").await.unwrap();
        assert_eq!(agent.state, AgentState::Idle);
        assert_eq!(
            agent.last_notification.as_deref(),
            Some(CLAUDE_IDLE_PROMPT_NOTIFICATION)
        );
    }

    #[tokio::test]
    async fn hydrate_preserves_real_waiting_input_notifications() {
        let store = Store::shared();
        store
            .hydrate(vec![stored_agent(
                "permission",
                AgentKind::ClaudeCode,
                AgentState::WaitingInput,
                Some("permission required"),
            )])
            .await;

        assert_eq!(
            store.by_session("permission").await.unwrap().state,
            AgentState::WaitingInput
        );
    }

    #[tokio::test]
    async fn tool_started_recovers_from_waiting_input() {
        // Codex permission-grant scenario: Notification flips the row
        // to WaitingInput, user grants, the next tool runs, and
        // ToolStarted should auto-recover the row to Working without
        // needing the timeout sweep.
        let store = Store::shared();
        let now = datetime!(2026-05-05 12:00:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: id("c"),
                at: now,
            })
            .await;
        store
            .apply(&AgentEvent::NotificationFired {
                id: id("c"),
                level: NotificationLevel::NeedsInput,
                message: "permission".into(),
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("c").await.unwrap().state,
            AgentState::WaitingInput
        );

        store
            .apply(&AgentEvent::ToolStarted {
                id: id("c"),
                tool: "Bash".into(),
                subagent: None,
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("c").await.unwrap().state,
            AgentState::Working,
            "ToolStarted should recover WaitingInput → Working"
        );
    }

    #[tokio::test]
    async fn task_tool_tracks_subagents_as_first_class_rows() {
        let store = Store::shared();
        let now = datetime!(2026-05-05 12:00:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: id("c"),
                at: now,
            })
            .await;

        let spawn = |kind: &str| AgentEvent::ToolStarted {
            id: id("c"),
            tool: "Task".into(),
            subagent: Some(crate::event::SubagentSpec {
                kind: kind.into(),
                description: None,
            }),
            at: now,
        };
        store.apply(&spawn("Explore")).await;
        store.apply(&spawn("general-purpose")).await;

        let a = store.by_session("c").await.unwrap();
        assert_eq!(a.subagents.len(), 2, "both Task children are tracked");
        assert_eq!(a.subagents[0].kind, "Explore");
        assert_eq!(a.subagents[1].kind, "general-purpose");

        // A completed Task retires the oldest in-flight subagent (FIFO).
        store
            .apply(&AgentEvent::ToolCompleted {
                id: id("c"),
                tool: "Task".into(),
                success: true,
                at: now,
            })
            .await;
        let a = store.by_session("c").await.unwrap();
        assert_eq!(a.subagents.len(), 1);
        assert_eq!(a.subagents[0].kind, "general-purpose");

        // A non-Task completion leaves the subagent list untouched.
        store
            .apply(&AgentEvent::ToolCompleted {
                id: id("c"),
                tool: "Bash".into(),
                success: true,
                at: now,
            })
            .await;
        assert_eq!(store.by_session("c").await.unwrap().subagents.len(), 1);

        // Turn end clears any stragglers so an idle row never shows phantoms.
        store
            .apply(&AgentEvent::TurnStopped {
                id: id("c"),
                response: Some("done".into()),
                recap: None,
                ai_title: None,
                at: now,
            })
            .await;
        assert!(store.by_session("c").await.unwrap().subagents.is_empty());
    }

    #[tokio::test]
    async fn tool_completed_recovers_from_waiting_input() {
        // Generic Notification flow (e.g., free-text permission prompt):
        // NeedsInput lands the row in WaitingInput; the matching
        // PostToolUse → ToolCompleted should flip the row back to
        // Working.
        let store = Store::shared();
        let now = datetime!(2026-05-05 12:00:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: id("c"),
                at: now,
            })
            .await;
        store
            .apply(&AgentEvent::NotificationFired {
                id: id("c"),
                level: NotificationLevel::NeedsInput,
                message: "ask".into(),
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("c").await.unwrap().state,
            AgentState::WaitingInput
        );

        store
            .apply(&AgentEvent::ToolCompleted {
                id: id("c"),
                tool: "AskUserQuestion".into(),
                success: true,
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("c").await.unwrap().state,
            AgentState::Working,
            "ToolCompleted should recover WaitingInput → Working"
        );
    }

    #[tokio::test]
    async fn tool_completed_recovers_from_waiting_choice() {
        // Claude AskUserQuestion scenario: PreToolUse routes through
        // NotificationFired { NeedsChoice } so the row reads
        // WaitingChoice while the menu is up; the matching PostToolUse
        // → ToolCompleted lands when the user picks an option and
        // should flip the row back to Working.
        let store = Store::shared();
        let now = datetime!(2026-05-05 12:00:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: id("c"),
                at: now,
            })
            .await;
        store
            .apply(&AgentEvent::NotificationFired {
                id: id("c"),
                level: NotificationLevel::NeedsChoice,
                message: "waiting on AskUserQuestion".into(),
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("c").await.unwrap().state,
            AgentState::WaitingChoice
        );

        store
            .apply(&AgentEvent::ToolCompleted {
                id: id("c"),
                tool: "AskUserQuestion".into(),
                success: true,
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("c").await.unwrap().state,
            AgentState::Working,
            "ToolCompleted should recover WaitingChoice → Working"
        );
    }

    #[tokio::test]
    async fn response_less_turn_stopped_preserves_waiting_input() {
        let store = Store::shared();
        let now = datetime!(2026-05-05 12:00:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: id("codex"),
                at: now,
            })
            .await;
        store
            .apply(&AgentEvent::NotificationFired {
                id: id("codex"),
                level: NotificationLevel::NeedsInput,
                message: "codex permission: Bash".into(),
                at: now,
            })
            .await;

        store
            .apply(&AgentEvent::TurnStopped {
                id: id("codex"),
                response: None,
                recap: None,
                ai_title: None,
                at: now,
            })
            .await;

        assert_eq!(
            store.by_session("codex").await.unwrap().state,
            AgentState::WaitingInput,
            "Codex Stop without response must not hide an outstanding permission prompt"
        );
    }

    #[tokio::test]
    async fn tool_started_preserves_error_state() {
        // Errors aren't transient activity — a tool firing while a row
        // is red shouldn't silently mask the failure.
        let store = Store::shared();
        let now = datetime!(2026-05-05 12:00:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: id("e"),
                at: now,
            })
            .await;
        store
            .apply(&AgentEvent::NotificationFired {
                id: id("e"),
                level: NotificationLevel::Error,
                message: "boom".into(),
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("e").await.unwrap().state,
            AgentState::Error
        );

        store
            .apply(&AgentEvent::ToolStarted {
                id: id("e"),
                tool: "Read".into(),
                subagent: None,
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("e").await.unwrap().state,
            AgentState::Error,
            "ToolStarted must not clobber Error"
        );
    }

    #[tokio::test]
    async fn tool_completed_leaves_idle_alone() {
        // A stray ToolCompleted with no PromptSubmitted before it
        // shouldn't fake activity — only the WaitingInput → Working
        // recovery path is special.
        let store = Store::shared();
        let t0 = datetime!(2026-05-05 12:00:00 UTC);
        let t1 = datetime!(2026-05-05 12:05:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: id("i"),
                at: t0,
            })
            .await;
        assert_eq!(store.by_session("i").await.unwrap().state, AgentState::Idle);

        store
            .apply(&AgentEvent::ToolCompleted {
                id: id("i"),
                tool: "Read".into(),
                success: true,
                at: t1,
            })
            .await;
        let agent = store.by_session("i").await.unwrap();
        assert_eq!(agent.state, AgentState::Idle);
        assert_eq!(
            agent.last_activity_at, t0,
            "stray ToolCompleted must not refresh ACT for an idle row"
        );
    }

    #[tokio::test]
    async fn heartbeat_promotes_starting_to_idle() {
        // Common Claude case: a synthetic discovery placeholder (or
        // a fresh row from `or_insert_with(Agent::new)`) starts at
        // `Starting`. The first Heartbeat from the statusLine
        // doesn't carry an explicit transition — without the
        // catch-all promotion the row would paint cyan forever.
        let store = Store::shared();
        let now = datetime!(2026-05-05 12:00:00 UTC);
        store
            .apply(&AgentEvent::Heartbeat {
                id: id("hb"),
                model: Some("opus".into()),
                context_used_pct: None,
                cost_usd: None,
                rate_limit_5h_pct: None,
                rate_limit_5h_resets_at: None,
                rate_limit_7d_pct: None,
                rate_limit_7d_resets_at: None,
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("hb").await.unwrap().state,
            AgentState::Idle,
            "Heartbeat as the first event should promote Starting → Idle"
        );
    }

    #[tokio::test]
    async fn tool_completed_promotes_starting_to_idle() {
        // Out-of-order case: PostToolUse lands before we've seen the
        // matching PreToolUse / SessionStart. Without promotion the
        // row would stay `Starting` until something else fires.
        let store = Store::shared();
        let now = datetime!(2026-05-05 12:00:00 UTC);
        store
            .apply(&AgentEvent::ToolCompleted {
                id: id("tc"),
                tool: "Read".into(),
                success: true,
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("tc").await.unwrap().state,
            AgentState::Idle
        );
    }

    #[tokio::test]
    async fn rate_limited_sets_error_not_starting() {
        // RateLimited triggers `apply_rate_limited` which sets
        // state = Error. The catch-all promotion only fires for
        // `Starting`, so Error wins (and the agent reads as red,
        // which is the correct UX for a hit limit).
        let store = Store::shared();
        let now = datetime!(2026-05-05 12:00:00 UTC);
        store
            .apply(&AgentEvent::RateLimited {
                id: id("rl"),
                scope: crate::event::RateLimitScope::Unknown,
                source: crate::event::RateLimitSource::Transcript,
                resets_at: None,
                message: Some("hit".into()),
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("rl").await.unwrap().state,
            AgentState::Error,
            "RateLimited explicitly sets Error — the Starting promotion must not interfere"
        );
    }

    #[tokio::test]
    async fn promotion_does_not_clobber_explicit_states() {
        // Sanity: events that DO set a state explicitly (Started →
        // Idle, PromptSubmitted → Working, NotificationFired
        // NeedsInput → WaitingInput, etc.) win — the catch-all is a
        // no-op because the state is already non-`Starting`.
        let store = Store::shared();
        let now = datetime!(2026-05-05 12:00:00 UTC);
        store
            .apply(&AgentEvent::PromptSubmitted {
                id: id("ps"),
                prompt: "hi".into(),
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("ps").await.unwrap().state,
            AgentState::Working,
            "PromptSubmitted explicitly sets Working — promotion must not interfere"
        );
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
                recap: None,
                ai_title: None,
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("s").await.unwrap().state,
            AgentState::WaitingInput
        );

        store
            .apply(&AgentEvent::ToolStarted {
                id: id("s"),
                tool: "Bash".into(),
                subagent: None,
                at: now,
            })
            .await;
        store
            .apply(&AgentEvent::TurnStopped {
                id: id("s"),
                response: None,
                recap: None,
                ai_title: None,
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
                recap: None,
                ai_title: None,
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
                recap: None,
                ai_title: None,
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
                recap: None,
                ai_title: None,
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
                recap: None,
                ai_title: None,
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
                recap: None,
                ai_title: None,
                at: now,
            })
            .await;
        let agent = store.by_session("s").await.unwrap();
        assert_eq!(agent.last_response.as_deref(), Some("first answer"));
    }

    #[tokio::test]
    async fn responseless_stop_keeps_real_waiting_row_waiting() {
        // A REAL (hook) row waiting on a permission prompt must stay waiting on
        // a response-less TurnStopped — the Codex Stop-during-permission guard.
        let store = Store::shared();
        let now = datetime!(2026-07-20 12:00:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: id("real"),
                at: now,
            })
            .await;
        store
            .apply(&AgentEvent::NotificationFired {
                id: id("real"),
                level: NotificationLevel::NeedsInput,
                message: "approve?".into(),
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("real").await.unwrap().state,
            AgentState::WaitingInput
        );
        store
            .apply(&AgentEvent::TurnStopped {
                id: id("real"),
                response: None,
                recap: None,
                ai_title: None,
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("real").await.unwrap().state,
            AgentState::WaitingInput,
            "a real waiting row stays waiting on a response-less stop",
        );
    }

    #[tokio::test]
    async fn responseless_stop_clears_synthetic_waiting_row_to_idle() {
        // A SYNTHETIC detection row (screen inference / herdr bridge) that was
        // WaitingInput and now reports idle (a response-less TurnStopped) must
        // fall through to Idle — the screen no longer shows the prompt, so the
        // wait genuinely cleared. This is what lets a screen `blocked -> idle`
        // transition land.
        let store = Store::shared();
        let now = datetime!(2026-07-20 12:00:00 UTC);
        store
            .apply(&AgentEvent::NotificationFired {
                id: id("synthetic-%1"),
                level: NotificationLevel::NeedsInput,
                message: "approve?".into(),
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("synthetic-%1").await.unwrap().state,
            AgentState::WaitingInput,
        );
        store
            .apply(&AgentEvent::TurnStopped {
                id: id("synthetic-%1"),
                response: None,
                recap: None,
                ai_title: None,
                at: now,
            })
            .await;
        assert_eq!(
            store.by_session("synthetic-%1").await.unwrap().state,
            AgentState::Idle,
            "a synthetic waiting row clears to Idle on a response-less stop",
        );
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
        assert!(t.agent.state_entered_at > stale_at);
        assert_eq!(t.agent.state_entered_at, t.agent.last_activity_at);
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
            tmux_socket: None,
            kind: AgentKind::ClaudeCode,
            session_id: "synthetic-%1".into(),
            surface: None,
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
    async fn synthetic_started_distinguishes_same_pane_id_on_different_sockets() {
        let store = Store::shared();
        let now = datetime!(2026-04-24 12:00:00 UTC);

        for (session_id, socket) in [
            ("synthetic-7:default:%1", "default"),
            ("synthetic-4:amux:%1", "amux"),
        ] {
            store
                .apply(&AgentEvent::Started {
                    id: AgentId {
                        kind: AgentKind::ClaudeCode,
                        session_id: session_id.into(),
                        surface: None,
                        pane: Some("%1".into()),
                        tmux_socket: Some(socket.into()),
                        cwd: None,
                    },
                    at: now,
                })
                .await;
        }

        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 2);
        assert!(snap.iter().all(|agent| agent.state == AgentState::Idle));
        assert!(snap
            .iter()
            .any(|agent| agent.tmux_socket.as_deref() == Some("default")));
        assert!(snap
            .iter()
            .any(|agent| agent.tmux_socket.as_deref() == Some("amux")));
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
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "synthetic-%7".into(),
                    surface: None,
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
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "real-sess".into(),
                    surface: None,
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
    async fn later_real_event_binds_paneless_session_and_removes_synthetic() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        let t1 = datetime!(2026-04-24 12:01:00 UTC);
        let t2 = datetime!(2026-04-24 12:02:00 UTC);

        // Discovery sees Codex in the pane, while SessionStart arrives
        // without TMUX_PANE and creates a separate paneless real row.
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::Codex,
                    session_id: "synthetic-%7".into(),
                    surface: None,
                    pane: Some("%7".into()),
                    tmux_socket: None,
                    cwd: None,
                },
                at: t0,
            })
            .await;
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::Codex,
                    session_id: "real-sess".into(),
                    surface: None,
                    pane: None,
                    tmux_socket: None,
                    cwd: Some("/work".into()),
                },
                at: t1,
            })
            .await;
        assert_eq!(store.snapshot().await.len(), 2);

        // A later hook recovers the pane through ancestry. SessionStart is
        // not repeated, so the ordinary event must heal both identity and
        // the stale synthetic placeholder.
        store
            .apply(&AgentEvent::ToolStarted {
                id: AgentId {
                    kind: AgentKind::Codex,
                    session_id: "real-sess".into(),
                    surface: None,
                    pane: Some("%7".into()),
                    tmux_socket: None,
                    cwd: Some("/work".into()),
                },
                tool: "Bash".into(),
                subagent: None,
                at: t2,
            })
            .await;

        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1, "late pane binding should remove synthetic");
        assert_eq!(snap[0].session_id, "real-sess");
        assert_eq!(snap[0].pane.as_deref(), Some("%7"));
        assert_eq!(snap[0].state, AgentState::Working);
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
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "real-sess".into(),
                    surface: None,
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
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "synthetic-%9".into(),
                    surface: None,
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

    /// A `code_mode_host` codex splits into a synthetic pane placeholder
    /// (discovery) plus a paneless real hook row (the app-server fires hooks
    /// with no `TMUX_PANE`). Reconcile must rejoin them by cwd: the real row
    /// adopts the pane, and the synthetic is demoted in the same pass.
    #[tokio::test]
    async fn reconcile_correlates_paneless_codex_by_cwd() {
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        let codex = |sid: &str, pane: Option<String>, cwd: Option<String>| AgentEvent::Started {
            id: AgentId {
                tmux_socket: None,
                kind: AgentKind::Codex,
                session_id: sid.into(),
                surface: None,
                pane,
                cwd,
            },
            at: t0,
        };

        // 1:1 pairing — the real hook row adopts the pane, synthetic collapses.
        let store = Store::shared();
        store
            .apply(&codex("synthetic-%1", Some("%1".into()), None))
            .await;
        store
            .apply(&codex("019f-real", None, Some("/Users/jiun/proj".into())))
            .await;
        let mut p = pane("%1");
        p.current_path = "/Users/jiun/proj".into();
        let report = store.reconcile(&[p]).await;
        assert_eq!(report.paneless_correlated, 1, "real row should adopt %1");
        assert_eq!(report.synthetic_demoted, 1, "synthetic collapses same pass");
        let snap = store.snapshot().await;
        assert_eq!(
            snap.iter()
                .find(|a| a.session_id == "019f-real")
                .unwrap()
                .pane
                .as_deref(),
            Some("%1"),
        );
        assert!(!snap.iter().any(|a| a.session_id == "synthetic-%1"));

        // Ambiguous — two paneless rows share a cwd with one candidate pane,
        // so neither is adopted (a wrong pane is worse than none).
        let store = Store::shared();
        store
            .apply(&codex("synthetic-%2", Some("%2".into()), None))
            .await;
        store
            .apply(&codex("real-a", None, Some("/amb".into())))
            .await;
        store
            .apply(&codex("real-b", None, Some("/amb".into())))
            .await;
        let mut p = pane("%2");
        p.current_path = "/amb".into();
        let report = store.reconcile(&[p]).await;
        assert_eq!(report.paneless_correlated, 0, "ambiguous cwd left alone");
        let snap = store.snapshot().await;
        assert!(snap
            .iter()
            .find(|a| a.session_id == "real-a")
            .unwrap()
            .pane
            .is_none());
        assert!(snap
            .iter()
            .find(|a| a.session_id == "real-b")
            .unwrap()
            .pane
            .is_none());

        // No cwd match — a paneless row whose cwd matches no codex pane stays
        // paneless (and a pane with no current_path can't be a candidate).
        let store = Store::shared();
        store
            .apply(&codex("synthetic-%3", Some("%3".into()), None))
            .await;
        store
            .apply(&codex("real-c", None, Some("/elsewhere".into())))
            .await;
        let mut p = pane("%3");
        p.current_path = "/different".into();
        let report = store.reconcile(&[p]).await;
        assert_eq!(report.paneless_correlated, 0, "no cwd match");
        assert!(store
            .snapshot()
            .await
            .iter()
            .find(|a| a.session_id == "real-c")
            .unwrap()
            .pane
            .is_none());
    }

    fn pane(id: &str) -> PaneInfo {
        PaneInfo {
            socket: None,
            pane_id: id.into(),
            session: "s".into(),
            window_index: "0".into(),
            pane_index: "0".into(),
            tty: String::new(),
            current_command: "claude".into(),
            title: String::new(),
            pane_pid: 0,
            current_path: String::new(),
        }
    }

    /// Start an agent on an arbitrary pane id (any host namespace) so the
    /// cross-host reaping-guard tests can plant `herdr:…` / `zellij:…` / `%N`
    /// rows without the `%1`-defaulting `id()` helper.
    fn started_on(sid: &str, pane_id: &str) -> AgentEvent {
        AgentEvent::Started {
            id: AgentId {
                tmux_socket: None,
                kind: AgentKind::ClaudeCode,
                session_id: sid.into(),
                surface: None,
                pane: Some(pane_id.into()),
                cwd: None,
            },
            at: datetime!(2026-04-24 12:00:00 UTC),
        }
    }

    /// Cross-host reaping guard: a `herdr:`-namespaced row must survive a
    /// **tmux** observation that doesn't contain it. A tmux-backend daemon
    /// physically can't see herdr panes, so their absence from its scan is not
    /// death — reaping them would be fatal during a tmux→herdr migration where
    /// both hosts run at once. The same-host tmux ghost is still reaped.
    #[tokio::test]
    async fn herdr_row_survives_tmux_observation() {
        let store = Store::shared();
        store.apply(&started_on("herdr-live", "herdr:7")).await;
        store.apply(&started_on("tmux-live", "%1")).await;
        store.apply(&started_on("tmux-ghost", "%2")).await;

        // Tmux backend observed only %1. It never enumerates herdr panes.
        let report = store.reconcile_hosted(&[pane("%1")], HostKind::Tmux).await;

        assert_eq!(report.stale_panes_reaped, 1, "only the tmux ghost reaps");
        let ids: Vec<String> = store
            .snapshot()
            .await
            .into_iter()
            .map(|a| a.session_id)
            .collect();
        assert!(ids.contains(&"herdr-live".to_string()), "herdr row exempt");
        assert!(ids.contains(&"tmux-live".to_string()));
        assert!(
            !ids.contains(&"tmux-ghost".to_string()),
            "tmux ghost reaped"
        );
    }

    /// Mirror of the above from the other side: a tmux `%N` row survives a
    /// **herdr** observation. A herdr-backend daemon can't see tmux panes, so
    /// their absence isn't evidence of death. The same-host herdr ghost reaps.
    #[tokio::test]
    async fn tmux_row_survives_herdr_observation() {
        let store = Store::shared();
        store.apply(&started_on("tmux-live", "%9")).await;
        store.apply(&started_on("herdr-live", "herdr:1")).await;
        store.apply(&started_on("herdr-ghost", "herdr:2")).await;

        // Herdr backend observed only herdr:1.
        let report = store
            .reconcile_hosted(&[pane("herdr:1")], HostKind::Herdr)
            .await;

        assert_eq!(report.stale_panes_reaped, 1, "only the herdr ghost reaps");
        let ids: Vec<String> = store
            .snapshot()
            .await
            .into_iter()
            .map(|a| a.session_id)
            .collect();
        assert!(ids.contains(&"tmux-live".to_string()), "tmux row exempt");
        assert!(ids.contains(&"herdr-live".to_string()));
        assert!(
            !ids.contains(&"herdr-ghost".to_string()),
            "herdr ghost reaped"
        );
    }

    /// An unknown-shape pane id (no `%` / `zellij:` / `herdr:` namespace) has
    /// no host classification, so it stays governed by the active backend —
    /// exactly as before the guard existed. Here a tmux observation without it
    /// reaps it. Guards against the exemption silently swallowing legacy rows.
    #[tokio::test]
    async fn unknown_shape_pane_governed_by_active_backend() {
        let store = Store::shared();
        store.apply(&started_on("legacy", "weird-id")).await;

        // Tmux observation contains no matching pane → legacy row reaped,
        // because `pane_id_host_kind("weird-id")` is `None` (not exempt).
        let report = store.reconcile_hosted(&[], HostKind::Tmux).await;

        assert_eq!(report.stale_panes_reaped, 1, "unknown-shape row reaped");
        assert!(store.snapshot().await.is_empty());
    }

    /// Synthetic must lose to a real session even when the real session is
    /// already `Stopped`. Without this rule, a `muxa sync` pass after the
    /// real agent ended would re-introduce a synthetic placeholder for the
    /// same pane, producing a duplicate row in `muxa watch`.
    /// `muxa sync` ran after the user restarted claude in the same pane:
    /// the old real session is `Stopped` but the pane is alive again.
    /// The synthetic placeholder must evict the stale `Stopped` entry
    /// and take over — otherwise `muxa status` would report the pane
    /// as `stopped` indefinitely (until a real hook fires, which may
    /// take minutes for an idle claude).
    #[tokio::test]
    async fn synthetic_replaces_stopped_real_on_same_pane() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);

        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "real".into(),
                    surface: None,
                    pane: Some("%5".into()),
                    cwd: None,
                },
                at: t0,
            })
            .await;
        store
            .apply(&AgentEvent::SessionEnded {
                id: AgentId {
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "real".into(),
                    surface: None,
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
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "synthetic-%5".into(),
                    surface: None,
                    pane: Some("%5".into()),
                    cwd: None,
                },
                at: t0,
            })
            .await;

        let snap = store.snapshot().await;
        assert_eq!(
            snap.len(),
            1,
            "synthetic should evict the stale stopped predecessor"
        );
        assert_eq!(snap[0].session_id, "synthetic-%5");
        assert_ne!(snap[0].state, AgentState::Stopped);
        assert!(
            store.by_session("real").await.is_none(),
            "stale Stopped real session must be evicted"
        );
    }

    /// One pane id, two servers: the reconciler must backfill each agent's
    /// `tmux_session` from the pane on ITS server (socket-tagged rows), and
    /// prefer the default server for legacy untagged rows.
    #[tokio::test]
    async fn reconcile_backfills_session_names_per_socket() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        let mk_started = |sid: &str, socket: Option<&str>| AgentEvent::Started {
            id: AgentId {
                kind: AgentKind::ClaudeCode,
                session_id: sid.into(),
                surface: None,
                pane: Some("%1".into()),
                tmux_socket: socket.map(Into::into),
                cwd: None,
            },
            at: t0,
        };
        store
            .apply(&mk_started("on-amux", Some("/tmp/tmux-501/amux")))
            .await;
        store.apply(&mk_started("untagged", None)).await;

        let mut amux_pane = pane("%1");
        amux_pane.socket = Some("amux".into());
        amux_pane.session = "amux-spike".into();
        let mut default_pane = pane("%1");
        default_pane.socket = Some("default".into());
        default_pane.session = "main".into();
        store.reconcile(&[amux_pane, default_pane]).await;

        let on_amux = store.by_session("on-amux").await.expect("agent kept");
        assert_eq!(on_amux.tmux_socket.as_deref(), Some("amux"));
        assert_eq!(on_amux.tmux_session.as_deref(), Some("amux-spike"));
        let untagged = store.by_session("untagged").await.expect("agent kept");
        assert_eq!(untagged.tmux_session.as_deref(), Some("main"));
        assert_eq!(untagged.tmux_socket.as_deref(), Some("default"));
    }

    /// A socket-tagged agent whose server no longer has its pane is reaped
    /// even when another server has a same-numbered pane.
    #[tokio::test]
    async fn reconcile_reaps_socket_tagged_agent_when_its_server_lacks_the_pane() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "amux-only".into(),
                    surface: None,
                    pane: Some("%9".into()),
                    tmux_socket: Some("/tmp/tmux-501/amux".into()),
                    cwd: None,
                },
                at: t0,
            })
            .await;
        let mut default_pane = pane("%9");
        default_pane.socket = Some("default".into());
        let report = store.reconcile(&[default_pane]).await;
        assert_eq!(report.stale_panes_reaped, 1);
        assert!(store.by_session("amux-only").await.is_none());
    }

    #[tokio::test]
    async fn reconcile_reaps_agents_whose_pane_is_gone() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        for sid in ["a", "b", "c"] {
            store
                .apply(&AgentEvent::Started {
                    id: AgentId {
                        tmux_socket: None,
                        kind: AgentKind::ClaudeCode,
                        session_id: sid.into(),
                        surface: None,
                        pane: Some(format!("%{sid}")),
                        cwd: None,
                    },
                    at: t0,
                })
                .await;
        }
        // Only %a is still alive; %b and %c are gone.
        let live = vec![pane("%a")];
        let mut transitions = store.subscribe();

        let report = store.reconcile(&live).await;

        assert_eq!(report.stale_panes_reaped, 2);
        assert_eq!(report.synthetic_demoted, 0);
        assert_eq!(report.duplicates_collapsed, 0);
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].session_id, "a");
        let mut stopped = Vec::new();
        for _ in 0..2 {
            let transition = transitions.recv().await.expect("stale pane transition");
            assert_eq!(transition.to, AgentState::Stopped);
            assert_eq!(transition.agent.state, AgentState::Stopped);
            assert_eq!(
                transition.agent.state_entered_at,
                transition.agent.last_activity_at,
            );
            stopped.push(transition.agent.session_id.clone());
        }
        stopped.sort();
        assert_eq!(stopped, ["b", "c"]);
    }

    #[tokio::test]
    async fn reconcile_keeps_paneless_agents_untouched() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "no-pane".into(),
                    surface: None,
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
    #[allow(clippy::too_many_lines)] // scenario setup: several planted rows, one assertion pass
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
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "synthetic-%1".into(),
                    surface: None,
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
                    tmux_socket: None,
                    tmux_session: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "real-old".into(),
                    surface: None,
                    pid: None,
                    workload: crate::WorkloadSummary::default(),
                    subagents: Vec::new(),
                    pane: Some("%1".into()),
                    cwd: None,
                    state: AgentState::Stopped,
                    last_prompt: None,
                    last_response: None,
                    recap: None,
                    ai_title: None,
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
                    state_entered_at: mid,
                },
            );
            agents.insert(
                "real-new".into(),
                Agent {
                    tmux_socket: None,
                    tmux_session: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "real-new".into(),
                    surface: None,
                    pid: None,
                    workload: crate::WorkloadSummary::default(),
                    subagents: Vec::new(),
                    pane: Some("%1".into()),
                    cwd: None,
                    state: AgentState::Working,
                    last_prompt: None,
                    last_response: None,
                    recap: None,
                    ai_title: None,
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
                    state_entered_at: new,
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
                        tmux_socket: None,
                        tmux_session: None,
                        kind: AgentKind::ClaudeCode,
                        session_id: sid.into(),
                        surface: None,
                        pid: None,
                        workload: crate::WorkloadSummary::default(),
                        subagents: Vec::new(),
                        pane: Some("%1".into()),
                        cwd: None,
                        state: AgentState::Working,
                        last_prompt: None,
                        last_response: None,
                        recap: None,
                        ai_title: None,
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
                        state_entered_at: at,
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
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "live".into(),
                    surface: None,
                    pane: Some("%1".into()),
                    cwd: None,
                },
                at: t0,
            })
            .await;

        // Two candidates: "live" already exists (must skip),
        // "fresh" is new (must insert).
        let mk = |sid: &str, pane: &str, prompt: &str| Agent {
            tmux_socket: None,
            tmux_session: None,
            kind: AgentKind::ClaudeCode,
            session_id: sid.into(),
            surface: None,
            pid: None,
            workload: crate::WorkloadSummary::default(),
            subagents: Vec::new(),
            pane: Some(pane.into()),
            cwd: None,
            state: AgentState::Idle,
            last_prompt: Some(prompt.into()),
            last_response: None,
            recap: None,
            ai_title: None,
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
            state_entered_at: t0,
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
    async fn state_entered_at_updates_on_transition() {
        let store = Store::shared();
        let t0 = datetime!(2026-05-05 12:00:00 UTC);
        let t1 = datetime!(2026-05-05 12:05:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: id("t"),
                at: t0,
            })
            .await;
        assert_eq!(store.by_session("t").await.unwrap().state_entered_at, t0);
        store
            .apply(&AgentEvent::PromptSubmitted {
                id: id("t"),
                prompt: "go".into(),
                at: t1,
            })
            .await;
        let a = store.by_session("t").await.unwrap();
        assert_eq!(a.state, AgentState::Working);
        assert_eq!(
            a.state_entered_at, t1,
            "PromptSubmitted (Idle → Working) must reset state_entered_at"
        );
    }

    #[tokio::test]
    async fn heartbeat_does_not_update_state_entered_at() {
        let store = Store::shared();
        let t0 = datetime!(2026-05-05 12:00:00 UTC);
        let t1 = datetime!(2026-05-05 12:10:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: id("h"),
                at: t0,
            })
            .await;
        store
            .apply(&AgentEvent::Heartbeat {
                id: id("h"),
                model: Some("claude-opus-4-7".into()),
                context_used_pct: Some(12.5),
                cost_usd: None,
                rate_limit_5h_pct: None,
                rate_limit_5h_resets_at: None,
                rate_limit_7d_pct: None,
                rate_limit_7d_resets_at: None,
                at: t1,
            })
            .await;
        let a = store.by_session("h").await.unwrap();
        // Heartbeat carries metadata only, doesn't move state — the
        // stuck-duration clock and ACT column must keep ticking from t0.
        assert_eq!(a.state, AgentState::Idle);
        assert_eq!(a.state_entered_at, t0);
        assert_eq!(a.last_activity_at, t0);
        assert_eq!(a.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(a.context_used_pct, Some(12.5));
    }

    #[tokio::test]
    async fn same_state_event_preserves_state_entered_at() {
        // Notification(NeedsInput) → already WaitingInput → another
        // Notification(NeedsInput): the second one shouldn't reset the
        // clock, otherwise a chatty adapter would mask the real stuck
        // duration.
        let store = Store::shared();
        let t0 = datetime!(2026-05-05 12:00:00 UTC);
        let t1 = datetime!(2026-05-05 12:01:00 UTC);
        let t2 = datetime!(2026-05-05 12:07:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: id("s"),
                at: t0,
            })
            .await;
        store
            .apply(&AgentEvent::NotificationFired {
                id: id("s"),
                level: NotificationLevel::NeedsInput,
                message: "first".into(),
                at: t1,
            })
            .await;
        assert_eq!(store.by_session("s").await.unwrap().state_entered_at, t1);
        store
            .apply(&AgentEvent::NotificationFired {
                id: id("s"),
                level: NotificationLevel::NeedsInput,
                message: "second".into(),
                at: t2,
            })
            .await;
        let a = store.by_session("s").await.unwrap();
        assert_eq!(a.state, AgentState::WaitingInput);
        assert_eq!(
            a.state_entered_at, t1,
            "re-entering the same state must not reset state_entered_at"
        );
    }

    #[tokio::test]
    async fn reconcile_is_idempotent_on_clean_state() {
        let store = Store::shared();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "lone".into(),
                    surface: None,
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
