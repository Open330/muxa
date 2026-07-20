//! herdr backend — [`PaneBackend`] over the herdr socket API.
//!
//! herdr (<https://herdr.dev>) is an agent-native terminal multiplexer that
//! serves a newline-delimited JSON API on a local unix socket (default
//! `~/.config/herdr/herdr.sock`, overridden by `$HERDR_SOCKET_PATH` inside
//! herdr panes). Unlike the zellij CLI baseline this gives muxa a full
//! query surface without a plugin: `pane.list` for enumeration,
//! `pane.read` for captures, `pane.process_info` for the pid map, and
//! `pane.focus` for attach.
//!
//! Pane ids are namespaced as `herdr:<herdr_pane_id>` everywhere they
//! enter muxa (registry rows, history keys, hook correlation) so they
//! cannot collide with tmux `%N` ids and so cross-host code can tell
//! which host governs a row. The prefix is stripped before ids are sent
//! back over the herdr socket.
//!
//! ## Wire protocol
//!
//! Every call is a fresh connection (the surface is stateless — callers
//! already wrap it in `spawn_blocking`, mirroring the tmux shell-out
//! shape). We write one request line
//! `{"id":"muxa-<n>","method":…,"params":{…}}` and read response lines
//! until one echoes our `id`, skipping anything else (a concurrent
//! subscription event, a stale reply). Read/write are bounded by a ~1s
//! per-read timeout matching [`crate::tmux`]'s `TMUX_COMMAND_TIMEOUT`, plus
//! an aggregate ~2× read deadline over the whole reply loop so a server that
//! *streams* unrelated lines sub-timeout can't wedge a watch refresh open. A
//! success carries a
//! `type`-tagged `result`; an `error` object (or a malformed line, or a
//! timeout) degrades the call the way a host-down backend would — empty
//! vec / `None` / `false`, never a panic.
//!
//! See `docs/HERDR.md` for the full design.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};

use super::{BackendCaps, HostKind, PaneBackend, PaneObservation};
use crate::tmux::PaneInfo;

/// Namespace prefix for herdr pane ids inside muxa.
pub const PANE_ID_PREFIX: &str = "herdr:";

/// Bound on each socket round-trip, mirroring `TMUX_COMMAND_TIMEOUT`. A
/// herdr server that accepts the connection but never answers must not
/// stall the reconciler or a watch refresh past this.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1);

/// Monotonic request-id source. Only needs to disambiguate replies on a
/// single connection, but a process-global counter keeps ids unique in
/// logs across concurrent calls too. `Relaxed` is enough — we never
/// order anything off this value.
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// [`PaneBackend`] implementation speaking the herdr socket API.
///
/// Cheap to construct (resolves the socket path, no I/O) and cheap to
/// clone into an `Arc`. Holds no connection: each method opens, queries,
/// and drops a `UnixStream`.
pub struct HerdrBackend {
    socket_path: PathBuf,
    timeout: Duration,
}

impl HerdrBackend {
    #[must_use]
    pub fn new() -> Self {
        Self::with_socket_path(default_socket_path())
    }

    /// Build a backend pointed at an explicit socket path, bypassing env
    /// resolution. Used by tests to target a temp-dir listener without
    /// mutating `std::env` (forbidden by the workspace's
    /// `forbid(unsafe_code)` posture).
    #[must_use]
    pub fn with_socket_path(socket_path: PathBuf) -> Self {
        Self {
            socket_path,
            timeout: REQUEST_TIMEOUT,
        }
    }

    /// Shorten the per-call timeout. Test-only so the "server accepts
    /// then hangs" case exercises the real timeout path without a
    /// full-second wait in the suite.
    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// One request/response round-trip. Returns the `result` value on a
    /// success reply; classifies every failure so callers can tell an
    /// absent server (its panes are truly gone) from a transient error
    /// (must not drive destructive reaping) — see [`Self::observe_panes`].
    fn request(&self, method: &str, params: Value) -> Result<Value, HerdrError> {
        // A missing socket file means no server is listening — distinct
        // from a connect/timeout error because it is authoritative: there
        // are no herdr panes to observe. `try_exists()` (not `exists()`)
        // so a stat that *errors* — EACCES, EIO, a stalled automount — maps
        // to a transient `Io` error rather than being swallowed as an
        // authoritative "socket absent" and triggering a mass reap.
        classify_socket_presence(self.socket_path.try_exists())?;

        let mut stream = UnixStream::connect(&self.socket_path).map_err(map_io)?;
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(map_io)?;
        stream
            .set_write_timeout(Some(self.timeout))
            .map_err(map_io)?;

        let id = format!("muxa-{}", REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed));
        let mut payload = serde_json::to_string(&json!({
            "id": id,
            "method": method,
            "params": params,
        }))
        .map_err(|_| HerdrError::Protocol("serialize request"))?;
        payload.push('\n');
        stream.write_all(payload.as_bytes()).map_err(map_io)?;

