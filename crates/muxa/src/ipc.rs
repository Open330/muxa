//! Unix-domain-socket IPC.
//!
//! **Wire format.** Line-delimited JSON (one request per line, one
//! response per line). `serde_json` never emits newlines inside a value, so
//! embedded `\n` in strings is safely escaped — see the round-trip test.
//!
//! Every request carries a `protocol` field set to `PROTOCOL_VERSION`. The
//! server rejects mismatched versions to prevent schema drift from silently
//! corrupting state.
//!
//! **Socket permissions.** The server chmods the socket file to `0600` after
//! binding so only the owning user can send events.
//!
//! **Shutdown.** The server accepts a `CancellationToken`-style signal via
//! the `shutdown` channel and stops accepting new connections, then drains
//! its tracked in-flight handlers (with a bounded timeout) before
//! returning. The drain is what gives the snapshotter task its
//! "last-to-die" guarantee: by the time `Server::run` returns, no handler
//! can call `Store::apply` afterwards, so the daemon's final flush
//! captures every state change the user actually triggered.

use crate::ask::{AskEntry, AskStore};
use crate::backend::{default_backend, HostKind, SharedBackend};
use crate::collaboration::{
    self, AirArtifactReference, CollaborationClientKind, CollaborationOptions, CollaborationOrigin,
    CollaborationOriginMatch, CollaborationPaneEvidence, CollaborationProvenance,
    CollaborationRequest, CollaborationStore, NewRequest, Participant, RequestMailbox,
    RequestStatus, RoomContext,
};
use crate::collaboration_audit::{
    CollaborationAuditContext, CollaborationAuditLog, CollaborationAuditOperation,
};
use crate::event::{AgentEvent, PROTOCOL_VERSION};
use crate::fleet::{
    FleetCommandResult, FleetOperation, FleetRuntime, FleetSnapshot, FleetUpdate, LabelSelector,
};
use crate::session::{
    PtySessionBackend, SessionBackend, SessionOutput, SessionRef, SharedSessionBackend,
    SpawnSession, TerminalSnapshot,
};
use crate::state::{Agent, SharedStore};
use crate::tmux::PaneInfo;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio::task::JoinSet;

/// Maximum time `Server::run` will wait for in-flight handlers to finish
/// after the shutdown signal lands. Sized for the longest plausible
/// handler — a `recap_all` query reading several MB of NDJSON — plus
/// generous slack. If a handler hangs past this we abort it rather than
/// blocking the daemon's exit indefinitely.
const HANDLER_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_IPC_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Upper bound on concurrent connection handlers. Kept well under the
/// daemon's file-descriptor budget so a burst — or a leak — of handlers can
/// never drive `accept()` into `EMFILE` and wedge the whole listener the way
/// a runaway of hung hook connections once did. The server reserves a permit
/// before `accept()`, so over-budget connections wait in the OS backlog or
/// time out client-side instead of consuming another daemon fd.
const MAX_INFLIGHT_HANDLERS: usize = 256;

/// How long a connection may sit between requests without sending a complete
/// line before the handler closes it. Generous enough that a legitimately
/// idle persistent client is never dropped mid-session, tight enough that a
/// client which connects and never sends EOF cannot pin a file descriptor
/// indefinitely. Does not apply to the streaming pump — a `Subscribe`
/// connection leaves the request loop entirely (see [`stream_transitions`]).
const IDLE_CONN_TIMEOUT: Duration = Duration::from_secs(10);

/// Cadence of keepalive writes on a `Subscribe` stream. A dead watch client
/// that has stopped reading is detected on the next keepalive write (broken
/// pipe) instead of lingering until the next real transition — bounding a
/// dead stream's fd lifetime to roughly one interval.
const STREAM_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Overall deadline for a client request/response round trip (connect +
/// hello + write + read). No caller should ever block forever against a
/// wedged or half-dead daemon.
const CLIENT_CALL_TIMEOUT: Duration = Duration::from_secs(3);

/// Tighter deadline for hook ingest, which runs on the agent's critical path
/// (every prompt / every tool call). A wedged daemon must never stall the
/// agent, so fail fast and let `best_effort_ingest` treat it as a no-op.
const HOOK_CALL_TIMEOUT: Duration = Duration::from_millis(750);

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("socket already exists and is in use at {0}; another daemon may be running")]
    SocketInUse(PathBuf),

    #[error(
        "daemon not reachable at {} — is `muxad` running? (start `muxad`, run `muxa doctor`, or set MUXA_SOCKET)",
        .0.display()
    )]
    NotConnected(PathBuf),

    #[error("ipc message exceeds {0} bytes")]
    MessageTooLarge(usize),

    #[error("ipc request timed out after {0:?}")]
    Timeout(Duration),
}

impl RuntimeError {
    fn is_client_disconnect(&self) -> bool {
        matches!(self, Self::Io(e) if is_client_disconnect(e))
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RequestBody {
    Ingest {
        event: AgentEvent,
    },
    Snapshot,
    /// Snapshot of every configured physical SSH host. `selector` follows
    /// Kubernetes label-selector syntax and is evaluated against central
    /// inventory metadata, never against untrusted remote data.
    FleetSnapshot {
        #[serde(default)]
        selector: Option<String>,
    },
    /// Long-lived notification stream for changes to the central Fleet cache.
    /// Payloads are deliberately tiny; clients fetch one coherent filtered
    /// snapshot after coalescing notifications.
    FleetSubscribe {
        #[serde(default)]
        selector: Option<String>,
    },
    /// Route one exact operation through the per-host persistent SSH relay.
    FleetCommand {
        host: String,
        operation: FleetOperation,
    },
    ByPane {
        pane: String,
    },
    BySession {
        session_id: String,
    },
    BySurface {
        surface_id: String,
    },
    /// Disk-backed prompt audit log. `pane = None` returns prompts across
    /// every tracked pane, sorted newest-first; otherwise filtered to one
    /// pane. `limit = 0` (or absent) returns everything available, capped
    /// by the daemon's in-memory retention.
    RecentPrompts {
        #[serde(default)]
        pane: Option<String>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Health,
    /// Ask the daemon to drain and re-exec itself onto the binary currently
    /// installed at its argv[0]. Opt-in: only the real daemon installs a
    /// restart controller; embedders refuse rather than shutting down with no
    /// way to come back.
    Restart,
    /// Capability handshake. Optional first message; opts the connection
    /// into negotiated-protocol mode. The server replies with its
    /// `[min, max]` supported range and a list of capability tags, then
    /// downgrades wire-visible enum variants for the rest of the
    /// connection so a client pinned to an older protocol stays usable.
    Hello {
        #[serde(default)]
        client: Option<String>,
    },
    /// Long-lived streaming subscribe. Server replies with a one-shot
    /// `ok` ack, then writes one JSON-encoded `Transition` per
    /// state change (newline-delimited) until the client closes the
    /// socket. Used by `muxa watch` to switch from 500 ms polling to
    /// push-based updates, and by the `muxa mcp` server's
    /// `muxa_wait_for_change` tool.
    ///
    /// `lagged_markers` (default `false`) opts the connection into receiving
    /// the `{"event":"lagged","dropped":N}` control frame after a broadcast
    /// overflow. It defaults OFF so a pre-marker client (whose `Transition`
    /// parser would choke on the frame and abandon push mode) keeps the
    /// historical behavior — the server silently continues after a lag, and the
    /// client reconciles the gap via its fallback snapshot poll. muxa's own
    /// `TransitionStream` reader understands the frame and opts in.
    Subscribe {
        #[serde(default)]
        lagged_markers: bool,
    },

    /// Control action: inject `text` into `pane` as literal keystrokes,
    /// resolving the backend from the pane-id namespace. When `submit`,
    /// a trailing carriage return is sent as a second injection so the
    /// agent's current line is committed. Refused with a structured
    /// error (never a panic) when the target backend lacks the
    /// `send_text` capability (e.g. zellij). Backs `muxa mcp`'s
    /// `muxa_send_prompt` tool.
    SendPrompt {
        pane: String,
        text: String,
        #[serde(default)]
        submit: bool,
    },

    /// Control/observation: capture the visible contents of `pane` via
    /// the namespace-resolved backend. Backs `muxa mcp`'s
    /// `muxa_capture_pane` tool. Returns `capture = null` when the pane
    /// is gone or the backend can't capture (best-effort).
    Capture {
        pane: String,
    },

    CollaborationContext {
        origin: CollaborationOrigin,
    },
    CollaborationSetIdentity {
        origin: CollaborationOrigin,
        #[serde(default)]
        alias: Option<String>,
        #[serde(default)]
        roles: Vec<String>,
    },
    CollaborationSend {
        origin: CollaborationOrigin,
        target: String,
        request: NewRequest,
    },
    /// Queue a headless question. Returns the pending entry immediately;
    /// the answer lands in the store when the agent exits.
    AskSend {
        prompt: String,
    },
    AskList {},
    /// Point the next question at a different agent, or read back which
    /// one is selected when `agent` is omitted.
    AskAgent {
        #[serde(default)]
        agent: Option<String>,
    },
    /// Start a fresh conversation. History is untouched.
    AskReset {},
    /// Remove completed ask history. Running asks and conversation ids stay.
    AskClear {},
    /// Remove one completed ask history entry by opaque id.
    AskDelete {
        id: String,
    },
    CollaborationInbox {
        origin: CollaborationOrigin,
    },
    CollaborationList {
        origin: CollaborationOrigin,
        #[serde(default)]
        mailbox: RequestMailbox,
    },
    CollaborationReply {
        origin: CollaborationOrigin,
        request_id: String,
        status: RequestStatus,
        #[serde(default)]
        body: String,
        #[serde(default)]
        artifacts: Vec<String>,
        #[serde(default)]
        air_artifacts: Vec<AirArtifactReference>,
    },
    CollaborationGet {
        origin: CollaborationOrigin,
        request_id: String,
    },
    CollaborationCancel {
        origin: CollaborationOrigin,
        request_id: String,
    },

    /// Wholesale pane-metadata snapshot pushed by an out-of-process
    /// backend source — today the zellij WASM plugin forwarding
    /// `PaneUpdate` events. The daemon hands the panes to its
    /// `SharedBackend::ingest_pane_snapshot`, which the zellij backend
    /// caches so `list_panes` / `resolve_pane` answer from real data. A
    /// no-op on tmux (that backend enumerates panes itself).
    BackendPaneSnapshot {
        panes: Vec<PaneInfo>,
    },
    SpawnSession {
        command: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: Vec<(String, String)>,
        #[serde(default)]
        cwd: Option<PathBuf>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        cols: Option<u16>,
        #[serde(default)]
        rows: Option<u16>,
    },
    ListSessions,
    CaptureSession {
        session_id: String,
    },
    ReadSession {
        session_id: String,
        offset: u64,
    },
    WriteSession {
        session_id: String,
        data: String,
    },
    ResizeSession {
        session_id: String,
        cols: u16,
        rows: u16,
    },
    SetSessionAttached {
        session_id: String,
        attached: bool,
    },
    TerminateSession {
        session_id: String,
    },
    /// Register an arbitrary background process as a pid-tracked `Task` row
    /// so it shows up in `muxa status`/`muxa watch`. Backs `muxa register`.
    Register {
        name: String,
        #[serde(default)]
        pid: Option<u32>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        pane: Option<String>,
        #[serde(default)]
        command: Option<String>,
    },
    /// Delete fully orphaned rows (no pane, surface, or pid) idle longer than
    /// `max_age_secs`. Backs `muxa prune` — the on-demand cleanup of
    /// remote/detached ghost rows the reconciler would otherwise only age out
    /// after its 24h sweep. `max_age_secs = 0` (or absent) removes every
    /// orphan regardless of age. Replies with `pruned` = rows removed.
    Prune {
        #[serde(default)]
        max_age_secs: u64,
    },
}

#[derive(Debug, Deserialize)]
struct Request {
    /// Wire protocol version the client expects. Must equal `PROTOCOL_VERSION`.
    #[serde(default)]
    protocol: u32,
    #[serde(flatten)]
    body: RequestBody,
}

/// Oldest protocol the server can still serve via the negotiated regime
/// (i.e. with v1-compat enum downgrade). Bumped when we drop the
/// downgrade path for an older variant.
pub const MIN_PROTOCOL_VERSION: u32 = 1;

/// Stable feature tags advertised by `hello`. Each token names a
/// semver-additive capability the server supports; clients use the list
/// to feature-gate behaviour without re-reading `protocol`.
const CAPABILITIES: &[&str] = &[
    "agent_session_id",
    "waiting_choice",
    "needs_choice",
    "rate_limited",
    "collaboration_mailbox",
    "collaboration_lifecycle",
    "collaboration_identity",
    "collaboration_provenance",
    "fleet_v1",
    "fleet_subscribe",
];

/// Advertised only when the server has the controller required to come back
/// after draining. A server without one refuses `restart`.
const RESTART_CAPABILITY: &str = "restart";

#[derive(Debug, Serialize)]
pub struct Response {
    pub ok: bool,
    pub protocol: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<Agent>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<crate::history::HistoryEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_protocol: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_protocol: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<&'static str>>,
    /// Present only when the daemon can restart itself. It increments across
    /// each re-exec so a client can distinguish the replacement image from
    /// the old daemon still finishing an in-flight response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionRef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<SessionOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pruned: Option<usize>,
    /// Visible pane contents for a `capture` request. `Some("")` is a
    /// real empty capture; `None` means the field is absent (any other
    /// response kind, or a `capture` whose backend returned nothing).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capture: Option<String>,
    /// Whether a `send_prompt`'s text injection landed. Present only on a
    /// `send_prompt` response. `true` means the text is already in the pane
    /// — a caller MUST NOT resend it, even if `submitted` is `false`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sent: Option<bool>,
    /// Whether a `send_prompt`'s submit CR landed. Present only on a
    /// `send_prompt` response. `false` with `sent:true` and a requested
    /// `submit:true` is a PARTIAL success: the text is in the pane but the
    /// Enter didn't commit — retry the submit alone (e.g. `send_prompt` with
    /// empty text + submit), never the whole prompt. `false` is also the
    /// normal value when `submit:false` was requested (nothing to submit).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub submitted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room: Option<RoomContext>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collaboration_requests: Option<Vec<CollaborationRequest>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collaboration_request: Option<CollaborationRequest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_entries: Option<Vec<AskEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_entry: Option<AskEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fleet: Option<FleetSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fleet_result: Option<FleetCommandResult>,
}

#[derive(Debug, Serialize)]
pub struct HealthInfo {
    pub version: &'static str,
    pub protocol: u32,
}

impl Response {
    fn ok() -> Self {
        Self {
            ok: true,
            protocol: PROTOCOL_VERSION,
            error: None,
            agents: None,
            prompts: None,
            health: None,
            min_protocol: None,
            max_protocol: None,
            capabilities: None,
            generation: None,
            sessions: None,
            session: None,
            terminal: None,
            output: None,
            pruned: None,
            capture: None,
            sent: None,
            submitted: None,
            room: None,
            collaboration_requests: None,
            collaboration_request: None,
            ask_entries: None,
            ask_entry: None,
            ask_agent: None,
            fleet: None,
            fleet_result: None,
        }
    }
    fn err(msg: impl Into<String>) -> Self {
        let mut r = Self::ok();
        r.ok = false;
        r.error = Some(msg.into());
        r
    }
    fn with_agents(agents: Vec<Agent>) -> Self {
        let mut r = Self::ok();
        r.agents = Some(agents);
        r
    }
    fn with_fleet(fleet: FleetSnapshot) -> Self {
        let mut response = Self::ok();
        response.fleet = Some(fleet);
        response
    }
    fn with_fleet_result(result: FleetCommandResult) -> Self {
        let mut response = Self::ok();
        response.fleet_result = Some(result);
        response
    }
    fn with_prompts(prompts: Vec<crate::history::HistoryEntry>) -> Self {
        let mut r = Self::ok();
        r.prompts = Some(prompts);
        r
    }
    fn health() -> Self {
        let mut r = Self::ok();
        r.health = Some(HealthInfo {
            version: env!("CARGO_PKG_VERSION"),
            protocol: PROTOCOL_VERSION,
        });
        r
    }
    fn with_sessions(sessions: Vec<SessionRef>) -> Self {
        let mut r = Self::ok();
        r.sessions = Some(sessions);
        r
    }
    fn with_session(session: SessionRef) -> Self {
        let mut r = Self::ok();
        r.session = Some(session);
        r
    }
    fn with_terminal(terminal: TerminalSnapshot) -> Self {
        let mut r = Self::ok();
        r.terminal = Some(terminal);
        r
    }
    fn with_output(output: SessionOutput) -> Self {
        let mut r = Self::ok();
        r.output = Some(output);
        r
    }
    fn with_pruned(pruned: usize) -> Self {
        let mut r = Self::ok();
        r.pruned = Some(pruned);
        r
    }
    fn with_capture(capture: Option<String>) -> Self {
        let mut r = Self::ok();
        r.capture = capture;
        r
    }
    /// A `send_prompt` success carrying the two non-atomic outcomes distinctly
    /// (Fix: honest partial-failure signal). Only built when the text landed
    /// (`sent = true`), so `ok = true`; `submitted` reflects whether the
    /// follow-up submit CR also landed.
    fn with_send_result(sent: bool, submitted: bool) -> Self {
        let mut r = Self::ok();
        r.sent = Some(sent);
        r.submitted = Some(submitted);
        r
    }
    fn with_room(room: RoomContext) -> Self {
        let mut r = Self::ok();
        r.room = Some(room);
        r
    }
    fn with_collaboration_requests(requests: Vec<CollaborationRequest>) -> Self {
        let mut r = Self::ok();
        r.collaboration_requests = Some(requests);
        r
    }
    fn with_collaboration_request(request: CollaborationRequest) -> Self {
        let mut r = Self::ok();
        r.collaboration_request = Some(request);
        r
    }
    fn with_ask_entries(entries: Vec<AskEntry>) -> Self {
        let mut r = Self::ok();
        r.ask_entries = Some(entries);
        r
    }
    fn with_ask_agent(agent: String) -> Self {
        let mut r = Self::ok();
        r.ask_agent = Some(agent);
        r
    }
    fn with_ask_entry(entry: AskEntry) -> Self {
        let mut r = Self::ok();
        r.ask_entry = Some(entry);
        r
    }
    fn hello(restart: Option<&RestartController>) -> Self {
        let mut r = Self::ok();
        r.min_protocol = Some(MIN_PROTOCOL_VERSION);
        r.max_protocol = Some(PROTOCOL_VERSION);
        let mut capabilities = CAPABILITIES.to_vec();
        if restart.is_some() {
            capabilities.push(RESTART_CAPABILITY);
        }
        r.capabilities = Some(capabilities);
        r.generation = restart.map(RestartController::generation);
        r
    }
}

const RESTART_RUNNING: u8 = 0;
const RESTART_REQUESTED: u8 = 1;
const RESTART_STOPPING: u8 = 2;

/// Coordinates daemon shutdown and self-restart without allowing an already
/// open IPC handler to undo an operator's later SIGTERM/SIGINT.
///
/// The state transition is monotonic: `running -> restart_requested ->
/// stopping`, while a signal may move `running -> stopping` directly. Once
/// stopping, a restart request is refused permanently. This closes the race
/// in which a signal cleared a boolean and a draining handler set it again.
#[derive(Debug)]
pub struct RestartController {
    generation: u64,
    state: AtomicU8,
    trigger: broadcast::Sender<()>,
}

impl RestartController {
    #[must_use]
    pub fn new(generation: u64, trigger: broadcast::Sender<()>) -> Self {
        Self {
            generation,
            state: AtomicU8::new(RESTART_RUNNING),
            trigger,
        }
    }

    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Commit to a normal stop and wake every shutdown subscriber. A later
    /// IPC request cannot move the state back to restart-requested.
    pub fn stop(&self) {
        self.state.store(RESTART_STOPPING, AtomicOrdering::SeqCst);
        let _ = self.trigger.send(());
    }

    #[must_use]
    pub fn restart_requested(&self) -> bool {
        self.state.load(AtomicOrdering::SeqCst) == RESTART_REQUESTED
    }

    /// Returns false only after an explicit stop has won. Repeated restart
    /// requests are idempotently accepted while the first request drains.
    fn request_restart(&self) -> bool {
        match self.state.compare_exchange(
            RESTART_RUNNING,
            RESTART_REQUESTED,
            AtomicOrdering::SeqCst,
            AtomicOrdering::SeqCst,
        ) {
            Ok(_) => {
                let _ = self.trigger.send(());
                true
            }
            Err(RESTART_REQUESTED) => true,
            Err(_) => false,
        }
    }
}

/// Daemon-side server. Construct once, call `run` under the tokio runtime.
pub struct Server {
    socket_path: PathBuf,
    store: SharedStore,
    /// Backend the server forwards `BackendPaneSnapshot` pushes to (the
    /// zellij backend in a multi-host daemon; see `with_backend`).
    backend: SharedBackend,
    /// The full set of backends the daemon observes, for namespace-scoped
    /// control routing (`send_prompt` / `capture`). Never empty:
    /// `backends[0]` is the primary/env-preferred host, used as the
    /// fallback when a pane id doesn't classify to a known namespace.
    backends: Vec<SharedBackend>,
    sessions: SharedSessionBackend,
    collaboration: Arc<CollaborationStore>,
    collaboration_audit: Arc<CollaborationAuditLog>,
    ask: Arc<AskStore>,
    restart: Option<Arc<RestartController>>,
    fleet: Option<FleetRuntime>,
    handler_limit: usize,
}

impl Server {
    pub fn new(socket_path: PathBuf, store: SharedStore) -> Self {
        let backend = default_backend();
        Self {
            socket_path,
            store,
            backend: backend.clone(),
            backends: vec![backend],
            sessions: PtySessionBackend::shared(),
            collaboration: CollaborationStore::in_memory(CollaborationOptions::default()),
            collaboration_audit: CollaborationAuditLog::in_memory(),
            ask: crate::ask::AskStore::in_memory(crate::ask::AskOptions::default()),
            restart: None,
            fleet: None,
            handler_limit: MAX_INFLIGHT_HANDLERS,
        }
    }

    /// Set the pane backend the server forwards `BackendPaneSnapshot`
    /// pushes to. The daemon passes the same `SharedBackend` the
    /// reconciler and discovery hold, so a plugin push updates the
    /// snapshot every consumer reads. Defaults to [`default_backend`]
    /// for callers (mostly tests) that never push.
    #[must_use]
    pub fn with_backend(mut self, backend: SharedBackend) -> Self {
        self.backend = backend;
        self
    }

    /// Thread the daemon's full backend set into the server so control
    /// methods (`send_prompt`, `capture`) can resolve the backend that
    /// governs a given pane id's namespace (`%…` → tmux, `herdr:…` →
    /// herdr, …), falling back to the primary (`backends[0]`) for
    /// unclassifiable ids. An empty set is ignored (keeps the
    /// [`Self::new`] default) so the invariant "`backends` is never
    /// empty" holds for the resolver.
    #[must_use]
    pub fn with_backends(mut self, backends: Vec<SharedBackend>) -> Self {
        if !backends.is_empty() {
            self.backends = backends;
        }
        self
    }

    #[must_use]
    pub fn with_sessions(mut self, sessions: SharedSessionBackend) -> Self {
        self.sessions = sessions;
        self
    }

    #[must_use]
    pub fn with_ask(mut self, ask: Arc<AskStore>) -> Self {
        self.ask = ask;
        self
    }

    #[must_use]
    pub fn with_collaboration(mut self, collaboration: Arc<CollaborationStore>) -> Self {
        self.collaboration = collaboration;
        self
    }

    #[must_use]
    pub fn with_collaboration_audit(mut self, audit: Arc<CollaborationAuditLog>) -> Self {
        self.collaboration_audit = audit;
        self
    }

    /// Allow this server to accept the restart control method. Kept opt-in so
    /// embedded servers and tests never drain unless they can re-exec.
    #[must_use]
    pub fn with_restart_controller(mut self, restart: Arc<RestartController>) -> Self {
        self.restart = Some(restart);
        self
    }

    /// Install the physical-host fleet cache and command router. Keeping this
    /// optional preserves embedders/tests and makes a disabled fleet consume
    /// no SSH processes or background resources.
    #[must_use]
    pub fn with_fleet(mut self, fleet: FleetRuntime) -> Self {
        self.fleet = Some(fleet);
        self
    }

    #[cfg(test)]
    #[must_use]
    fn with_handler_limit(mut self, handler_limit: usize) -> Self {
        self.handler_limit = handler_limit;
        self
    }

    /// Run until `shutdown` fires or an I/O error occurs.
    ///
    /// In-flight connection handlers are tracked on a `JoinSet` so a
    /// clean shutdown can drain them before returning. Without that
    /// drain, an ingest landing during shutdown could call
    /// `Store::apply` *after* the snapshotter task has already done its
    /// final flush, losing that event on the next restart. Drained with
    /// a bounded timeout so a hung handler can't block daemon exit.
    #[allow(clippy::too_many_lines)] // accept loop plus bounded drain and socket cleanup
    pub async fn run(self, mut shutdown: broadcast::Receiver<()>) -> Result<(), RuntimeError> {
        self.bind_with_perms()?;
        let listener = UnixListener::bind(&self.socket_path)?;
        harden_permissions(&self.socket_path)?;
        tracing::info!(socket = %self.socket_path.display(), "listening");

        let mut handlers: JoinSet<()> = JoinSet::new();
        // Fixed budget of concurrent handlers. A permit is held for the
        // lifetime of each handler and released when it ends, so live fds
        // from handlers can never exceed `MAX_INFLIGHT_HANDLERS` — keeping
        // the process comfortably below its fd limit no matter how many
        // clients (or hung hooks) pile up.
        let handler_limit = self.handler_limit;
        let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(handler_limit));

        loop {
            let permit = tokio::select! {
                _ = shutdown.recv() => {
                    tracing::info!("shutdown signal received; closing listener");
                    break;
                }
                joined = handlers.join_next(), if !handlers.is_empty() => {
                    if let Some(Err(e)) = joined {
                        tracing::warn!(error = %e, "connection handler task failed");
                    }
                    continue;
                }
                permit = permits.clone().acquire_owned() => {
                    match permit {
                        Ok(permit) => permit,
                        Err(_) => break,
                    }
                }
            };

            tokio::select! {
                _ = shutdown.recv() => {
                    drop(permit);
                    tracing::info!("shutdown signal received; closing listener");
                    break;
                }
                accept = listener.accept() => {
                    let (stream, _) = match accept {
                        Ok(pair) => pair,
                        Err(e) if is_fd_exhaustion(&e) => {
                            // Out of file descriptors. Do NOT propagate: a
                            // returned error kills the accept loop and wedges
                            // the daemon into refusing every connection
                            // forever (the failure mode this whole change
                            // exists to prevent). Back off briefly so we
                            // neither spin at 100% CPU nor starve in-flight
                            // handlers of the CPU they need to free fds.
                            tracing::error!(
                                error = %e,
                                inflight = handler_limit - permits.available_permits(),
                                "accept hit fd exhaustion; backing off",
                            );
                            drop(permit);
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                        Err(e) => {
                            // Per-connection errors (client aborted mid-accept,
                            // etc.) are transient; log and keep serving.
                            tracing::warn!(error = %e, "accept error; continuing");
                            drop(permit);
                            continue;
                        }
                    };
                    let store = self.store.clone();
                    let backend = self.backend.clone();
                    let backends = self.backends.clone();
                    let sessions = self.sessions.clone();
                    let collaboration = self.collaboration.clone();
                    let collaboration_audit = self.collaboration_audit.clone();
                    let ask = self.ask.clone();
                    let restart = self.restart.clone();
                    let fleet = self.fleet.clone();
                    handlers.spawn(async move {
                        // Held for the handler's lifetime; released here on exit.
                        let _permit = permit;
                        if let Err(e) =
                            Box::pin(handle(
                                stream,
                                store,
                                backend,
                                backends,
                                sessions,
                                collaboration,
                                collaboration_audit,
                                ask,
                                restart,
                                fleet,
                            ))
                            .await
                        {
                            if e.is_client_disconnect() {
                                tracing::debug!(error = %e, "client disconnected");
                                return;
                            }
                            tracing::warn!(error = %e, "connection handler failed");
                        }
                    });
                    // Reap finished handlers opportunistically so the JoinSet
                    // doesn't grow unboundedly under steady traffic.
                    while handlers.try_join_next().is_some() {}
                }
            }
        }

        // Drain in-flight handlers with a bounded timeout. Closes the
        // lost-update window where a handler could call `Store::apply`
        // after the daemon's snapshotter has already exited.
        let drain = async { while handlers.join_next().await.is_some() {} };
        if tokio::time::timeout(HANDLER_DRAIN_TIMEOUT, drain)
            .await
            .is_err()
        {
            tracing::warn!(
                timeout_secs = HANDLER_DRAIN_TIMEOUT.as_secs(),
                remaining = handlers.len(),
                "ipc handlers did not drain within timeout; aborting",
            );
            handlers.abort_all();
            // Best-effort: let the abort propagate.
            while handlers.join_next().await.is_some() {}
        } else {
            tracing::debug!("ipc handlers drained cleanly");
        }

        // Remove our own socket file so next startup is clean.
        let _ = std::fs::remove_file(&self.socket_path);
        Ok(())
    }

    /// Pre-bind sequence: if a stale socket exists, remove it; then the
    /// caller binds and immediately chmods 0600 in `run`.
    fn bind_with_perms(&self) -> Result<(), RuntimeError> {
        if self.socket_path.exists() {
            // Probe: is anything listening?
            if std::os::unix::net::UnixStream::connect(&self.socket_path).is_ok() {
                return Err(RuntimeError::SocketInUse(self.socket_path.clone()));
            }
            // Stale socket, safe to remove.
            std::fs::remove_file(&self.socket_path)?;
        }
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }
}

/// Rewrite enum string values introduced in a newer protocol to their
/// older-protocol equivalents so a client that negotiated an older
/// protocol doesn't choke on unknown variants. Walks the JSON tree and
/// mutates standalone string values; substrings inside larger strings
/// (e.g. a prompt that happens to contain the word `waiting_choice`) are
/// deliberately left alone. Called on the write path only when the
/// negotiated protocol is below the current `PROTOCOL_VERSION`.
fn downgrade_wire(v: &mut serde_json::Value, protocol: u32) {
    match v {
        serde_json::Value::String(s) => {
            // `task` AgentKind is a v3 addition → Unknown for older peers.
            if protocol < 3 && s == "task" {
                *s = "unknown".to_string();
            }
            // `waiting_choice` / `needs_choice` are v2 additions.
            if protocol < 2 {
                if s == "waiting_choice" {
                    *s = "waiting_input".to_string();
                } else if s == "needs_choice" {
                    *s = "needs_input".to_string();
                }
            }
        }
        serde_json::Value::Array(xs) => xs.iter_mut().for_each(|x| downgrade_wire(x, protocol)),
        serde_json::Value::Object(m) => {
            // `agent_session_id` is the v4 canonical name. Older peers still
            // expect the historical `session_id` key, so preserve it on
            // negotiated v1-v3 responses without emitting both names.
            if protocol < 4 {
                if let Some(value) = m.remove("agent_session_id") {
                    m.insert("session_id".to_string(), value);
                }
            }
            m.values_mut().for_each(|x| downgrade_wire(x, protocol));
        }
        _ => {}
    }
}

/// Serialize a payload as a single JSON line, applying the wire downgrade
/// when the connection negotiated a protocol older than the current
/// `PROTOCOL_VERSION`. Keeps the fast path (no negotiation, or current)
/// on a direct `to_vec` so we don't pay for a `Value` round-trip on every
/// response.
fn encode_line<T: Serialize>(value: &T, protocol: u32) -> Result<Vec<u8>, serde_json::Error> {
    let mut bytes = if protocol < PROTOCOL_VERSION {
        let mut v = serde_json::to_value(value)?;
        downgrade_wire(&mut v, protocol);
        serde_json::to_vec(&v)?
    } else {
        serde_json::to_vec(value)?
    };
    bytes.push(b'\n');
    Ok(bytes)
}

fn is_client_disconnect(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
    )
}

async fn write_line_or_closed(
    writer: &mut OwnedWriteHalf,
    bytes: &[u8],
) -> Result<bool, RuntimeError> {
    match writer.write_all(bytes).await {
        Ok(()) => {}
        Err(e) if is_client_disconnect(&e) => return Ok(false),
        Err(e) => return Err(RuntimeError::Io(e)),
    }
    match writer.flush().await {
        Ok(()) => Ok(true),
        Err(e) if is_client_disconnect(&e) => Ok(false),
        Err(e) => Err(RuntimeError::Io(e)),
    }
}

async fn read_limited_line<R>(reader: &mut R, line: &mut String) -> Result<usize, RuntimeError>
where
    R: AsyncBufRead + Unpin,
{
    let mut bytes = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            if bytes.is_empty() {
                return Ok(0);
            }
            break;
        }
        let take = available
            .iter()
            .position(|b| *b == b'\n')
            .map_or(available.len(), |pos| pos + 1);
        if bytes.len().saturating_add(take) > MAX_IPC_LINE_BYTES {
            return Err(RuntimeError::MessageTooLarge(MAX_IPC_LINE_BYTES));
        }
        bytes.extend_from_slice(&available[..take]);
        reader.consume(take);
        if bytes.last() == Some(&b'\n') {
            break;
        }
    }
    *line = String::from_utf8(bytes)
        .map_err(|e| RuntimeError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))?;
    Ok(line.len())
}

