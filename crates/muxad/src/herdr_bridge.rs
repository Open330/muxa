//! herdr event bridge (Phase 2).
//!
//! When the active pane backend is herdr, this task holds a long-lived
//! connection to the herdr socket, subscribes to `pane.agent_status_changed`,
//! and translates each status change into synthetic muxa [`AgentEvent`]s. That
//! gives `muxa status` / `watch` / stats visibility into *every* agent herdr
//! detects — cursor, amp, copilot, and the rest — even the ones muxa has no
//! hooks for.
//!
//! ## Row identity and precedence
//!
//! Bridge rows reuse muxa's existing SYNTHETIC-row convention: the pane id is
//! namespaced `herdr:<pane_id>` (via [`PANE_ID_PREFIX`]) and the session id is
//! `synthetic-herdr:<pane_id>` — exactly the shape `discovery` mints for a
//! herdr pane (whose `socket` is always `None`). Two consequences fall out for
//! free:
//!
//! * When a real hook row later claims the same pane, `Store::apply`'s
//!   synthetic-eviction pass drops the bridge row — the hook is authoritative.
//! * Before applying any bridge event we query [`Store::by_pane`]: if a
//!   *non-synthetic* row already owns the pane, the update is dropped. A hooked
//!   agent owns its pane; herdr's screen-detected view must not clobber it.
//!
//! ## Wire
//!
//! newline-delimited JSON over the same socket path the query backend resolves
//! ([`default_socket_path`]). herdr's `pane.agent_status_changed` subscription
//! is *per pane* (each subscription item carries a `pane_id`), so on every
//! connect the bridge:
//!
//! 1. enumerates panes with `pane.list` (which reports each pane's *current*
//!    `agent` / `agent_status`),
//! 2. subscribes to all of them in one `events.subscribe` call,
//! 3. seeds current state from step 1 (subscribing does **not** replay the
//!    present status — only future changes stream), then
//! 4. streams deltas, re-listing on a timer and reconnecting whenever the pane
//!    set changes so newly-created panes get covered.
//!
//! A dropped connection is retried with capped exponential backoff.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use muxa::backend::herdr::{default_socket_path, PANE_ID_PREFIX};
use muxa::event::{AgentEvent, AgentId, AgentKind, NotificationLevel};
use muxa::state::{Agent, SYNTHETIC_SESSION_PREFIX};
use muxa::{AgentState, HostKind, SharedBackend, SharedStore};
use serde_json::{json, Value};
use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::sync::broadcast;

/// The one herdr subscription kind the bridge cares about. The other
/// subscribable kinds (`pane.output_matched`, `pane.scroll_changed`) carry no
/// agent-state signal.
const AGENT_STATUS_EVENT: &str = "pane.agent_status_changed";

/// First reconnect delay after a failed connect/subscribe. Kept short so a
/// herdr server that comes up a moment after muxad is picked up quickly.
const INITIAL_BACKOFF: Duration = Duration::from_millis(500);

/// Ceiling on the reconnect delay. A herdr server that stays down settles into
/// one probe every 30 s rather than busy-looping.
const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// How often, while streaming, to re-`pane.list` and check whether the pane
/// set changed. A change triggers a reconnect so new panes get subscribed.
const RESCAN_INTERVAL: Duration = Duration::from_secs(10);

/// Bound on a one-shot `pane.list` round-trip, mirroring the query backend's
/// per-call timeout so a wedged server can't stall the bridge's rescan.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Why a herdr event did not produce a muxa update. Returned by [`translate`]
/// so the caller can log at the right level (unknown status is routine noise;
/// a malformed envelope is worth a debug line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropReason {
    /// The envelope was not a `pane.agent_status_changed` event.
    NotAgentStatusEvent,
    /// The event carried no usable `pane_id`.
    MissingPaneId,
    /// The event named no agent (`agent` and `display_agent` both absent) — a
    /// plain shell pane, not something muxa should synthesize a row for.
    NoAgent,
    /// `agent_status` was `unknown` (or an unrecognized value) — herdr can't
    /// classify the pane, so muxa shouldn't invent a state for it.
    StatusUnknown,
    /// The envelope was missing the `data` object or the `agent_status` field.
    Malformed,
}

/// A translated herdr status change: the muxa-namespaced pane it targets plus
/// the ordered events to apply once the pane is confirmed hook-free.
///
/// [`AgentEvent`] is not `PartialEq`, so neither is this — tests inspect the
/// `events` vec by pattern-matching rather than comparing whole values.
#[derive(Debug, Clone)]
pub struct BridgeUpdate {
    /// muxa-namespaced pane id (`herdr:<pane_id>`). The caller runs the
    /// hook-authoritative [`Store::by_pane`] check against this.
    pub pane_id: String,
    /// Events to apply in order. The status-bearing event comes first (so a
    /// fresh row transitions straight into its real state), followed by an
    /// [`AgentEvent::Heartbeat`] that stamps the herdr agent name into the
    /// row's `model` metadata.
    pub events: Vec<AgentEvent>,
}

/// Map a herdr agent name to a muxa [`AgentKind`].
///
/// herdr's canonical agent slugs (from `server.agent_manifests`) aren't pinned
/// in the schema dump, so matching is deliberately loose and case-insensitive:
/// substring hits on the well-known agents muxa has a kind for, everything else
/// (cursor, amp, copilot, …) falls through to [`AgentKind::Unknown`] and rides
/// on the herdr name carried in `model`.
fn classify_agent(agent: &str) -> AgentKind {
    let agent = agent.to_ascii_lowercase();
    if agent.contains("claude") {
        AgentKind::ClaudeCode
    } else if agent.contains("codex") {
        AgentKind::Codex
    } else if agent.contains("gemini") {
        AgentKind::GeminiCli
    } else if agent.contains("opencode") {
        AgentKind::Opencode
    } else {
        AgentKind::Unknown
    }
}