        // Read lines until one echoes our id. Skip non-matching lines
        // (subscription events carry no `id`; stale/foreign replies carry
        // a different one) and unparsable noise.
        //
        // The per-read socket timeout resets on every line, so a chatty
        // server that streams unrelated lines *faster* than that timeout
        // would loop here forever and wedge the reconciler / watch refresh.
        // Bound the whole read with an aggregate wall-clock deadline
        // (~2× the per-read timeout — enough for a healthy server's reply
        // to arrive after at most one skipped line, but not unbounded).
        let deadline = Instant::now() + self.timeout.saturating_mul(2);
        let reader = BufReader::new(&stream);
        for line in reader.lines() {
            if Instant::now() >= deadline {
                return Err(HerdrError::Timeout);
            }
            let line = line.map_err(map_io)?;
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if value.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                continue;
            }
            if let Some(error) = value.get("error") {
                let code = error
                    .get("code")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                tracing::debug!(method, code, message, "herdr returned an error");
                return Err(HerdrError::Method);
            }
            return value
                .get("result")
                .cloned()
                .ok_or(HerdrError::Protocol("response missing result"));
        }
        Err(HerdrError::Protocol(
            "connection closed before a matching reply",
        ))
    }

    /// `pane.process_info` for one raw (unprefixed) herdr pane id. Purely
    /// enrichment — any failure degrades to `None` so a single pane's
    /// process lookup can't fail a whole `pane.list`.
    fn process_info(&self, raw_pane_id: &str) -> Option<HerdrProcessInfo> {
        let result = self
            .request("pane.process_info", json!({ "pane_id": raw_pane_id }))
            .ok()?;
        serde_json::from_value(result.get("process_info")?.clone()).ok()
    }

    /// `pane.list` plus per-pane `pane.process_info`, mapped into muxa
    /// [`PaneInfo`]. Returns the classified error so [`Self::observe_panes`]
    /// can decide whether an empty result is authoritative.
    fn query_panes(&self) -> Result<Vec<PaneInfo>, HerdrError> {
        let result = self.request("pane.list", json!({}))?;
        let panes: Vec<HerdrPaneInfo> = result
            .get("panes")
            .cloned()
            .and_then(|p| serde_json::from_value(p).ok())
            .ok_or(HerdrError::Protocol("pane.list missing panes"))?;
        // Per-pane `pane.process_info` enrichment is a full socket round-trip
        // each, so N panes cost N× serial worst-case against a slow server.
        // Cap the *total* enrichment time per list so one wedged server can't
        // turn a single `pane.list` into an N-second stall: once the budget
        // is spent we return the remaining panes with empty process fields
        // (degraded `current_command`/`pane_pid`/`tty`) rather than block.
        let enrich_deadline = Instant::now() + self.timeout.saturating_mul(2);
        Ok(panes
            .iter()
            .map(|pane| {
                let process = if Instant::now() < enrich_deadline {
                    self.process_info(&pane.pane_id)
                } else {
                    None
                };
                to_pane_info(pane, process.as_ref())
            })
            .collect())
    }
}

impl Default for HerdrBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PaneBackend for HerdrBackend {
    fn kind(&self) -> HostKind {
        HostKind::Herdr
    }

    fn list_panes(&self) -> Vec<PaneInfo> {
        self.query_panes().unwrap_or_default()
    }

    fn observe_panes(&self) -> PaneObservation {
        match self.query_panes() {
            Ok(panes) => PaneObservation::complete(panes),
            // Server truly down ⇒ its panes are gone; an empty set is
            // authoritative (tmux semantics). Reaping may proceed.
            Err(HerdrError::SocketMissing) => PaneObservation::complete(Vec::new()),
            // Connect/protocol/timeout/error ⇒ transient; absence here is
            // not evidence a pane died, so reaping must not proceed.
            Err(err) => {
                tracing::debug!(reason = err.describe(), "herdr observe_panes degraded");
                PaneObservation::incomplete(Vec::new())
            }
        }
    }

    fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo> {
        let raw = strip_prefix(pane_id);
        let result = self.request("pane.get", json!({ "pane_id": raw })).ok()?;
        let pane: HerdrPaneInfo = serde_json::from_value(result.get("pane")?.clone()).ok()?;
        // Enrich with process metadata so resolve returns the same shape
        // `list_panes` does (callers use `current_command` / `pane_pid`).
        let process = self.process_info(&pane.pane_id);
        Some(to_pane_info(&pane, process.as_ref()))
    }

    fn capture_pane(&self, pane_id: &str) -> Option<String> {
        let raw = strip_prefix(pane_id);
        let result = self
            .request("pane.read", json!({ "pane_id": raw, "source": "visible" }))
            .ok()?;
        result.get("read")?.get("text")?.as_str().map(str::to_owned)
    }

    fn pane_pid_map(&self) -> HashMap<u32, String> {
        self.query_panes()
            .unwrap_or_default()
            .into_iter()
            // A zeroed `pane_pid` means the shell pid was null/absent —
            // no process tree to walk, so it can't anchor an ancestry
            // lookup. Skip it rather than mapping pid 0.
            .filter(|pane| pane.pane_pid != 0)
            .map(|pane| (pane.pane_pid, pane.pane_id))
            .collect()
    }

