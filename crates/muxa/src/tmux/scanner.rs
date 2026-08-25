//! Multi-socket tmux pane enumeration.
//!
//! tmux runs as a per-server process; a user can have several servers
//! alive at once (each owning its own Unix socket under `$TMUX_TMPDIR`,
//! defaulting to `/tmp/tmux-$UID/`). The single-socket helpers in
//! [`super`] only see the server pointed at by `$TMUX` / the `default`
//! socket. This module enumerates *every* server we can read and folds
//! their pane lists into a single [`ScanResult`], with per-socket failures
//! collected in [`ScanResult::errors`] rather than aborting the scan.
//!
//! All `tmux` invocations carry a 1-second timeout — a hung server (e.g.
//! a wedged `attach-session` losing the controlling terminal) cannot
//! stall the dashboard.
//!
//! ## Non-tmux hosts
//!
//! The dashboard runs [`scan`] together with every active non-tmux backend and
//! merges their common pane rows into one [`ScanResult`]. Herdr rows are folded
//! by [`herdr_scan_result`]; rmux retains its full endpoint path; zellij can
//! contribute its plugin snapshot. Running only one side would drop panes in a
//! mixed-host migration. `MUXA_TMUX_SOCKET` remains a tmux-only scope.
//!
//! [`HerdrBackend`]: crate::backend::herdr::HerdrBackend

use crate::backend::HostKind;
use crate::tmux::{PaneInfo, PANE_FMT};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::timeout;

/// Per-socket `tmux list-panes` timeout. Picked to stay imperceptible on
/// the dashboard's lazy-refresh path while still giving a healthy server
/// orders of magnitude more than it needs.
const TMUX_LIST_TIMEOUT: Duration = Duration::from_secs(1);

/// One pane, augmented with the tmux socket it came from and a ready-made
/// `tmux attach` invocation a UI can copy to the clipboard. The socket is
/// part of the identity — the same `pane_id` like `%1` exists in every
/// server.
#[derive(Debug, Clone, Serialize)]
pub struct PaneSummary {
    pub host: HostKind,
    pub pane_id: String,
    pub session_id: String,
    pub session: String,
    pub window_id: String,
    pub window_name: String,
    pub window_index: String,
    pub pane_index: String,
    pub tty: String,
    pub current_command: String,
    pub title: String,
    pub current_path: String,
    pub socket: PathBuf,
    pub muxa: MuxaPaneMetadata,
    /// Shell-quoted command that, when run, attaches to this pane. Uses
    /// `tmux -S <socket> attach-session -t <session>` plus
    /// `select-window` / `select-pane` to land on the exact pane.
    pub attach_command: String,
}

/// Muxa-managed logical identity stored in tmux user options. These values
/// survive muxad restarts and are intentionally separate from mutable tmux
/// names. Non-tmux and unmanaged panes serialize a well-formed empty object.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MuxaPaneMetadata {
    pub managed_workspace: bool,
    pub managed_work: bool,
    pub managed_agent: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_stable_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_status: Option<String>,
}

#[derive(Debug)]
struct ScannedPane {
    pane: PaneInfo,
    muxa: MuxaPaneMetadata,
}

#[cfg(test)]
impl ScannedPane {
    fn unmanaged(pane: PaneInfo) -> Self {
        Self {
            pane,
            muxa: MuxaPaneMetadata::default(),
        }
    }
}

/// A single per-socket failure during a scan. We collect these instead of
/// failing the whole scan — one wedged server should not blank the
/// dashboard.
#[derive(Debug, Clone, Serialize)]
pub struct ScanError {
    pub socket: PathBuf,
    pub message: String,
}

/// Aggregate result of a global scan. `panes` and `errors` are always
/// populated independently — a partial scan is the normal case.
#[derive(Debug, Clone, Serialize)]
pub struct ScanResult {
    pub panes: Vec<PaneSummary>,
    pub errors: Vec<ScanError>,
    #[serde(with = "time::serde::rfc3339")]
    pub fetched_at: OffsetDateTime,
}

