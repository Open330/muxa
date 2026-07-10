//! `~/Library/LaunchAgents/dev.open330.muxad.plist` installer.
//!
//! macOS analogue of `systemd.rs`. Same shape: pure content layer
//! that decides what the file should look like, plus a thin shell-out
//! driver for `launchctl bootstrap` / `bootout`.
//!
//! Why a per-user `LaunchAgent` and not a system-wide `LaunchDaemon`: muxad
//! holds per-user IPC state (the socket lives under `/tmp/muxa-<uid>.sock`)
//! and is opt-in; nothing in muxa wants to touch system-level scope.

use crate::init::marker::Outcome;
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use std::process::Command;

pub const LABEL: &str = "dev.open330.muxad";
pub const UNIT_FILENAME: &str = "dev.open330.muxad.plist";

/// `~/Library/LaunchAgents/dev.open330.muxad.plist`. Returns `None`
/// only if `$HOME` is unset, which is exotic enough to surface as a
/// caller-side error.
pub fn default_unit_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/LaunchAgents").join(UNIT_FILENAME))
}

/// Build the plist body. We resolve `muxad`'s on-disk path at install
/// time rather than relying on `$PATH`, since launchd unsets a lot of
/// the user's environment.
pub fn render_plist(muxad_path: &str) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(640);
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    s.push_str("<plist version=\"1.0\">\n");
    s.push_str("<dict>\n");
    s.push_str("  <key>Label</key>\n");
    let _ = writeln!(s, "  <string>{LABEL}</string>");
    s.push_str("  <key>ProgramArguments</key>\n");
    s.push_str("  <array>\n");
    let _ = writeln!(s, "    <string>{muxad_path}</string>");
    s.push_str("  </array>\n");
    s.push_str("  <key>RunAtLoad</key>\n");
    s.push_str("  <true/>\n");
    s.push_str("  <key>KeepAlive</key>\n");
    s.push_str("  <true/>\n");
    // muxad serves latency-sensitive menu-bar and tmux status-line IPC over a
    // Unix socket. launchd's Adaptive class only boosts jobs for XPC activity,
    // so it cannot observe these requests; Background can therefore starve the
    // daemon for seconds under CPU pressure. Interactive is appropriate here
    // because the foreground UI's responsiveness directly depends on muxad.
    s.push_str("  <key>ProcessType</key>\n");
    s.push_str("  <string>Interactive</string>\n");
    // Raise the file-descriptor soft limit well above launchd's stingy
    // default (256 on macOS). The daemon caps concurrent IPC handlers below
    // this, so it's headroom, not a hard reliance — but a 256-fd ceiling is
    // exactly what let a burst of hung hook connections wedge `accept()`.
    s.push_str("  <key>SoftResourceLimits</key>\n");
    s.push_str("  <dict>\n");
    s.push_str("    <key>NumberOfFiles</key>\n");
    s.push_str("    <integer>4096</integer>\n");
    s.push_str("  </dict>\n");
    s.push_str("  <key>StandardOutPath</key>\n");
    s.push_str("  <string>/tmp/muxad.log</string>\n");
    s.push_str("  <key>StandardErrorPath</key>\n");
    s.push_str("  <string>/tmp/muxad.err</string>\n");
    s.push_str("</dict>\n");
    s.push_str("</plist>\n");
    s
}

/// Compute the install outcome given the file's current content and
/// the target plist body. Pure — caller does the I/O.
pub fn upsert(existing: Option<&str>, want: &str) -> (String, Outcome) {
    match existing {
        None => (want.to_string(), Outcome::Inserted),
        Some(prev) if prev == want => (want.to_string(), Outcome::Unchanged),
        Some(_) => (want.to_string(), Outcome::Replaced),
    }
}

/// Is `launchctl` runnable on this host? Effectively "are we on
/// macOS with a real user session". Returns false in CI containers
/// even if they're macOS images, since launchd isn't running there.
pub fn launchctl_available() -> bool {
    Command::new("launchctl")
        .arg("help")
        .output()
        .is_ok_and(|o| o.status.success() || o.status.code() == Some(64))
    // `launchctl help` exits 64 (EX_USAGE) on some versions but still
    // proves the binary works; treat that as "available".
}