/// A short, human-facing label for the `working` [`AgentEvent::ToolStarted`].
/// The tool string isn't persisted on the row (no `current_tool` field), so it
/// only ever surfaces in the activity ledger — the herdr agent name is the
/// most informative thing to record there.
fn tool_label(name: &str) -> String {
    name.to_owned()
}

/// The `last_notification` text for a `blocked` row. Prefers the herdr pane
/// title (often "waiting for approval" style copy), else a generic line naming
/// the agent.
fn blocked_message(data: &Value, name: &str) -> String {
    data.get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map_or_else(|| format!("{name} is waiting"), ToOwned::to_owned)
}

/// Translate one `pane.agent_status_changed` envelope into muxa events. Pure:
/// no I/O, no clock read (the caller supplies `at`), so the full mapping table
/// is unit-testable.
///
/// Status mapping:
///
/// | herdr `agent_status` | muxa event          | resulting state |
/// |----------------------|---------------------|-----------------|
/// | `working`            | `ToolStarted`       | `Working`       |
/// | `blocked`            | `NotificationFired` | `WaitingInput`  |
/// | `idle` / `done`      | `TurnStopped`       | `Idle`          |
/// | `unknown`            | — (dropped)         | —               |
pub fn translate(envelope: &Value, at: OffsetDateTime) -> Result<BridgeUpdate, DropReason> {
    if envelope.get("event").and_then(Value::as_str) != Some(AGENT_STATUS_EVENT) {
        return Err(DropReason::NotAgentStatusEvent);
    }
    let data = envelope.get("data").ok_or(DropReason::Malformed)?;

    let raw_pane = data
        .get("pane_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or(DropReason::MissingPaneId)?;
    let status = data
        .get("agent_status")
        .and_then(Value::as_str)
        .ok_or(DropReason::Malformed)?;

    // `agent` is herdr's canonical slug; `display_agent` is its pretty label.
    // The name (for the visible `model` metadata) prefers the pretty label;
    // the kind is classified from whichever is present. A pane with neither is
    // a plain shell — nothing to synthesize.
    let canonical = data
        .get("agent")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let display = data
        .get("display_agent")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let name = display.or(canonical).ok_or(DropReason::NoAgent)?;
    let kind = classify_agent(canonical.unwrap_or(name));

    let pane_id = format!("{PANE_ID_PREFIX}{raw_pane}");
    // herdr panes carry no tmux socket, so the synthetic session id is the
    // plain `synthetic-<pane_id>` shape `discovery::synthetic_session_id`
    // produces for a socket-less pane — the two coincide by design so a
    // discovery placeholder and a bridge row collapse onto one registry key.
    let session_id = format!("{SYNTHETIC_SESSION_PREFIX}{pane_id}");
    let id = AgentId {
        kind,
        session_id,
        surface: None,
        pane: Some(pane_id.clone()),
        tmux_socket: None,
        cwd: None,
    };

    let status_event = match status {
        "working" => AgentEvent::ToolStarted {
            id: id.clone(),
            tool: tool_label(name),
            subagent: None,
            at,
        },
        "blocked" => AgentEvent::NotificationFired {
            id: id.clone(),
            level: NotificationLevel::NeedsInput,
            message: blocked_message(data, name),
            at,
        },
        "idle" | "done" => AgentEvent::TurnStopped {
            id: id.clone(),
            response: None,
            at,
        },
        "unknown" => return Err(DropReason::StatusUnknown),
        _ => return Err(DropReason::Malformed),
    };

    // Carry the herdr agent name into `model` so an Unknown-kind row (cursor,
    // amp, …) still tells the operator *which* agent it is. Harmless for a row
    // that later gets real hooks — the bridge row is evicted first.
    let heartbeat = AgentEvent::Heartbeat {
        id,
        model: Some(name.to_owned()),
        context_used_pct: None,
        cost_usd: None,
        rate_limit_5h_pct: None,
        rate_limit_5h_resets_at: None,
        rate_limit_7d_pct: None,
        rate_limit_7d_resets_at: None,
        at,
    };

    Ok(BridgeUpdate {
        pane_id,
        events: vec![status_event, heartbeat],
    })
}

/// Apply a translated update, enforcing the hook-authoritative rule: if a
/// non-synthetic (real hook) row already owns the pane, drop the bridge update
/// wholesale.
async fn apply_update(store: &SharedStore, update: BridgeUpdate) {
    let occupants = store.by_pane(&update.pane_id).await;
    if occupants
        .iter()
        .any(|a| !a.session_id.starts_with(SYNTHETIC_SESSION_PREFIX))
    {
        tracing::debug!(
            pane = %update.pane_id,
            "herdr bridge: pane owned by a hooked agent, dropping bridge update",
        );
        return;
    }
    for ev in &update.events {
        store.apply(ev).await;
    }
}

/// Translate a wire/seed envelope and, when it yields an update, apply it.
async fn ingest_envelope(store: &SharedStore, envelope: &Value) {
    match translate(envelope, OffsetDateTime::now_utc()) {
        Ok(update) => apply_update(store, update).await,
        // Unknown status / plain shells are routine — keep them off the debug
        // stream so the log isn't swamped on a busy session.
        Err(DropReason::StatusUnknown | DropReason::NoAgent) => {}
        Err(reason) => tracing::debug!(?reason, "herdr bridge: dropped event"),
    }
}

/// One herdr pane's current agent state, as read from `pane.list`.
struct HerdrPane {
    pane_id: String,
    agent: Option<String>,
    display_agent: Option<String>,
    agent_status: String,
}

impl HerdrPane {
    /// Rebuild the `pane.agent_status_changed` envelope shape so seeding reuses
    /// the exact same [`translate`] path the live wire events take.
    fn as_envelope(&self) -> Value {
        json!({
            "event": AGENT_STATUS_EVENT,
            "data": {
                "pane_id": self.pane_id,
                "agent": self.agent,
                "display_agent": self.display_agent,
                "agent_status": self.agent_status,
            },
        })
    }
}

/// One request/response round-trip on a fresh connection, bounded by
/// [`REQUEST_TIMEOUT`]. Returns the `result` value on success. Used for the
/// bridge's `pane.list` enumeration; the persistent stream stays dedicated to
/// events.
async fn herdr_request(socket_path: &Path, method: &str, params: Value) -> Option<Value> {
    let stream = UnixStream::connect(socket_path).await.ok()?;
    let (read_half, mut write_half) = stream.into_split();
    let id = "muxa-herdr-bridge-req";
    let mut payload = json!({ "id": id, "method": method, "params": params }).to_string();
    payload.push('\n');
    write_half.write_all(payload.as_bytes()).await.ok()?;

    let mut lines = BufReader::new(read_half).lines();
    tokio::time::timeout(REQUEST_TIMEOUT, async {
        while let Ok(Some(line)) = lines.next_line().await {
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value.get("id").and_then(Value::as_str) == Some(id) {
                return value.get("result").cloned();
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

/// Enumerate herdr panes with their current agent state.
async fn list_panes(socket_path: &Path) -> Option<Vec<HerdrPane>> {
    let result = herdr_request(socket_path, "pane.list", json!({})).await?;
    let panes = result.get("panes")?.as_array()?;
    Some(
        panes
            .iter()
            .filter_map(|p| {
                Some(HerdrPane {
                    pane_id: p.get("pane_id").and_then(Value::as_str)?.to_owned(),
                    agent: p
                        .get("agent")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    display_agent: p
                        .get("display_agent")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned),
                    agent_status: p.get("agent_status").and_then(Value::as_str)?.to_owned(),
                })
            })
            .collect(),
    )
}

/// The `events.subscribe` request line (newline-terminated) for the given raw
/// herdr pane ids, requesting only `pane.agent_status_changed` on each.
fn subscribe_request(pane_ids: &[String]) -> String {
    let subscriptions: Vec<Value> = pane_ids
        .iter()
        .map(|id| json!({ "type": AGENT_STATUS_EVENT, "pane_id": id }))
        .collect();
    let mut line = json!({
        "id": "muxa-herdr-bridge",
        "method": "events.subscribe",
        "params": { "subscriptions": subscriptions },
    })
    .to_string();
    line.push('\n');
    line
}

/// Outcome of one connect-and-stream attempt, so the reconnect loop can pick
/// the right backoff.
enum ConnOutcome {
    /// Shutdown was signalled — the task should return.
    Shutdown,
    /// Never established a subscription (socket absent, no panes, connect/
    /// write/list failed). Escalate the backoff.
    NotConnected(&'static str),
    /// Was subscribed, then the stream ended or the pane set changed. Retry
    /// promptly.
    StreamEnded(&'static str),
}

/// Connect, subscribe to every current pane, seed their state, and stream
/// deltas until shutdown, disconnect, or a pane-set change.
async fn connect_and_stream(
    store: &SharedStore,
    socket_path: &Path,
    shutdown_rx: &mut broadcast::Receiver<()>,
) -> ConnOutcome {
    if !socket_path.exists() {
        return ConnOutcome::NotConnected("socket file missing");
    }
    let Some(panes) = list_panes(socket_path).await else {
        return ConnOutcome::NotConnected("pane.list failed");
    };
    if panes.is_empty() {
        // Nothing to watch yet; back off and re-list rather than holding an
        // empty subscription open.
        return ConnOutcome::NotConnected("no panes");
    }
    let pane_ids: Vec<String> = panes.iter().map(|p| p.pane_id.clone()).collect();

    let stream = match UnixStream::connect(socket_path).await {
        Ok(stream) => stream,
        Err(e) => {
            tracing::debug!(error = %e, "herdr bridge: connect failed");
            return ConnOutcome::NotConnected("connect failed");
        }
    };
    let (read_half, mut write_half) = stream.into_split();
    if let Err(e) = write_half
        .write_all(subscribe_request(&pane_ids).as_bytes())
        .await
    {
        tracing::debug!(error = %e, "herdr bridge: subscribe write failed");
        return ConnOutcome::NotConnected("subscribe write failed");
    }
    tracing::info!(
        panes = pane_ids.len(),
        "herdr event bridge subscribed to {AGENT_STATUS_EVENT}",
    );

    // Seed current state — subscribing streams only *future* changes.
    for pane in &panes {
        ingest_envelope(store, &pane.as_envelope()).await;
    }

    let known: HashSet<String> = pane_ids.into_iter().collect();
    let mut lines = BufReader::new(read_half).lines();
    let mut rescan = tokio::time::interval(RESCAN_INTERVAL);
    rescan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    rescan.tick().await; // consume the immediate first tick
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => return ConnOutcome::Shutdown,
            line = lines.next_line() => match line {
                Ok(Some(line)) => {
                    if let Ok(value) = serde_json::from_str::<Value>(&line) {
                        // Events carry `event`; the subscribe ack carries `id`.
                        if value.get("event").is_some() {
                            ingest_envelope(store, &value).await;
                        }
                    }
                }
                Ok(None) => return ConnOutcome::StreamEnded("connection closed"),
                Err(e) => {
                    tracing::debug!(error = %e, "herdr bridge: read error");
                    return ConnOutcome::StreamEnded("read error");
                }
            },
            _ = rescan.tick() => {
                if let Some(current) = list_panes(socket_path).await {
                    let current: HashSet<String> =
                        current.into_iter().map(|p| p.pane_id).collect();
                    if current != known {
                        return ConnOutcome::StreamEnded("pane set changed");
                    }
                }
            }
        }
    }
}

/// Reconnect loop. Owns the backoff; races every wait against shutdown so a
/// SIGTERM never leaves the process lingering on a sleep.
async fn run(store: SharedStore, socket_path: PathBuf, shutdown_tx: broadcast::Sender<()>) {
    let mut backoff = INITIAL_BACKOFF;
    loop {
        let mut shutdown_rx = shutdown_tx.subscribe();
        match connect_and_stream(&store, &socket_path, &mut shutdown_rx).await {
            ConnOutcome::Shutdown => {
                tracing::debug!("herdr bridge shutting down");
                return;
            }
            ConnOutcome::StreamEnded(reason) => {
                // A healthy connection that ended — reset so we reconnect
                // promptly rather than inheriting a stale long backoff.
                backoff = INITIAL_BACKOFF;
                tracing::debug!(reason, "herdr bridge: stream ended; reconnecting");
            }
            ConnOutcome::NotConnected(reason) => {
                tracing::debug!(
                    reason,
                    backoff_secs = backoff.as_secs_f64(),
                    "herdr bridge: not connected; backing off",
                );
            }
        }

        let mut shutdown_rx = shutdown_tx.subscribe();
        tokio::select! {
            _ = shutdown_rx.recv() => return,
            () = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// Spawn the herdr bridge task, but only when the active backend is herdr.
/// Returns the join handle so the daemon can drain it on shutdown; `None` on
/// every other host.
pub fn spawn_herdr_bridge_task(
    backend: &SharedBackend,
    store: SharedStore,
    shutdown_tx: &broadcast::Sender<()>,
) -> Option<tokio::task::JoinHandle<()>> {
    if backend.kind() != HostKind::Herdr {
        return None;
    }
    let socket_path = default_socket_path();
    let shutdown_tx = shutdown_tx.clone();
    tracing::info!(
        socket = %socket_path.display(),
        "herdr event bridge enabled",
    );
    Some(tokio::spawn(run(store, socket_path, shutdown_tx)))
}

// ============================================================================
// Reverse path: pane.report_agent — push muxa's hook-derived state INTO herdr.
// ============================================================================
//
// ## What and why
//
// The forward bridge above turns herdr's *own* screen detection into synthetic
// muxa rows. This reverse path does the opposite for the rows muxa is
// authoritative about: whenever a REAL (hook-driven, non-synthetic) agent on a
// `herdr:` pane changes state, muxa reports that state to herdr via
// `pane.report_agent` (source `"muxa"`). herdr treats an installed
// integration's report as authoritative over its own screen detection, so its
// sidebar/UI shows muxa's exact hook-derived state instead of guessing from the
// pane's scrollback — and stops its screen-detection state flapping for that
// pane while muxa owns it.
//
// ## No feedback loop (the load-bearing invariant)
//
// We report ONLY non-synthetic rows. Synthetic rows (`synthetic-…` session ids)
// are the ones the forward bridge MINTS from herdr's detection — echoing them
// back would form a loop: report → herdr adopts muxa as authority → herdr stops
// emitting `pane.agent_status_changed` → the forward bridge sees no more deltas
// → muxa's synthetic row goes stale. The two directions stay disjoint by pane
// ownership:
//
//   * A `herdr:` pane with a real hook row → reported here; the forward bridge's
//     `apply_update` already DROPS herdr's screen-detection updates for that
//     same pane (hook-authoritative), so herdr's flapping is silenced from both
//     ends and muxa is the single source of truth.
//   * A `herdr:` pane with only a synthetic bridge row → never reported; herdr
//     keeps its own detection and the forward bridge keeps mirroring it.
//
// ## Release
//
// When a reported row transitions to `Stopped` we send `pane.release_agent`,
// handing authority back to herdr's own detection. `Stopped` is muxa's terminal
// state — reached on a `SessionEnded` hook and on reconciler/GC reaping (the
// reaper flips the row to `Stopped`, which emits a `Transition`). NOTE: a row
// that is GC-evicted from the registry *after* it already went `Stopped` emits
// no further transition, and there is no distinct "row removed" transition on
// the stream to observe. Releasing on the `…→Stopped` edge is therefore both
// necessary and sufficient — that single release is the last thing herdr hears
// from muxa for the pane, after which its own detection resumes.

/// The `source` every reverse-path call carries, so herdr attributes the
/// authority to this integration (and its `pane.release_agent` matches).
const REPORT_SOURCE: &str = "muxa";

/// Process-global monotonic sequence for `pane.report_agent` /
/// `pane.release_agent`. herdr uses `seq` to discard out-of-order reports, so a
/// single ever-increasing counter across every pane keeps a late-delivered
/// report from resurrecting stale state.
static REPORT_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_seq() -> u64 {
    REPORT_SEQ.fetch_add(1, Ordering::Relaxed)
}

/// herdr's canonical agent slug for a muxa [`AgentKind`]. Chosen to line up
/// with herdr's own agent names (its `IntegrationTarget` uses `claude`,
/// `codex`, …) and with the forward bridge's [`classify_agent`], so a pane muxa
/// reports and a pane herdr detects carry the same agent label.
fn agent_slug(kind: AgentKind) -> &'static str {
    match kind {
        AgentKind::ClaudeCode => "claude",
        AgentKind::Codex => "codex",
        AgentKind::GeminiCli => "gemini",
        AgentKind::Opencode => "opencode",
        AgentKind::Task => "task",
        AgentKind::Unknown => "unknown",
    }
}

/// A wire-ready decision for one muxa [`Transition`]: report a state to herdr,
/// or release muxa's authority so herdr's own detection resumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HerdrReport {
    /// `pane.report_agent` — muxa is authoritative for this pane's state.
    Report {
        /// Bare herdr pane id (the `herdr:` prefix stripped for the wire).
        pane_id: String,
        /// herdr agent slug (see [`agent_slug`]).
        agent: String,
        /// One of herdr's `PaneAgentState`: `idle` | `working` | `blocked`.
        state: &'static str,
        /// Optional human message — blocked/error rows carry the notification.
        message: Option<String>,
        /// muxa's real session id, forwarded as herdr's `agent_session_id`.
        agent_session_id: String,
    },
    /// `pane.release_agent` — hand authority back to herdr.
    Release {
        /// Bare herdr pane id (the `herdr:` prefix stripped for the wire).
        pane_id: String,
        /// herdr agent slug (see [`agent_slug`]).
        agent: String,
    },
}

impl HerdrReport {
    /// The herdr JSON-RPC method and params for this decision, stamped with
    /// `seq`. `source` is always [`REPORT_SOURCE`].
    fn request(&self, seq: u64) -> (&'static str, Value) {
        match self {
            HerdrReport::Report {
                pane_id,
                agent,
                state,
                message,
                agent_session_id,
            } => (
                "pane.report_agent",
                json!({
                    "pane_id": pane_id,
                    "source": REPORT_SOURCE,
                    "agent": agent,
                    "state": state,
                    "message": message,
                    "agent_session_id": agent_session_id,
                    "seq": seq,
                }),
            ),
            HerdrReport::Release { pane_id, agent } => (
                "pane.release_agent",
                json!({
                    "pane_id": pane_id,
                    "source": REPORT_SOURCE,
                    "agent": agent,
                    "seq": seq,
                }),
            ),
        }
    }
}

/// Decide what (if anything) to report to herdr for a post-transition agent
/// snapshot. Pure and total, so the full mapping is unit-testable.
///
/// Returns `None` — report nothing — unless the row is BOTH on a `herdr:` pane
/// AND non-synthetic (a real hook row). See the module-level no-loop note.
///
/// State mapping:
///
/// | muxa `AgentState`              | herdr call / state      |
/// |--------------------------------|-------------------------|
/// | `Working`                      | report `working`        |
/// | `WaitingInput`/`WaitingChoice` | report `blocked` (+msg) |
/// | `Error`                        | report `blocked` (+msg) |
/// | `Idle`                         | report `idle`           |
/// | `Stopped`                      | release                 |
/// | `Starting`                     | — (nothing; transient)  |
pub fn report_decision(agent: &Agent) -> Option<HerdrReport> {
    let pane = agent.pane.as_deref()?;
    // Only herdr-namespaced panes; the bare id is what crosses the wire.
    let bare = pane.strip_prefix(PANE_ID_PREFIX)?;
    // NEVER report synthetic rows — those are minted FROM herdr's own detection
    // and echoing them back would loop (see the module-level no-loop note).
    if agent.session_id.starts_with(SYNTHETIC_SESSION_PREFIX) {
        return None;
    }
    let pane_id = bare.to_owned();
    let agent_name = agent_slug(agent.kind).to_owned();

    let (state, message): (&'static str, Option<String>) = match agent.state {
        AgentState::Working => ("working", None),
        // Both "needs input" and "needs a choice" are `blocked` to herdr; carry
        // the notification text so the sidebar shows *what* it's blocked on.
        AgentState::WaitingInput | AgentState::WaitingChoice => {
            ("blocked", agent.last_notification.clone())
        }
        // Error has no herdr equivalent; surface it as `blocked` with the error
        // message (NotificationFired at Error level populates last_notification).
        AgentState::Error => ("blocked", agent.last_notification.clone()),
        AgentState::Idle => ("idle", None),
        AgentState::Stopped => {
            return Some(HerdrReport::Release {
                pane_id,
                agent: agent_name,
            });
        }
        // A freshly-`Started` row is `Starting` only briefly before its first
        // prompt/tool/turn event moves it on; reporting a transient state herdr
        // has no equivalent for adds churn without value.
        AgentState::Starting => return None,
    };
    Some(HerdrReport::Report {
        pane_id,
        agent: agent_name,
        state,
        message,
        agent_session_id: agent.session_id.clone(),
    })
}

/// Fire one reverse-path call at herdr. Best-effort: a failure just means herdr
/// keeps its own detection for a beat. Bounded by [`REQUEST_TIMEOUT`] inside
/// [`herdr_request`] so a wedged server can't stall the transition stream.
async fn send_report(socket_path: &Path, report: &HerdrReport, seq: u64) {
    let (method, params) = report.request(seq);
    if herdr_request(socket_path, method, params).await.is_none() {
        // No `result` came back (transport error, timeout, or an error
        // response) — non-fatal, just note it and move on.
        tracing::debug!(method, seq, "herdr report: no result (dropped)");
    }
}

/// Subscribe to the store's transition stream and push every REAL row's state
/// change on a `herdr:` pane into herdr. Runs until shutdown or the store's
/// broadcast channel closes.
async fn run_report(store: SharedStore, socket_path: PathBuf, shutdown_tx: broadcast::Sender<()>) {
    let mut rx = store.subscribe();
    let mut shutdown_rx = shutdown_tx.subscribe();
    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                tracing::debug!("herdr report path shutting down");
                return;
            }
            msg = rx.recv() => match msg {
                Ok(transition) => {
                    if let Some(report) = report_decision(transition.agent.as_ref()) {
                        send_report(&socket_path, &report, next_seq()).await;
                    }
                }
                // A slow send while herdr was wedged let transitions pile up.
                // Reporting is best-effort and each transition carries the full
                // post-state, so skipping the gap is harmless — the next
                // transition re-reports current truth.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(dropped = n, "herdr report path lagged behind transitions");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

/// Spawn the herdr reverse-path (report) task, but only when the active backend
/// is herdr. Returns the join handle so the daemon can drain it on shutdown;
/// `None` on every other host.
pub fn spawn_herdr_report_task(
    backend: &SharedBackend,
    store: SharedStore,
    shutdown_tx: &broadcast::Sender<()>,
) -> Option<tokio::task::JoinHandle<()>> {
    if backend.kind() != HostKind::Herdr {
        return None;
    }
    let socket_path = default_socket_path();
    let shutdown_tx = shutdown_tx.clone();
    tracing::info!(
        socket = %socket_path.display(),
        "herdr report path enabled (pushing hook-derived state via pane.report_agent)",
    );
    Some(tokio::spawn(run_report(store, socket_path, shutdown_tx)))
}

#[cfg(test)]
mod tests {
    use muxa::event::AgentEvent;
    use muxa::state::Store;
    use muxa::AgentState;
    use serde_json::json;
    use time::macros::datetime;

    use super::*;

    const AT: OffsetDateTime = datetime!(2026-07-20 12:00:00 UTC);

    fn status_event(agent: &str, display: &str, status: &str, pane: &str) -> Value {
        json!({
            "event": "pane.agent_status_changed",
            "data": {
                "pane_id": pane,
                "workspace_id": "w1",
                "agent": agent,
                "display_agent": display,
                "agent_status": status,
                "state_labels": {},
                "title": null,
            },
        })
    }

    fn drop_reason(ev: &Value) -> DropReason {
        match translate(ev, AT) {
            Err(reason) => reason,
            Ok(_) => panic!("expected a drop, got an update"),
        }
    }

    #[test]
    fn working_maps_to_tool_started_plus_heartbeat() {
        let ev = status_event("cursor", "Cursor", "working", "w1:p1");
        let out = translate(&ev, AT).expect("working translates");
        assert_eq!(out.pane_id, "herdr:w1:p1");
        assert_eq!(out.events.len(), 2, "status event + name heartbeat");
        match &out.events[0] {
            AgentEvent::ToolStarted { id, tool, .. } => {
                assert_eq!(id.session_id, "synthetic-herdr:w1:p1");
                assert_eq!(id.pane.as_deref(), Some("herdr:w1:p1"));
                assert_eq!(id.kind, AgentKind::Unknown, "cursor has no muxa kind");
                assert_eq!(tool, "Cursor");
            }
            other => panic!("expected ToolStarted, got {other:?}"),
        }
        match &out.events[1] {
            AgentEvent::Heartbeat { model, .. } => {
                assert_eq!(model.as_deref(), Some("Cursor"), "name carried in model");
            }
            other => panic!("expected Heartbeat, got {other:?}"),
        }
    }

    #[test]
    fn blocked_maps_to_needs_input_notification() {
        let ev = status_event("amp", "Amp", "blocked", "w1:p2");
        let out = translate(&ev, AT).expect("blocked translates");
        match &out.events[0] {
            AgentEvent::NotificationFired { level, message, .. } => {
                assert_eq!(*level, NotificationLevel::NeedsInput);
                assert_eq!(message, "Amp is waiting");
            }
            other => panic!("expected NotificationFired, got {other:?}"),
        }
    }

    #[test]
    fn blocked_prefers_title_for_message() {
        let mut ev = status_event("amp", "Amp", "blocked", "w1:p2");
        ev["data"]["title"] = json!("Approve running `rm`?");
        let out = translate(&ev, AT).unwrap();
        match &out.events[0] {
            AgentEvent::NotificationFired { message, .. } => {
                assert_eq!(message, "Approve running `rm`?");
            }
            other => panic!("expected NotificationFired, got {other:?}"),
        }
    }

    #[test]
    fn idle_and_done_map_to_turn_stopped() {
        for status in ["idle", "done"] {
            let ev = status_event("cursor", "Cursor", status, "w1:p1");
            let out = translate(&ev, AT).unwrap();
            assert!(
                matches!(out.events[0], AgentEvent::TurnStopped { .. }),
                "{status} should stop the turn",
            );
        }
    }

    #[test]
    fn unknown_status_is_dropped() {
        let ev = status_event("cursor", "Cursor", "unknown", "w1:p1");
        assert_eq!(drop_reason(&ev), DropReason::StatusUnknown);
    }

    #[test]
    fn non_status_event_is_dropped() {
        let ev = json!({ "event": "pane.scroll_changed", "data": {} });
        assert_eq!(drop_reason(&ev), DropReason::NotAgentStatusEvent);
    }

    #[test]
    fn missing_pane_id_is_dropped() {
        let ev = json!({
            "event": "pane.agent_status_changed",
            "data": { "workspace_id": "w1", "agent": "cursor", "agent_status": "working" },
        });
        assert_eq!(drop_reason(&ev), DropReason::MissingPaneId);
    }

    #[test]
    fn shell_pane_without_agent_is_dropped() {
        let ev = json!({
            "event": "pane.agent_status_changed",
            "data": { "pane_id": "w1:p1", "workspace_id": "w1", "agent_status": "idle" },
        });
        assert_eq!(drop_reason(&ev), DropReason::NoAgent);
    }

    #[test]
    fn missing_agent_status_is_malformed() {
        let ev = json!({
            "event": "pane.agent_status_changed",
            "data": { "pane_id": "w1:p1", "workspace_id": "w1", "agent": "cursor" },
        });
        assert_eq!(drop_reason(&ev), DropReason::Malformed);
    }

    #[test]
    fn known_agents_map_to_their_kinds() {
        let cases = [
            ("claude", AgentKind::ClaudeCode),
            ("codex", AgentKind::Codex),
            ("gemini", AgentKind::GeminiCli),
            ("opencode", AgentKind::Opencode),
            ("cursor", AgentKind::Unknown),
            ("copilot", AgentKind::Unknown),
        ];
        for (agent, expected) in cases {
            let ev = status_event(agent, agent, "working", "w1:p1");
            let out = translate(&ev, AT).unwrap();
            assert_eq!(out.events[0].id().kind, expected, "agent {agent}");
        }
    }

    #[test]
    fn name_falls_back_to_agent_when_display_absent() {
        let mut ev = status_event("cursor", "Cursor", "working", "w1:p1");
        ev["data"]["display_agent"] = json!(null);
        let out = translate(&ev, AT).unwrap();
        match &out.events[0] {
            AgentEvent::ToolStarted { tool, .. } => assert_eq!(tool, "cursor"),
            other => panic!("expected ToolStarted, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn bridge_row_appears_then_transitions() {
        let store = Store::shared();
        let working = translate(&status_event("cursor", "Cursor", "working", "w1:p1"), AT).unwrap();
        apply_update(&store, working).await;

        let rows = store.by_pane("herdr:w1:p1").await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, AgentState::Working);
        assert_eq!(rows[0].kind, AgentKind::Unknown);
        assert_eq!(rows[0].model.as_deref(), Some("Cursor"));

        let blocked = translate(&status_event("cursor", "Cursor", "blocked", "w1:p1"), AT).unwrap();
        apply_update(&store, blocked).await;
        let rows = store.by_pane("herdr:w1:p1").await;
        assert_eq!(rows.len(), 1, "same synthetic row, not a duplicate");
        assert_eq!(rows[0].state, AgentState::WaitingInput);
    }

    #[tokio::test]
    async fn hook_owned_pane_drops_bridge_update() {
        let store = Store::shared();
        // A real hook row (non-synthetic) claims the pane first.
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "real-session".into(),
                    surface: None,
                    pane: Some("herdr:w1:p1".into()),
                    tmux_socket: None,
                    cwd: None,
                },
                at: AT,
            })
            .await;

        // A bridge update for the same pane must be dropped wholesale.
        let working = translate(&status_event("cursor", "Cursor", "working", "w1:p1"), AT).unwrap();
        apply_update(&store, working).await;

        let rows = store.by_pane("herdr:w1:p1").await;
        assert_eq!(rows.len(), 1, "only the real row remains");
        assert_eq!(rows[0].session_id, "real-session");
        assert_eq!(rows[0].kind, AgentKind::ClaudeCode);
        assert_eq!(rows[0].state, AgentState::Idle, "unchanged by the bridge");
    }

    // --- Reverse path: report_decision --------------------------------------

    /// Build an `Agent` for the reverse-path mapping tests. Only the fields
    /// `report_decision` reads (`kind`, `session_id`, `pane`, `state`,
    /// `last_notification`) carry meaning; the rest are inert.
    fn agent(kind: AgentKind, session_id: &str, pane: &str, state: AgentState) -> Agent {
        Agent {
            kind,
            session_id: session_id.to_owned(),
            surface: None,
            pane: Some(pane.to_owned()),
            tmux_socket: None,
            tmux_session: None,
            cwd: None,
            pid: None,
            workload: muxa::WorkloadSummary::default(),
            subagents: Vec::new(),
            state,
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
            started_at: AT,
            last_activity_at: AT,
            state_entered_at: AT,
        }
    }

    #[test]
    fn working_row_reports_working_with_stripped_pane() {
        let a = agent(
            AgentKind::ClaudeCode,
            "sess-1",
            "herdr:w1:p1",
            AgentState::Working,
        );
        match report_decision(&a).expect("herdr working row reports") {
            HerdrReport::Report {
                pane_id,
                agent,
                state,
                message,
                agent_session_id,
            } => {
                assert_eq!(pane_id, "w1:p1", "herdr: prefix stripped for the wire");
                assert_eq!(agent, "claude", "ClaudeCode -> herdr slug");
                assert_eq!(state, "working");
                assert_eq!(message, None);
                assert_eq!(agent_session_id, "sess-1");
            }
            report @ HerdrReport::Release { .. } => panic!("expected Report, got {report:?}"),
        }
    }

    #[test]
    fn waiting_states_report_blocked_with_notification() {
        for state in [AgentState::WaitingInput, AgentState::WaitingChoice] {
            let mut a = agent(AgentKind::ClaudeCode, "s", "herdr:w1:p1", state);
            a.last_notification = Some("Approve running `rm`?".into());
            match report_decision(&a).unwrap() {
                HerdrReport::Report { state, message, .. } => {
                    assert_eq!(state, "blocked", "{state:?} -> blocked");
                    assert_eq!(message.as_deref(), Some("Approve running `rm`?"));
                }
                report @ HerdrReport::Release { .. } => panic!("expected Report, got {report:?}"),
            }
        }
    }

    #[test]
    fn error_reports_blocked_with_error_message() {
        let mut a = agent(AgentKind::Codex, "s", "herdr:w1:p1", AgentState::Error);
        a.last_notification = Some("StopFailure: auth".into());
        match report_decision(&a).unwrap() {
            HerdrReport::Report {
                agent,
                state,
                message,
                ..
            } => {
                assert_eq!(agent, "codex");
                assert_eq!(state, "blocked");
                assert_eq!(message.as_deref(), Some("StopFailure: auth"));
            }
            report @ HerdrReport::Release { .. } => panic!("expected Report, got {report:?}"),
        }
    }

    #[test]
    fn idle_reports_idle() {
        let a = agent(AgentKind::ClaudeCode, "s", "herdr:w1:p1", AgentState::Idle);
        match report_decision(&a).unwrap() {
            HerdrReport::Report { state, .. } => assert_eq!(state, "idle"),
            report @ HerdrReport::Release { .. } => panic!("expected Report, got {report:?}"),
        }
    }

    #[test]
    fn stopped_releases_authority() {
        let a = agent(
            AgentKind::ClaudeCode,
            "s",
            "herdr:w1:p1",
            AgentState::Stopped,
        );
        match report_decision(&a).expect("stopped releases") {
            HerdrReport::Release { pane_id, agent } => {
                assert_eq!(pane_id, "w1:p1");
                assert_eq!(agent, "claude");
            }
            report @ HerdrReport::Report { .. } => panic!("expected Release, got {report:?}"),
        }
    }

    #[test]
    fn starting_reports_nothing() {
        let a = agent(
            AgentKind::ClaudeCode,
            "s",
            "herdr:w1:p1",
            AgentState::Starting,
        );
        assert_eq!(
            report_decision(&a),
            None,
            "transient Starting is not reported"
        );
    }

    #[test]
    fn synthetic_row_is_never_reported_no_loop() {
        // A synthetic (bridge-owned) row on a herdr pane must NEVER be reported
        // back to herdr — that would close the detection loop.
        let a = agent(
            AgentKind::Unknown,
            "synthetic-herdr:w1:p1",
            "herdr:w1:p1",
            AgentState::Working,
        );
        assert_eq!(
            report_decision(&a),
            None,
            "synthetic rows are not echoed back"
        );
    }

    #[test]
    fn non_herdr_pane_is_not_reported() {
        // A real hook row on a tmux pane is none of herdr's business.
        let a = agent(AgentKind::ClaudeCode, "s", "%7", AgentState::Working);
        assert_eq!(report_decision(&a), None);
        // …and a row with no pane at all.
        let mut paneless = agent(AgentKind::ClaudeCode, "s", "%7", AgentState::Working);
        paneless.pane = None;
        assert_eq!(report_decision(&paneless), None);
    }

    #[test]
    fn agent_slug_covers_every_kind() {
        assert_eq!(agent_slug(AgentKind::ClaudeCode), "claude");
        assert_eq!(agent_slug(AgentKind::Codex), "codex");
        assert_eq!(agent_slug(AgentKind::GeminiCli), "gemini");
        assert_eq!(agent_slug(AgentKind::Opencode), "opencode");
        assert_eq!(agent_slug(AgentKind::Task), "task");
        assert_eq!(agent_slug(AgentKind::Unknown), "unknown");
    }

    #[test]
    fn report_request_shape_is_wire_correct() {
        let a = agent(
            AgentKind::ClaudeCode,
            "sess-1",
            "herdr:w1:p1",
            AgentState::Working,
        );
        let report = report_decision(&a).unwrap();
        let (method, params) = report.request(42);
        assert_eq!(method, "pane.report_agent");
        assert_eq!(params["pane_id"], json!("w1:p1"));
        assert_eq!(params["source"], json!("muxa"));
        assert_eq!(params["agent"], json!("claude"));
        assert_eq!(params["state"], json!("working"));
        assert_eq!(params["agent_session_id"], json!("sess-1"));
        assert_eq!(params["seq"], json!(42));
    }

    #[test]
    fn release_request_shape_is_wire_correct() {
        let a = agent(
            AgentKind::ClaudeCode,
            "s",
            "herdr:w1:p1",
            AgentState::Stopped,
        );
        let report = report_decision(&a).unwrap();
        let (method, params) = report.request(7);
        assert_eq!(method, "pane.release_agent");
        assert_eq!(params["pane_id"], json!("w1:p1"));
        assert_eq!(params["source"], json!("muxa"));
        assert_eq!(params["agent"], json!("claude"));
        assert_eq!(params["seq"], json!(7));
    }
}