    fn current_pane(&self) -> Option<String> {
        std::env::var("HERDR_PANE_ID")
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| format!("{PANE_ID_PREFIX}{v}"))
    }

    fn focus_pane(&self, pane_id: &str) -> bool {
        let raw = strip_prefix(pane_id);
        // Any non-error reply means the focus landed; herdr answers with a
        // `pane_focused` result, but we don't inspect the tag — a success
        // envelope is the whole signal.
        self.request("pane.focus", json!({ "pane_id": raw }))
            .is_ok()
    }

    fn caps(&self) -> BackendCaps {
        // The socket API covers every capability directly; nothing is
        // plugin-gated the way zellij is.
        BackendCaps::default()
    }
}

/// Resolve the socket path the way herdr itself does: `$HERDR_SOCKET_PATH`
/// wins, else `~/.config/herdr/herdr.sock`. herdr uses the XDG-style
/// `~/.config` path on every platform (including macOS), so this joins
/// `home` directly rather than going through `dirs::config_dir()`, which
/// would resolve to `~/Library/Application Support` on macOS and miss the
/// real socket.
///
/// `pub` so the Phase-2 event bridge (`muxad::herdr_bridge`) resolves the
/// same socket the query backend does, keeping named-session support
/// (`$HERDR_SOCKET_PATH`) consistent across both connections.
pub fn default_socket_path() -> PathBuf {
    if let Ok(path) = std::env::var("HERDR_SOCKET_PATH") {
        if !path.is_empty() {
            return PathBuf::from(path);
        }
    }
    let rel = ".config/herdr/herdr.sock";
    dirs::home_dir().map_or_else(|| PathBuf::from(rel), |home| home.join(rel))
}

/// Bound on the auto-detect reachability probe. A herdr server accepts a
/// connection instantly; a stale socket file (crashed server) refuses one
/// instantly (`ECONNREFUSED`) and an absent path fails instantly
/// (`ENOENT`). The only way `connect()` can linger is a wedged listener
/// whose backlog never drains — vanishingly rare — so this timeout is a
/// belt-and-suspenders bound that keeps daemon startup from stalling on a
/// pathological socket. A probe that can't answer within it is treated as
/// unreachable (conservative: better to exclude a barely-alive host than
/// drag it into the set).
const REACHABLE_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

/// Liveness probe for [`crate::backend::active_backends`] auto-detect: does a
/// herdr server actually **answer** on `socket_path` right now?
///
/// A bare `try_exists()` on the socket file is not enough. A crashed herdr
/// server leaves its socket file behind, and that stale file would otherwise
/// drag a permanently-dead herdr backend into a tmux-only daemon's active set
/// forever — every reconcile tick observing it incomplete (never reaping its
/// ghost rows), every read fanning out a doomed connect. An actual
/// `connect()` distinguishes the two: it refuses immediately on a stale
/// socket and succeeds only when a server is listening, so the ghost is
/// excluded. Env presence (`HERDR_PANE_ID`/`HERDR_ENV`) remains a separate
/// inclusion signal at the call site — inside a herdr pane the server is alive
/// by construction, so no probe is needed there.
///
/// The connect runs on a throwaway thread so a wedged listener can't block
/// startup past [`REACHABLE_PROBE_TIMEOUT`]; the thread finishes on its own
/// once `connect()` returns (its send just no-ops after we've stopped
/// waiting).
pub(crate) fn server_reachable(socket_path: &Path) -> bool {
    let path = socket_path.to_path_buf();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(UnixStream::connect(&path).is_ok());
    });
    rx.recv_timeout(REACHABLE_PROBE_TIMEOUT).unwrap_or(false)
}

/// The focused herdr workspace, mapped to muxa's foreground-tracking keys:
/// the stable `workspace_id` becomes the session id (matching
/// [`to_pane_info`]'s `session` mapping so ledger keys line up) and the
/// mutable `label` becomes the display name (tmux session-name analog).
pub(crate) struct FocusedWorkspace {
    /// herdr `workspace_id` (e.g. `w1`). Same value `list_panes` puts in
    /// [`PaneInfo::session`].
    pub id: String,
    /// herdr workspace `label` — the human-facing name. Falls back to the
    /// id when absent.
    pub label: String,
}

/// Query the herdr socket for the currently focused workspace, for
/// `session_activity`'s herdr foreground-time analog. Uses `workspace.list`
/// (the cheapest call that reports each workspace's `focused` flag —
/// lighter than `session.snapshot`, which also serializes every tab, pane,
/// and agent) and returns the one workspace herdr marks `focused`.
///
/// Returns `None` when the server is unreachable/absent, no workspace is
/// focused, or the reply is malformed — the sampler treats all of these as
/// "no focused workspace this tick" (see `session_activity`).
pub(crate) fn herdr_focused_workspace(socket_path: &Path) -> Option<FocusedWorkspace> {
    let backend = HerdrBackend::with_socket_path(socket_path.to_path_buf());
    let result = backend.request("workspace.list", json!({})).ok()?;
    let workspaces = result.get("workspaces")?.as_array()?;
    workspaces
        .iter()
        .find(|ws| ws.get("focused").and_then(Value::as_bool) == Some(true))
        .and_then(|ws| {
            let id = ws.get("workspace_id").and_then(Value::as_str)?.to_string();
            let label = ws
                .get("label")
                .and_then(Value::as_str)
                .filter(|l| !l.is_empty())
                .unwrap_or(&id)
                .to_string();
            Some(FocusedWorkspace { id, label })
        })
}