/// Enumerate every tmux socket file readable by this user across the
/// known socket directories.
///
/// Lookup order: `$TMUX_TMPDIR/tmux-$UID/`, `/tmp/tmux-$UID/`,
/// `/private/tmp/tmux-$UID/` (macOS resolves `/tmp` → `/private/tmp`,
/// so we deduplicate by canonical path). Each entry must be a Unix
/// socket; regular files and subdirectories are filtered out.
pub fn enumerate_sockets() -> Vec<PathBuf> {
    enumerate_sockets_with(scoped_socket(), &default_socket_dirs())
}

/// `MUXA_TMUX_SOCKET`, when set to a tmux socket path, scopes the multi-server
/// pane scan to that single server instead of globbing every socket under the
/// standard dirs. Opt-in — unset keeps the default global view. Lets an
/// isolated context (a demo recording, integration tests, a single-server
/// user) avoid picking up unrelated tmux servers running under the same uid.
fn scoped_socket() -> Option<PathBuf> {
    let v = std::env::var("MUXA_TMUX_SOCKET").ok()?;
    let trimmed = v.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

/// The `MUXA_TMUX_SOCKET` scope canonicalized once for the process lifetime.
/// `None` when the env var is unset/empty. Canonicalization collapses the
/// macOS `/tmp` → `/private/tmp` symlink (and any others) so the scope
/// compares equal to a hook's captured `$TMUX` socket regardless of which
/// form either side spelled.
fn scope_socket_canonical() -> Option<&'static Path> {
    static SCOPE: OnceLock<Option<PathBuf>> = OnceLock::new();
    SCOPE
        .get_or_init(|| scoped_socket().map(|p| p.canonicalize().unwrap_or(p)))
        .as_deref()
}

/// Whether a hook event's captured tmux socket falls within the configured
/// `MUXA_TMUX_SOCKET` scope.
///
/// The daemon calls this on every ingested hook event to drop events from
/// unrelated tmux servers that happen to share muxa's globally-installed
/// agent hooks — e.g. agents another multiplexer (cmux) launches on its own
/// `-L`/`-S` server. Those agents report pane ids from a server muxa doesn't
/// track, so without scoping they surface as unmappable `%NN` ghost rows that
/// no reap/prune pass can hold down (their live hooks keep re-registering
/// them). `MUXA_TMUX_SOCKET` already scopes the pane *scanner*; this extends
/// the same scope to *ingest* so the two agree.
///
/// Keeps the event (`true`) when no scope is configured (the historical
/// global-ingest behavior) or when the event carries no tmux socket
/// (surface/paneless/non-tmux agents are never tmux-scoped). Drops it
/// (`false`) only when a scope is set and the event's socket resolves to a
/// different server.
#[must_use]
pub fn event_tmux_socket_in_scope(event_socket: Option<&str>) -> bool {
    event_in_scope_with(scope_socket_canonical(), event_socket)
}

/// Pure core of [`event_tmux_socket_in_scope`], with the scope injected so
/// it's testable without touching the process-global env cache. `scope` is
/// expected already-canonical (as the cached env scope is).
fn event_in_scope_with(scope: Option<&Path>, event_socket: Option<&str>) -> bool {
    let Some(scope) = scope else {
        return true;
    };
    let Some(sock) = event_socket else {
        return true;
    };
    match Path::new(sock).canonicalize() {
        Ok(p) => p == scope,
        // A socket that no longer exists can't be canonicalized; fall back
        // to a raw compare rather than silently keeping a foreign event.
        Err(_) => Path::new(sock) == scope,
    }
}

fn enumerate_sockets_with(scoped: Option<PathBuf>, dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut socks = match scoped {
        Some(sock) => vec![sock],
        None => enumerate_sockets_in(dirs),
    };
    // Drop dead socket files before any caller spawns a `tmux -S <sock>`
    // against them. tmux only unlinks a server's *own* socket on a clean
    // shutdown, so a server killed abnormally (test timeout, SIGKILL,
    // parent shell dying) leaves an orphan file behind, and tmux has no
    // sweep for other servers' sockets. On a host without a working /tmp
    // reaper (e.g. a container where systemd-tmpfiles never runs) these
    // accumulate without bound — hundreds were observed in the wild. The
    // old code paid a full `tmux` process spawn per orphan on every scan
    // (≈2 ms each → ~0.5 s for a few hundred), which surfaced as a visibly
    // late first paint in `muxa watch`. A connect() refuses instantly on a
    // dead socket, so we keep the per-socket cost at one cheap syscall and
    // only spawn tmux for sockets that actually have a server.
    socks.retain(|p| socket_is_live(p));
    socks
}

