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

use crate::tmux::{parse_pane_lines, PaneInfo, PANE_FMT};
use serde::Serialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use tokio::process::Command;
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
    pub pane_id: String,
    pub session: String,
    pub window_index: String,
    pub pane_index: String,
    pub tty: String,
    pub current_command: String,
    pub title: String,
    pub socket: PathBuf,
    /// Shell-quoted command that, when run, attaches to this pane. Uses
    /// `tmux -S <socket> attach-session -t <session>` plus
    /// `select-window` / `select-pane` to land on the exact pane.
    pub attach_command: String,
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
    enumerate_sockets_in(&default_socket_dirs())
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
pub(crate) async fn scan_with<F, Fut>(sockets: Vec<PathBuf>, mut fetcher: F) -> ScanResult
where
    F: FnMut(PathBuf) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<PaneInfo>, String>>,
{
    let mut panes: Vec<PaneSummary> = Vec::new();
    let mut errors: Vec<ScanError> = Vec::new();
    for sock in sockets {
        match fetcher(sock.clone()).await {
            Ok(items) => {
                for p in items {
                    panes.push(to_summary(p, sock.clone()));
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

async fn list_panes_for_socket(sock: PathBuf) -> Result<Vec<PaneInfo>, String> {
    let sock_str = sock
        .to_str()
        .ok_or_else(|| "non-utf8 socket path".to_string())?;
    let fut = Command::new("tmux")
        .args(["-S", sock_str, "list-panes", "-a", "-F", PANE_FMT])
        .output();
    let out = timeout(TMUX_LIST_TIMEOUT, fut)
        .await
        .map_err(|_| format!("timed out after {TMUX_LIST_TIMEOUT:?}"))?
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ok(parse_pane_lines(&stdout))
}

fn to_summary(p: PaneInfo, socket: PathBuf) -> PaneSummary {
    let attach_command = format!(
        "tmux -S {sock} attach-session -t {sess} \\; select-window -t {sess}:{win} \\; select-pane -t {pane}",
        sock = shell_quote(&socket),
        sess = p.session,
        win = p.window_index,
        pane = p.pane_id,
    );
    PaneSummary {
        pane_id: p.pane_id,
        session: p.session,
        window_index: p.window_index,
        pane_index: p.pane_index,
        tty: p.tty,
        current_command: p.current_command,
        title: p.title,
        socket,
        attach_command,
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
    /// store its output. Concurrent callers may both end up refreshing —
    /// the last to write wins. Acceptable: scans are idempotent and
    /// short.
    pub async fn get_or_refresh<F, Fut>(&self, refresh: F) -> ScanResult
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ScanResult>,
    {
        if let Some(fresh) = self.fresh_cached() {
            return fresh;
        }
        let result = refresh().await;
        let mut guard = self.inner.lock().expect("PaneCache mutex poisoned");
        *guard = Some(Cached {
            result: result.clone(),
            refreshed_at: Instant::now(),
        });
        result
    }

    fn fresh_cached(&self) -> Option<ScanResult> {
        let guard = self.inner.lock().expect("PaneCache mutex poisoned");
        let cached = guard.as_ref()?;
        if cached.refreshed_at.elapsed() < self.ttl {
            Some(cached.result.clone())
        } else {
            None
        }
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
            pane_id: id.into(),
            session: session.into(),
            window_index: "0".into(),
            pane_index: "0".into(),
            tty: "/dev/pts/0".into(),
            current_command: "zsh".into(),
            title: String::new(),
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

    #[tokio::test]
    async fn scan_with_one_bad_socket_does_not_fail_the_whole_scan() {
        let s_good = PathBuf::from("/run/fake/good");
        let s_bad = PathBuf::from("/run/fake/bad");
        let result = scan_with(vec![s_good.clone(), s_bad.clone()], |sock| async move {
            if sock.file_name().and_then(|s| s.to_str()) == Some("good") {
                Ok(vec![fake_pane("%1", "main")])
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
            Ok(vec![fake_pane("%7", "work")])
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