/// A herdr workspace as the watch/session view needs it: the stable
/// `workspace_id` (session id, matching [`to_pane_info`]'s `session` mapping
/// and the `session_activity` ledger key) plus the mutable human-facing
/// `label` (display name, falling back to the id when herdr reports none).
pub struct WorkspaceSummary {
    /// herdr `workspace_id` (e.g. `w1`). Same value `list_panes` puts in
    /// [`PaneInfo::session`] and the ledger keys foreground time under.
    pub id: String,
    /// herdr workspace `label` — the display name. Falls back to the id.
    pub label: String,
}

/// List every herdr workspace as a [`WorkspaceSummary`], so the watch
/// session view can source "sessions" on herdr hosts (the tmux
/// `list_sessions` analog). Uses `workspace.list` — the same cheap call
/// [`herdr_focused_workspace`] uses, but returns *all* workspaces rather
/// than only the focused one.
///
/// Returns an empty `Vec` when the server is unreachable/absent, reports no
/// workspaces, or the reply is malformed — every failure degrades to "no
/// sessions this refresh", mirroring `list_sessions().unwrap_or_default()`
/// on tmux (a downed tmux server likewise yields no session rows).
pub fn herdr_list_workspaces(socket_path: &Path) -> Vec<WorkspaceSummary> {
    let backend = HerdrBackend::with_socket_path(socket_path.to_path_buf());
    let Ok(result) = backend.request("workspace.list", json!({})) else {
        return Vec::new();
    };
    let Some(workspaces) = result.get("workspaces").and_then(Value::as_array) else {
        return Vec::new();
    };
    workspaces
        .iter()
        .filter_map(|ws| {
            let id = ws.get("workspace_id").and_then(Value::as_str)?.to_string();
            let label = ws
                .get("label")
                .and_then(Value::as_str)
                .filter(|l| !l.is_empty())
                .unwrap_or(&id)
                .to_string();
            Some(WorkspaceSummary { id, label })
        })
        .collect()
}

/// Strip the `herdr:` namespace before an id crosses the socket. Lenient:
/// an already-bare id (or a foreign shape) passes through unchanged.
fn strip_prefix(pane_id: &str) -> &str {
    pane_id.strip_prefix(PANE_ID_PREFIX).unwrap_or(pane_id)
}

/// Classify a `try_exists()` result on the socket path. `Ok(false)` is the
/// authoritative "no server listening" signal (`SocketMissing` — reaping may
/// proceed); `Ok(true)` clears the check; an `Err` (EACCES, EIO, a stalled
/// automount) is transient — mapped to `Io` so `observe_panes` yields
/// `incomplete` and destructive reaping is withheld. Split out from
/// [`HerdrBackend::request`] so the three-way mapping is unit-testable without
/// having to provoke a real stat error.
fn classify_socket_presence(exists: std::io::Result<bool>) -> Result<(), HerdrError> {
    match exists {
        Ok(true) => Ok(()),
        Ok(false) => Err(HerdrError::SocketMissing),
        Err(_) => Err(HerdrError::Io),
    }
}

/// Map an I/O failure to the classification `observe_panes` needs: a read
/// that hit the socket timeout surfaces as `WouldBlock`/`TimedOut`, which
/// we tag distinctly from other transport errors for clearer logs (both
/// still degrade to `incomplete`).
fn map_io(err: std::io::Error) -> HerdrError {
    match err.kind() {
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => HerdrError::Timeout,
        _ => HerdrError::Io,
    }
}

/// Map a herdr `PaneInfo` (+ optional process info) into muxa's flat
/// [`PaneInfo`]. See `docs/HERDR.md` for the field-by-field rationale.
fn to_pane_info(pane: &HerdrPaneInfo, process: Option<&HerdrProcessInfo>) -> PaneInfo {
    let (current_command, pane_pid, tty) = match process {
        Some(info) => (
            info.foreground_command(),
            info.shell_pid.unwrap_or(0),
            info.tty.clone().unwrap_or_default(),
        ),
        None => (String::new(), 0, String::new()),
    };
    PaneInfo {
        pane_id: format!("{PANE_ID_PREFIX}{}", pane.pane_id),
        session: pane.workspace_id.clone(),
        window_index: pane.tab_id.clone(),
        pane_index: pane.pane_id.clone(),
        tty,
        current_command,
        // Prefer the user/agent-facing title, fall back to the raw
        // terminal title (OSC-set), then empty.
        title: pane
            .title
            .clone()
            .filter(|t| !t.is_empty())
            .or_else(|| pane.terminal_title.clone())
            .unwrap_or_default(),
        current_path: pane.cwd.clone().unwrap_or_default(),
        pane_pid,
        socket: None,
    }
}