/// Best-effort liveness probe for a tmux socket file. A live server is
/// accepting on its socket, so `connect()` succeeds; an orphaned socket file
/// (server gone) refuses immediately with `ECONNREFUSED` — no blocking, no
/// process spawn. We treat *only* "refused" and "not found" as dead; any other
/// error (e.g. permission denied) keeps the socket so we never hide one we
/// merely failed to probe — the worst case there is the old per-socket cost
/// for that single file.
fn socket_is_live(path: &Path) -> bool {
    classify_socket_connect(std::os::unix::net::UnixStream::connect(path).map(|_| ()))
}

fn classify_socket_connect(result: std::io::Result<()>) -> bool {
    use std::io::ErrorKind;
    match result {
        Ok(()) => true,
        Err(e) => !matches!(e.kind(), ErrorKind::ConnectionRefused | ErrorKind::NotFound),
    }
}

fn default_socket_dirs() -> Vec<PathBuf> {
    let uid = current_uid();
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(d) = std::env::var("TMUX_TMPDIR") {
        let trimmed = d.trim();
        if !trimmed.is_empty() {
            dirs.push(PathBuf::from(trimmed).join(format!("tmux-{uid}")));
        }
    }
    dirs.push(PathBuf::from(format!("/tmp/tmux-{uid}")));
    dirs.push(PathBuf::from(format!("/private/tmp/tmux-{uid}")));
    dirs
}

/// Inner enumerator with the search dirs injected — the unit tests use
/// this to point at a temp fixture instead of the real `/tmp`.
fn enumerate_sockets_in(dirs: &[PathBuf]) -> Vec<PathBuf> {
    use std::os::unix::fs::FileTypeExt;
    let mut out = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else {
                continue;
            };
            if !ft.is_socket() {
                continue;
            }
            let path = entry.path();
            let canon = path.canonicalize().unwrap_or_else(|_| path.clone());
            if seen.insert(canon) {
                out.push(path);
            }
        }
    }
    out
}

/// Get the current user's UID. Cached for the process lifetime — stable
/// data, called on every scan. Forbidden-unsafe-code shop, so we shell
/// out to `id -u` rather than reach for `libc::getuid()`. UID 0 (root)
/// on failure is the least-surprising fallback: if we can't introspect
/// our own UID, everything else is broken too.
fn current_uid() -> u32 {
    static CACHED: OnceLock<u32> = OnceLock::new();
    *CACHED.get_or_init(|| {
        std::process::Command::new("id")
            .arg("-u")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    })
}

/// Run a global scan. Per-socket failures are collected, never raised.
pub async fn scan() -> ScanResult {
    let sockets = enumerate_sockets();
    scan_with(sockets, list_panes_for_socket).await
}