/// Encode the `{"event":"lagged","dropped":N}` overflow control frame — but
/// only for a subscriber that opted in (`emit = true`, from
/// `subscribe { lagged_markers: true }`).
///
/// Returns `Ok(None)` when the connection did NOT opt in, so the caller writes
/// nothing and the stream silently continues past the lag — the pre-marker
/// behavior a legacy client's `Transition` parser needs (it would otherwise
/// choke on the marker and abandon push mode). Split out so the opt-in gate is
/// unit-testable without forcing a real broadcast overflow.
fn lagged_marker_bytes(
    emit: bool,
    dropped: u64,
    protocol: u32,
) -> Result<Option<Vec<u8>>, serde_json::Error> {
    if !emit {
        return Ok(None);
    }
    let marker = serde_json::json!({ "event": "lagged", "dropped": dropped });
    encode_line(&marker, protocol).map(Some)
}

/// Pump every state transition from `store` to `writer` as a JSON
/// line. Runs until the broadcast channel closes (daemon shutting
/// down) or the client closes its half of the socket — the first
/// failed write returns Ok(()) so the per-connection task wraps
/// cleanly.
///
/// `Lagged` errors are logged but do not terminate the stream:
/// dropping a few transitions on a slow consumer is preferable to
/// disconnecting them. The next snapshot the client takes (via the
/// fallback polling tick) will reconcile any holes.
async fn stream_transitions(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    store: SharedStore,
    protocol: u32,
    emit_lagged: bool,
) -> Result<(), RuntimeError> {
    let mut rx = store.subscribe();
    // Periodic keepalive so a watch client that dies without a clean close is
    // detected on the next write (broken pipe) rather than lingering until the
    // next real transition — which, on an idle daemon, might be never.
    let mut keepalive = tokio::time::interval(STREAM_KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first tick fires immediately; consume it so we don't emit a
    // keepalive the instant the stream opens.
    keepalive.tick().await;
    loop {
        tokio::select! {
            recv = rx.recv() => match recv {
                Ok(t) => {
                    let bytes = encode_line(&t, protocol)?;
                    if writer.write_all(&bytes).await.is_err() {
                        return Ok(());
                    }
                    if writer.flush().await.is_err() {
                        return Ok(());
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        dropped = n,
                        "subscribe lagged; client will reconcile via fallback poll"
                    );
                    // Emit a lagged marker ONLY for clients that opted in
                    // (`subscribe { lagged_markers: true }`). It's a distinct
                    // object shape (`event` tag, no `from`/`to`), which an
                    // opted-in reader (`TransitionStream::recv`) skips — but a
                    // pre-marker client's `Transition` parser would choke on it
                    // and abandon push mode, so an un-opted client gets the
                    // historical behavior: silently continue after the lag and
                    // let its fallback snapshot poll reconcile the gap.
                    if let Some(bytes) = lagged_marker_bytes(emit_lagged, n, protocol)? {
                        if writer.write_all(&bytes).await.is_err() {
                            return Ok(());
                        }
                        if writer.flush().await.is_err() {
                            return Ok(());
                        }
                    }
                }
            },
            _ = keepalive.tick() => {
                // A bare newline: an empty line the client's stream reader
                // skips (see `TransitionStream::recv`). Its only purpose is to
                // provoke a write error against a dead peer so this task exits
                // and frees the fd.
                if writer.write_all(b"\n").await.is_err() {
                    return Ok(());
                }
                if writer.flush().await.is_err() {
                    return Ok(());
                }
            }
        }
    }
}

/// Stream compact Fleet cache invalidations. A notification names the host and
/// revision but is not itself a snapshot; clients coalesce bursts and fetch a
/// coherent selector-filtered snapshot. This keeps one busy remote agent from
/// making the central TUI clone and redraw every host on a fixed timer.
async fn stream_fleet_updates(
    mut writer: tokio::net::unix::OwnedWriteHalf,
    store: Arc<crate::fleet::FleetStore>,
    mut rx: broadcast::Receiver<FleetUpdate>,
    protocol: u32,
    selector: Option<LabelSelector>,
    mut visible_hosts: HashSet<String>,
) -> Result<(), RuntimeError> {
    let mut keepalive = tokio::time::interval(STREAM_KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    keepalive.tick().await;
    loop {
        tokio::select! {
            recv = rx.recv() => match recv {
                Ok(update) => {
                    let was_visible = visible_hosts.contains(&update.host);
                    let is_visible = if selector.is_none() {
                        true
                    } else {
                        store
                            .host_matches_selector(&update.host, selector.as_ref())
                            .await
                    };
                    if is_visible {
                        visible_hosts.insert(update.host.clone());
                    } else {
                        visible_hosts.remove(&update.host);
                    }
                    // A host entering or leaving the selector must invalidate
                    // the filtered snapshot. Unrelated hosts remain silent.
                    if !was_visible && !is_visible {
                        continue;
                    }
                    let bytes = encode_line(&update, protocol)?;
                    if writer.write_all(&bytes).await.is_err()
                        || writer.flush().await.is_err()
                    {
                        return Ok(());
                    }
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
                Err(broadcast::error::RecvError::Lagged(dropped)) => {
                    tracing::warn!(dropped, "fleet subscribe lagged; fallback snapshot will reconcile");
                    let selected = store.snapshot_selected(selector.as_ref()).await;
                    visible_hosts = selected
                        .hosts
                        .into_iter()
                        .map(|host| host.alias)
                        .collect();
                    let update = FleetUpdate {
                        host: "*".into(),
                        state: crate::fleet::FleetHostState::Degraded,
                        revision: None,
                        resync: true,
                    };
                    let bytes = encode_line(&update, protocol)?;
                    if writer.write_all(&bytes).await.is_err()
                        || writer.flush().await.is_err()
                    {
                        return Ok(());
                    }
                }
            },
            _ = keepalive.tick() => {
                if writer.write_all(b"\n").await.is_err()
                    || writer.flush().await.is_err()
                {
                    return Ok(());
                }
            }
        }
    }
}

/// Resolve the backend that governs `pane`'s id namespace (`%…` → tmux,
/// `herdr:…` → herdr, `zellij:…` → zellij).
///
/// - A pane id that classifies to a KNOWN namespace whose backend is in the
///   active set → `Ok(backend)`.
/// - A pane id that classifies to a known namespace whose backend is NOT
///   observed → `Err(kind)`: a **structured refusal**. We must NOT fall back
///   to the primary here — routing e.g. a `herdr:` keystroke onto the tmux
///   backend would inject into the wrong host entirely. The caller turns this
///   into a `namespace-unavailable` error.
/// - An UNCLASSIFIED pane id (legacy/synthetic/unknown shape) → `Ok(primary)`:
///   only these fall back to `backends[0]`, which is never empty (the `Server`
///   builder guarantees it), preserving pre-guard behavior.
fn resolve_backend<'a>(
    backends: &'a [SharedBackend],
    pane: &str,
) -> Result<&'a SharedBackend, HostKind> {
    match crate::backend::pane_id_host_kind(pane) {
        Some(kind) => backends.iter().find(|b| b.kind() == kind).ok_or(kind),
        None => Ok(&backends[0]),
    }
}

/// Resolve the one recorded endpoint for a pane-id control operation.
/// Pane ids repeat across tmux and rmux servers, so silently choosing the
/// first `HashMap` row could inject into an unrelated pane. Duplicate agents on
/// the same endpoint are harmless; distinct endpoints are an explicit error.
fn unique_pane_endpoint(pane: &str, agents: &[Agent]) -> Result<Option<String>, String> {
    let mut endpoints = agents
        .iter()
        .filter_map(|agent| agent.tmux_socket.as_deref())
        .map(|endpoint| crate::backend::pane_endpoint_identity(Some(pane), endpoint))
        .collect::<Vec<_>>();
    endpoints.sort();
    endpoints.dedup();
    match endpoints.as_slice() {
        [] => Ok(None),
        [endpoint] => Ok(Some(endpoint.clone())),
        _ => Err(format!(
            "ambiguous pane {pane}: it exists on multiple endpoints; use socket-scoped dashboard control"
        )),
    }
}

/// The pane inventory a collaboration call is resolved against, plus the
/// participants derived from it. The raw panes travel alongside because an
/// operator console origin has no participant row — its room comes from the
/// pane it was opened from, which need not host an agent.
struct CollaborationTopology {
    participants: Vec<collaboration::Participant>,
    panes: Vec<crate::tmux::PaneInfo>,
}

impl CollaborationTopology {
    fn resolve_origin(
        &self,
        origin: &CollaborationOrigin,
    ) -> Result<collaboration::Participant, collaboration::CollaborationError> {
        collaboration::resolve_origin(origin, &self.participants, &self.panes)
    }
}

async fn collaboration_participants(
    store: &SharedStore,
    backends: &[SharedBackend],
    collaboration: &CollaborationStore,
) -> CollaborationTopology {
    let agents = store.snapshot().await;
    let backends = backends.to_vec();
    let panes = tokio::task::spawn_blocking(move || {
        backends
            .iter()
            .flat_map(|backend| backend.list_panes())
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let mut participants = collaboration::participants_from(&agents, &panes);
    collaboration.enrich_participants(&mut participants).await;
    CollaborationTopology {
        participants,
        panes,
    }
}

#[derive(Debug, Clone)]
struct CollaborationConnectionActor {
    client_kind: CollaborationClientKind,
    caller_pid: Option<u32>,
    caller_uid: Option<u32>,
    caller_gid: Option<u32>,
    executable: Option<String>,
    observed_pane: Option<String>,
    pane_evidence: Option<CollaborationPaneEvidence>,
    pane_observed: bool,
}

impl CollaborationConnectionActor {
    async fn observe_pane(&mut self, backends: &[SharedBackend]) {
        if self.pane_observed {
            return;
        }
        self.pane_observed = true;
        let Some(pid) = self.caller_pid else {
            return;
        };
        let pane_backends = backends.to_vec();
        let observed =
            tokio::task::spawn_blocking(move || observed_process_pane(pid, &pane_backends))
                .await
                .ok()
                .flatten();
        if let Some((pane, evidence)) = observed {
            self.observed_pane = Some(pane);
            self.pane_evidence = Some(evidence);
        }
    }

    fn provenance(&self, origin: &CollaborationOrigin) -> CollaborationProvenance {
        let origin_match = match self.observed_pane.as_deref() {
            Some(pane) if pane == origin.pane => CollaborationOriginMatch::Matched,
            Some(_) => CollaborationOriginMatch::Mismatched,
            None => CollaborationOriginMatch::Unverifiable,
        };
        CollaborationProvenance {
            client_kind: self.client_kind,
            caller_pid: self.caller_pid,
            caller_uid: self.caller_uid,
            caller_gid: self.caller_gid,
            executable: self.executable.clone(),
            observed_pane: self.observed_pane.clone(),
            pane_evidence: self.pane_evidence,
            origin_match,
        }
    }
}

#[allow(clippy::similar_names)] // PID/UID/GID are the exact peer credential fields
fn observe_collaboration_actor(stream: &UnixStream) -> CollaborationConnectionActor {
    let credentials = stream.peer_cred().ok();
    let caller_pid = credentials
        .as_ref()
        .and_then(tokio::net::unix::UCred::pid)
        .and_then(|pid| u32::try_from(pid).ok());
    let caller_uid: Option<u32> = credentials.as_ref().map(tokio::net::unix::UCred::uid);
    let caller_gid: Option<u32> = credentials.as_ref().map(tokio::net::unix::UCred::gid);
    let (executable, process_kind) =
        caller_pid.map_or((None, CollaborationClientKind::Unknown), process_identity);
    CollaborationConnectionActor {
        client_kind: process_kind,
        caller_pid,
        caller_uid,
        caller_gid,
        executable,
        observed_pane: None,
        pane_evidence: None,
        pane_observed: false,
    }
}

fn observed_process_pane(
    pid: u32,
    backends: &[SharedBackend],
) -> Option<(String, CollaborationPaneEvidence)> {
    if let Some(pane) = process_environment_pane(pid) {
        if backends
            .iter()
            .any(|backend| backend.resolve_pane(&pane).is_some())
        {
            return Some((pane, CollaborationPaneEvidence::ProcessEnvironment));
        }
    }
    let pane_pids = backends
        .iter()
        .flat_map(|backend| backend.pane_pid_map())
        .collect::<std::collections::HashMap<_, _>>();
    if pane_pids.is_empty() {
        return None;
    }
    if let Some(pane) = pane_pids.get(&pid) {
        return Some((pane.clone(), CollaborationPaneEvidence::ProcessAncestry));
    }
    let candidates = pane_pids
        .keys()
        .copied()
        .collect::<std::collections::HashSet<_>>();
    let pane_pid = crate::adapters::proc_ancestry::ancestor_in_set(
        pid,
        &candidates,
        crate::adapters::proc_ancestry::parent_pid,
    )?;
    pane_pids
        .get(&pane_pid)
        .cloned()
        .map(|pane| (pane, CollaborationPaneEvidence::ProcessAncestry))
}

#[cfg(target_os = "linux")]
fn process_environment_pane(pid: u32) -> Option<String> {
    let environment = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    environment
        .split(|byte| *byte == 0)
        .filter_map(|entry| std::str::from_utf8(entry).ok())
        .find_map(|entry| entry.strip_prefix("TMUX_PANE="))
        .filter(|pane| pane.starts_with('%'))
        .map(str::to_string)
}

#[cfg(not(target_os = "linux"))]
fn process_environment_pane(_pid: u32) -> Option<String> {
    None
}

fn process_identity(pid: u32) -> (Option<String>, CollaborationClientKind) {
    #[cfg(target_os = "linux")]
    {
        let executable = std::fs::read_link(format!("/proc/{pid}/exe"))
            .ok()
            .and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            });
        let kind = std::fs::read(format!("/proc/{pid}/cmdline")).ok().map_or(
            CollaborationClientKind::Unknown,
            |bytes| {
                bytes
                    .split(|byte| *byte == 0)
                    .filter_map(|arg| std::str::from_utf8(arg).ok())
                    .find_map(client_kind_from_arg)
                    .unwrap_or(CollaborationClientKind::Unknown)
            },
        );
        (executable, kind)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = std::process::Command::new("ps")
            .args(["-o", "comm=", "-p", &pid.to_string()])
            .output()
            .ok();
        let executable = output.and_then(|output| {
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        });
        (executable, CollaborationClientKind::Unknown)
    }
}

fn client_kind_from_arg(arg: &str) -> Option<CollaborationClientKind> {
    match arg {
        "watch" => Some(CollaborationClientKind::Watch),
        "mcp" => Some(CollaborationClientKind::Mcp),
        "dashboard" => Some(CollaborationClientKind::Dashboard),
        "msg" | "peers" | "identity" => Some(CollaborationClientKind::Cli),
        _ => None,
    }
}

fn represented_participant(
    response: &Response,
    origin: &CollaborationOrigin,
) -> Option<Participant> {
    if let Some(room) = response.room.as_ref() {
        return Some(room.current.clone());
    }
    let requests = response
        .collaboration_request
        .iter()
        .chain(response.collaboration_requests.iter().flatten());
    if origin.console {
        // The origin pane is provenance (where the operator opened the
        // console), not the represented identity. In particular, when the
        // console sends to that same pane, matching by `origin.pane` would
        // incorrectly record the recipient as the represented sender.
        return requests
            .flat_map(|request| [&request.from, &request.to])
            .find(|participant| participant.console)
            .cloned();
    }
    for request in requests {
        for participant in [&request.from, &request.to] {
            if participant.pane == origin.pane
                && origin
                    .socket
                    .as_deref()
                    .is_none_or(|socket| participant.socket.as_deref() == Some(socket))
            {
                return Some(participant.clone());
            }
        }
    }
    None
}

async fn record_collaboration_audit(
    audit: &CollaborationAuditLog,
    actor: &CollaborationConnectionActor,
    context: CollaborationAuditContext,
    response: &Response,
) {
    let represented = represented_participant(response, &context.represented_origin);
    let response_request_id = response
        .collaboration_request
        .as_ref()
        .map(|request| request.id.as_str());
    let result_count = response
        .collaboration_requests
        .as_ref()
        .map(Vec::len)
        .or_else(|| response.collaboration_request.as_ref().map(|_| 1));
    let provenance = actor.provenance(&context.represented_origin);
    let entry = context.finish(
        provenance,
        represented.as_ref(),
        response_request_id,
        result_count,
        response.error.as_deref(),
    );
    audit.append(entry).await;
}

#[tracing::instrument(
    level = "debug",
    skip(
        stream,
        store,
        backend,
        backends,
        sessions,
        collaboration,
        collaboration_audit,
        ask,
        restart,
        fleet
    )
)]
#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // IPC dispatch table and its shared daemon state
async fn handle(
    stream: UnixStream,
    store: SharedStore,
    backend: SharedBackend,
    backends: Vec<SharedBackend>,
    sessions: SharedSessionBackend,
    collaboration: Arc<CollaborationStore>,
    collaboration_audit: Arc<CollaborationAuditLog>,
    ask: Arc<AskStore>,
    restart: Option<Arc<RestartController>>,
    fleet: Option<FleetRuntime>,
) -> Result<(), RuntimeError> {
    let mut collaboration_actor = observe_collaboration_actor(&stream);
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    // Per-connection negotiated protocol. `None` until the client sends
    // `hello` — keeps the legacy strict-match check in force for clients
    // that never opt into negotiation. Once set, the daemon honors the
    // pinned version on every subsequent message on this connection,
    // including the streaming pump.
    let mut negotiated: Option<u32> = None;

    loop {
        line.clear();
        // Bound the wait for the *next* request line. A client that connects
        // and then neither sends a complete request nor closes its half would
        // otherwise park this handler — and pin its fd — forever. A live
        // persistent client simply reconnects; a half-open one can't leak.
        // (A `Subscribe` connection never reaches a second iteration: it hands
        // off to `stream_transitions` and returns, so streams are unaffected.)
        let Ok(read_result) =
            tokio::time::timeout(IDLE_CONN_TIMEOUT, read_limited_line(&mut reader, &mut line))
                .await
        else {
            tracing::debug!("idle connection timed out; closing");
            return Ok(());
        };
        let n = match read_result {
            Ok(n) => n,
            Err(e) if e.is_client_disconnect() => return Ok(()),
            Err(e) => return Err(e),
        };
        if n == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Per-message timer. Start after the read so we don't bake
        // client-side blocking time into the handler latency we're
        // trying to measure. `Instant::now()` is a vDSO call on Linux
        // — effectively free.
        let started = Instant::now();
        // Track which message kind we just dispatched so the timing
        // line below can include it as a structured field. Initialised
        // to a sentinel that every match arm overwrites — the
        // assignment is preserved deliberately so an added arm that
        // forgets to label itself shows up as `dispatch_unknown` in
        // logs rather than mis-attributing the timing.
        #[allow(unused_assignments)]
        let mut kind: &'static str = "dispatch_unknown";
        let resp = match serde_json::from_str::<Request>(trimmed) {
            // Strict-match only applies in the legacy regime, and only
            // for non-`hello` kinds. Once the client has sent `hello`,
            // the negotiated version governs and per-message `protocol`
            // fields are advisory. `hello` itself carries the requested
            // version and is checked inside its own arm.
            Ok(req)
                if negotiated.is_none()
                    && !matches!(req.body, RequestBody::Hello { .. })
                    && req.protocol != 0
                    && req.protocol != PROTOCOL_VERSION =>
            {
                kind = "protocol_mismatch";
                Response::err(format!(
                    "protocol mismatch: server={PROTOCOL_VERSION} client={}",
                    req.protocol
                ))
            }
            Ok(req) => match req.body {
                RequestBody::Hello { client } => {
                    kind = "hello";
                    let requested = if req.protocol == 0 {
                        PROTOCOL_VERSION
                    } else {
                        req.protocol
                    };
                    if (MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&requested) {
                        negotiated = Some(requested);
                        if let Some(client_kind) = client
                            .as_deref()
                            .map(CollaborationClientKind::from_hello_label)
                            .filter(|kind| *kind != CollaborationClientKind::Unknown)
                        {
                            collaboration_actor.client_kind = client_kind;
                        }
                        tracing::debug!(
                            client = client.as_deref().unwrap_or("(unknown)"),
                            protocol = requested,
                            "hello"
                        );
                        let mut r = Response::hello(restart.as_deref());
                        r.protocol = requested;
                        r
                    } else {
                        Response::err(format!(
                            "unsupported protocol: server supports [{MIN_PROTOCOL_VERSION},{PROTOCOL_VERSION}] client={requested}",
                        ))
                    }
                }
                RequestBody::Ingest { event } => {
                    kind = "ingest";
                    // Drop events from tmux servers outside the configured
                    // `MUXA_TMUX_SOCKET` scope. muxa's agent hooks are
                    // installed globally, so an agent another multiplexer
                    // (e.g. cmux) launched on its own server would otherwise
                    // register here with pane ids muxa can't correlate —
                    // unmappable `%NN` ghost rows. Ack it either way so the
                    // agent's hook never sees an error on its critical path.
                    let pane_host = event
                        .id()
                        .pane
                        .as_deref()
                        .and_then(crate::backend::pane_id_host_kind);
                    let in_scope = pane_host == Some(HostKind::Rmux)
                        || crate::tmux::scanner::event_tmux_socket_in_scope(
                            event.id().tmux_socket.as_deref(),
                        );
                    if in_scope {
                        tracing::debug!(?event, "ingest");
                        store.apply(&event).await;
                    } else {
                        tracing::debug!(
                            socket = event.id().tmux_socket.as_deref(),
                            "ingest skipped: tmux socket outside MUXA_TMUX_SOCKET scope",
                        );
                    }
                    Response::ok()
                }
                RequestBody::Register {
                    name,
                    pid,
                    cwd,
                    pane,
                    command,
                } => {
                    kind = "register";
                    match store.register_task(name, pid, cwd, pane, command).await {
                        Ok(session_id) => {
                            tracing::debug!(session_id, ?pid, "register task");
                            Response::ok()
                        }
                        Err(e) => Response::err(e),
                    }
                }
                RequestBody::Prune { max_age_secs } => {
                    kind = "prune";
                    let cutoff = time::OffsetDateTime::now_utc()
                        - std::time::Duration::from_secs(max_age_secs);
                    let pruned = store.prune_orphans(cutoff).await;
                    tracing::debug!(pruned, max_age_secs, "prune orphans");
                    Response::with_pruned(pruned)
                }
                RequestBody::Snapshot => {
                    kind = "snapshot";
                    Response::with_agents(store.snapshot().await)
                }
                RequestBody::FleetSnapshot { selector } => {
                    kind = "fleet_snapshot";
                    match &fleet {
                        Some(fleet) => match selector
                            .as_deref()
                            .map(str::parse::<LabelSelector>)
                            .transpose()
                        {
                            Ok(selector) => Response::with_fleet(
                                fleet.store.snapshot_selected(selector.as_ref()).await,
                            ),
                            Err(error) => Response::err(format!("invalid label selector: {error}")),
                        },
                        None => Response::err("fleet is not enabled in muxad"),
                    }
                }
                RequestBody::FleetSubscribe { selector } => {
                    kind = "fleet_subscribe";
                    let Some(fleet) = &fleet else {
                        let response = Response::err("fleet is not enabled in muxad");
                        let bytes = encode_line(&response, negotiated.unwrap_or(PROTOCOL_VERSION))?;
                        let _ = write_line_or_closed(&mut writer, &bytes).await?;
                        return Ok(());
                    };
                    let selector = match selector
                        .as_deref()
                        .map(str::parse::<LabelSelector>)
                        .transpose()
                    {
                        Ok(selector) => selector,
                        Err(error) => {
                            let response =
                                Response::err(format!("invalid label selector: {error}"));
                            let bytes =
                                encode_line(&response, negotiated.unwrap_or(PROTOCOL_VERSION))?;
                            let _ = write_line_or_closed(&mut writer, &bytes).await?;
                            return Ok(());
                        }
                    };
                    // Subscribe before observing the initial membership and
                    // before ACK. The client fetches a fresh snapshot after
                    // ACK, so every mutation is represented either there or
                    // in this already-live receiver (duplicates are harmless).
                    let updates = fleet.store.subscribe();
                    let visible_hosts = fleet
                        .store
                        .snapshot_selected(selector.as_ref())
                        .await
                        .hosts
                        .into_iter()
                        .map(|host| host.alias)
                        .collect::<HashSet<_>>();
                    let stream_proto = negotiated.unwrap_or(PROTOCOL_VERSION);
                    let ack_bytes = encode_line(&Response::ok(), stream_proto)?;
                    if !write_line_or_closed(&mut writer, &ack_bytes).await? {
                        return Ok(());
                    }
                    tracing::debug!(
                        elapsed_us =
                            u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                        kind,
                        "ipc.handle (fleet stream takeover)",
                    );
                    return stream_fleet_updates(
                        writer,
                        fleet.store.clone(),
                        updates,
                        stream_proto,
                        selector,
                        visible_hosts,
                    )
                    .await;
                }
                RequestBody::FleetCommand { host, operation } => {
                    kind = "fleet_command";
                    match &fleet {
                        Some(fleet) => match fleet
                            .execute(host, operation, Duration::from_secs(20))
                            .await
                        {
                            Ok(result) => Response::with_fleet_result(result),
                            Err(error) => Response::err(error),
                        },
                        None => Response::err("fleet is not enabled in muxad"),
                    }
                }
                RequestBody::ByPane { pane } => {
                    kind = "by_pane";
                    Response::with_agents(store.by_pane(&pane).await)
                }
                RequestBody::BySession { session_id } => {
                    kind = "by_session";
                    let v = store
                        .by_session(&session_id)
                        .await
                        .into_iter()
                        .collect::<Vec<_>>();
                    Response::with_agents(v)
                }
                RequestBody::BySurface { surface_id } => {
                    kind = "by_surface";
                    Response::with_agents(store.by_surface(&surface_id).await)
                }
                RequestBody::RecentPrompts { pane, limit } => {
                    kind = "recent_prompts";
                    let prompts = store
                        .recent_prompts(pane.as_deref(), limit.unwrap_or(0))
                        .await;
                    Response::with_prompts(prompts)
                }
                RequestBody::Health => {
                    kind = "health";
                    Response::health()
                }
                RequestBody::Restart => {
                    kind = "restart";
                    match &restart {
                        Some(controller) if controller.request_restart() => {
                            tracing::info!(
                                generation = controller.generation(),
                                "restart requested over IPC",
                            );
                            Response::ok()
                        }
                        Some(_) => {
                            Response::err("daemon is already stopping; restart request refused")
                        }
                        None => Response::err(
                            "this server cannot restart itself (no restart controller installed)",
                        ),
                    }
                }
                RequestBody::BackendPaneSnapshot { panes } => {
                    kind = "backend_pane_snapshot";
                    let count = panes.len();
                    backend.ingest_pane_snapshot(panes);
                    tracing::debug!(panes = count, "backend_pane_snapshot");
                    Response::ok()
                }
                RequestBody::SendPrompt { pane, text, submit } => {
                    kind = "send_prompt";
                    match resolve_backend(&backends, &pane) {
                        // Known namespace, but no active backend observes it —
                        // refuse rather than mis-route keystrokes to another
                        // host (routing `herdr:` onto tmux would type into the
                        // wrong pane entirely).
                        Err(missing) => Response::err(format!(
                            "namespace unavailable: no active {missing} backend for pane {pane}",
                        )),
                        // Structured refusal, not a panic: the backend that
                        // owns this pane's namespace can't inject keystrokes
                        // (e.g. zellij).
                        Ok(target) if !target.caps().send_text => Response::err(format!(
                            "backend {} does not support send_text (pane {pane})",
                            target.kind(),
                        )),
                        Ok(target) => {
                            let target = target.clone();
                            // Pin the injection to the specific server this
                            // pane's agent row was recorded on — `%5` exists on
                            // every tmux server, so an env-scoped send could hit
                            // the wrong one. `None` for hosts without a server
                            // concept (herdr) or an untracked pane, which falls
                            // back to the env-scoped default.
                            let agents = store.by_pane(&pane).await;
                            match unique_pane_endpoint(&pane, &agents) {
                                Err(error) => Response::err(error),
                                Ok(socket) => {
                                    // `send_text_on` is a blocking shell-out /
                                    // socket call, so run it off the async worker.
                                    // Text and submit CR are TWO non-atomic
                                    // injections; report the outcomes separately.
                                    let (sent, submitted) =
                                        tokio::task::spawn_blocking(move || {
                                            let s = socket.as_deref();
                                            let sent = target.send_text_on(s, &pane, &text);
                                            let submitted = if sent && submit {
                                                if !text.is_empty() {
                                                    std::thread::sleep(
                                                        crate::backend::PROMPT_SUBMIT_GRACE,
                                                    );
                                                }
                                                target.send_text_on(s, &pane, "\r")
                                            } else {
                                                false
                                            };
                                            (sent, submitted)
                                        })
                                        .await
                                        .unwrap_or((false, false));
                                    if sent {
                                        tracing::debug!(submit, submitted, "send_prompt");
                                        Response::with_send_result(sent, submitted)
                                    } else {
                                        // Nothing landed — safe for the caller to
                                        // retry the whole send.
                                        Response::err(
                                            "send_text failed: pane gone or host unreachable",
                                        )
                                    }
                                }
                            }
                        }
                    }
                }
                RequestBody::Capture { pane } => {
                    kind = "capture";
                    match resolve_backend(&backends, &pane) {
                        // Same structured refusal as send_prompt: capturing via
                        // the wrong backend would read a different host's screen.
                        Err(missing) => Response::err(format!(
                            "namespace unavailable: no active {missing} backend for pane {pane}",
                        )),
                        Ok(target) => {
                            let target = target.clone();
                            // Capture the RIGHT `%5` by pinning to the pane's
                            // recorded server (see send_prompt above).
                            let agents = store.by_pane(&pane).await;
                            match unique_pane_endpoint(&pane, &agents) {
                                Err(error) => Response::err(error),
                                Ok(socket) => {
                                    let text = tokio::task::spawn_blocking(move || {
                                        target.capture_pane_on(socket.as_deref(), &pane)
                                    })
                                    .await
                                    .unwrap_or(None);
                                    Response::with_capture(text)
                                }
                            }
                        }
                    }
                }
                RequestBody::CollaborationContext { origin } => {
                    kind = "collaboration_context";
                    collaboration_actor.observe_pane(&backends).await;
                    let audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::Context,
                        origin.clone(),
                    );
                    let response = if collaboration.enabled() {
                        let topology =
                            collaboration_participants(&store, &backends, &collaboration).await;
                        match topology.resolve_origin(&origin) {
                            Ok(current) => Response::with_room(
                                collaboration::room_context(
                                    collaboration.as_ref(),
                                    current,
                                    &topology.participants,
                                )
                                .await,
                            ),
                            Err(error) => Response::err(error.to_string()),
                        }
                    } else {
                        Response::err(
                            "agent collaboration is disabled; enable [collaboration].enabled",
                        )
                    };
                    record_collaboration_audit(
                        &collaboration_audit,
                        &collaboration_actor,
                        audit_context,
                        &response,
                    )
                    .await;
                    response
                }
                RequestBody::CollaborationSetIdentity {
                    origin,
                    alias,
                    roles,
                } => {
                    kind = "collaboration_set_identity";
                    collaboration_actor.observe_pane(&backends).await;
                    let audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::SetIdentity,
                        origin.clone(),
                    );
                    let topology =
                        collaboration_participants(&store, &backends, &collaboration).await;
                    let response = match topology.resolve_origin(&origin) {
                        Ok(current) => match collaboration
                            .set_identity(&current, &topology.participants, alias, roles)
                            .await
                        {
                            Ok(current) => Response::with_room(
                                collaboration::room_context(
                                    collaboration.as_ref(),
                                    current,
                                    &topology.participants,
                                )
                                .await,
                            ),
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    };
                    record_collaboration_audit(
                        &collaboration_audit,
                        &collaboration_actor,
                        audit_context,
                        &response,
                    )
                    .await;
                    response
                }
                RequestBody::AskSend { prompt } => {
                    kind = "ask_send";
                    match ask.ask(&prompt).await {
                        Ok(entry) => Response::with_ask_entry(entry),
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::AskList {} => {
                    kind = "ask_list";
                    Response::with_ask_entries(ask.list().await)
                }
                RequestBody::AskAgent { agent } => {
                    kind = "ask_agent";
                    match agent {
                        Some(name) => match ask.set_agent(&name).await {
                            Ok(label) => Response::with_ask_agent(label),
                            Err(error) => Response::err(error.to_string()),
                        },
                        None => Response::with_ask_agent(ask.agent().await),
                    }
                }
                RequestBody::AskReset {} => {
                    kind = "ask_reset";
                    ask.reset_thread().await;
                    Response::ok()
                }
                RequestBody::AskClear {} => {
                    kind = "ask_clear";
                    Response::with_pruned(ask.clear_history().await)
                }
                RequestBody::AskDelete { id } => {
                    kind = "ask_delete";
                    Response::with_pruned(usize::from(ask.delete_history_entry(&id).await))
                }
                RequestBody::CollaborationSend {
                    origin,
                    target,
                    request,
                } => {
                    kind = "collaboration_send";
                    collaboration_actor.observe_pane(&backends).await;
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::Send,
                        origin.clone(),
                    );
                    audit_context.target = Some(target.clone());
                    audit_context.message_bytes = Some(request.body.len());
                    let topology =
                        collaboration_participants(&store, &backends, &collaboration).await;
                    let result = topology.resolve_origin(&origin).and_then(|sender| {
                        collaboration::resolve_target(
                            &sender,
                            &target,
                            &topology.participants,
                            collaboration.scope(),
                        )
                        .map(|recipient| (sender, recipient))
                    });
                    let response = match result {
                        Ok((sender, recipient)) => {
                            let provenance = collaboration_actor.provenance(&origin);
                            match collaboration
                                .create_with_provenance(
                                    sender,
                                    recipient,
                                    request,
                                    Some(provenance),
                                )
                                .await
                            {
                                Ok(request) => Response::with_collaboration_request(request),
                                Err(error) => Response::err(error.to_string()),
                            }
                        }
                        Err(error) => Response::err(error.to_string()),
                    };
                    record_collaboration_audit(
                        &collaboration_audit,
                        &collaboration_actor,
                        audit_context,
                        &response,
                    )
                    .await;
                    response
                }
                RequestBody::CollaborationInbox { origin } => {
                    kind = "collaboration_inbox";
                    collaboration_actor.observe_pane(&backends).await;
                    let audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::Inbox,
                        origin.clone(),
                    );
                    let topology =
                        collaboration_participants(&store, &backends, &collaboration).await;
                    let response = match topology.resolve_origin(&origin) {
                        Ok(current) => match collaboration.claim_for(&current).await {
                            Ok(requests) => Response::with_collaboration_requests(requests),
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    };
                    record_collaboration_audit(
                        &collaboration_audit,
                        &collaboration_actor,
                        audit_context,
                        &response,
                    )
                    .await;
                    response
                }
                RequestBody::CollaborationList { origin, mailbox } => {
                    kind = "collaboration_list";
                    collaboration_actor.observe_pane(&backends).await;
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::List,
                        origin.clone(),
                    );
                    audit_context.mailbox = Some(mailbox);
                    let topology =
                        collaboration_participants(&store, &backends, &collaboration).await;
                    let response = match topology.resolve_origin(&origin) {
                        Ok(current) => match collaboration.list_for(&current, mailbox).await {
                            Ok(requests) => Response::with_collaboration_requests(requests),
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    };
                    record_collaboration_audit(
                        &collaboration_audit,
                        &collaboration_actor,
                        audit_context,
                        &response,
                    )
                    .await;
                    response
                }
                RequestBody::CollaborationReply {
                    origin,
                    request_id,
                    status,
                    body,
                    artifacts,
                    air_artifacts,
                } => {
                    kind = "collaboration_reply";
                    collaboration_actor.observe_pane(&backends).await;
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::Reply,
                        origin.clone(),
                    );
                    audit_context.request_id = Some(request_id.clone());
                    audit_context.status = Some(status);
                    audit_context.message_bytes = Some(body.len());
                    let topology =
                        collaboration_participants(&store, &backends, &collaboration).await;
                    let response = match topology.resolve_origin(&origin) {
                        Ok(current) => match collaboration
                            .reply(
                                &current,
                                &request_id,
                                status,
                                body,
                                artifacts,
                                air_artifacts,
                            )
                            .await
                        {
                            Ok(request) => Response::with_collaboration_request(request),
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    };
                    record_collaboration_audit(
                        &collaboration_audit,
                        &collaboration_actor,
                        audit_context,
                        &response,
                    )
                    .await;
                    response
                }
                RequestBody::CollaborationGet { origin, request_id } => {
                    kind = "collaboration_get";
                    collaboration_actor.observe_pane(&backends).await;
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::Get,
                        origin.clone(),
                    );
                    audit_context.request_id = Some(request_id.clone());
                    let topology =
                        collaboration_participants(&store, &backends, &collaboration).await;
                    let response = match topology.resolve_origin(&origin) {
                        Ok(current) => match collaboration.get_for(&current, &request_id).await {
                            Ok(request) => Response::with_collaboration_request(request),
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    };
                    record_collaboration_audit(
                        &collaboration_audit,
                        &collaboration_actor,
                        audit_context,
                        &response,
                    )
                    .await;
                    response
                }
                RequestBody::CollaborationCancel { origin, request_id } => {
                    kind = "collaboration_cancel";
                    collaboration_actor.observe_pane(&backends).await;
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::Cancel,
                        origin.clone(),
                    );
                    audit_context.request_id = Some(request_id.clone());
                    let topology =
                        collaboration_participants(&store, &backends, &collaboration).await;
                    let response = match topology.resolve_origin(&origin) {
                        Ok(current) => {
                            match collaboration.cancel_for(&current, &request_id).await {
                                Ok(request) => Response::with_collaboration_request(request),
                                Err(error) => Response::err(error.to_string()),
                            }
                        }
                        Err(error) => Response::err(error.to_string()),
                    };
                    record_collaboration_audit(
                        &collaboration_audit,
                        &collaboration_actor,
                        audit_context,
                        &response,
                    )
                    .await;
                    response
                }
                RequestBody::SpawnSession {
                    command,
                    args,
                    env,
                    cwd,
                    name,
                    cols,
                    rows,
                } => {
                    kind = "spawn_session";
                    match sessions.spawn_session(SpawnSession {
                        command,
                        args,
                        env,
                        cwd,
                        name,
                        cols,
                        rows,
                    }) {
                        Ok(session) => {
                            // Surface the PTY child as a pid-tracked Task row
                            // so `muxa run` processes appear in `muxa status`.
                            // Best-effort: a name collision with a real agent
                            // just skips the task row, the session still runs.
                            let _ = store
                                .register_task(
                                    session
                                        .display_name
                                        .clone()
                                        .unwrap_or_else(|| session.id.clone()),
                                    session.pid,
                                    session.cwd.clone(),
                                    None,
                                    None,
                                )
                                .await;
                            Response::with_session(session)
                        }
                        Err(e) => Response::err(e.to_string()),
                    }
                }
                RequestBody::ListSessions => {
                    kind = "list_sessions";
                    Response::with_sessions(sessions.list_sessions())
                }
                RequestBody::CaptureSession { session_id } => {
                    kind = "capture_session";
                    match sessions.capture(&session_id) {
                        Ok(snapshot) => Response::with_terminal(snapshot),
                        Err(e) => Response::err(e.to_string()),
                    }
                }
                RequestBody::ReadSession { session_id, offset } => {
                    kind = "read_session";
                    match sessions.read_output(&session_id, offset) {
                        Ok(output) => Response::with_output(output),
                        Err(e) => Response::err(e.to_string()),
                    }
                }
                RequestBody::WriteSession { session_id, data } => {
                    kind = "write_session";
                    match sessions.send_input(&session_id, data.as_bytes()) {
                        Ok(()) => Response::ok(),
                        Err(e) => Response::err(e.to_string()),
                    }
                }
                RequestBody::ResizeSession {
                    session_id,
                    cols,
                    rows,
                } => {
                    kind = "resize_session";
                    match sessions.resize(&session_id, cols, rows) {
                        Ok(()) => Response::ok(),
                        Err(e) => Response::err(e.to_string()),
                    }
                }
                RequestBody::SetSessionAttached {
                    session_id,
                    attached,
                } => {
                    kind = "set_session_attached";
                    match sessions.set_attached(&session_id, attached) {
                        Ok(()) => Response::ok(),
                        Err(e) => Response::err(e.to_string()),
                    }
                }
                RequestBody::TerminateSession { session_id } => {
                    kind = "terminate_session";
                    match sessions.terminate(&session_id) {
                        Ok(()) => Response::ok(),
                        Err(e) => Response::err(e.to_string()),
                    }
                }
                RequestBody::Subscribe { lagged_markers } => {
                    kind = "subscribe";
                    let stream_proto = negotiated.unwrap_or(PROTOCOL_VERSION);
                    // Stream takeover. Send ack, then write transitions
                    // until the client disconnects or muxad shuts down.
                    // We deliberately do NOT return to the request loop
                    // — this connection is now owned by the streaming
                    // pump.
                    let ack_bytes = encode_line(&Response::ok(), stream_proto)?;
                    if !write_line_or_closed(&mut writer, &ack_bytes).await? {
                        return Ok(());
                    }
                    tracing::debug!(
                        elapsed_us =
                            u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                        kind,
                        "ipc.handle (stream takeover)",
                    );
                    return stream_transitions(writer, store, stream_proto, lagged_markers).await;
                }
            },
            Err(e) => {
                kind = "parse_error";
                Response::err(format!("bad request: {e}"))
            }
        };

        let bytes = encode_line(&resp, negotiated.unwrap_or(PROTOCOL_VERSION))?;
        if !write_line_or_closed(&mut writer, &bytes).await? {
            return Ok(());
        }

        // Per-message timing. `debug!` so it's filtered out by default
        // (production: `info`); the field-style call defers any
        // formatting until the subscriber actually wants the line.
        tracing::debug!(
            elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            kind,
            ok = resp.ok,
            "ipc.handle",
        );
    }
}