/// Best-effort lookup of `muxad` for the plist's `ProgramArguments`.
/// `which::which` covers the standard case; the static fallbacks
/// behind it pick up the common cargo / Homebrew layouts in priority
/// order so a brew-installed muxad on macOS doesn't silently land at
/// the wrong path on first install.
pub fn locate_muxad() -> String {
    if let Ok(path) = which::which("muxad") {
        return path.to_string_lossy().into_owned();
    }
    let candidates: &[PathBuf] = &[
        dirs::home_dir()
            .map(|h| h.join(".cargo/bin/muxad"))
            .unwrap_or_default(),
        PathBuf::from("/opt/homebrew/bin/muxad"),
        PathBuf::from("/usr/local/bin/muxad"),
    ];
    for cand in candidates {
        if cand.is_file() {
            return cand.to_string_lossy().into_owned();
        }
    }
    "muxad".into()
}

/// Reload the plist with `bootout` → `bootstrap`, then `kickstart -k` so the
/// agent comes up immediately. A plain kickstart only reloads the executable;
/// launchd keeps cached plist properties such as `ProcessType`, resource
/// limits, and log paths. The sleep is load-bearing: bootstrapping immediately
/// after bootout can return EIO while launchd is still tearing down the old
/// job. 1.5 s is empirically enough on Apple silicon + Intel.
pub fn enable_service(plist_path: &std::path::Path) -> Result<()> {
    let target = format!("gui/{}", super::super::util::uid_string());
    let label_target = format!("{target}/{LABEL}");

    // `bootout` is best-effort: it returns "No such process" (errno 3) when
    // the agent isn't loaded, which is equivalent to the desired state.
    let _ = Command::new("launchctl")
        .args(["bootout", &label_target])
        .output();
    // Critical sleep — without it, bootstrap right after bootout
    // hits EIO because launchd hasn't finished tearing down the
    // previous agent. 1.5 s is a generous buffer; the no-bootout
    // case (fresh install) doesn't hit this code path.
    std::thread::sleep(std::time::Duration::from_millis(1500));

    let status = Command::new("launchctl")
        .args(["bootstrap", &target])
        .arg(plist_path)
        .status()
        .with_context(|| format!("spawning launchctl bootstrap {target}"))?;
    if !status.success() {
        return Err(anyhow!(
            "launchctl bootstrap exited with {}",
            status
                .code()
                .map_or_else(|| "signal".into(), |c| c.to_string())
        ));
    }
    let _ = Command::new("launchctl")
        .args(["kickstart", "-k", &label_target])
        .status();
    Ok(())
}

/// `launchctl bootout gui/<uid>/<label>`. Idempotent — non-zero exit
/// when the agent isn't loaded is treated as success.
pub fn disable_service() {
    let target = format!("gui/{}/{LABEL}", super::super::util::uid_string());
    let _ = Command::new("launchctl")
        .args(["bootout", &target])
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rendered_plist_has_required_keys() {
        let body = render_plist("/Users/jane/.cargo/bin/muxad");
        assert!(body.contains("<key>Label</key>"));
        assert!(body.contains(&format!("<string>{LABEL}</string>")));
        assert!(body.contains("<key>ProgramArguments</key>"));
        assert!(body.contains("<string>/Users/jane/.cargo/bin/muxad</string>"));
        assert!(body.contains("<key>RunAtLoad</key>"));
        assert!(body.contains("<key>KeepAlive</key>"));
        assert!(body.contains("<key>ProcessType</key>"));
        assert!(body.contains("<string>Interactive</string>"));
        assert!(!body.contains("<string>Background</string>"));
        // Closing tags + xml prologue
        assert!(body.starts_with("<?xml"));
        assert!(body.trim_end().ends_with("</plist>"));
    }

    #[test]
    fn upsert_decides_outcome() {
        let want = render_plist("/p/muxad");
        assert!(matches!(upsert(None, &want).1, Outcome::Inserted));
        assert!(matches!(upsert(Some(&want), &want).1, Outcome::Unchanged));
        assert!(matches!(
            upsert(Some("<plist>old</plist>"), &want).1,
            Outcome::Replaced
        ));
    }

    #[test]
    fn locate_muxad_returns_some_path() {
        // We can't assert what it returns (depends on host), but the
        // call must not panic and must return a non-empty string.
        let p = locate_muxad();
        assert!(!p.is_empty());
    }
}