/// Generic scan entry point — runs `fetcher` against each socket and
/// aggregates the results. Public-in-crate so the unit tests can swap in
/// a deterministic fetcher.
async fn scan_with<F, Fut>(sockets: Vec<PathBuf>, mut fetcher: F) -> ScanResult
where
    F: FnMut(PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<ScannedPane>, String>>,
{
    let mut panes: Vec<PaneSummary> = Vec::new();
    let mut errors: Vec<ScanError> = Vec::new();
    for sock in sockets {
        match fetcher(sock.clone()).await {
            Ok(items) => {
                for pane in items {
                    panes.push(to_summary(pane, sock.clone()));
                }
            }
            Err(message) => errors.push(ScanError {
                socket: sock,
                message,
            }),
        }
    }
    ScanResult {
        panes,
        errors,
        fetched_at: OffsetDateTime::now_utc(),
    }
}

async fn list_panes_for_socket(sock: PathBuf) -> Result<Vec<ScannedPane>, String> {
    let sock_str = sock
        .to_str()
        .ok_or_else(|| "non-utf8 socket path".to_string())?;
    let fut = Command::new(crate::tmux::tmux_binary())
        .env("LC_ALL", "en_US.UTF-8")
        .args(["-S", sock_str, "list-panes", "-a", "-F", PANE_FMT])
        .output();
    let out = timeout(TMUX_LIST_TIMEOUT, fut)
        .await
        .map_err(|_| format!("timed out after {TMUX_LIST_TIMEOUT:?}"))?
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        // Stale socket file with no server bound — common when a tmux
        // session crashes without unlinking, or when a recording tool
        // (vhs, asciinema) leaves a `muxa-demo` socket behind. tmux
        // emits this exact phrase as a stable indicator; treat it as
        // "no panes here" rather than a scan failure so the dashboard
        // doesn't surface it as an error.
        if is_no_server_running(&stderr) {
            return Ok(Vec::new());
        }
        return Err(stderr);
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let socket = crate::tmux::socket_short_name(sock_str);
    Ok(parse_scanned_panes(&stdout, &socket))
}

fn parse_scanned_panes(stdout: &str, socket: &str) -> Vec<ScannedPane> {
    crate::tmux::parse_pane_lines_for_socket(stdout, Some(socket))
        .into_iter()
        .zip(stdout.lines().filter(|line| line.split('\t').count() >= 7))
        .map(|(pane, line)| {
            let columns = line.split('\t').collect::<Vec<_>>();
            let value = |index: usize| {
                columns
                    .get(index)
                    .map(|value| value.trim())
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            };
            let workspace_id = value(22).or_else(|| value(12));
            let work_id = value(23).or_else(|| value(15));
            ScannedPane {
                pane,
                muxa: MuxaPaneMetadata {
                    managed_workspace: columns.get(14).is_some_and(|value| *value == "1"),
                    managed_work: columns.get(17).is_some_and(|value| *value == "1"),
                    managed_agent: columns.get(21).is_some_and(|value| *value == "1"),
                    workspace_id,
                    workspace_cwd: value(13),
                    work_id,
                    work_cwd: value(16),
                    agent: value(18),
                    role: value(19),
                    task: value(20),
                    external_source: value(24),
                    external_scope: value(25),
                    external_stable_id: value(26),
                    external_key: value(27),
                    external_title: value(28),
                    external_url: value(29),
                    external_status: value(30),
                },
            }
        })
        .collect()
}

fn is_no_server_running(stderr: &str) -> bool {
    stderr.starts_with("no server running on")
}

fn to_summary(scanned: ScannedPane, socket: PathBuf) -> PaneSummary {
    let p = scanned.pane;
    let attach_command = format!(
        "tmux -S {sock} attach-session -t {sess} \\; select-window -t {sess}:{win} \\; select-pane -t {pane}",
        sock = shell_quote(&socket),
        sess = p.session,
        win = p.window_index,
        pane = p.pane_id,
    );
    PaneSummary {
        host: HostKind::Tmux,
        pane_id: p.pane_id,
        session_id: p.session_id,
        session: p.session,
        window_id: p.window_id,
        window_name: p.window_name,
        window_index: p.window_index,
        pane_index: p.pane_index,
        tty: p.tty,
        current_command: p.current_command,
        title: p.title,
        current_path: p.current_path,
        socket,
        muxa: scanned.muxa,
        attach_command,
    }
}

/// The synthetic socket identity stamped on every herdr [`PaneSummary`].
///
/// A herdr [`PaneInfo`] carries `socket: None` (herdr never reuses the
/// tmux-socket channel — see `backend::herdr::to_pane_info`), but the
/// dashboard's [`PaneSummary`] socket is a required part of a pane's
/// identity and the web UI splits it on `/` for the socket-filter chip.
/// The daemon observes exactly one herdr server, so a single constant
/// "server" name is both accurate and gives the UI one clean chip.
const HERDR_SOCKET_LABEL: &str = "herdr";

/// Fold the daemon's herdr `pane.list` result into the dashboard's
/// [`ScanResult`] shape, so `/api/panes` renders herdr panes without the
/// tmux multi-socket scanner (which sees nothing on a herdr host).
///
/// Called from the dashboard pane-cache refresh closure when the active
/// backend is herdr; the [`scan`] tmux path is untouched. The muxa
/// [`PaneInfo`] rows already carry herdr-native fields — `pane_id`
/// `herdr:<id>`, `session` = workspace id, `window_index` = tab id,
/// `current_command`/`current_path` enriched from `pane.process_info` —
/// so this only supplies the two dashboard-only fields the herdr backend
/// leaves blank: a synthetic [`HERDR_SOCKET_LABEL`] socket identity and an
/// empty `attach_command` (herdr has no copyable shell attach line; the
/// CLI attaches over the socket via `pane.focus`). `errors` is always
/// empty — a single in-process backend call has no per-socket partial
/// failures to collect. `MUXA_TMUX_SOCKET` is not consulted: it scopes
/// tmux sockets only, and herdr panes are never subject to it.
pub(crate) fn herdr_scan_result(panes: Vec<PaneInfo>) -> ScanResult {
    ScanResult {
        panes: panes.into_iter().map(to_herdr_summary).collect(),
        errors: Vec::new(),
        fetched_at: OffsetDateTime::now_utc(),
    }
}

fn to_herdr_summary(p: PaneInfo) -> PaneSummary {
    PaneSummary {
        host: HostKind::Herdr,
        pane_id: p.pane_id,
        session_id: p.session_id,
        session: p.session,
        window_id: p.window_id,
        window_name: p.window_name,
        window_index: p.window_index,
        pane_index: p.pane_index,
        tty: p.tty,
        current_command: p.current_command,
        title: p.title,
        current_path: p.current_path,
        socket: PathBuf::from(HERDR_SOCKET_LABEL),
        muxa: MuxaPaneMetadata::default(),
        // herdr has no `tmux attach`-style command a user copies; the CLI
        // (`muxa` jump / `jump_to_pane`) focuses over the socket instead.
        attach_command: String::new(),
    }
}

fn shell_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    if s.bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'))
    {
        s.into_owned()
    } else {
        // Single-quote and escape embedded single quotes the standard
        // POSIX way: ' -> '\''.
        let escaped = s.replace('\'', "'\\''");
        format!("'{escaped}'")
    }
}