/// True for the "too many open files" family of `accept()` errors:
/// `EMFILE` (this process hit its fd limit) or `ENFILE` (system-wide table
/// full). Both are transient — shedding load frees descriptors — so the
/// accept loop backs off and retries rather than treating them as fatal.
fn is_fd_exhaustion(e: &std::io::Error) -> bool {
    // `ErrorKind` has no stable variant for either, so match the raw errno.
    // EMFILE = 24, ENFILE = 23 on both Linux and macOS.
    matches!(e.raw_os_error(), Some(24 | 23))
}

/// After `UnixListener::bind`, chmod the path so only the owner can connect.
pub fn harden_permissions(socket_path: &Path) -> std::io::Result<()> {
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(socket_path, perms)
}

/// Client-side helper. Single-shot request/response.
#[derive(Debug, Clone)]
pub struct Client {
    socket_path: PathBuf,
    collaboration_client_kind: CollaborationClientKind,
}

/// Identity and feature information returned by the daemon's `hello` method.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub capabilities: Vec<String>,
    pub generation: Option<u64>,
}

/// The result of a [`Client::send_prompt`]: the two non-atomic keystroke
/// injections (the text, then the optional submit CR) reported distinctly.
///
/// Only produced on `Ok`, i.e. when the text landed. `submitted` is `false`
/// either because `submit:false` was requested (nothing to submit) or because
/// a requested submit CR failed after the text landed — a **partial failure**
/// the caller distinguishes using its own `submit` intent. Either way, when
/// this value exists the text is already in the pane and MUST NOT be resent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendPromptOutcome {
    /// The text injection landed. Always `true` here (a failed text send is an
    /// `Err`); carried explicitly for symmetry and forward-compatibility.
    pub sent: bool,
    /// The submit carriage return landed and committed the line.
    pub submitted: bool,
}

