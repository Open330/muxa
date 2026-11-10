//! `~/Library/LaunchAgents/dev.open330.tendd.plist` installer.
//!
//! macOS analogue of `systemd.rs`. Same shape: pure content layer
//! that decides what the file should look like, plus a thin shell-out
//! driver for `launchctl bootstrap` / `bootout`.
//!
//! Why a per-user `LaunchAgent` and not a system-wide `LaunchDaemon`: tendd
//! holds per-user IPC state (the socket lives under `/tmp/tend-<uid>.sock`)
//! and is opt-in; nothing in tend wants to touch system-level scope.

use crate::init::marker::Outcome;
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;
use std::process::Command;

pub const LABEL: &str = "dev.open330.tendd";
pub const UNIT_FILENAME: &str = "dev.open330.tendd.plist";

/// `~/Library/LaunchAgents/dev.open330.tendd.plist`. Returns `None`
/// only if `$HOME` is unset, which is exotic enough to surface as a
/// caller-side error.
pub fn default_unit_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join("Library/LaunchAgents").join(UNIT_FILENAME))
}

/// Build the plist body. We resolve `tendd`'s on-disk path at install
/// time rather than relying on `$PATH`, since launchd unsets a lot of
/// the user's environment.
pub fn render_plist(tendd_path: &str) -> String {
    let path = provider_path(
        dirs::home_dir().as_deref(),
        &std::env::var("PATH").unwrap_or_default(),
    );
    render_plist_with_path(tendd_path, &path)
}

// Preserve shell/account-shim precedence, but a GUI or minimal installing
// environment must still reach the common per-user and Homebrew CLI locations.
fn provider_path(home: Option<&std::path::Path>, inherited: &str) -> String {
    let mut paths: Vec<PathBuf> = std::env::split_paths(inherited)
        .filter(|path| path.is_absolute())
        .collect();
    let mut fallback = Vec::new();
    if let Some(home) = home {
        fallback.extend([home.join(".local/bin"), home.join(".cargo/bin")]);
    }
    fallback.extend(
        [
            "/opt/homebrew/bin",
            "/usr/local/bin",
            "/usr/bin",
            "/bin",
            "/usr/sbin",
            "/sbin",
        ]
        .map(PathBuf::from),
    );
    for path in fallback {
        if !paths.contains(&path) {
            paths.push(path);
        }
    }
    std::env::join_paths(paths)
        .expect("PATH entries contain no separator")
        .to_string_lossy()
        .into_owned()
}