/// Why a herdr call did not yield a usable result. The variant matters
/// only to `observe_panes`, which reaps on [`Self::SocketMissing`] but not
/// on the transient variants.
enum HerdrError {
    /// Socket file absent — no server ⇒ authoritatively no panes.
    SocketMissing,
    /// Connect or transport failure (server may be up but unreachable).
    Io,
    /// Read outlived the timeout — server accepted but didn't answer.
    Timeout,
    /// Server answered with an `error` object.
    Method,
    /// Reply was malformed or missing the expected shape.
    Protocol(&'static str),
}

impl HerdrError {
    /// A stable, log-friendly reason string for the reconciler trace.
    fn describe(&self) -> &'static str {
        match self {
            Self::SocketMissing => "socket file missing",
            Self::Io => "connect/transport error",
            Self::Timeout => "read timed out",
            Self::Method => "server returned an error",
            Self::Protocol(reason) => reason,
        }
    }
}

/// herdr `PaneInfo` — only the fields muxa maps. serde ignores the rest
/// (agent state, revision, scroll, tokens) by default.
#[derive(Deserialize)]
struct HerdrPaneInfo {
    pane_id: String,
    workspace_id: String,
    tab_id: String,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    terminal_title: Option<String>,
}

/// herdr `PaneProcessInfo`. `shell_pid`/`tty` are nullable; the
/// foreground list is empty when herdr couldn't inspect the pane.
#[derive(Deserialize)]
struct HerdrProcessInfo {
    #[serde(default)]
    shell_pid: Option<u32>,
    #[serde(default)]
    tty: Option<String>,
    #[serde(default)]
    foreground_processes: Vec<HerdrProcess>,
}

impl HerdrProcessInfo {
    /// The pane's foreground command name, tmux-`#{pane_current_command}`
    /// style: prefer the deepest process that isn't the shell (the actual
    /// running command), and fall back to the shell itself when it's the
    /// only thing in the foreground group (an idle prompt).
    fn foreground_command(&self) -> String {
        self.foreground_processes
            .iter()
            .rev()
            .find(|p| Some(p.pid) != self.shell_pid)
            .or_else(|| self.foreground_processes.last())
            .map(|p| p.name.clone())
            .unwrap_or_default()
    }
}

#[derive(Deserialize)]
struct HerdrProcess {
    pid: u32,
    name: String,
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::os::unix::net::UnixListener;
    use std::path::Path;
    use std::thread;

    use serde_json::json;
    use tempfile::TempDir;

    use super::*;

    /// How the canned test server answers one request. The server always
    /// echoes the request's real `id` (which increments globally), so
    /// tests describe the payload, not the envelope.
    enum Reply {
        /// Wrap `value` as `{"id":…,"result":value}`.
        Result(Value),
        /// Reply `{"id":…,"error":{code,message}}`.
        Error,
        /// Emit a line under a foreign id first, then the real result —
        /// exercises reply-id matching (the client must skip line one).
        ForeignIdThen(Value),
        /// Accept the connection but never answer — drives the timeout.
        Hang,
        /// Stream unrelated (foreign-id) lines faster than the per-read
        /// timeout and never send the matching reply — drives the aggregate
        /// read deadline (a per-read timeout alone would reset on every line
        /// and loop forever).
        Chatter,
    }

