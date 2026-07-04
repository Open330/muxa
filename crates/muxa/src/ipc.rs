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

use crate::backend::{default_backend, SharedBackend};
use crate::event::{AgentEvent, PROTOCOL_VERSION};
use crate::session::{
    PtySessionBackend, SessionBackend, SessionOutput, SessionRef, SharedSessionBackend,
    SpawnSession, TerminalSnapshot,
};
use crate::state::{Agent, SharedStore};
use crate::tmux::PaneInfo;
use serde::{Deserialize, Serialize};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
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

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("socket already exists and is in use at {0}; another daemon may be running")]
    SocketInUse(PathBuf),

    #[error(
        "daemon not reachable at {} — is `muxad` running? (start `muxad`, or set MUXA_SOCKET)",
        .0.display()
    )]
    NotConnected(PathBuf),

    #[error("ipc message exceeds {0} bytes")]
    MessageTooLarge(usize),
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RequestBody {
    Ingest {
        event: AgentEvent,
    },
    Snapshot,
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
    /// push-based updates.
    Subscribe,

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
const CAPABILITIES: &[&str] = &["waiting_choice", "needs_choice", "rate_limited"];

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<SessionRef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalSnapshot>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<SessionOutput>,
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
            sessions: None,
            session: None,
            terminal: None,
            output: None,
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
    fn hello() -> Self {
        let mut r = Self::ok();
        r.min_protocol = Some(MIN_PROTOCOL_VERSION);
        r.max_protocol = Some(PROTOCOL_VERSION);
        r.capabilities = Some(CAPABILITIES.to_vec());
        r
    }
}

/// Daemon-side server. Construct once, call `run` under the tokio runtime.
pub struct Server {
    socket_path: PathBuf,
    store: SharedStore,
    backend: SharedBackend,
    sessions: SharedSessionBackend,
}