/// Long-lived handle returned by [`Client::subscribe`]. Calls to
/// [`Self::recv`] yield successive `Transition`s as they happen on
/// the daemon. Returns `Ok(None)` when the daemon closes the
/// connection (shutdown) or `Err(_)` on a parse / IO failure that
/// the caller will probably want to handle by reconnecting.
pub struct TransitionStream {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    line: String,
}

/// Long-lived compact invalidation stream returned by
/// [`Client::fleet_subscribe`]. Callers fetch a coherent snapshot after
/// coalescing one or more updates.
pub struct FleetUpdateStream {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    line: String,
}

/// Whether a subscribe-stream line is the daemon's `lagged` control marker
/// (`{"event":"lagged",…}`) rather than a `Transition`. Kept cheap: a real
/// `Transition` is tagged by `from`/`to`, never an `event` field, so a
/// single parse-and-check disambiguates without a speculative `Transition`
/// deserialize.
fn is_lagged_marker(line: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| {
            v.get("event")
                .and_then(serde_json::Value::as_str)
                .map(|e| e == "lagged")
        })
        .unwrap_or(false)
}

fn decode_agents(resp: &serde_json::Value) -> Vec<Agent> {
    resp["agents"]
        .as_array()
        .cloned()
        .map(|v| serde_json::from_value(serde_json::Value::Array(v)).unwrap_or_default())
        .unwrap_or_default()
}

