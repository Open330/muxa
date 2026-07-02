//! Small utilities shared across the init wizard's submodules.
//!
//! Lives here (instead of inside any one file) because at least two
//! consumers (`detect.rs`, `files/launchd.rs`) need the same uid +
//! socket-probe helpers, and we don't want them to drift.

use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// The current process's POSIX uid as a decimal string. Falls back to
/// `"501"` (typical macOS user) only when `id -u` itself fails — exotic
/// enough that any well-formed install will have already failed
/// elsewhere.
pub fn uid_string() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "501".into(), |s| s.trim().to_string())
}

/// Where `muxad` listens by default. Delegates to
/// `muxa::paths::default_socket` so the wizard's probe path can never
/// drift from the daemon's actual binding logic.
pub fn default_muxad_socket() -> PathBuf {
    muxa::paths::default_socket()
}

/// Resolve the absolute path to the `muxa` CLI binary.
///
/// Used by integrations that invoke `muxa` from a context where PATH
/// may not include `~/.cargo/bin` — e.g. a TypeScript extension spawned
/// inside another agent's process. Mirrors [`locate_muxad`] in
/// `files/launchd.rs`: PATH first, then the well-known install
/// locations, finally a bare `muxa` fallback (relies on PATH again).
pub fn locate_muxa() -> String {
    if let Ok(path) = which::which("muxa") {
        return path.to_string_lossy().into_owned();
    }
    let candidates: &[PathBuf] = &[
        dirs::home_dir()
            .map(|h| h.join(".cargo/bin/muxa"))
            .unwrap_or_default(),
        PathBuf::from("/opt/homebrew/bin/muxa"),
        PathBuf::from("/usr/local/bin/muxa"),
    ];
    for cand in candidates {
        if cand.is_file() {
            return cand.to_string_lossy().into_owned();
        }
    }
    "muxa".into()
}

/// Is `muxad` actually serving requests on `socket`? Lightweight: we
/// just confirm we can establish a unix-socket connection. A
/// listening daemon accepts the connect immediately; a stale socket
/// or missing daemon errors in microseconds.
///
/// This is materially better than `pgrep -x muxad` — the latter
/// returns true for zombie processes whose listen socket has gone
/// away, which is exactly the failure mode users hit when an old
/// muxad died but its pid was still around.
pub fn muxad_responsive(socket: &Path) -> bool {
    UnixStream::connect(socket).is_ok()
}

/// Block until either `muxad_responsive(socket)` returns true or the
/// timeout elapses, polling every `interval`. Returns whether muxad
/// became responsive in time.
///
/// Why polling beats a flat sleep: systemd's `enable --now` and
/// launchd's `bootstrap` return as soon as the spawn is *initiated*,
/// not when the child has bound its socket. A 300 ms guess works on
/// fast hardware and fails on cold-cached / VM / CI runners. Polling
/// adapts: typical hot-path hits the first iteration (<20 ms) and
/// only slow boots actually wait.
pub fn wait_for_muxad(socket: &Path, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let interval = Duration::from_millis(20);
    loop {
        if muxad_responsive(socket) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(interval);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use tempfile::tempdir;

    #[test]
    fn uid_string_returns_decimal() {
        let s = uid_string();
        assert!(
            s.chars().all(|c| c.is_ascii_digit()),
            "uid_string `{s}` should be decimal"
        );
    }

    #[test]
    fn default_socket_path_is_absolute() {
        let p = default_muxad_socket();
        assert!(p.is_absolute(), "{p:?} should be absolute");
    }

    #[test]
    fn responsive_false_when_socket_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("absent.sock");
        assert!(!muxad_responsive(&path));
    }

    #[test]
    fn responsive_true_when_listener_bound() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        assert!(muxad_responsive(&path));
    }

    #[test]
    fn wait_returns_immediately_when_already_up() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("up.sock");
        let _listener = UnixListener::bind(&path).unwrap();
        let start = Instant::now();
        let ok = wait_for_muxad(&path, Duration::from_millis(500));
        assert!(ok);
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn wait_times_out_when_never_up() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("never.sock");
        let start = Instant::now();
        let ok = wait_for_muxad(&path, Duration::from_millis(60));
        assert!(!ok);
        let elapsed = start.elapsed();
        // Should respect the timeout reasonably tightly. We allow
        // some slack for the polling interval + scheduler jitter.
        assert!(elapsed >= Duration::from_millis(60));
        assert!(elapsed < Duration::from_millis(250));
    }
}