fn render_plist_with_path(tendd_path: &str, path: &str) -> String {
    use std::fmt::Write as _;
    let tendd_path = xml_escape(tendd_path);
    let path = xml_escape(path);
    let mut s = String::with_capacity(640);
    s.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    s.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    s.push_str("<plist version=\"1.0\">\n");
    s.push_str("<dict>\n");
    s.push_str("  <key>Label</key>\n");
    let _ = writeln!(s, "  <string>{LABEL}</string>");
    s.push_str("  <key>ProgramArguments</key>\n");
    s.push_str("  <array>\n");
    let _ = writeln!(s, "    <string>{tendd_path}</string>");
    s.push_str("  </array>\n");
    // Child providers (codex/claude, npm and account shims) need the same
    // executable search path as the installing shell, not launchd's minimal one.
    s.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n    <key>PATH</key>\n");
    let _ = writeln!(s, "    <string>{path}</string>");
    s.push_str("  </dict>\n");
    s.push_str("  <key>RunAtLoad</key>\n");
    s.push_str("  <true/>\n");
    s.push_str("  <key>KeepAlive</key>\n");
    s.push_str("  <true/>\n");
    // tendd serves latency-sensitive menu-bar and tmux status-line IPC over a
    // Unix socket. launchd's Adaptive class only boosts jobs for XPC activity,
    // so it cannot observe these requests; Background can therefore starve the
    // daemon for seconds under CPU pressure. Interactive is appropriate here
    // because the foreground UI's responsiveness directly depends on tendd.
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
    s.push_str("  <string>/tmp/tendd.log</string>\n");
    s.push_str("  <key>StandardErrorPath</key>\n");
    s.push_str("  <string>/tmp/tendd.err</string>\n");
    s.push_str("</dict>\n");
    s.push_str("</plist>\n");
    s
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
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

/// Best-effort lookup of `tendd` for the plist's `ProgramArguments`.
/// `which::which` covers the standard case; the static fallbacks
/// behind it pick up the common cargo / Homebrew layouts in priority
/// order so a brew-installed tendd on macOS doesn't silently land at
/// the wrong path on first install.
pub fn locate_tendd() -> String {
    if let Ok(path) = which::which("tendd") {
        return path.to_string_lossy().into_owned();
    }
    let candidates: &[PathBuf] = &[
        dirs::home_dir()
            .map(|h| h.join(".cargo/bin/tendd"))
            .unwrap_or_default(),
        PathBuf::from("/opt/homebrew/bin/tendd"),
        PathBuf::from("/usr/local/bin/tendd"),
    ];
    for cand in candidates {
        if cand.is_file() {
            return cand.to_string_lossy().into_owned();
        }
    }
    "tendd".into()
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

    // Clear any persistent disable override before touching the job.
    // `launchctl disable` writes to
    // `/var/db/com.apple.xpc.launchd/disabled.<uid>.plist`, survives reboots,
    // and outranks the plist's own `RunAtLoad`: a disabled label bootstraps
    // fine and then never runs, at this install and at every later login.
    // Older tend.app builds set exactly this when they replaced a running
    // daemon, so installing has to be able to undo it.
    let _ = Command::new("launchctl")
        .args(["enable", &label_target])
        .output();

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

/// Read the label's entry out of `launchctl print-disabled gui/<uid>`,
/// whose body is one `"<label>" => <state>` line per override.
///
/// Both spellings of the state are accepted on purpose. macOS 26 prints
/// `enabled`/`disabled` while the `true`/`false` of the backing
/// `/var/db/com.apple.xpc.launchd/disabled.<uid>.plist` is what older
/// launchctl echoed, and either can turn up depending on the host.
///
/// `Some(true)` is the state that makes tendd look broken for no visible
/// reason: the plist is present and correct, `RunAtLoad` is set, and launchd
/// skips the job at every login anyway. `None` means launchd holds no opinion
/// about this label, which is the ordinary case.
pub fn parse_disabled(output: &str, label: &str) -> Option<bool> {
    let needle = format!("\"{label}\"");
    output.lines().find_map(|line| {
        let rest = line.trim().strip_prefix(&needle)?.trim_start();
        match rest.strip_prefix("=>")?.trim() {
            "true" | "disabled" => Some(true),
            "false" | "enabled" => Some(false),
            _ => None,
        }
    })
}

/// Ask launchd whether `LABEL` carries a disable override. `None` when
/// `launchctl` can't be run or says nothing about the label.
pub fn service_disabled() -> Option<bool> {
    let target = format!("gui/{}", super::super::util::uid_string());
    let out = Command::new("launchctl")
        .args(["print-disabled", &target])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_disabled(&String::from_utf8_lossy(&out.stdout), LABEL)
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
        let body = render_plist("/Users/jane/.cargo/bin/tendd");
        assert!(body.contains("<key>Label</key>"));
        assert!(body.contains(&format!("<string>{LABEL}</string>")));
        assert!(body.contains("<key>ProgramArguments</key>"));
        assert!(body.contains("<string>/Users/jane/.cargo/bin/tendd</string>"));
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
    #[cfg(unix)]
    fn minimal_installer_path_still_finds_user_providers_without_current_directory() {
        let path = provider_path(
            Some(std::path::Path::new("/Users/jane")),
            "/usr/bin::.:relative",
        );
        let paths: Vec<_> = std::env::split_paths(&path).collect();
        assert_eq!(paths[0], PathBuf::from("/usr/bin"));
        assert!(paths.contains(&PathBuf::from("/Users/jane/.local/bin")));
        assert!(paths.contains(&PathBuf::from("/opt/homebrew/bin")));
        assert!(paths.iter().all(|path| path.is_absolute()));
    }

    #[test]
    fn launchd_keeps_provider_path_and_escapes_xml() {
        let body = render_plist_with_path(
            "/Users/A&B/bin/tendd",
            "/Users/A&B/.local/bin:/opt/homebrew/bin:/usr/bin",
        );
        assert!(body.contains("<key>EnvironmentVariables</key>"));
        assert!(
            body.contains("<string>/Users/A&amp;B/.local/bin:/opt/homebrew/bin:/usr/bin</string>")
        );
        assert!(body.contains("<string>/Users/A&amp;B/bin/tendd</string>"));
        assert!(!body.contains("/Users/A&B"));
    }

    #[test]
    fn parse_disabled_reads_the_override_table() {
        // Verbatim shape of `launchctl print-disabled gui/501` on macOS 26.
        let out = "\
disabled services = {
\t\t\"com.apple.Siri.agent\" => disabled
\t\t\"dev.open330.tendd\" => disabled
\t\t\"homebrew.mxcl.tend\" => enabled
}
";
        assert_eq!(parse_disabled(out, LABEL), Some(true));
        assert_eq!(parse_disabled(out, "homebrew.mxcl.tend"), Some(false));
        assert_eq!(parse_disabled(out, "dev.open330.absent"), None);
    }

    #[test]
    fn parse_disabled_also_reads_the_boolean_spelling() {
        // What the backing disabled.<uid>.plist stores, and what older
        // launchctl builds echo back.
        let out = "\t\t\"dev.open330.tendd\" => true\n";
        assert_eq!(parse_disabled(out, LABEL), Some(true));
        assert_eq!(
            parse_disabled("\t\t\"dev.open330.tendd\" => false\n", LABEL),
            Some(false)
        );
    }

    #[test]
    fn parse_disabled_ignores_a_label_that_is_only_a_suffix() {
        let out = "\t\t\"legacy.dev.open330.tendd\" => true\n";
        assert_eq!(parse_disabled(out, LABEL), None);
    }

    #[test]
    fn upsert_decides_outcome() {
        let want = render_plist("/p/tendd");
        assert!(matches!(upsert(None, &want).1, Outcome::Inserted));
        assert!(matches!(upsert(Some(&want), &want).1, Outcome::Unchanged));
        assert!(matches!(
            upsert(Some("<plist>old</plist>"), &want).1,
            Outcome::Replaced
        ));
    }

    #[test]
    fn locate_tendd_returns_some_path() {
        // We can't assert what it returns (depends on host), but the
        // call must not panic and must return a non-empty string.
        let p = locate_tendd();
        assert!(!p.is_empty());
    }
}