impl TransitionStream {
    /// Wait for and return the next streamed `Transition`.
    ///
    /// Blank lines are the daemon's keepalive frames (a bare newline it emits
    /// on an idle stream to detect dead clients); they carry no payload, so we
    /// skip them and keep waiting for the next real transition.
    pub async fn recv(&mut self) -> Result<Option<crate::state::Transition>, RuntimeError> {
        loop {
            self.line.clear();
            let n = read_limited_line(&mut self.reader, &mut self.line).await?;
            if n == 0 {
                return Ok(None);
            }
            let trimmed = self.line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // The daemon interleaves non-transition control frames on this
            // stream: a bare newline keepalive (handled above) and a lagged
            // marker (`{"event":"lagged","dropped":N}`) after a broadcast
            // overflow. Skip the lagged marker — the caller reconciles holes
            // via its fallback snapshot poll — so it never reaches the
            // `Transition` deserializer below.
            if is_lagged_marker(trimmed) {
                tracing::debug!("subscribe stream lagged; skipping marker");
                continue;
            }
            let t: crate::state::Transition = serde_json::from_str(trimmed)?;
            return Ok(Some(t));
        }
    }
}

impl FleetUpdateStream {
    pub async fn recv(&mut self) -> Result<Option<FleetUpdate>, RuntimeError> {
        loop {
            self.line.clear();
            let n = read_limited_line(&mut self.reader, &mut self.line).await?;
            if n == 0 {
                return Ok(None);
            }
            let trimmed = self.line.trim();
            if trimmed.is_empty() {
                continue;
            }
            return serde_json::from_str(trimmed)
                .map(Some)
                .map_err(RuntimeError::Json);
        }
    }
}