    /// Spin a `UnixListener` on a temp path, answering each connection via
    /// `handler(method) -> Reply`. Returns the socket path plus the
    /// `TempDir` guard (dropping it removes the socket). The accept loop
    /// thread is detached; it dies with the test process.
    fn spawn_server<F>(handler: F) -> (PathBuf, TempDir)
    where
        F: Fn(&str) -> Reply + Send + 'static,
    {
        let dir = TempDir::new().unwrap();
        let socket_path = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let reader_stream = stream.try_clone().unwrap();
                let mut reader = BufReader::new(reader_stream);
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.is_empty() {
                    continue;
                }
                let request: Value = match serde_json::from_str(&line) {
                    Ok(value) => value,
                    Err(_) => continue,
                };
                let id = request.get("id").and_then(Value::as_str).unwrap_or("");
                let method = request.get("method").and_then(Value::as_str).unwrap_or("");
                match handler(method) {
                    Reply::Result(value) => {
                        write_line(&mut stream, &json!({ "id": id, "result": value }));
                    }
                    Reply::Error => {
                        write_line(
                            &mut stream,
                            &json!({
                                "id": id,
                                "error": { "code": "not_found", "message": "no such pane" },
                            }),
                        );
                    }
                    Reply::ForeignIdThen(value) => {
                        // A subscription-style line without our id, then the
                        // real answer. The client must skip the first.
                        write_line(
                            &mut stream,
                            &json!({ "id": "muxa-foreign", "result": { "type": "ok" } }),
                        );
                        write_line(&mut stream, &json!({ "id": id, "result": value }));
                    }
                    Reply::Hang => {
                        // Hold the connection open with no reply.
                        thread::sleep(Duration::from_secs(30));
                    }
                    Reply::Chatter => {
                        // Flood foreign-id lines sub-timeout, forever. The
                        // client must bail at its aggregate read deadline
                        // rather than loop on the resetting per-read timeout.
                        loop {
                            write_line(
                                &mut stream,
                                &json!({ "id": "muxa-foreign", "result": { "type": "ok" } }),
                            );
                            thread::sleep(Duration::from_millis(5));
                        }
                    }
                }
            }
        });
        (socket_path, dir)
    }

    fn write_line(stream: &mut UnixStream, value: &Value) {
        let mut line = serde_json::to_string(value).unwrap();
        line.push('\n');
        let _ = stream.write_all(line.as_bytes());
    }

    /// A herdr `pane_list` result with one pane.
    fn pane_list_result() -> Value {
        json!({
            "type": "pane_list",
            "panes": [{
                "pane_id": "p1",
                "terminal_id": "t1",
                "workspace_id": "ws1",
                "tab_id": "tab1",
                "focused": true,
                "agent_status": "idle",
                "revision": 1,
                "cwd": "/home/u/proj",
                "title": "editor",
                "terminal_title": "raw-title",
            }],
        })
    }

    /// A herdr `pane_process_info` result: a shell with `vim` running in
    /// the foreground.
    fn process_info_result() -> Value {
        json!({
            "type": "pane_process_info",
            "process_info": {
                "pane_id": "p1",
                "shell_pid": 4242,
                "tty": "/dev/pts/3",
                "foreground_processes": [
                    { "pid": 4242, "name": "zsh" },
                    { "pid": 5001, "name": "vim" },
                ],
            },
        })
    }

    fn backend_at(socket_path: &Path) -> HerdrBackend {
        HerdrBackend::with_socket_path(socket_path.to_path_buf())
    }

    #[test]
    fn kind_reports_herdr() {
        let backend = HerdrBackend::with_socket_path(PathBuf::from("/nonexistent.sock"));
        assert_eq!(backend.kind(), HostKind::Herdr);
    }

    #[test]
    fn caps_are_all_true() {
        let backend = HerdrBackend::with_socket_path(PathBuf::from("/nonexistent.sock"));
        assert_eq!(backend.caps(), BackendCaps::default());
    }

    #[test]
    fn list_panes_maps_fields_and_prefixes_id() {
        let (socket, _dir) = spawn_server(|method| match method {
            "pane.list" => Reply::Result(pane_list_result()),
            "pane.process_info" => Reply::Result(process_info_result()),
            _ => Reply::Error,
        });
        let panes = backend_at(&socket).list_panes();
        assert_eq!(panes.len(), 1);
        let p = &panes[0];
        assert_eq!(p.pane_id, "herdr:p1", "id must carry the namespace prefix");
        assert_eq!(p.pane_index, "p1", "pane_index is the raw herdr id");
        assert_eq!(p.session, "ws1", "session ← workspace_id");
        assert_eq!(p.window_index, "tab1", "window_index ← tab_id");
        assert_eq!(p.current_path, "/home/u/proj", "current_path ← cwd");
        assert_eq!(p.title, "editor", "title preferred over terminal_title");
        assert_eq!(
            p.current_command, "vim",
            "foreground command, not the shell"
        );
        assert_eq!(p.pane_pid, 4242, "pane_pid ← shell_pid");
        assert_eq!(p.tty, "/dev/pts/3");
        assert!(
            p.socket.is_none(),
            "herdr never uses the tmux socket channel"
        );
    }

    #[test]
    fn observe_panes_complete_on_success() {
        let (socket, _dir) = spawn_server(|method| match method {
            "pane.list" => Reply::Result(pane_list_result()),
            "pane.process_info" => Reply::Result(process_info_result()),
            _ => Reply::Error,
        });
        let observation = backend_at(&socket).observe_panes();
        assert!(observation.is_complete());
        assert_eq!(observation.panes.len(), 1);
    }

    #[test]
    fn observe_panes_complete_empty_when_socket_missing() {
        // No server, no socket file: authoritatively no panes.
        let backend =
            HerdrBackend::with_socket_path(PathBuf::from("/definitely/missing/herdr-test.sock"));
        let observation = backend.observe_panes();
        assert!(
            observation.is_complete(),
            "absent socket ⇒ server down ⇒ its panes are gone (reap-safe)",
        );
        assert!(observation.panes.is_empty());
    }

    #[test]
    fn observe_panes_incomplete_on_timeout() {
        // Socket exists and connects, but the server never answers. A
        // shortened client timeout keeps the test fast.
        let (socket, _dir) = spawn_server(|_| Reply::Hang);
        let backend = backend_at(&socket).with_timeout(Duration::from_millis(150));
        let observation = backend.observe_panes();
        assert!(
            !observation.is_complete(),
            "a timeout is transient — must not authorize reaping",
        );
        assert!(observation.panes.is_empty());
    }

    #[test]
    fn classify_socket_presence_maps_each_variant() {
        // Present ⇒ proceed.
        assert!(classify_socket_presence(Ok(true)).is_ok());
        // Authoritatively absent ⇒ SocketMissing (reap-safe).
        assert!(matches!(
            classify_socket_presence(Ok(false)),
            Err(HerdrError::SocketMissing),
        ));
        // Stat *errored* (EACCES/EIO/stalled automount) ⇒ transient Io, NOT
        // an authoritative empty — this is the whole point of `try_exists`.
        assert!(matches!(
            classify_socket_presence(Err(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied
            ))),
            Err(HerdrError::Io),
        ));
    }

    #[test]
    fn observe_panes_incomplete_on_chatty_server() {
        // The server streams unrelated (foreign-id) lines forever, faster
        // than the per-read timeout — so only the aggregate read deadline can
        // end the call. Without it the client loops on the resetting per-read
        // timeout indefinitely and wedges the reconciler.
        let (socket, _dir) = spawn_server(|_| Reply::Chatter);
        let backend = backend_at(&socket).with_timeout(Duration::from_millis(100));
        let start = std::time::Instant::now();
        let observation = backend.observe_panes();
        assert!(
            !observation.is_complete(),
            "an aggregate-deadline timeout is transient — must not authorize reaping",
        );
        assert!(observation.panes.is_empty());
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "call must bail at the ~2× aggregate deadline, not loop on chatter",
        );
    }

    #[test]
    fn capture_pane_returns_visible_text() {
        let (socket, _dir) = spawn_server(|method| match method {
            "pane.read" => Reply::Result(json!({
                "type": "pane_read",
                "read": {
                    "pane_id": "p1",
                    "workspace_id": "ws1",
                    "tab_id": "tab1",
                    "source": "visible",
                    "format": "text",
                    "text": "hello from the pane\n",
                    "revision": 7,
                    "truncated": false,
                },
            })),
            _ => Reply::Error,
        });
        let text = backend_at(&socket).capture_pane("herdr:p1");
        assert_eq!(text.as_deref(), Some("hello from the pane\n"));
    }

    #[test]
    fn focus_pane_true_on_ok_reply() {
        let (socket, _dir) = spawn_server(|method| match method {
            "pane.focus" => Reply::Result(json!({
                "type": "pane_focused",
                "pane_id": "p1",
                "workspace_id": "ws1",
            })),
            _ => Reply::Error,
        });
        assert!(backend_at(&socket).focus_pane("herdr:p1"));
    }

    #[test]
    fn pane_pid_map_keys_shell_pid_to_prefixed_id() {
        let (socket, _dir) = spawn_server(|method| match method {
            "pane.list" => Reply::Result(pane_list_result()),
            "pane.process_info" => Reply::Result(process_info_result()),
            _ => Reply::Error,
        });
        let map = backend_at(&socket).pane_pid_map();
        assert_eq!(map.get(&4242).map(String::as_str), Some("herdr:p1"));
    }

    #[test]
    fn pane_pid_map_skips_panes_without_a_pid() {
        let (socket, _dir) = spawn_server(|method| match method {
            "pane.list" => Reply::Result(pane_list_result()),
            // Null shell_pid ⇒ no process tree ⇒ excluded from the map.
            "pane.process_info" => Reply::Result(json!({
                "type": "pane_process_info",
                "process_info": { "pane_id": "p1", "shell_pid": null },
            })),
            _ => Reply::Error,
        });
        assert!(backend_at(&socket).pane_pid_map().is_empty());
    }

    #[test]
    fn resolve_pane_strips_prefix_and_maps() {
        let (socket, _dir) = spawn_server(|method| match method {
            "pane.get" => Reply::Result(json!({
                "type": "pane_info",
                "pane": {
                    "pane_id": "p1",
                    "terminal_id": "t1",
                    "workspace_id": "ws1",
                    "tab_id": "tab1",
                    "focused": false,
                    "agent_status": "idle",
                    "revision": 1,
                    "cwd": null,
                    "title": null,
                    "terminal_title": "fallback-title",
                },
            })),
            "pane.process_info" => Reply::Result(process_info_result()),
            _ => Reply::Error,
        });
        let pane = backend_at(&socket).resolve_pane("herdr:p1").unwrap();
        assert_eq!(pane.pane_id, "herdr:p1");
        assert_eq!(pane.current_path, "", "null cwd ⇒ empty string");
        assert_eq!(
            pane.title, "fallback-title",
            "null title falls back to terminal_title",
        );
    }

    #[test]
    fn reply_id_matching_skips_foreign_lines() {
        // The server emits a foreign-id line before the real reply; the
        // client must ignore it and return the matching one.
        let (socket, _dir) = spawn_server(|method| match method {
            "pane.list" => Reply::ForeignIdThen(pane_list_result()),
            "pane.process_info" => Reply::Result(process_info_result()),
            _ => Reply::Error,
        });
        let panes = backend_at(&socket).list_panes();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_id, "herdr:p1");
    }

    #[test]
    fn error_reply_degrades_gracefully() {
        let (socket, _dir) = spawn_server(|_| Reply::Error);
        let backend = backend_at(&socket);
        // Every operational method must swallow an error envelope.
        assert!(backend.list_panes().is_empty());
        assert!(!backend.observe_panes().is_complete());
        assert!(backend.resolve_pane("herdr:p1").is_none());
        assert!(backend.capture_pane("herdr:p1").is_none());
        assert!(backend.pane_pid_map().is_empty());
        assert!(!backend.focus_pane("herdr:p1"));
    }

    /// A herdr `workspace_list` result: `w1` focused, `w2` not.
    fn workspace_list_result() -> Value {
        json!({
            "type": "workspace_list",
            "workspaces": [
                {
                    "workspace_id": "w1",
                    "number": 1,
                    "label": "main",
                    "focused": true,
                    "pane_count": 2,
                    "tab_count": 1,
                    "active_tab_id": "tab1",
                    "agent_status": "working",
                },
                {
                    "workspace_id": "w2",
                    "number": 2,
                    "label": "scratch",
                    "focused": false,
                    "pane_count": 1,
                    "tab_count": 1,
                    "active_tab_id": "tab2",
                    "agent_status": "idle",
                },
            ],
        })
    }

    #[test]
    fn focused_workspace_returns_the_focused_one() {
        let (socket, _dir) = spawn_server(|method| match method {
            "workspace.list" => Reply::Result(workspace_list_result()),
            _ => Reply::Error,
        });
        let ws = herdr_focused_workspace(&socket).expect("a workspace is focused");
        assert_eq!(ws.id, "w1", "session id ← focused workspace_id");
        assert_eq!(ws.label, "main", "display name ← workspace label");
    }

    #[test]
    fn focused_workspace_none_when_no_workspace_focused() {
        let (socket, _dir) = spawn_server(|method| match method {
            // Same list but nothing focused (e.g. a fully detached server).
            "workspace.list" => Reply::Result(json!({
                "type": "workspace_list",
                "workspaces": [{
                    "workspace_id": "w1",
                    "number": 1,
                    "label": "main",
                    "focused": false,
                    "pane_count": 1,
                    "tab_count": 1,
                    "active_tab_id": "tab1",
                    "agent_status": "idle",
                }],
            })),
            _ => Reply::Error,
        });
        assert!(herdr_focused_workspace(&socket).is_none());
    }

    #[test]
    fn focused_workspace_none_when_server_down() {
        // No socket file ⇒ server absent ⇒ no sample (tmux "no server" analog).
        let missing = PathBuf::from("/definitely/missing/herdr-activity.sock");
        assert!(herdr_focused_workspace(&missing).is_none());
    }

    #[test]
    fn focused_workspace_none_on_error_reply() {
        let (socket, _dir) = spawn_server(|_| Reply::Error);
        assert!(herdr_focused_workspace(&socket).is_none());
    }

    #[test]
    fn focused_workspace_label_falls_back_to_id() {
        let (socket, _dir) = spawn_server(|method| match method {
            "workspace.list" => Reply::Result(json!({
                "type": "workspace_list",
                "workspaces": [{
                    "workspace_id": "w7",
                    "number": 1,
                    "label": "",
                    "focused": true,
                    "pane_count": 1,
                    "tab_count": 1,
                    "active_tab_id": "tab1",
                    "agent_status": "idle",
                }],
            })),
            _ => Reply::Error,
        });
        let ws = herdr_focused_workspace(&socket).unwrap();
        assert_eq!(ws.label, "w7", "empty label falls back to the id");
    }

    #[test]
    fn list_workspaces_returns_all_with_label_or_id() {
        let (socket, _dir) = spawn_server(|method| match method {
            "workspace.list" => Reply::Result(workspace_list_result()),
            _ => Reply::Error,
        });
        let ws = herdr_list_workspaces(&socket);
        assert_eq!(ws.len(), 2, "every workspace becomes a session row");
        assert_eq!(ws[0].id, "w1");
        assert_eq!(ws[0].label, "main");
        assert_eq!(ws[1].id, "w2");
        assert_eq!(ws[1].label, "scratch");
    }

    #[test]
    fn list_workspaces_label_falls_back_to_id() {
        let (socket, _dir) = spawn_server(|method| match method {
            "workspace.list" => Reply::Result(json!({
                "type": "workspace_list",
                "workspaces": [{
                    "workspace_id": "w9",
                    "number": 1,
                    "label": "",
                    "focused": false,
                    "pane_count": 1,
                    "tab_count": 1,
                    "active_tab_id": "tab1",
                    "agent_status": "idle",
                }],
            })),
            _ => Reply::Error,
        });
        let ws = herdr_list_workspaces(&socket);
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].label, "w9", "empty label falls back to the id");
    }

    #[test]
    fn list_workspaces_empty_when_no_workspaces() {
        let (socket, _dir) = spawn_server(|method| match method {
            "workspace.list" => Reply::Result(json!({
                "type": "workspace_list",
                "workspaces": [],
            })),
            _ => Reply::Error,
        });
        assert!(herdr_list_workspaces(&socket).is_empty());
    }

    #[test]
    fn list_workspaces_empty_when_server_down() {
        // No socket file ⇒ server absent ⇒ no session rows (list_sessions analog).
        let missing = PathBuf::from("/definitely/missing/herdr-workspaces.sock");
        assert!(herdr_list_workspaces(&missing).is_empty());
    }

    #[test]
    fn list_workspaces_empty_on_error_reply() {
        let (socket, _dir) = spawn_server(|_| Reply::Error);
        assert!(herdr_list_workspaces(&socket).is_empty());
    }

    #[test]
    fn backend_is_object_safe() {
        let _b: Box<dyn PaneBackend> =
            Box::new(HerdrBackend::with_socket_path(PathBuf::from("/x.sock")));
    }
}