/// TTL-based cache for [`scan`] results. Refreshes are pull-based —
/// `get_or_refresh` calls the supplied refresh closure only when the
/// cached value is stale or absent. Cheap to clone; cheap to share via
/// `Arc<PaneCache>` across handler tasks.
#[derive(Debug)]
pub struct PaneCache {
    ttl: Duration,
    inner: Mutex<Option<Cached>>,
}

#[derive(Debug, Clone)]
struct Cached {
    result: ScanResult,
    refreshed_at: Instant,
}

impl PaneCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            inner: Mutex::new(None),
        }
    }

    /// Return the cached result if fresh; otherwise call `refresh` and
    /// store its output. The async mutex is held across refresh so concurrent
    /// dashboard requests coalesce onto one tmux scan.
    pub async fn get_or_refresh<F, Fut>(&self, refresh: F) -> ScanResult
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ScanResult>,
    {
        let mut guard = self.inner.lock().await;
        if let Some(cached) = guard.as_ref() {
            if cached.refreshed_at.elapsed() < self.ttl {
                return cached.result.clone();
            }
        }
        let result = refresh().await;
        *guard = Some(Cached {
            result: result.clone(),
            refreshed_at: Instant::now(),
        });
        result
    }
}

impl Default for PaneCache {
    fn default() -> Self {
        Self::new(Duration::from_secs(2))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    fn fake_pane(id: &str, session: &str) -> PaneInfo {
        PaneInfo {
            agent_role: None,
            agent_alias: None,
            work_done: Vec::new(),
            socket: None,
            pane_id: id.into(),
            session_id: String::new(),
            session: session.into(),
            window_id: String::new(),
            window_name: String::new(),
            window_index: "0".into(),
            pane_index: "0".into(),
            tty: "/dev/pts/0".into(),
            current_command: "zsh".into(),
            title: String::new(),
            pane_pid: 0,
            current_path: String::new(),
        }
    }

    fn empty_scan() -> ScanResult {
        ScanResult {
            panes: Vec::new(),
            errors: Vec::new(),
            fetched_at: OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn enumerate_sockets_in_filters_to_socket_files() {
        let dir = tempfile::tempdir().unwrap();
        // Regular file — should be ignored.
        std::fs::write(dir.path().join("not-a-socket.txt"), b"hi").unwrap();
        // Subdirectory — should be ignored.
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        // Real Unix socket — bind via UnixListener (sync std works fine).
        let sock_path = dir.path().join("default");
        let _listener = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();

        let found = enumerate_sockets_in(&[dir.path().to_path_buf()]);
        assert_eq!(found.len(), 1, "expected only the socket: {found:?}");
        assert_eq!(found[0], sock_path);
    }

    #[test]
    fn enumerate_sockets_with_scopes_to_a_single_socket() {
        let dir = tempfile::tempdir().unwrap();
        let scoped = dir.path().join("muxa-demo");
        let _s = std::os::unix::net::UnixListener::bind(&scoped).unwrap();
        // A sibling live socket that a global scan would also pick up.
        let other = dir.path().join("default");
        let _o = std::os::unix::net::UnixListener::bind(&other).unwrap();

        // Scoped: only the named socket, sibling ignored even though it's live.
        let scoped_found =
            enumerate_sockets_with(Some(scoped.clone()), &[dir.path().to_path_buf()]);
        assert_eq!(scoped_found, vec![scoped]);

        // Unscoped: the full dir scan finds both live sockets.
        let all = enumerate_sockets_with(None, &[dir.path().to_path_buf()]);
        assert_eq!(all.len(), 2, "unscoped scan should see both: {all:?}");
    }

    #[test]
    fn event_in_scope_keeps_everything_when_unscoped() {
        // No scope configured → every event is in scope, including foreign
        // sockets. This is the historical global-ingest default.
        assert!(event_in_scope_with(None, Some("/tmp/anything")));
        assert!(event_in_scope_with(None, None));
    }

    #[test]
    fn event_in_scope_drops_foreign_socket_but_keeps_scoped_and_paneless() {
        let dir = tempfile::tempdir().unwrap();
        let scope_path = dir.path().join("default");
        let _scope = std::os::unix::net::UnixListener::bind(&scope_path).unwrap();
        let foreign = dir.path().join("cmux-debug");
        let _foreign = std::os::unix::net::UnixListener::bind(&foreign).unwrap();

        // Production caches the scope already-canonical; mirror that here.
        let scope = scope_path.canonicalize().unwrap();

        // Same server → kept, even if the caller spelled the path uncanonically.
        assert!(event_in_scope_with(
            Some(&scope),
            Some(scope_path.to_str().unwrap())
        ));
        // A different live server (the cmux case) → dropped.
        assert!(!event_in_scope_with(
            Some(&scope),
            Some(foreign.to_str().unwrap())
        ));
        // A socket-less event (surface/paneless/non-tmux) is never scoped out.
        assert!(event_in_scope_with(Some(&scope), None));
        // A vanished socket path can't canonicalize; raw compare still drops
        // a clearly-foreign one.
        assert!(!event_in_scope_with(
            Some(&scope),
            Some("/tmp/tmux-1000/gone")
        ));
    }

    #[test]
    fn enumerate_sockets_in_dedupes_canonical_paths() {
        // /tmp and /private/tmp resolve to the same dir on macOS — the
        // enumerator should not double-count. We simulate by listing the
        // same directory twice.
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("default");
        let _l = std::os::unix::net::UnixListener::bind(&sock_path).unwrap();

        let dirs = vec![dir.path().to_path_buf(), dir.path().to_path_buf()];
        let found = enumerate_sockets_in(&dirs);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn enumerate_sockets_in_skips_missing_dirs() {
        // Non-existent dirs should not panic or short-circuit later
        // (existing) dirs.
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("default");
        let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();

        let dirs = vec![
            PathBuf::from("/definitely/does/not/exist/0xabad1dea"),
            dir.path().to_path_buf(),
        ];
        let found = enumerate_sockets_in(&dirs);
        assert_eq!(found, vec![sock]);
    }

    #[test]
    fn socket_is_live_distinguishes_listening_from_orphan() {
        let dir = tempfile::tempdir().unwrap();

        // Live: a bound listener accepts, so connect() succeeds.
        let live = dir.path().join("live");
        let _listener = std::os::unix::net::UnixListener::bind(&live).unwrap();
        assert!(socket_is_live(&live));

        // Classify the two dead outcomes deterministically. Exercising a
        // dropped listener with a real connect is flaky under the full
        // parallel suite: process-wide fd pressure can replace ECONNREFUSED
        // with EMFILE, which production deliberately treats as unknown/live.
        assert!(!classify_socket_connect(Err(std::io::Error::from(
            std::io::ErrorKind::ConnectionRefused
        ))));
        assert!(!classify_socket_connect(Err(std::io::Error::from(
            std::io::ErrorKind::NotFound
        ))));
        assert!(classify_socket_connect(Err(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        ))));
    }

    #[test]
    fn no_server_running_phrase_is_recognised() {
        // tmux's stable phrasing for a stale socket file. Trim leading
        // whitespace mirrors what `String::from_utf8_lossy(...).trim()`
        // produces in the scanner.
        assert!(is_no_server_running(
            "no server running on /tmp/tmux-1000/default"
        ));
        assert!(is_no_server_running("no server running on /weird/path"));
        // Anything else stays an error.
        assert!(!is_no_server_running(
            "error connecting to /tmp/tmux-1000/default (Permission denied)"
        ));
        assert!(!is_no_server_running(""));
        assert!(!is_no_server_running("server exiting"));
    }

    #[test]
    fn parse_scanned_panes_reads_managed_workspace_work_and_agent_metadata() {
        let line = [
            "%7",
            "physical-session",
            "2",
            "0",
            "/dev/pts/7",
            "codex",
            "agent",
            "4242",
            "/work/payments",
            "$7",
            "@7",
            "shell",
            "workspace-from-session",
            "/work/payments",
            "1",
            "work-from-window",
            "/work/payments/ticket",
            "1",
            "codex-primary",
            "implementer",
            "repair settlement retries",
            "1",
            "payments",
            "PAY-42",
        ]
        .join("\t");

        let panes = parse_scanned_panes(&line, "default");
        assert_eq!(panes.len(), 1);
        let pane = &panes[0];
        assert_eq!(pane.pane.socket.as_deref(), Some("default"));
        assert!(pane.muxa.managed_workspace);
        assert!(pane.muxa.managed_work);
        assert!(pane.muxa.managed_agent);
        assert_eq!(pane.muxa.workspace_id.as_deref(), Some("payments"));
        assert_eq!(pane.muxa.work_id.as_deref(), Some("PAY-42"));
        assert_eq!(pane.muxa.role.as_deref(), Some("implementer"));
        assert_eq!(pane.muxa.task.as_deref(), Some("repair settlement retries"));
    }

    #[tokio::test]
    async fn scan_with_one_bad_socket_does_not_fail_the_whole_scan() {
        let s_good = PathBuf::from("/run/fake/good");
        let s_bad = PathBuf::from("/run/fake/bad");
        let result = scan_with(vec![s_good.clone(), s_bad.clone()], |sock| async move {
            if sock.file_name().and_then(|s| s.to_str()) == Some("good") {
                Ok(vec![ScannedPane::unmanaged(fake_pane("%1", "main"))])
            } else {
                Err("simulated tmux failure".to_string())
            }
        })
        .await;
        assert_eq!(result.panes.len(), 1, "{result:?}");
        assert_eq!(result.panes[0].socket, s_good);
        assert_eq!(result.panes[0].pane_id, "%1");
        assert_eq!(result.errors.len(), 1);
        assert_eq!(result.errors[0].socket, s_bad);
        assert!(result.errors[0].message.contains("simulated"));
    }

    #[tokio::test]
    async fn scan_with_attaches_socket_and_attach_command_per_pane() {
        let s = PathBuf::from("/tmp/tmux-1000/default");
        let result = scan_with(vec![s.clone()], |_| async {
            Ok(vec![ScannedPane::unmanaged(fake_pane("%7", "work"))])
        })
        .await;
        assert_eq!(result.panes.len(), 1);
        let p = &result.panes[0];
        assert_eq!(p.socket, s);
        let cmd = &p.attach_command;
        assert!(cmd.contains("attach-session -t work"), "{cmd}");
        assert!(cmd.contains("-S /tmp/tmux-1000/default"), "{cmd}");
        assert!(cmd.contains("select-pane -t %7"), "{cmd}");
    }

    #[tokio::test]
    async fn cache_returns_within_ttl_without_refresh() {
        let cache = PaneCache::new(Duration::from_secs(60));
        let counter = Arc::new(AtomicUsize::new(0));

        let c = counter.clone();
        let r1 = cache
            .get_or_refresh(|| async move {
                c.fetch_add(1, Ordering::SeqCst);
                empty_scan()
            })
            .await;

        let c = counter.clone();
        let r2 = cache
            .get_or_refresh(|| async move {
                c.fetch_add(1, Ordering::SeqCst);
                empty_scan()
            })
            .await;

        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "second hit should not refresh"
        );
        assert_eq!(r1.fetched_at, r2.fetched_at);
    }

    #[tokio::test]
    async fn cache_coalesces_concurrent_refreshes() {
        let cache = Arc::new(PaneCache::new(Duration::from_secs(60)));
        let counter = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let cache = Arc::clone(&cache);
            let counter = Arc::clone(&counter);
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                barrier.wait().await;
                cache
                    .get_or_refresh(|| async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        empty_scan()
                    })
                    .await
            }));
        }
        barrier.wait().await;
        for task in tasks {
            task.await.unwrap();
        }
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cache_refreshes_after_ttl() {
        let cache = PaneCache::new(Duration::from_millis(10));
        let counter = Arc::new(AtomicUsize::new(0));

        let c = counter.clone();
        cache
            .get_or_refresh(|| async move {
                c.fetch_add(1, Ordering::SeqCst);
                empty_scan()
            })
            .await;

        tokio::time::sleep(Duration::from_millis(25)).await;

        let c = counter.clone();
        cache
            .get_or_refresh(|| async move {
                c.fetch_add(1, Ordering::SeqCst);
                empty_scan()
            })
            .await;

        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn herdr_scan_result_maps_panes_and_stamps_synthetic_socket() {
        // Mirror what `HerdrBackend::list_panes` hands the dashboard: a
        // muxa PaneInfo already namespaced/enriched, socket None.
        let mut pane = fake_pane("herdr:p1", "ws1");
        pane.window_index = "tab1".into();
        pane.pane_index = "p1".into();
        pane.current_command = "vim".into();
        pane.title = "editor".into();
        pane.current_path = "/home/u/proj".into();

        let result = herdr_scan_result(vec![pane]);
        assert!(
            result.errors.is_empty(),
            "single backend call has no per-socket errors"
        );
        assert_eq!(result.panes.len(), 1);
        let s = &result.panes[0];
        assert_eq!(s.pane_id, "herdr:p1");
        assert_eq!(s.session, "ws1", "session ← workspace_id");
        assert_eq!(s.window_index, "tab1", "window_index ← tab_id");
        assert_eq!(s.pane_index, "p1", "pane_index is the raw herdr id");
        assert_eq!(s.current_command, "vim");
        assert_eq!(s.title, "editor");
        assert_eq!(
            s.socket,
            PathBuf::from(HERDR_SOCKET_LABEL),
            "herdr panes carry the synthetic socket identity, not None",
        );
        assert!(
            s.attach_command.is_empty(),
            "herdr has no copyable tmux-style attach command",
        );
    }

    #[test]
    fn herdr_scan_result_empty_input_is_well_formed() {
        let result = herdr_scan_result(Vec::new());
        assert!(result.panes.is_empty());
        assert!(result.errors.is_empty());
    }

    #[test]
    fn shell_quote_passes_through_simple_paths() {
        assert_eq!(
            shell_quote(Path::new("/tmp/tmux-1000/default")),
            "/tmp/tmux-1000/default"
        );
    }

    #[test]
    fn shell_quote_escapes_spaces_and_quotes() {
        assert_eq!(shell_quote(Path::new("/tmp/has space")), "'/tmp/has space'");
        assert_eq!(
            shell_quote(Path::new("/tmp/it's/weird")),
            "'/tmp/it'\\''s/weird'"
        );
    }
}