impl Server {
    pub fn new(socket_path: PathBuf, store: SharedStore) -> Self {
        Self {
            socket_path,
            store,
            backend: default_backend(),
            sessions: PtySessionBackend::shared(),
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

    #[must_use]
    pub fn with_sessions(mut self, sessions: SharedSessionBackend) -> Self {
        self.sessions = sessions;
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
    pub async fn run(self, mut shutdown: broadcast::Receiver<()>) -> Result<(), RuntimeError> {
        self.bind_with_perms()?;
        let listener = UnixListener::bind(&self.socket_path)?;
        harden_permissions(&self.socket_path)?;
        tracing::info!(socket = %self.socket_path.display(), "listening");

        let mut handlers: JoinSet<()> = JoinSet::new();

        loop {
            tokio::select! {
                accept = listener.accept() => {
                    let (stream, _) = accept?;
                    let store = self.store.clone();
                    let backend = self.backend.clone();
                    let sessions = self.sessions.clone();
                    handlers.spawn(async move {
                        if let Err(e) = handle(stream, store, backend, sessions).await {
                            tracing::warn!(error = %e, "connection handler failed");
                        }
                    });
                    // Reap finished handlers opportunistically so the JoinSet
                    // doesn't grow unboundedly under steady traffic.
                    while handlers.try_join_next().is_some() {}
                }
                _ = shutdown.recv() => {
                    tracing::info!("shutdown signal received; closing listener");
                    break;
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
            // `pi` AgentKind is a v4 addition → Unknown for older peers.
            if protocol < 4 && s == "pi" {
                *s = "unknown".to_string();
            }
            // `cmux` SurfaceKind is a v4 addition. Downgrade to `pty`,
            // the closest non-host surface: like cmux it is not a
            // tmux/zellij pane, so a v3 client's host-pane reconciler
            // still treats it as non-reapable. The id is opaque to the
            // older client and harmlessly ignored.
            if protocol < 4 && s == "cmux" {
                *s = "pty".to_string();
            }
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
        serde_json::Value::Object(m) => m.values_mut().for_each(|x| downgrade_wire(x, protocol)),
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
) -> Result<(), RuntimeError> {
    let mut rx = store.subscribe();
    loop {
        match rx.recv().await {
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
            }
        }
    }
}

#[tracing::instrument(level = "debug", skip(stream, store, backend, sessions))]
#[allow(clippy::too_many_lines)] // dispatch table — splitting would only spread the match across files
async fn handle(
    stream: UnixStream,
    store: SharedStore,
    backend: SharedBackend,
    sessions: SharedSessionBackend,
) -> Result<(), RuntimeError> {
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
        let n = read_limited_line(&mut reader, &mut line).await?;
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
                        tracing::debug!(
                            client = client.as_deref().unwrap_or("(unknown)"),
                            protocol = requested,
                            "hello"
                        );
                        let mut r = Response::hello();
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
                    tracing::debug!(?event, "ingest");
                    store.apply(&event).await;
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
                RequestBody::Snapshot => {
                    kind = "snapshot";
                    Response::with_agents(store.snapshot().await)
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
                RequestBody::BackendPaneSnapshot { panes } => {
                    kind = "backend_pane_snapshot";
                    let count = panes.len();
                    backend.ingest_pane_snapshot(panes);
                    tracing::debug!(panes = count, "backend_pane_snapshot");
                    Response::ok()
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
                RequestBody::Subscribe => {
                    kind = "subscribe";
                    let stream_proto = negotiated.unwrap_or(PROTOCOL_VERSION);
                    // Stream takeover. Send ack, then write transitions
                    // until the client disconnects or muxad shuts down.
                    // We deliberately do NOT return to the request loop
                    // — this connection is now owned by the streaming
                    // pump.
                    let ack_bytes = encode_line(&Response::ok(), stream_proto)?;
                    writer.write_all(&ack_bytes).await?;
                    writer.flush().await?;
                    tracing::debug!(
                        elapsed_us =
                            u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                        kind,
                        "ipc.handle (stream takeover)",
                    );
                    return stream_transitions(writer, store, stream_proto).await;
                }
            },
            Err(e) => {
                kind = "parse_error";
                Response::err(format!("bad request: {e}"))
            }
        };

        let bytes = encode_line(&resp, negotiated.unwrap_or(PROTOCOL_VERSION))?;
        writer.write_all(&bytes).await?;
        writer.flush().await?;

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

/// After `UnixListener::bind`, chmod the path so only the owner can connect.
pub fn harden_permissions(socket_path: &Path) -> std::io::Result<()> {
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(socket_path, perms)
}

/// Client-side helper. Single-shot request/response.
#[derive(Debug, Clone)]
pub struct Client {
    socket_path: PathBuf,
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

impl TransitionStream {
    /// Wait for and return the next streamed `Transition`.
    pub async fn recv(&mut self) -> Result<Option<crate::state::Transition>, RuntimeError> {
        self.line.clear();
        let n = read_limited_line(&mut self.reader, &mut self.line).await?;
        if n == 0 {
            return Ok(None);
        }
        let t: crate::state::Transition = serde_json::from_str(self.line.trim())?;
        Ok(Some(t))
    }
}

impl Client {
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    pub async fn ingest(&self, event: &AgentEvent) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ingest",
            "event": event
        });
        let _ = self.call(&req).await?;
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
        Ok(resp["agents"]
            .as_array()
            .cloned()
            .map(|v| serde_json::from_value(serde_json::Value::Array(v)).unwrap_or_default())
            .unwrap_or_default())
    }

    pub async fn by_pane(&self, pane: &str) -> Result<Vec<Agent>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "by_pane",
            "pane": pane
        });
        let resp = self.call(&req).await?;
        Ok(resp["agents"]
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

        Self::send_hello(&mut reader, &mut writer).await?;

        let mut req = serde_json::to_vec(&serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "subscribe",
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
    async fn send_hello<R, W>(reader: &mut BufReader<R>, writer: &mut W) -> Result<(), RuntimeError>
    where
        R: tokio::io::AsyncRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut bytes = serde_json::to_vec(&serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "hello",
            "client": concat!("muxa/", env!("CARGO_PKG_VERSION")),
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

        Self::send_hello(&mut reader, &mut writer).await?;

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
    use crate::event::{AgentEvent, AgentId, AgentKind, AgentState, SurfaceKind, SurfaceRef};
    use crate::state::Store;
    use tempfile::tempdir;
    use time::OffsetDateTime;

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

    /// v4 enum variants (`pi` `AgentKind`, `cmux` `SurfaceKind`) must be
    /// downgraded for a v3-negotiated client so its deserializer doesn't
    /// fall back to an empty agent list. `pi` → `unknown`, `cmux` →
    /// `pty` (closest non-host surface; keeps the host-pane reconciler
    /// from reaping it).
    #[tokio::test]
    async fn v3_hello_downgrades_pi_and_cmux_in_snapshot() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("muxa-v4-downgrade.sock");
        let store = Store::shared();
        let server = Server::new(sock.clone(), store.clone());
        let (tx, rx) = broadcast::channel(1);
        let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
        wait_for_socket(&sock).await;

        let id = AgentId {
            kind: AgentKind::Pi,
            session_id: "pi-v4-test".into(),
            surface: Some(SurfaceRef {
                kind: SurfaceKind::Cmux,
                id: "cmux-surface-1".into(),
            }),
            pane: Some("%7".into()),
            cwd: None,
        };
        store
            .apply(&AgentEvent::Started {
                id: id.clone(),
                at: OffsetDateTime::now_utc(),
            })
            .await;

        let mut stream = tokio::net::UnixStream::connect(&sock).await.unwrap();
        let mut hello = serde_json::to_vec(&serde_json::json!({
            "protocol": 3, "kind": "hello", "client": "v3-test",
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
        // The agent must still be visible (not dropped by a deser
        // failure) and its v4-only variants rewritten.
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0]["kind"], "unknown");
        assert_eq!(agents[0]["surface"]["kind"], "pty");
        assert!(
            !line.contains("\"pi\""),
            "v3 snapshot still contains pi kind: {line}"
        );
        assert!(
            !line.contains("\"cmux\""),
            "v3 snapshot still contains cmux surface: {line}"
        );

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
            pane_id: "zellij:3".into(),
            session: "z".into(),
            window_index: "0".into(),
            pane_index: "3".into(),
            tty: String::new(),
            current_command: "claude".into(),
            title: String::new(),
            pane_pid: 0,
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
}
