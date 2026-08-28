//! Small utilities shared across the init wizard's submodules.
//!
//! Lives here (instead of inside any one file) because at least two
//! consumers (`detect.rs`, `files/launchd.rs`) need the same uid +
//! socket-probe helpers, and we don't want them to drift.

use std::path::{Path, PathBuf};
use std::process::Command;

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
    crate::daemon::socket_responding(socket)
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
}