impl Client {
    pub fn new(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            collaboration_client_kind: CollaborationClientKind::Unknown,
        }
    }

    /// Label this client for collaboration provenance. This changes audit
    /// metadata only; it grants and removes no authority.
    #[must_use]
    pub fn with_collaboration_client_kind(mut self, kind: CollaborationClientKind) -> Self {
        self.collaboration_client_kind = kind;
        self
    }

    pub async fn ingest(&self, event: &AgentEvent) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ingest",
            "event": event
        });
        // Hook ingest is on the agent's critical path — use the tighter
        // deadline so a wedged daemon fails fast (the caller treats any error
        // as a best-effort no-op) instead of stalling the agent.
        let _ = self.call_with_timeout(&req, HOOK_CALL_TIMEOUT).await?;
        Ok(())
    }

    /// Push a wholesale pane snapshot to the daemon. The zellij
    /// WASM-plugin bridge calls this to forward `PaneUpdate` events; the
    /// daemon hands the panes to its `SharedBackend::ingest_pane_snapshot`.
    pub async fn push_pane_snapshot(&self, panes: &[PaneInfo]) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "backend_pane_snapshot",
            "panes": panes,
        });
        let _ = self.call(&req).await?;
        Ok(())
    }

    pub async fn snapshot(&self) -> Result<Vec<Agent>, RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "snapshot" });
        let resp = self.call(&req).await?;
        Ok(decode_agents(&resp))
    }

    /// Read the central physical-host cache. This is a local Unix-socket
    /// operation; SSH collection runs continuously in muxad's `FleetManager`.
    pub async fn fleet_snapshot(
        &self,
        selector: Option<&str>,
    ) -> Result<FleetSnapshot, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "fleet_snapshot",
            "selector": selector,
        });
        let response = self.call_checked(&req).await?;
        serde_json::from_value(response["fleet"].clone()).map_err(RuntimeError::Json)
    }

    /// Subscribe to compact Fleet cache invalidations. The stream carries no
    /// remote terminal contents and grants no additional authority; callers
    /// fetch a normal selector-filtered snapshot after coalescing updates.
    pub async fn fleet_subscribe(
        &self,
        selector: Option<&str>,
    ) -> Result<FleetUpdateStream, RuntimeError> {
        tokio::time::timeout(CLIENT_CALL_TIMEOUT, self.fleet_subscribe_inner(selector))
            .await
            .map_err(|_| RuntimeError::Timeout(CLIENT_CALL_TIMEOUT))?
    }

    async fn fleet_subscribe_inner(
        &self,
        selector: Option<&str>,
    ) -> Result<FleetUpdateStream, RuntimeError> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound => {
                    RuntimeError::NotConnected(self.socket_path.clone())
                }
                _ => RuntimeError::Io(error),
            })?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        self.send_hello(&mut reader, &mut writer).await?;

        let mut request = serde_json::to_vec(&serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "fleet_subscribe",
            "selector": selector,
        }))?;
        request.push(b'\n');
        writer.write_all(&request).await?;
        writer.flush().await?;

        let mut ack = String::new();
        read_limited_line(&mut reader, &mut ack).await?;
        let ack: serde_json::Value = serde_json::from_str(ack.trim())?;
        if !ack["ok"].as_bool().unwrap_or(false) {
            return Err(RuntimeError::Json(serde::de::Error::custom(format!(
                "fleet subscribe rejected: {}",
                ack["error"].as_str().unwrap_or("(no error message)")
            ))));
        }
        drop(writer);
        Ok(FleetUpdateStream {
            reader,
            line: String::new(),
        })
    }

    /// Execute an exact operation on one configured host. Mutations are
    /// authorized again by the manager's per-host access mode.
    pub async fn fleet_execute(
        &self,
        host: &str,
        operation: &FleetOperation,
    ) -> Result<FleetCommandResult, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "fleet_command",
            "host": host,
            "operation": operation,
        });
        let response = self
            .call_with_timeout(&req, Duration::from_secs(25))
            .await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            return Err(RuntimeError::Json(serde::de::Error::custom(
                response["error"]
                    .as_str()
                    .unwrap_or("fleet command failed")
                    .to_string(),
            )));
        }
        serde_json::from_value(response["fleet_result"].clone()).map_err(RuntimeError::Json)
    }

    /// Ask the daemon which additive features it supports and, when it can
    /// self-restart, which process-image generation is currently serving.
    pub async fn hello(&self, deadline: Duration) -> Result<Hello, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "hello",
            "client": self.collaboration_client_kind.hello_label(),
        });
        let resp = self.call_with_timeout(&req, deadline).await?;
        if !resp["ok"].as_bool().unwrap_or(false) {
            return Err(RuntimeError::Json(serde::de::Error::custom(format!(
                "hello rejected: {}",
                resp["error"].as_str().unwrap_or("(no error message)")
            ))));
        }
        Ok(Hello {
            capabilities: resp["capabilities"]
                .as_array()
                .map(|capabilities| {
                    capabilities
                        .iter()
                        .filter_map(|capability| capability.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default(),
            generation: resp["generation"].as_u64(),
        })
    }

    /// Ask the daemon on this socket to drain and re-exec itself. Acceptance
    /// is not completion; callers confirm completion by waiting for `hello`'s
    /// generation to advance.
    pub async fn restart(&self, deadline: Duration) -> Result<(), RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "restart" });
        let resp = self.call_with_timeout(&req, deadline).await?;
        if !resp["ok"].as_bool().unwrap_or(false) {
            return Err(RuntimeError::Json(serde::de::Error::custom(format!(
                "restart rejected: {}",
                resp["error"].as_str().unwrap_or("(no error message)")
            ))));
        }
        Ok(())
    }

    /// Ask the daemon to delete fully orphaned rows (no pane, surface, or
    /// pid) idle longer than `max_age`. `max_age = Duration::ZERO` removes
    /// every orphan regardless of age. Returns the number removed. Backs
    /// `muxa prune`.
    pub async fn prune(&self, max_age: Duration) -> Result<usize, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "prune",
            "max_age_secs": max_age.as_secs(),
        });
        let resp = self.call(&req).await?;
        Ok(usize::try_from(resp["pruned"].as_u64().unwrap_or(0)).unwrap_or(usize::MAX))
    }

    /// Inject `text` into `pane` via the daemon, resolving the backend
    /// from the pane-id namespace. When `submit`, the daemon follows the
    /// text with a carriage return so the agent's line is committed.
    ///
    /// `Ok` means the **text landed** — the returned [`SendPromptOutcome`]
    /// reports the two non-atomic injections distinctly (`sent` / `submitted`)
    /// so a caller can tell a partial failure (text in, Enter not) from a total
    /// one and must NOT resend the text on the former. `Err` means nothing
    /// landed (structured refusal — unavailable namespace / unsupported backend
    /// — or a failed text send), so the whole send is safe to retry. Backs
    /// `muxa mcp`'s `muxa_send_prompt`.
    pub async fn send_prompt(
        &self,
        pane: &str,
        text: &str,
        submit: bool,
    ) -> Result<SendPromptOutcome, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "send_prompt",
            "pane": pane,
            "text": text,
            "submit": submit,
        });
        let resp = self.call_checked(&req).await?;
        Ok(SendPromptOutcome {
            // On `Ok` the text landed; default to `true` for forward-compat
            // with a daemon that predates the explicit field.
            sent: resp["sent"].as_bool().unwrap_or(true),
            // Absent field (older daemon) → fall back to the requested intent.
            submitted: resp["submitted"].as_bool().unwrap_or(submit),
        })
    }

    /// Capture the visible contents of `pane` through the daemon's
    /// namespace-resolved backend. Returns `None` when the pane is gone or
    /// the backend can't capture. Backs `muxa mcp`'s `muxa_capture_pane`.
    pub async fn capture(&self, pane: &str) -> Result<Option<String>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "capture",
            "pane": pane,
        });
        let resp = self.call_checked(&req).await?;
        Ok(resp["capture"].as_str().map(str::to_owned))
    }

    pub async fn collaboration_context(
        &self,
        origin: &CollaborationOrigin,
    ) -> Result<RoomContext, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "collaboration_context",
            "origin": origin,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["room"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn collaboration_set_identity(
        &self,
        origin: &CollaborationOrigin,
        alias: Option<&str>,
        roles: &[String],
    ) -> Result<RoomContext, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "collaboration_set_identity",
            "origin": origin,
            "alias": alias,
            "roles": roles,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["room"].clone()).map_err(RuntimeError::Json)
    }

    /// Queue a headless question; the returned entry is `Running`.
    pub async fn ask_send(&self, prompt: &str) -> Result<AskEntry, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ask_send",
            "prompt": prompt,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["ask_entry"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn ask_list(&self) -> Result<Vec<AskEntry>, RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "ask_list" });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["ask_entries"].clone()).map_err(RuntimeError::Json)
    }

    /// Read the selected agent (`None`) or switch to another (`Some`).
    pub async fn ask_agent(&self, agent: Option<&str>) -> Result<String, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ask_agent",
            "agent": agent,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["ask_agent"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn ask_reset(&self) -> Result<(), RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "ask_reset" });
        self.call_checked(&req).await.map(|_| ())
    }

    /// Delete completed ask history while leaving active work and the current
    /// per-agent conversation ids intact. Returns the number removed.
    pub async fn ask_clear(&self) -> Result<usize, RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "ask_clear" });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["pruned"].clone()).map_err(RuntimeError::Json)
    }

    /// Delete one completed ask history entry. Returns whether an entry was
    /// removed; running and unknown ids return `false`.
    pub async fn ask_delete(&self, id: &str) -> Result<bool, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ask_delete",
            "id": id,
        });
        let resp = self.call_checked(&req).await?;
        let removed: usize =
            serde_json::from_value(resp["pruned"].clone()).map_err(RuntimeError::Json)?;
        Ok(removed == 1)
    }

    pub async fn collaboration_send(
        &self,
        origin: &CollaborationOrigin,
        target: &str,
        request: &NewRequest,
    ) -> Result<CollaborationRequest, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "collaboration_send",
            "origin": origin,
            "target": target,
            "request": request,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["collaboration_request"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn collaboration_inbox(
        &self,
        origin: &CollaborationOrigin,
    ) -> Result<Vec<CollaborationRequest>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "collaboration_inbox",
            "origin": origin,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["collaboration_requests"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn collaboration_list(
        &self,
        origin: &CollaborationOrigin,
        mailbox: RequestMailbox,
    ) -> Result<Vec<CollaborationRequest>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "collaboration_list",
            "origin": origin,
            "mailbox": mailbox,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["collaboration_requests"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn collaboration_reply(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
        status: RequestStatus,
        body: &str,
        artifacts: &[String],
        air_artifacts: &[AirArtifactReference],
    ) -> Result<CollaborationRequest, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "collaboration_reply",
            "origin": origin,
            "request_id": request_id,
            "status": status,
            "body": body,
            "artifacts": artifacts,
            "air_artifacts": air_artifacts,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["collaboration_request"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn collaboration_get(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
    ) -> Result<CollaborationRequest, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "collaboration_get",
            "origin": origin,
            "request_id": request_id,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["collaboration_request"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn collaboration_cancel(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
    ) -> Result<CollaborationRequest, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "collaboration_cancel",
            "origin": origin,
            "request_id": request_id,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["collaboration_request"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn snapshot_with_timeout(
        &self,
        deadline: Duration,
    ) -> Result<Vec<Agent>, RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "snapshot" });
        let resp = self.call_with_timeout(&req, deadline).await?;
        Ok(decode_agents(&resp))
    }

    pub async fn by_pane(&self, pane: &str) -> Result<Vec<Agent>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "by_pane",
            "pane": pane
        });
        let resp = self.call(&req).await?;
        Ok(decode_agents(&resp))
    }

    pub async fn by_pane_with_timeout(
        &self,
        pane: &str,
        deadline: Duration,
    ) -> Result<Vec<Agent>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "by_pane",
            "pane": pane
        });
        let resp = self.call_with_timeout(&req, deadline).await?;
        Ok(decode_agents(&resp))
    }

    /// [`Self::recent_prompts`] under an explicit deadline, for callers on
    /// a redraw budget. The daemon serves this from an in-memory deque, so
    /// the deadline guards against a wedged daemon rather than a slow read.
    pub async fn recent_prompts_with_timeout(
        &self,
        pane: Option<&str>,
        limit: Option<usize>,
        deadline: Duration,
    ) -> Result<Vec<crate::history::HistoryEntry>, RuntimeError> {
        let mut req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "recent_prompts",
        });
        if let Some(p) = pane {
            req["pane"] = serde_json::Value::String(p.to_string());
        }
        if let Some(l) = limit {
            req["limit"] = serde_json::Value::from(l);
        }
        let resp = self.call_with_timeout(&req, deadline).await?;
        Ok(resp["prompts"]
            .as_array()
            .cloned()
            .map(|v| serde_json::from_value(serde_json::Value::Array(v)).unwrap_or_default())
            .unwrap_or_default())
    }

    pub async fn by_surface(&self, surface_id: &str) -> Result<Vec<Agent>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "by_surface",
            "surface_id": surface_id
        });
        let resp = self.call(&req).await?;
        Ok(resp["agents"]
            .as_array()
            .cloned()
            .map(|v| serde_json::from_value(serde_json::Value::Array(v)).unwrap_or_default())
            .unwrap_or_default())
    }

    /// Query the daemon's prompt history. `pane = None` returns prompts
    /// across every tracked pane (newest first); otherwise filters to
    /// one pane. `limit = None` or 0 returns everything available.
    pub async fn recent_prompts(
        &self,
        pane: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<crate::history::HistoryEntry>, RuntimeError> {
        let mut req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "recent_prompts",
        });
        if let Some(p) = pane {
            req["pane"] = serde_json::Value::String(p.to_string());
        }
        if let Some(l) = limit {
            req["limit"] = serde_json::Value::from(l);
        }
        let resp = self.call(&req).await?;
        Ok(resp["prompts"]
            .as_array()
            .cloned()
            .map(|v| serde_json::from_value(serde_json::Value::Array(v)).unwrap_or_default())
            .unwrap_or_default())
    }

    /// Open a long-lived subscription to state transitions. Returns
    /// a stream-like handle whose `recv()` yields the next
    /// `Transition` from the daemon, or `None` when the daemon
    /// closes the connection (shutdown).
    ///
    /// Designed for `muxa watch` to drop polling latency from 500 ms
    /// to ~1 ms while keeping a slower fallback poll for catch-up
    /// after reconnects or `Lagged` drops on the server side.
    pub async fn subscribe(&self) -> Result<TransitionStream, RuntimeError> {
        // Only the handshake is bounded — the returned stream is long-lived by
        // design. A wedged daemon must not block watch's background setup here.
        tokio::time::timeout(CLIENT_CALL_TIMEOUT, self.subscribe_inner())
            .await
            .map_err(|_| RuntimeError::Timeout(CLIENT_CALL_TIMEOUT))?
    }

    async fn subscribe_inner(&self) -> Result<TransitionStream, RuntimeError> {
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound => {
                    RuntimeError::NotConnected(self.socket_path.clone())
                }
                _ => RuntimeError::Io(e),
            })?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        self.send_hello(&mut reader, &mut writer).await?;

        let mut req = serde_json::to_vec(&serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "subscribe",
            // muxa's `TransitionStream::recv` understands the lagged marker
            // frame, so opt in — `muxa watch` and `muxa mcp`'s
            // `muxa_wait_for_change` both consume the stream through this
            // client and want the explicit overflow signal.
            "lagged_markers": true,
        }))?;
        req.push(b'\n');
        if req.len() > MAX_IPC_LINE_BYTES {
            return Err(RuntimeError::MessageTooLarge(MAX_IPC_LINE_BYTES));
        }
        writer.write_all(&req).await?;
        writer.flush().await?;

        // Server replies with a one-shot ack before the streaming
        // pump takes over.
        let mut ack = String::new();
        read_limited_line(&mut reader, &mut ack).await?;
        let ack: serde_json::Value = serde_json::from_str(ack.trim())?;
        if !ack["ok"].as_bool().unwrap_or(false) {
            return Err(RuntimeError::Json(serde::de::Error::custom(format!(
                "subscribe rejected: {}",
                ack["error"].as_str().unwrap_or("(no error message)")
            ))));
        }

        // Drop the writer immediately — we never send another byte
        // on this connection. The server detects our close-when-done
        // via EOF on its read half.
        drop(writer);
        Ok(TransitionStream {
            reader,
            line: String::new(),
        })
    }

    pub async fn spawn_session(
        &self,
        spawn: crate::session::SpawnSession,
    ) -> Result<SessionRef, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "spawn_session",
            "command": spawn.command,
            "args": spawn.args,
            "env": spawn.env,
            "cwd": spawn.cwd,
            "name": spawn.name,
            "cols": spawn.cols,
            "rows": spawn.rows,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["session"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionRef>, RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "list_sessions" });
        let resp = self.call_checked(&req).await?;
        Ok(resp["sessions"]
            .as_array()
            .cloned()
            .map(|v| serde_json::from_value(serde_json::Value::Array(v)).unwrap_or_default())
            .unwrap_or_default())
    }

    pub async fn capture_session(
        &self,
        session_id: &str,
    ) -> Result<TerminalSnapshot, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "capture_session",
            "session_id": session_id,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["terminal"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn read_session(
        &self,
        session_id: &str,
        offset: u64,
    ) -> Result<SessionOutput, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "read_session",
            "session_id": session_id,
            "offset": offset,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["output"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn write_session(&self, session_id: &str, data: &str) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "write_session",
            "session_id": session_id,
            "data": data,
        });
        let _ = self.call_checked(&req).await?;
        Ok(())
    }

    pub async fn resize_session(
        &self,
        session_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "resize_session",
            "session_id": session_id,
            "cols": cols,
            "rows": rows,
        });
        let _ = self.call_checked(&req).await?;
        Ok(())
    }

    pub async fn set_session_attached(
        &self,
        session_id: &str,
        attached: bool,
    ) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "set_session_attached",
            "session_id": session_id,
            "attached": attached,
        });
        let _ = self.call_checked(&req).await?;
        Ok(())
    }

    pub async fn terminate_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "terminate_session",
            "session_id": session_id,
        });
        let _ = self.call_checked(&req).await?;
        Ok(())
    }

    /// Register an arbitrary background process as a pid-tracked `Task` row.
    /// Backs the `muxa register` CLI.
    pub async fn register(
        &self,
        name: &str,
        pid: Option<u32>,
        cwd: Option<&str>,
        pane: Option<&str>,
        command: Option<&str>,
    ) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "register",
            "name": name,
            "pid": pid,
            "cwd": cwd,
            "pane": pane,
            "command": command,
        });
        let _ = self.call_checked(&req).await?;
        Ok(())
    }

    /// Send the capability handshake as the first message on a freshly
    /// opened connection. Best-effort: a daemon that doesn't understand
    /// `hello` (older build) returns `ok:false` with a parse error or a
    /// protocol-mismatch error; we ignore the failure so the legacy
    /// strict-match path on the daemon stays usable.
    async fn send_hello<R, W>(
        &self,
        reader: &mut BufReader<R>,
        writer: &mut W,
    ) -> Result<(), RuntimeError>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut bytes = serde_json::to_vec(&serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "hello",
            "client": self.collaboration_client_kind.hello_label(),
        }))?;
        bytes.push(b'\n');
        if bytes.len() > MAX_IPC_LINE_BYTES {
            return Err(RuntimeError::MessageTooLarge(MAX_IPC_LINE_BYTES));
        }
        writer.write_all(&bytes).await?;
        writer.flush().await?;
        let mut ack = String::new();
        read_limited_line(reader, &mut ack).await?;
        // Parse but don't fail on a non-ok response — legacy daemons
        // will reject the unknown `kind`, which is fine; the caller's
        // request still goes through.
        let _ = serde_json::from_str::<serde_json::Value>(ack.trim());
        Ok(())
    }

    pub async fn call(&self, req: &serde_json::Value) -> Result<serde_json::Value, RuntimeError> {
        self.call_with_timeout(req, CLIENT_CALL_TIMEOUT).await
    }

    /// Like [`Self::call`] but with an explicit overall deadline covering the
    /// whole round trip (connect + hello + write + read). Guarantees no caller
    /// blocks forever against a wedged or half-dead daemon — the failure mode
    /// where hung hook connections once exhausted the daemon's fd budget.
    async fn call_with_timeout(
        &self,
        req: &serde_json::Value,
        deadline: Duration,
    ) -> Result<serde_json::Value, RuntimeError> {
        tokio::time::timeout(deadline, self.call_inner(req))
            .await
            .map_err(|_| RuntimeError::Timeout(deadline))?
    }

    async fn call_inner(&self, req: &serde_json::Value) -> Result<serde_json::Value, RuntimeError> {
        // Connect-time ECONNREFUSED/ENOENT mean the daemon socket isn't there
        // or nothing is listening — surface a friendly message that names the
        // socket path. Other IO errors (timeouts, permission denied, …) keep
        // their existing display via the `Io(#[from] _)` impl.
        let stream = UnixStream::connect(&self.socket_path)
            .await
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound => {
                    RuntimeError::NotConnected(self.socket_path.clone())
                }
                _ => RuntimeError::Io(e),
            })?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);

        self.send_hello(&mut reader, &mut writer).await?;

        let mut bytes = serde_json::to_vec(req)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_IPC_LINE_BYTES {
            return Err(RuntimeError::MessageTooLarge(MAX_IPC_LINE_BYTES));
        }
        writer.write_all(&bytes).await?;
        writer.flush().await?;

        let mut line = String::new();
        read_limited_line(&mut reader, &mut line).await?;
        Ok(serde_json::from_str(line.trim())?)
    }

    async fn call_checked(
        &self,
        req: &serde_json::Value,
    ) -> Result<serde_json::Value, RuntimeError> {
        let resp = self.call(req).await?;
        if resp["ok"].as_bool().unwrap_or(false) {
            Ok(resp)
        } else {
            Err(RuntimeError::Json(serde::de::Error::custom(
                resp["error"]
                    .as_str()
                    .unwrap_or("request failed")
                    .to_string(),
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, AgentId, AgentKind, AgentState};
    use crate::state::Store;
    use std::collections::HashMap;
    use tempfile::tempdir;
    use time::OffsetDateTime;

    struct CollaborationTestBackend {
        panes: Vec<PaneInfo>,
    }

    impl PaneBackend for CollaborationTestBackend {
        fn kind(&self) -> HostKind {
            HostKind::Tmux
        }

        fn list_panes(&self) -> Vec<PaneInfo> {
            self.panes.clone()
        }

        fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo> {
            self.panes
                .iter()
                .find(|pane| pane.pane_id == pane_id)
                .cloned()
        }

        fn capture_pane(&self, _pane_id: &str) -> Option<String> {
            None
        }

        fn pane_pid_map(&self) -> HashMap<u32, String> {
            HashMap::new()
        }

        fn current_pane(&self) -> Option<String> {
            self.panes.first().map(|pane| pane.pane_id.clone())
        }

        fn focus_pane(&self, _pane_id: &str) -> bool {
            true
        }

        fn caps(&self) -> BackendCaps {
            BackendCaps::default()
        }
    }

    fn collaboration_test_pane(pane_id: &str, pane_index: &str) -> PaneInfo {
        PaneInfo {
            pane_id: pane_id.into(),
            session_id: "$1".into(),
            session: "collaboration".into(),
            window_id: "@1".into(),
            window_name: "agents".into(),
            window_index: "0".into(),
            pane_index: pane_index.into(),
            tty: String::new(),
            current_command: "agent".into(),
            title: String::new(),
            current_path: "/repo".into(),
            pane_pid: 0,
            socket: Some("default".into()),
        }
    }

    async fn add_collaboration_agent(
        store: &SharedStore,
        pane: &str,
        session_id: &str,
        kind: AgentKind,
    ) {
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind,
                    session_id: session_id.into(),
                    surface: None,
                    pane: Some(pane.into()),
                    tmux_socket: Some("default".into()),
                    cwd: Some("/repo".into()),
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;
    }

    #[test]
    fn caller_pane_mismatch_is_audit_evidence_not_a_refusal() {
        let actor = CollaborationConnectionActor {
            client_kind: CollaborationClientKind::Cli,
            caller_pid: Some(77),
            caller_uid: Some(1000),
            caller_gid: Some(1000),
            executable: Some("muxa".into()),
            observed_pane: Some("%9".into()),
            pane_evidence: Some(CollaborationPaneEvidence::ProcessEnvironment),
            pane_observed: true,
        };
        let provenance = actor.provenance(&CollaborationOrigin {
            pane: "%1".into(),
            socket: Some("default".into()),
            console: false,
        });
        assert_eq!(
            provenance.origin_match,
            CollaborationOriginMatch::Mismatched
        );
        assert_eq!(provenance.observed_pane.as_deref(), Some("%9"));
    }

    #[tokio::test]
    async fn collaboration_round_trip_over_ipc_tracks_reply_and_cancellation() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-collaboration.sock");
        let store = Store::shared();
        add_collaboration_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_collaboration_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        add_collaboration_agent(&store, "%3", "verifier", AgentKind::GeminiCli).await;
        let backend: SharedBackend = Arc::new(CollaborationTestBackend {
            panes: vec![
                collaboration_test_pane("%1", "0"),
                collaboration_test_pane("%2", "1"),
                collaboration_test_pane("%3", "2"),
            ],
        });
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let audit = CollaborationAuditLog::in_memory();
        let server = Server::new(sock.clone(), store)
            .with_backends(vec![backend])
            .with_collaboration(mailbox.clone())
            .with_collaboration_audit(audit.clone());
        let (tx, rx) = broadcast::channel(1);
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&sock).await;

        let client =
            Client::new(sock).with_collaboration_client_kind(CollaborationClientKind::Watch);
        let sender = CollaborationOrigin {
            pane: "%1".into(),
            socket: Some("default".into()),
            console: false,
        };
        let recipient = CollaborationOrigin {
            pane: "%2".into(),
            socket: Some("default".into()),
            console: false,
        };
        let verifier = CollaborationOrigin {
            pane: "%3".into(),
            socket: Some("default".into()),
            console: false,
        };
        client
            .collaboration_set_identity(
                &recipient,
                Some("reviewer"),
                &["review".into(), "rust".into()],
            )
            .await
            .unwrap();
        assert!(client
            .collaboration_set_identity(&verifier, Some("reviewer"), &[])
            .await
            .is_err());
        client
            .collaboration_set_identity(&verifier, Some("verifier"), &["review".into()])
            .await
            .unwrap();
        let room = client.collaboration_context(&sender).await.unwrap();
        assert_eq!(room.peers.len(), 2);
        assert_eq!(
            room.peers
                .iter()
                .find(|peer| peer.pane == "%2")
                .unwrap()
                .alias
                .as_deref(),
            Some("reviewer")
        );
        assert!(client
            .collaboration_send(
                &sender,
                "role:review",
                &NewRequest {
                    kind: collaboration::RequestKind::Question,
                    body: "ambiguous".into(),
                    expects_reply: true,
                    work_mode: collaboration::WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .is_err());

        let request = client
            .collaboration_send(
                &sender,
                "@reviewer",
                &NewRequest {
                    kind: collaboration::RequestKind::Review,
                    body: "review this".into(),
                    expects_reply: true,
                    work_mode: collaboration::WorkMode::ReadOnly,
                    paths: vec!["src/**".into()],
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();
        let provenance = request.provenance.as_ref().unwrap();
        assert_eq!(provenance.client_kind, CollaborationClientKind::Watch);
        assert_eq!(provenance.caller_pid, Some(std::process::id()));
        assert_eq!(
            provenance.origin_match,
            CollaborationOriginMatch::Unverifiable
        );
        assert_eq!(
            client
                .collaboration_context(&recipient)
                .await
                .unwrap()
                .unread,
            1
        );
        let inbox = client.collaboration_inbox(&recipient).await.unwrap();
        assert_eq!(inbox[0].status, RequestStatus::Claimed);
        client
            .collaboration_reply(
                &recipient,
                &request.id,
                RequestStatus::Completed,
                "looks good",
                &[],
                &[],
            )
            .await
            .unwrap();
        assert_eq!(
            client
                .collaboration_context(&sender)
                .await
                .unwrap()
                .unread_replies,
            1
        );
        let sent = client
            .collaboration_list(&sender, RequestMailbox::Sent)
            .await
            .unwrap();
        assert_eq!(sent[0].status, RequestStatus::Completed);
        assert!(sent[0].reply_notified_at.is_none());
        let observed = client
            .collaboration_get(&sender, &request.id)
            .await
            .unwrap();
        assert!(observed.reply_notified_at.is_some());
        assert!(observed.reply_read_at.is_some());
        assert_eq!(
            client
                .collaboration_context(&sender)
                .await
                .unwrap()
                .unread_replies,
            0
        );

        let queued = client
            .collaboration_send(
                &sender,
                "role:rust",
                &NewRequest {
                    kind: collaboration::RequestKind::Question,
                    body: "obsolete question".into(),
                    expects_reply: true,
                    work_mode: collaboration::WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            client
                .collaboration_cancel(&sender, &queued.id)
                .await
                .unwrap()
                .status,
            RequestStatus::Cancelled
        );
        assert!(client
            .collaboration_inbox(&recipient)
            .await
            .unwrap()
            .is_empty());

        // The console may target the pane it was opened from. That pane stays
        // audit provenance, but the represented identity must remain the
        // console rather than being inferred from the matching recipient.
        let console_request = client
            .collaboration_send(
                &CollaborationOrigin {
                    pane: "%1".into(),
                    socket: Some("default".into()),
                    console: true,
                },
                "pane:%1",
                &NewRequest {
                    kind: collaboration::RequestKind::Task,
                    body: "dispatch to launch pane".into(),
                    expects_reply: true,
                    work_mode: collaboration::WorkMode::ReadOnly,
                    paths: Vec::new(),
                    air_artifacts: Vec::new(),
                },
            )
            .await
            .unwrap();

        let audit_entries = audit.entries().await;
        assert!(audit_entries.iter().any(|entry| {
            entry.operation == CollaborationAuditOperation::Send
                && entry.request_id.as_deref() == Some(request.id.as_str())
                && entry.actor.client_kind == CollaborationClientKind::Watch
                && entry.message_bytes == Some("review this".len())
        }));
        assert!(audit_entries
            .iter()
            .any(|entry| entry.operation == CollaborationAuditOperation::Reply));
        assert!(audit_entries.iter().any(|entry| {
            entry.operation == CollaborationAuditOperation::Send
                && entry.request_id.as_deref() == Some(console_request.id.as_str())
                && entry.represented_session_id.as_deref()
                    == Some(collaboration::CONSOLE_SESSION_ID)
        }));

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn end_to_end_ingest_and_query() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-test.sock");
        let store = Store::shared();
        let server = Server::new(sock.clone(), store.clone());
        let (tx, rx) = broadcast::channel(1);

        let sock_for_server = sock.clone();
        let handle = tokio::spawn(async move {
            server.run(rx).await.unwrap();
            drop(sock_for_server);
        });

        // Wait for the socket to appear.
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let client = Client::new(sock.clone());
        client
            .ingest(&AgentEvent::Started {
                id: AgentId {
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "sess-a".into(),
                    surface: None,
                    pane: Some("%1".into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await
            .unwrap();

        let agents = client.by_pane("%1").await.unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].session_id, "sess-a");

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn subscribe_streams_transitions_to_client() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-sub.sock");
        let store = Store::shared();
        let server = Server::new(sock.clone(), store.clone());
        let (tx, rx) = broadcast::channel(1);
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Open subscription before any events fire.
        let client = Client::new(sock.clone());
        let mut stream = client.subscribe().await.expect("subscribe");

        // Drive a state transition: Started → Idle (initial).
        let id = AgentId {
            tmux_socket: None,
            kind: AgentKind::ClaudeCode,
            session_id: "sub-test".into(),
            surface: None,
            pane: Some("%9".into()),
            cwd: None,
        };
        store
            .apply(&AgentEvent::Started {
                id: id.clone(),
                at: OffsetDateTime::now_utc(),
            })
            .await;

        // Then Idle → Working via PromptSubmitted.
        store
            .apply(&AgentEvent::PromptSubmitted {
                id: id.clone(),
                prompt: "hi".into(),
                at: OffsetDateTime::now_utc(),
            })
            .await;

        // Stream should deliver both transitions in order.
        let t1 = tokio::time::timeout(std::time::Duration::from_secs(2), stream.recv())
            .await
            .expect("first transition arrives within timeout")
            .expect("recv ok")
            .expect("transition present");
        assert_eq!(t1.from, AgentState::Starting);
        assert_eq!(t1.to, AgentState::Idle);

        let t2 = tokio::time::timeout(std::time::Duration::from_secs(2), stream.recv())
            .await
            .expect("second transition arrives within timeout")
            .expect("recv ok")
            .expect("transition present");
        assert_eq!(t2.from, AgentState::Idle);
        assert_eq!(t2.to, AgentState::Working);

        // Drop the stream to close the connection, then shut down.
        drop(stream);
        tx.send(()).unwrap();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn not_connected_when_socket_missing() {
        // ENOENT path: tempdir exists but the socket file doesn't.
        let dir = tempdir().unwrap();
        let sock = dir.path().join("does-not-exist.sock");
        let client = Client::new(sock.clone());
        let err = client
            .call(&serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "snapshot" }))
            .await
            .expect_err("expected NotConnected when socket does not exist");
        match err {
            RuntimeError::NotConnected(p) => assert_eq!(p, sock),
            other => panic!("expected NotConnected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn not_connected_when_socket_is_stale_file() {
        // Stale-file path: a regular file exists at the socket path but
        // nothing is listening. On Linux, connect(2) returns ECONNREFUSED for
        // a non-socket path; `tokio` may also surface ENOTSOCK. We accept any
        // mapping into NotConnected — the user-visible behaviour is the same.
        let dir = tempdir().unwrap();
        let sock = dir.path().join("stale.sock");
        std::fs::write(&sock, b"").unwrap();
        let client = Client::new(sock.clone());
        let res = client
            .call(&serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "snapshot" }))
            .await;
        // If the platform returns a kind we don't remap (e.g. ENOTSOCK on
        // some libc), the call still errors — just not necessarily with
        // NotConnected. Only assert the friendly mapping when we got it.
        if let Err(RuntimeError::NotConnected(p)) = &res {
            assert_eq!(p, &sock);
        }
        // Either way, the call must not succeed.
        assert!(res.is_err());
    }

    /// `Server::run` must wait for in-flight handlers to finish before
    /// returning. Otherwise, an ingest landing during shutdown could
    /// call `Store::apply` *after* the snapshotter's final flush, losing
    /// the event on next restart.
    ///
    /// We exercise this by piping a slow request through a handler:
    /// fire shutdown while the handler is mid-read, then verify
    /// `server.run` returns only after the handler has finished applying
    /// its event (visible in the store snapshot).
    #[tokio::test]
    async fn shutdown_drains_in_flight_handlers_before_returning() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-drain.sock");
        let store = Store::shared();
        let server = Server::new(sock.clone(), store.clone());
        let (tx, rx) = broadcast::channel(1);

        let server_handle = tokio::spawn(server.run(rx));

        for _ in 0..50 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // Open a raw stream and write the request *header* but withhold
        // the trailing newline so the handler is stuck inside
        // `read_line`. This simulates an in-flight handler at the moment
        // shutdown lands.
        let mut stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ingest",
            "event": {
                "type": "started",
                "id": {
                    "kind": "claude_code",
                    "session_id": "drain-test",
                    "pane": "%9",
                    "cwd": null,
                },
                "at": "2026-04-28T00:00:00Z",
            },
        });
        let bytes = serde_json::to_vec(&req).unwrap();
        // Note: no trailing '\n' yet.
        stream.write_all(&bytes).await.unwrap();
        stream.flush().await.unwrap();

        // Yield to give the spawned handler a chance to enter `read_line`.
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;

        // Fire shutdown. Server stops accepting; existing handler is
        // still blocked on its read.
        tx.send(()).unwrap();

        // Now finish the request (newline) so the handler can complete,
        // then close the stream so the handler's read loop sees EOF and
        // returns. Without the close, `handle()` would happily wait for
        // a follow-up request and the drain timeout would fire.
        stream.write_all(b"\n").await.unwrap();
        stream.flush().await.unwrap();
        // Read the single response so we know the apply landed before
        // we drop the stream — this also gives the handler enough time
        // to write its reply.
        let mut response_buf = Vec::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response_buf),
        )
        .await;
        drop(stream);

        // `server.run` must wait for the handler to finish before
        // returning. The bounded timeout here is the test's deadline,
        // not the production drain timeout — we expect this to complete
        // in milliseconds.
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), server_handle)
            .await
            .expect("server.run did not return after handler finished")
            .expect("server task panicked");
        outcome.expect("server.run returned an error");

        // The drained handler must have applied its event before
        // server.run returned. If we'd returned without waiting, the
        // store could be empty and we'd race the assertion.
        let snap = store.snapshot().await;
        assert_eq!(snap.len(), 1, "drained handler must have applied event");
        assert_eq!(snap[0].session_id, "drain-test");
    }

    #[tokio::test]
    async fn client_disconnect_before_response_is_clean_handler_exit() {
        let (server_stream, mut client_stream) = tokio::net::UnixStream::pair().unwrap();
        let store = Store::shared();
        let handle = tokio::spawn(handle(
            server_stream,
            store,
            default_backend(),
            vec![default_backend()],
            PtySessionBackend::shared(),
            CollaborationStore::in_memory(CollaborationOptions::default()),
            CollaborationAuditLog::in_memory(),
            crate::ask::AskStore::in_memory(crate::ask::AskOptions::default()),
            None,
            None,
        ));

        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "snapshot",
        });
        let mut bytes = serde_json::to_vec(&req).unwrap();
        bytes.push(b'\n');
        client_stream.write_all(&bytes).await.unwrap();
        client_stream.flush().await.unwrap();
        drop(client_stream);

        let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("handler should exit promptly")
            .expect("handler task panicked");
        outcome.expect("client disconnect should not be treated as a handler failure");
    }

    #[tokio::test]
    async fn handler_budget_is_reserved_before_accepting_connections() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-budget.sock");
        let store = Store::shared();
        let server = Server::new(sock.clone(), store).with_handler_limit(1);
        let (tx, rx) = broadcast::channel(1);
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&sock).await;

        let mut holder = tokio::net::UnixStream::connect(&sock).await.unwrap();
        holder.write_all(b"{").await.unwrap();
        holder.flush().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let mut second = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "snapshot",
        });
        let mut bytes = serde_json::to_vec(&req).unwrap();
        bytes.push(b'\n');
        second.write_all(&bytes).await.unwrap();
        second.flush().await.unwrap();
        let mut reader = BufReader::new(second);
        let mut line = String::new();

        let early = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            reader.read_line(&mut line),
        )
        .await;
        assert!(
            early.is_err(),
            "server accepted a connection while no handler permit was available",
        );

        drop(holder);
        line.clear();
        let n = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            reader.read_line(&mut line),
        )
        .await
        .expect("queued connection should be served after permit is released")
        .expect("read response");
        assert!(n > 0, "queued connection closed without a response");
        let resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(resp["ok"], true);

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// Server must wait for the socket to appear before tests dial in.
    async fn wait_for_socket(sock: &Path) {
        for _ in 0..50 {
            if sock.exists() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Raw single-line request/response on a fresh connection. Bypasses
    /// `Client::call`'s built-in `hello` handshake so tests can exercise
    /// the legacy strict-match path and the negotiated downgrade path
    /// in isolation.
    async fn raw_call(sock: &Path, req: &serde_json::Value) -> serde_json::Value {
        let mut stream = tokio::net::UnixStream::connect(sock).await.unwrap();
        let mut bytes = serde_json::to_vec(req).unwrap();
        bytes.push(b'\n');
        stream.write_all(&bytes).await.unwrap();
        stream.flush().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        serde_json::from_str(line.trim()).unwrap()
    }

    /// Legacy strict-match path: a client that never sends `hello` and
    /// pins a mismatched `protocol` on a snapshot request gets the
    /// "protocol mismatch" error. Negotiation is opt-in; pre-`hello`
    /// connections keep the old behaviour.
    #[tokio::test]
    async fn rejects_wrong_protocol_without_hello() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-test.sock");
        let store = Store::shared();
        let server = Server::new(sock.clone(), store);
        let (tx, rx) = broadcast::channel(1);
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&sock).await;

        let resp = raw_call(
            &sock,
            &serde_json::json!({ "protocol": 999, "kind": "snapshot" }),
        )
        .await;
        assert_eq!(resp["ok"], false);
        assert!(resp["error"]
            .as_str()
            .unwrap()
            .contains("protocol mismatch"));

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// `hello` returns the supported protocol range and the capability
    /// tag list, and echoes the requested `protocol` back in the
    /// response envelope.
    #[tokio::test]
    async fn hello_returns_capabilities_and_negotiated_protocol() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-hello.sock");
        let store = Store::shared();
        let server = Server::new(sock.clone(), store);
        let (tx, rx) = broadcast::channel(1);
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&sock).await;

        let resp = raw_call(
            &sock,
            &serde_json::json!({
                "protocol": PROTOCOL_VERSION,
                "kind": "hello",
                "client": "muxa-test/0.0.0",
            }),
        )
        .await;
        assert_eq!(resp["ok"], true);
        assert_eq!(resp["protocol"], i64::from(PROTOCOL_VERSION));
        assert_eq!(resp["min_protocol"], i64::from(MIN_PROTOCOL_VERSION));
        assert_eq!(resp["max_protocol"], i64::from(PROTOCOL_VERSION));
        let caps: Vec<&str> = resp["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(caps.contains(&"waiting_choice"));
        assert!(caps.contains(&"needs_choice"));
        assert!(caps.contains(&"rate_limited"));
        assert!(!caps.contains(&RESTART_CAPABILITY));
        assert!(resp["generation"].is_null());

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn restart_is_advertised_accepted_and_drained() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-restart.sock");
        let store = Store::shared();
        let (tx, rx) = broadcast::channel(1);
        let restart = Arc::new(RestartController::new(7, tx));
        let server = Server::new(sock.clone(), store).with_restart_controller(Arc::clone(&restart));
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&sock).await;

        let client = Client::new(sock.clone());
        let hello = client
            .hello(Duration::from_secs(2))
            .await
            .expect("hello answers");
        assert!(hello
            .capabilities
            .iter()
            .any(|cap| cap == RESTART_CAPABILITY));
        assert_eq!(hello.generation, Some(7));

        client
            .restart(Duration::from_secs(2))
            .await
            .expect("daemon accepts restart");
        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("daemon drains after accepting restart")
            .unwrap();
        assert!(restart.restart_requested());
        assert!(!sock.exists(), "drained server removes its socket");
    }

    #[tokio::test]
    async fn signal_stop_cannot_be_rearmed_by_an_inflight_restart() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-stopping.sock");
        let store = Store::shared();
        let (tx, rx) = broadcast::channel(1);
        let restart = Arc::new(RestartController::new(0, tx));
        let server = Server::new(sock.clone(), store).with_restart_controller(Arc::clone(&restart));
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&sock).await;

        // Get a handler accepted and parked mid-request before the stop. This
        // is the exact ordering that could re-arm the old AtomicBool design.
        let mut stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let mut request = serde_json::to_vec(&serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "restart",
        }))
        .unwrap();
        request.push(b'\n');
        let split = request.len() - 1;
        stream.write_all(&request[..split]).await.unwrap();
        stream.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;

        restart.stop();
        stream.write_all(&request[split..]).await.unwrap();
        stream.flush().await.unwrap();

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let response: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(response["ok"], false);
        assert!(response["error"]
            .as_str()
            .unwrap()
            .contains("already stopping"));
        drop(reader);

        tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("normal stop drains the in-flight handler")
            .unwrap();
        assert!(
            !restart.restart_requested(),
            "an in-flight restart must not override SIGTERM/SIGINT",
        );
    }

    #[tokio::test]
    async fn restart_is_refused_without_a_controller() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-no-restart.sock");
        let store = Store::shared();
        let server = Server::new(sock.clone(), store);
        let (tx, rx) = broadcast::channel(1);
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&sock).await;

        let client = Client::new(sock.clone());
        let error = client
            .restart(Duration::from_secs(2))
            .await
            .expect_err("embedded server refuses restart");
        assert!(error.to_string().contains("restart"));
        assert!(UnixStream::connect(&sock).await.is_ok());

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// After a v1-pinned `hello`, snapshot responses must downgrade
    /// `waiting_choice` to `waiting_input` so the old client's serde
    /// deserializer doesn't fail on the unknown variant.
    #[tokio::test]
    async fn v1_hello_downgrades_waiting_choice_in_snapshot() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-v1.sock");
        let store = Store::shared();
        let server = Server::new(sock.clone(), store.clone());
        let (tx, rx) = broadcast::channel(1);
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&sock).await;

        let id = AgentId {
            tmux_socket: None,
            kind: AgentKind::ClaudeCode,
            session_id: "v1-test".into(),
            surface: None,
            pane: Some("%1".into()),
            cwd: None,
        };
        store
            .apply(&AgentEvent::Started {
                id: id.clone(),
                at: OffsetDateTime::now_utc(),
            })
            .await;
        // Drive into WaitingChoice via NeedsChoice notification.
        store
            .apply(&AgentEvent::NotificationFired {
                id: id.clone(),
                level: crate::event::NotificationLevel::NeedsChoice,
                message: "pick one".into(),
                at: OffsetDateTime::now_utc(),
            })
            .await;

        // Open one connection: hello v1, then snapshot.
        let mut stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let mut hello = serde_json::to_vec(&serde_json::json!({
            "protocol": 1, "kind": "hello", "client": "v1-test",
        }))
        .unwrap();
        hello.push(b'\n');
        stream.write_all(&hello).await.unwrap();
        let mut snap = serde_json::to_vec(&serde_json::json!({
            "kind": "snapshot",
        }))
        .unwrap();
        snap.push(b'\n');
        stream.write_all(&snap).await.unwrap();
        stream.flush().await.unwrap();

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let hello_resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(hello_resp["protocol"], 1);
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let snap_resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let agents = snap_resp["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["state"], "waiting_input");
        // The literal v2 string must not appear anywhere in the payload.
        let body = line.clone();
        assert!(
            !body.contains("waiting_choice"),
            "v1 snapshot still contains waiting_choice: {body}"
        );

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// A `task` row (v3 `AgentKind`) must be downgraded to `unknown` for a
    /// client that negotiated v2, so older `muxa status`/`watch` can still
    /// deserialize the snapshot.
    #[tokio::test]
    async fn v2_hello_downgrades_task_kind_in_snapshot() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-task-v2.sock");
        let store = Store::shared();
        let server = Server::new(sock.clone(), store.clone());
        let (tx, rx) = broadcast::channel(1);
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&sock).await;

        store
            .register_task("job".into(), Some(std::process::id()), None, None, None)
            .await
            .unwrap();

        let mut stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let mut hello = serde_json::to_vec(&serde_json::json!({
            "protocol": 2, "kind": "hello", "client": "v2-test",
        }))
        .unwrap();
        hello.push(b'\n');
        stream.write_all(&hello).await.unwrap();
        let mut snap = serde_json::to_vec(&serde_json::json!({ "kind": "snapshot" })).unwrap();
        snap.push(b'\n');
        stream.write_all(&snap).await.unwrap();
        stream.flush().await.unwrap();

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap(); // hello ack
        line.clear();
        reader.read_line(&mut line).await.unwrap(); // snapshot
        let snap_resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let agents = snap_resp["agents"].as_array().unwrap();
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["kind"], "unknown");
        assert!(
            !line.contains("\"task\""),
            "v2 snapshot still contains task kind: {line}"
        );

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// A v2-pinned `hello` keeps `waiting_choice` intact.
    #[tokio::test]
    async fn v2_hello_keeps_waiting_choice_in_snapshot() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-v2.sock");
        let store = Store::shared();
        let server = Server::new(sock.clone(), store.clone());
        let (tx, rx) = broadcast::channel(1);
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&sock).await;

        let id = AgentId {
            tmux_socket: None,
            kind: AgentKind::ClaudeCode,
            session_id: "v2-test".into(),
            surface: None,
            pane: Some("%2".into()),
            cwd: None,
        };
        store
            .apply(&AgentEvent::Started {
                id: id.clone(),
                at: OffsetDateTime::now_utc(),
            })
            .await;
        store
            .apply(&AgentEvent::NotificationFired {
                id: id.clone(),
                level: crate::event::NotificationLevel::NeedsChoice,
                message: "pick".into(),
                at: OffsetDateTime::now_utc(),
            })
            .await;

        let mut stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let mut hello = serde_json::to_vec(&serde_json::json!({
            "protocol": 2, "kind": "hello", "client": "v2-test",
        }))
        .unwrap();
        hello.push(b'\n');
        stream.write_all(&hello).await.unwrap();
        let mut snap = serde_json::to_vec(&serde_json::json!({ "kind": "snapshot" })).unwrap();
        snap.push(b'\n');
        stream.write_all(&snap).await.unwrap();
        stream.flush().await.unwrap();

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap(); // hello resp
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let snap_resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        let agents = snap_resp["agents"].as_array().unwrap();
        assert_eq!(agents[0]["state"], "waiting_choice");

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// `hello` with a protocol outside `[MIN, MAX]` is rejected without
    /// pinning the connection — the legacy strict-match remains in force
    /// for the rest of the connection.
    #[tokio::test]
    async fn hello_rejects_out_of_range_protocol() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-bad-hello.sock");
        let store = Store::shared();
        let server = Server::new(sock.clone(), store);
        let (tx, rx) = broadcast::channel(1);
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&sock).await;

        let resp = raw_call(
            &sock,
            &serde_json::json!({ "protocol": 999, "kind": "hello" }),
        )
        .await;
        assert_eq!(resp["ok"], false);
        assert!(resp["error"]
            .as_str()
            .unwrap()
            .contains("unsupported protocol"));

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// A `backend_pane_snapshot` push reaches the server's
    /// `SharedBackend`, so the zellij backend's cache — and thus
    /// `list_panes` / `resolve_pane` — reflects it. The request/response
    /// is synchronous, so once the client call returns the ingest has
    /// already run daemon-side; no sleep/poll needed.
    #[tokio::test]
    async fn backend_pane_snapshot_push_updates_shared_backend() {
        use crate::backend::zellij::ZellijBackend;
        use crate::backend::PaneBackend;
        use std::sync::Arc;

        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-backend-snap.sock");
        let store = Store::shared();
        let backend = Arc::new(ZellijBackend::new());
        let server = Server::new(sock.clone(), store).with_backend(backend.clone());
        let (tx, rx) = broadcast::channel(1);
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&sock).await;

        // No plugin push yet → empty.
        assert!(backend.list_panes().is_empty());

        let pane = PaneInfo {
            socket: None,
            pane_id: "zellij:3".into(),
            session_id: String::new(),
            session: "z".into(),
            window_id: String::new(),
            window_name: String::new(),
            window_index: "0".into(),
            pane_index: "3".into(),
            tty: String::new(),
            current_command: "claude".into(),
            title: String::new(),
            pane_pid: 0,
            current_path: String::new(),
        };
        Client::new(sock.clone())
            .push_pane_snapshot(&[pane])
            .await
            .unwrap();

        // The same Arc the test holds now sees the pushed pane.
        let panes = backend.list_panes();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, "zellij:3");
        assert_eq!(
            backend.resolve_pane("zellij:3").unwrap().current_command,
            "claude"
        );

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    // --- Control-plane routing tests (send_prompt / capture) -------------

    // `HostKind` comes in via `use super::*` (it's imported at module top).
    use crate::backend::{BackendCaps, PaneBackend};
    use std::sync::Arc;
    use std::sync::Mutex;

    /// Recorded `(pane_id, text)` injections from a `RecordingBackend`.
    type SendLog = Arc<Mutex<Vec<(String, String)>>>;
    /// Recorded per-injection `socket` argument threaded into `send_text_on`
    /// (the pane row's recorded server, or `None`).
    type SocketLog = Arc<Mutex<Vec<Option<String>>>>;

    /// A fake backend that records every `send_text_on` call — both the
    /// `(pane_id, text)` and the pinned `socket` — and answers `capture_pane`
    /// with a canned string. `has_cap` (advertised `caps().send_text`) and
    /// `send_ok` (the runtime injection result) are decoupled so we can model
    /// "backend can't inject" (refusal) separately from "backend accepted the
    /// call but the pane is gone" (runtime failure). `fail_on_cr` makes just
    /// the submit CR (`"\r"`) fail while the text send still succeeds, to
    /// exercise the partial-failure signal.
    struct RecordingBackend {
        kind: HostKind,
        has_cap: bool,
        send_ok: bool,
        fail_on_cr: bool,
        sends: SendLog,
        sockets: SocketLog,
    }

    impl RecordingBackend {
        /// The common case: a backend whose `send_text` capability and runtime
        /// result are the same bool (`true` = injects fine; `false` = no cap,
        /// so it's refused before any injection is attempted).
        fn new(kind: HostKind, can_send: bool) -> (Arc<Self>, SendLog) {
            let (backend, sends, _sockets) = Self::new_full(kind, can_send, can_send, false);
            (backend, sends)
        }

        /// Full constructor exposing the socket log and decoupled cap / runtime
        /// / CR-failure toggles.
        fn new_full(
            kind: HostKind,
            has_cap: bool,
            send_ok: bool,
            fail_on_cr: bool,
        ) -> (Arc<Self>, SendLog, SocketLog) {
            let sends = Arc::new(Mutex::new(Vec::new()));
            let sockets = Arc::new(Mutex::new(Vec::new()));
            let backend = Arc::new(Self {
                kind,
                has_cap,
                send_ok,
                fail_on_cr,
                sends: sends.clone(),
                sockets: sockets.clone(),
            });
            (backend, sends, sockets)
        }
    }

    impl PaneBackend for RecordingBackend {
        fn kind(&self) -> HostKind {
            self.kind
        }
        fn list_panes(&self) -> Vec<PaneInfo> {
            Vec::new()
        }
        fn resolve_pane(&self, _: &str) -> Option<PaneInfo> {
            None
        }
        fn capture_pane(&self, pane_id: &str) -> Option<String> {
            Some(format!("captured:{pane_id}"))
        }
        fn pane_pid_map(&self) -> std::collections::HashMap<u32, String> {
            std::collections::HashMap::new()
        }
        fn current_pane(&self) -> Option<String> {
            None
        }
        fn focus_pane(&self, _: &str) -> bool {
            false
        }
        fn send_text(&self, pane_id: &str, text: &str) -> bool {
            self.sends
                .lock()
                .unwrap()
                .push((pane_id.to_string(), text.to_string()));
            if self.fail_on_cr && text == "\r" {
                return false;
            }
            self.send_ok
        }
        fn send_text_on(&self, socket: Option<&str>, pane_id: &str, text: &str) -> bool {
            self.sockets.lock().unwrap().push(socket.map(str::to_owned));
            self.send_text(pane_id, text)
        }
        fn caps(&self) -> BackendCaps {
            BackendCaps {
                send_text: self.has_cap,
                ..BackendCaps::default()
            }
        }
    }

    async fn serve<B: PaneBackend>(
        sock: &Path,
        store: SharedStore,
        backends: Vec<Arc<B>>,
    ) -> (broadcast::Sender<()>, tokio::task::JoinHandle<()>) {
        let backends: Vec<SharedBackend> =
            backends.into_iter().map(|b| b as SharedBackend).collect();
        let server = Server::new(sock.to_path_buf(), store).with_backends(backends);
        let (tx, rx) = broadcast::channel(1);
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(sock).await;
        (tx, handle)
    }

    /// `send_prompt` routes to the backend that governs the pane's
    /// namespace and, with `submit`, follows the text with a carriage
    /// return as a second injection.
    #[tokio::test]
    async fn send_prompt_injects_text_then_submit_cr() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-send.sock");
        let (backend, sends) = RecordingBackend::new(HostKind::Tmux, true);
        let (tx, handle) = serve(&sock, Store::shared(), vec![backend]).await;

        Client::new(sock.clone())
            .send_prompt("%1", "fix the bug", true)
            .await
            .expect("send_prompt ok");

        let recorded = sends.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![
                ("%1".to_string(), "fix the bug".to_string()),
                ("%1".to_string(), "\r".to_string()),
            ],
        );

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// Without `submit`, only the text is injected — no trailing CR.
    #[tokio::test]
    async fn send_prompt_without_submit_omits_cr() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-send-nosub.sock");
        let (backend, sends) = RecordingBackend::new(HostKind::Tmux, true);
        let (tx, handle) = serve(&sock, Store::shared(), vec![backend]).await;

        Client::new(sock.clone())
            .send_prompt("%1", "note", false)
            .await
            .expect("send_prompt ok");

        assert_eq!(
            sends.lock().unwrap().clone(),
            vec![("%1".to_string(), "note".to_string())],
        );

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// A backend that lacks the `send_text` capability is refused with a
    /// structured error — never a panic, and no injection is attempted.
    #[tokio::test]
    async fn send_prompt_refused_when_backend_lacks_cap() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-send-refuse.sock");
        let (backend, sends) = RecordingBackend::new(HostKind::Zellij, false);
        let (tx, handle) = serve(&sock, Store::shared(), vec![backend]).await;

        let err = Client::new(sock.clone())
            .send_prompt("zellij:3", "hi", true)
            .await
            .expect_err("must refuse without send_text cap");
        assert!(
            format!("{err}").contains("does not support send_text"),
            "unexpected error: {err}",
        );
        assert!(
            sends.lock().unwrap().is_empty(),
            "no injection when the cap is absent",
        );

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// With a mixed backend set, `send_prompt` resolves the target by the
    /// pane-id namespace: a `herdr:` pane routes to the herdr backend even
    /// though tmux is primary (`backends[0]`).
    #[tokio::test]
    async fn send_prompt_resolves_by_pane_namespace() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-send-ns.sock");
        let (tmux, tmux_sends) = RecordingBackend::new(HostKind::Tmux, true);
        let (herdr, herdr_sends) = RecordingBackend::new(HostKind::Herdr, true);
        // tmux leads (primary); herdr trails.
        let backends: Vec<SharedBackend> = vec![tmux as SharedBackend, herdr as SharedBackend];
        let server = Server::new(sock.clone(), Store::shared()).with_backends(backends);
        let (tx, rx) = broadcast::channel(1);
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&sock).await;

        Client::new(sock.clone())
            .send_prompt("herdr:p9", "hey", false)
            .await
            .expect("send_prompt ok");

        assert!(
            tmux_sends.lock().unwrap().is_empty(),
            "tmux must not receive"
        );
        assert_eq!(
            herdr_sends.lock().unwrap().clone(),
            vec![("herdr:p9".to_string(), "hey".to_string())],
        );

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// `capture` routes to the namespace backend and returns its screen text.
    #[tokio::test]
    async fn capture_returns_backend_screen_text() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-capture.sock");
        let (backend, _sends) = RecordingBackend::new(HostKind::Tmux, true);
        let (tx, handle) = serve(&sock, Store::shared(), vec![backend]).await;

        let text = Client::new(sock.clone())
            .capture("%7")
            .await
            .expect("capture ok");
        assert_eq!(text.as_deref(), Some("captured:%7"));

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// Fix 3 — routing: a pane whose namespace classifies to a KNOWN host
    /// (`herdr:`) but whose backend is NOT in the active set is a structured
    /// refusal (`Err(kind)`), never a silent fall-through to the primary. An
    /// unclassified id still falls back to `backends[0]`.
    #[test]
    fn resolve_backend_refuses_known_but_absent_namespace() {
        let (tmux, _s) = RecordingBackend::new(HostKind::Tmux, true);
        let backends: Vec<SharedBackend> = vec![tmux as SharedBackend];

        // Known + present → the tmux backend.
        assert!(matches!(
            resolve_backend(&backends, "%5"),
            Ok(b) if b.kind() == HostKind::Tmux
        ));
        // Known (herdr:) + absent from the set → refusal carrying the kind.
        assert!(matches!(
            resolve_backend(&backends, "herdr:p1"),
            Err(HostKind::Herdr)
        ));
        // Unclassified id → fall back to the primary (backends[0]).
        assert!(matches!(
            resolve_backend(&backends, "weird-legacy-id"),
            Ok(b) if b.kind() == HostKind::Tmux
        ));
    }

    /// Fix 3 — end to end: `send_prompt` to a pane in an unobserved namespace
    /// is refused with a `namespace unavailable` error and injects nothing —
    /// it must NOT type into the tmux (primary) backend.
    #[tokio::test]
    async fn send_prompt_refused_for_unavailable_namespace() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-send-ns-absent.sock");
        let (tmux, sends) = RecordingBackend::new(HostKind::Tmux, true);
        let (tx, handle) = serve(&sock, Store::shared(), vec![tmux]).await;

        let err = Client::new(sock.clone())
            .send_prompt("herdr:p1", "hi", true)
            .await
            .expect_err("must refuse an unobserved namespace");
        assert!(
            format!("{err}").contains("namespace unavailable"),
            "unexpected error: {err}",
        );
        assert!(
            sends.lock().unwrap().is_empty(),
            "no injection when the namespace is unavailable",
        );

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// Fix 1 — the control op is pinned to the pane's RECORDED server: the
    /// daemon looks the agent row up by pane and threads its `tmux_socket`
    /// into `send_text_on` (both the text and the submit CR), so a shared
    /// pane id like `%5` reaches the right tmux server.
    #[tokio::test]
    async fn send_prompt_threads_recorded_socket() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-send-socket.sock");
        let store = Store::shared();
        // Seed an agent on pane %5 recorded against the `amux` server.
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    tmux_socket: Some("amux".into()),
                    kind: AgentKind::ClaudeCode,
                    session_id: "sock-sess".into(),
                    surface: None,
                    pane: Some("%5".into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;

        let (backend, _sends, sockets) =
            RecordingBackend::new_full(HostKind::Tmux, true, true, false);
        let (tx, handle) = serve(&sock, store, vec![backend]).await;

        Client::new(sock.clone())
            .send_prompt("%5", "go", true)
            .await
            .expect("send_prompt ok");

        // Both injections (text + CR) were pinned to the recorded socket.
        assert_eq!(
            sockets.lock().unwrap().clone(),
            vec![Some("amux".to_string()), Some("amux".to_string())],
        );

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn rmux_send_prompt_routes_namespace_and_preserves_full_endpoint() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-send-rmux.sock");
        let store = Store::shared();
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    tmux_socket: Some("/tmp/rmux-501/default".into()),
                    kind: AgentKind::ClaudeCode,
                    session_id: "rmux-sess".into(),
                    surface: None,
                    pane: Some("rmux:%5".into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;

        let (backend, sends, sockets) =
            RecordingBackend::new_full(HostKind::Rmux, true, true, false);
        let (tx, handle) = serve(&sock, store, vec![backend]).await;

        Client::new(sock.clone())
            .send_prompt("rmux:%5", "go", false)
            .await
            .expect("rmux send_prompt ok");

        assert_eq!(
            sends.lock().unwrap().clone(),
            vec![("rmux:%5".to_string(), "go".to_string())],
        );
        assert_eq!(
            sockets.lock().unwrap().clone(),
            vec![Some("/tmp/rmux-501/default".to_string())],
        );

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn rmux_send_prompt_refuses_same_pane_id_on_multiple_endpoints() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-send-rmux-ambiguous.sock");
        let store = Store::shared();
        for (session_id, endpoint) in [
            ("rmux-one", "/tmp/rmux-one/default"),
            ("rmux-two", "/tmp/rmux-two/default"),
        ] {
            store
                .apply(&AgentEvent::Started {
                    id: AgentId {
                        tmux_socket: Some(endpoint.into()),
                        kind: AgentKind::ClaudeCode,
                        session_id: session_id.into(),
                        surface: None,
                        pane: Some("rmux:%5".into()),
                        cwd: None,
                    },
                    at: OffsetDateTime::now_utc(),
                })
                .await;
        }

        let (backend, sends, _sockets) =
            RecordingBackend::new_full(HostKind::Rmux, true, true, false);
        let (tx, handle) = serve(&sock, store, vec![backend]).await;

        let error = Client::new(sock.clone())
            .send_prompt("rmux:%5", "do not misroute", false)
            .await
            .expect_err("ambiguous endpoint must be refused");
        assert!(format!("{error}").contains("multiple endpoints"));
        assert!(sends.lock().unwrap().is_empty());

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// Fix 1 — with no recorded agent row for the pane, the threaded socket is
    /// `None` (the tmux backend then falls back to the env-scoped default).
    #[tokio::test]
    async fn send_prompt_threads_none_socket_when_untracked() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-send-nosock.sock");
        let (backend, _sends, sockets) =
            RecordingBackend::new_full(HostKind::Tmux, true, true, false);
        let (tx, handle) = serve(&sock, Store::shared(), vec![backend]).await;

        Client::new(sock.clone())
            .send_prompt("%9", "hi", false)
            .await
            .expect("send_prompt ok");

        assert_eq!(sockets.lock().unwrap().clone(), vec![None]);

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// Fix 6 — honest partial-failure signal: when the text lands but the
    /// submit CR fails, the response is `ok:true` with `sent:true,
    /// submitted:false` (NOT a total failure), so a caller knows the text is
    /// already in the pane and must not resend it.
    #[tokio::test]
    async fn send_prompt_reports_partial_failure_when_cr_fails() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-send-partial.sock");
        // Text send succeeds; the `\r` submit fails.
        let (backend, sends, _sockets) =
            RecordingBackend::new_full(HostKind::Tmux, true, true, true);
        let (tx, handle) = serve(&sock, Store::shared(), vec![backend]).await;

        let outcome = Client::new(sock.clone())
            .send_prompt("%1", "hello", true)
            .await
            .expect("text landed → Ok, not a total failure");
        assert!(outcome.sent, "text landed");
        assert!(!outcome.submitted, "submit CR failed");

        // The CR WAS attempted (text-send succeeded first), and the text was
        // sent exactly once — no double-inject.
        assert_eq!(
            sends.lock().unwrap().clone(),
            vec![
                ("%1".to_string(), "hello".to_string()),
                ("%1".to_string(), "\r".to_string()),
            ],
        );

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// Fix 6 — a total text-send failure is an `Err` (nothing landed, safe to
    /// retry the whole send), and the submit CR is NOT attempted.
    #[tokio::test]
    async fn send_prompt_total_failure_skips_cr() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-send-total-fail.sock");
        // Every send fails.
        let (backend, sends, _sockets) =
            RecordingBackend::new_full(HostKind::Tmux, true, false, false);
        let (tx, handle) = serve(&sock, Store::shared(), vec![backend]).await;

        let err = Client::new(sock.clone())
            .send_prompt("%1", "hello", true)
            .await
            .expect_err("nothing landed → Err");
        assert!(format!("{err}").contains("send_text failed"), "err: {err}");
        // Only the text was attempted; the CR is skipped when the text fails.
        assert_eq!(
            sends.lock().unwrap().clone(),
            vec![("%1".to_string(), "hello".to_string())],
        );

        tx.send(()).unwrap();
        handle.await.unwrap();
    }

    /// Fix 7 — the lagged marker is gated on the connection's opt-in: an
    /// un-opted subscriber gets `None` (silently continue, pre-marker
    /// behavior), an opted-in one gets the encoded `{"event":"lagged",…}` frame.
    #[test]
    fn lagged_marker_bytes_gated_on_opt_in() {
        // Not opted in → nothing on the wire.
        assert!(lagged_marker_bytes(false, 7, PROTOCOL_VERSION)
            .unwrap()
            .is_none());
        // Opted in → a newline-terminated lagged frame carrying the drop count.
        let bytes = lagged_marker_bytes(true, 7, PROTOCOL_VERSION)
            .unwrap()
            .expect("opted-in subscriber gets the marker");
        let line = String::from_utf8(bytes).unwrap();
        assert!(line.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["event"], "lagged");
        assert_eq!(v["dropped"], 7);
    }

    /// Fix 7 — muxa's own client opts in: the `subscribe` request it sends
    /// carries `lagged_markers: true` so `muxa watch` / `muxa mcp` receive the
    /// overflow signal their `TransitionStream` reader knows how to skip.
    #[test]
    fn subscribe_request_defaults_and_opt_in_parse() {
        // Absent field → default false (a pre-marker client stays legacy).
        let req: Request = serde_json::from_str(r#"{"protocol":3,"kind":"subscribe"}"#).unwrap();
        assert!(matches!(
            req.body,
            RequestBody::Subscribe {
                lagged_markers: false
            }
        ));
        // Explicit opt-in parses through.
        let req: Request =
            serde_json::from_str(r#"{"protocol":3,"kind":"subscribe","lagged_markers":true}"#)
                .unwrap();
        assert!(matches!(
            req.body,
            RequestBody::Subscribe {
                lagged_markers: true
            }
        ));
    }

    #[test]
    fn v3_wire_downgrade_restores_legacy_agent_session_key() {
        let mut value = serde_json::json!({
            "agent": {
                "agent_session_id": "codex-session",
                "kind": "codex"
            }
        });
        downgrade_wire(&mut value, 3);
        assert_eq!(value["agent"]["session_id"], "codex-session");
        assert!(value["agent"].get("agent_session_id").is_none());
    }

    /// `is_lagged_marker` recognizes the daemon's overflow marker and
    /// rejects a normal transition line.
    #[test]
    fn lagged_marker_is_recognized() {
        assert!(is_lagged_marker(r#"{"event":"lagged","dropped":5}"#));
        assert!(!is_lagged_marker(
            r#"{"from":"idle","to":"working","agent":{}}"#
        ));
        assert!(!is_lagged_marker("not json"));
    }

    /// `TransitionStream::recv` skips a lagged marker frame and returns the
    /// next real `Transition`.
    #[tokio::test]
    async fn transition_stream_skips_lagged_marker() {
        use crate::event::{AgentEvent, AgentId, AgentKind};
        use time::OffsetDateTime;

        // Produce a real serialized Transition via a store.
        let store = Store::shared();
        let mut rx = store.subscribe();
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "lag".into(),
                    surface: None,
                    pane: Some("%1".into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;
        let transition = rx.recv().await.unwrap();
        let transition_json = serde_json::to_string(&transition).unwrap();

        // Wire the marker + transition into a TransitionStream's reader.
        let (client_side, server_side) = UnixStream::pair().unwrap();
        let (cr, _cw) = client_side.into_split();
        let mut ts = TransitionStream {
            reader: BufReader::new(cr),
            line: String::new(),
        };
        let (_sr, mut sw) = server_side.into_split();
        let mut payload = String::from(r#"{"event":"lagged","dropped":3}"#);
        payload.push('\n');
        payload.push_str(&transition_json);
        payload.push('\n');
        sw.write_all(payload.as_bytes()).await.unwrap();
        sw.flush().await.unwrap();

        let got = ts
            .recv()
            .await
            .expect("recv ok")
            .expect("a transition after the skipped marker");
        assert_eq!(got.to, transition.to);
        assert_eq!(got.agent.session_id, "lag");
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // one end-to-end flow verifies selector stream membership
    async fn fleet_snapshot_selector_and_command_round_trip_over_ipc() {
        use crate::fleet::{
            FleetHostSnapshot, FleetHostState, FleetOperation, FleetRuntime, FleetStore,
            HostAccessMode, FLEET_PROTOCOL_VERSION,
        };

        let dir = tempdir().unwrap();
        let socket = dir.path().join("fleet-ipc.sock");
        let fleet_store = Arc::new(FleetStore::new());
        fleet_store
            .upsert_host(FleetHostSnapshot {
                alias: "dev".into(),
                local: false,
                ssh_target: "devbox".into(),
                labels: std::collections::BTreeMap::from([(
                    "environment".into(),
                    "development".into(),
                )]),
                annotations: std::collections::BTreeMap::new(),
                mode: HostAccessMode::Control,
                state: FleetHostState::Online,
                node_id: None,
                hostname: Some("devbox".into()),
                os: Some("linux".into()),
                arch: Some("x86_64".into()),
                muxa_version: Some(env!("CARGO_PKG_VERSION").into()),
                protocol: Some(FLEET_PROTOCOL_VERSION),
                capabilities: Vec::new(),
                daemon_generation: Some(0),
                boot_id: Some("boot".into()),
                latency_ms: Some(3),
                last_seen_at: Some(OffsetDateTime::now_utc()),
                received_at: Some(OffsetDateTime::now_utc()),
                error: None,
                remote: None,
            })
            .await;
        let mut production = fleet_store.snapshot().await.hosts[0].clone();
        production.alias = "prod".into();
        production.ssh_target = "prodbox".into();
        production
            .labels
            .insert("environment".into(), "production".into());
        fleet_store.upsert_host(production).await;
        let (runtime, mut commands) = FleetRuntime::new(fleet_store.clone());
        let command_task = tokio::spawn(async move {
            let command = commands.recv().await.expect("fleet command");
            assert_eq!(command.host, "dev");
            assert!(matches!(command.operation, FleetOperation::Refresh));
            let _ = command
                .reply
                .send(Ok(FleetCommandResult::accepted("refreshed")));
        });
        let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
        let server = Server::new(socket.clone(), Store::shared()).with_fleet(runtime);
        let server_task = tokio::spawn(async move { server.run(shutdown_rx).await.unwrap() });
        wait_for_socket(&socket).await;

        let client = Client::new(socket);
        let mut updates = client
            .fleet_subscribe(Some("environment=development"))
            .await
            .expect("fleet update subscription");
        fleet_store
            .mutate_host("prod", |host| host.latency_ms = Some(8))
            .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), updates.recv())
                .await
                .is_err()
        );
        fleet_store
            .mutate_host("dev", |host| host.latency_ms = Some(4))
            .await;
        let update = tokio::time::timeout(Duration::from_secs(1), updates.recv())
            .await
            .expect("fleet update timeout")
            .expect("fleet update read")
            .expect("fleet update stream closed");
        assert_eq!(update.host, "dev");
        assert_eq!(update.state, FleetHostState::Online);
        fleet_store
            .mutate_host("dev", |host| {
                host.labels
                    .insert("environment".into(), "production".into());
            })
            .await;
        let leaving = tokio::time::timeout(Duration::from_secs(1), updates.recv())
            .await
            .expect("selector leaving update timeout")
            .expect("selector leaving update read")
            .expect("fleet update stream closed");
        assert_eq!(leaving.host, "dev");
        fleet_store
            .mutate_host("dev", |host| host.latency_ms = Some(5))
            .await;
        assert!(
            tokio::time::timeout(Duration::from_millis(100), updates.recv())
                .await
                .is_err()
        );
        fleet_store
            .mutate_host("dev", |host| {
                host.labels
                    .insert("environment".into(), "development".into());
            })
            .await;
        let entering = tokio::time::timeout(Duration::from_secs(1), updates.recv())
            .await
            .expect("selector entering update timeout")
            .expect("selector entering update read")
            .expect("fleet update stream closed");
        assert_eq!(entering.host, "dev");
        let selected = client
            .fleet_snapshot(Some("environment=development"))
            .await
            .expect("fleet snapshot");
        assert_eq!(selected.hosts.len(), 1);
        let excluded = client
            .fleet_snapshot(Some("environment=staging"))
            .await
            .expect("filtered fleet snapshot");
        assert!(excluded.hosts.is_empty());
        let result = client
            .fleet_execute("dev", &FleetOperation::Refresh)
            .await
            .expect("fleet command");
        assert_eq!(result.message.as_deref(), Some("refreshed"));

        command_task.await.unwrap();
        drop(updates);
        fleet_store
            .mutate_host("dev", |host| host.latency_ms = Some(6))
            .await;
        tokio::task::yield_now().await;
        let _ = shutdown_tx.send(());
        server_task.await.unwrap();
    }
}
