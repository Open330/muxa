//! `muxa upgrade` — one-command source-build update flow.
//!
//! Distills the manual update recipe from the README:
//!
//! ```text
//! git pull
//! cargo install --path crates/muxad     --locked --force
//! cargo install --path crates/muxa-cli  --locked --force
//! launchctl kickstart -k gui/<uid>/dev.open330.muxad   # macOS
//! systemctl --user restart muxad                       # Linux
//! ```
//!
//! into a single command, plus a verify step that probes the IPC
//! socket so the user knows the new daemon is actually serving.
//!
//! **Why a separate command and not a wrapper script**: shipping a
//! shell script outside the binary means we can't ship it via the
//! release tarball without inventing a second installer. Doing it in
//! Rust keeps the upgrade flow on the same release cadence as the
//! binary itself, and the planner/dry-run path is the same shape as
//! `muxa init` so users get a consistent UX between the two.
//!
//! **Why we require running from the source repo**: the v0.5.x line
//! is distributed as a clone-and-`cargo install` flow (no homebrew
//! tap, no prebuilt binaries yet). Until that changes, the only
//! upgrade path that works is the one users already use by hand —
//! and detecting "am I in the muxa source repo" is far less ambiguous
//! than auto-detecting "did they install via cargo / brew / aur /
//! source". When a managed install path exists we'll add adapters
//! here.

use crate::init::util::wait_for_muxad;
use anyhow::{anyhow, Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// CLI flags for `muxa upgrade`. See module docstring for the
/// upgrade flow each flag opts out of.
#[derive(Debug, Parser, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct Args {
    /// Skip `git pull` — build from whatever HEAD currently is.
    /// Useful when you've cherry-picked or rebased locally and don't
    /// want the upgrade to clobber your working state.
    #[arg(long)]
    pub no_pull: bool,

    /// Build only — don't kick the daemon afterwards. The caller is
    /// expected to restart muxad themselves (or wait for the next
    /// reboot if it's launchd-managed and `KeepAlive` will catch it).
    #[arg(long)]
    pub no_restart: bool,

    /// Print the steps that would run and exit. No process spawned,
    /// no files touched.
    #[arg(long)]
    pub dry_run: bool,
}

/// Entry point dispatched from `main.rs`.
///
/// Async to match the rest of the CLI's command-dispatch shape (we
/// `.await` it in `main.rs` next to `init::run`); the body itself
/// is synchronous because cargo / git / launchctl are all blocking
/// child processes.
#[allow(clippy::unused_async)]
pub async fn run(args: Args, socket: PathBuf) -> Result<()> {
    let _ = cliclack::intro("muxa upgrade");

    // Locate the muxa source tree. We walk *up* from cwd so users can
    // run `muxa upgrade` from anywhere inside the checkout (a nested
    // crate dir, the docs/ folder, …) without having to cd to the
    // root first.
    let cwd = std::env::current_dir().context("getting current directory")?;
    let repo = find_repo_root(&cwd).ok_or_else(|| {
        anyhow!(
            "muxa upgrade requires running from the muxa source repo; \
             clone https://github.com/Open330/muxa first or use `cargo install` manually"
        )
    })?;
    let _ = cliclack::log::info(format!("repo: {}", repo.display()));

    let plan = Plan {
        repo: repo.clone(),
        do_pull: !args.no_pull,
        do_restart: !args.no_restart,
    };

    if args.dry_run {
        let _ = cliclack::note("Plan", render_plan(&plan));
        let _ = cliclack::outro("Dry run — no changes made.");
        return Ok(());
    }

    if plan.do_pull {
        let _ = cliclack::log::step("git pull");
        run_streaming(Command::new("git").arg("pull").current_dir(&repo)).context("git pull")?;
    } else {
        let _ = cliclack::log::info("git pull skipped (--no-pull)");
    }

    let _ = cliclack::log::step("building muxad");
    cargo_install(&repo, "crates/muxad").context("cargo install muxad")?;

    let _ = cliclack::log::step("building muxa-cli");
    cargo_install(&repo, "crates/muxa-cli").context("cargo install muxa-cli")?;

    if plan.do_restart {
        let _ = cliclack::log::step("restarting daemon");
        restart_daemon();
    } else {
        let _ = cliclack::log::info("daemon restart skipped (--no-restart)");
    }

    // Verification only makes sense if we actually asked the daemon
    // to come back up. With --no-restart the user owns the lifecycle,
    // so we'd be reporting on a stale process.
    if plan.do_restart {
        let _ = cliclack::log::step("verifying");
        if wait_for_muxad(&socket, Duration::from_secs(3)) {
            let _ = cliclack::log::success(format!("muxad responsive on {}", socket.display()));
        } else {
            let _ = cliclack::log::warning(format!(
                "muxad did not respond on {} within 3s — check /tmp/muxad.log",
                socket.display()
            ));
        }
    }

    let head = current_head(&repo).unwrap_or_else(|| "HEAD".into());
    let _ = cliclack::outro(format!(
        "Upgraded to {head} — try `muxa doctor` for a health check."
    ));
    Ok(())
}

/// Concrete upgrade plan. Pulled out of `Args` so the dry-run
/// renderer and the live-execution path share the same shape.
#[derive(Debug, Clone)]
struct Plan {
    repo: PathBuf,
    do_pull: bool,
    do_restart: bool,
}

fn render_plan(plan: &Plan) -> String {
    let mut lines = Vec::new();
    lines.push(format!("repo: {}", plan.repo.display()));
    if plan.do_pull {
        lines.push("git pull".into());
    } else {
        lines.push("git pull (skipped)".into());
    }
    lines.push("cargo install --path crates/muxad --locked --force".into());
    lines.push("cargo install --path crates/muxa-cli --locked --force".into());
    if plan.do_restart {
        let cmd = restart_command_args(std::env::consts::OS).join(" ");
        if cmd.is_empty() {
            lines.push("restart muxad (no service manager — best-effort pkill+spawn)".into());
        } else {
            lines.push(format!("restart: {cmd}"));
        }
    } else {
        lines.push("restart (skipped)".into());
    }
    lines.join("\n")
}

/// Walk upward from `start` until we find a `Cargo.toml` whose
/// workspace members include `crates/muxa-cli`, or whose `[package]`
/// name is `muxa-cli`. That's our marker for "this is the muxa
/// source tree".
///
/// Returns the directory containing that `Cargo.toml` so the caller
/// can pass it to `git -C` / `cargo install --path`.
pub(crate) fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start.to_path_buf());
    while let Some(dir) = cur {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file() && is_muxa_manifest(&manifest) {
            return Some(dir);
        }
        cur = dir.parent().map(Path::to_path_buf);
    }
    None
}

/// True iff `manifest` looks like the muxa workspace or muxa-cli
/// crate manifest. We deliberately *don't* parse TOML — a substring
/// match is good enough for a shape-check, faster, and keeps us off
/// the toml dep graph for one read.
fn is_muxa_manifest(manifest: &Path) -> bool {
    let Ok(s) = std::fs::read_to_string(manifest) else {
        return false;
    };
    // Workspace root: `members = ["crates/muxa", "crates/muxa-cli", "crates/muxad"]`
    if s.contains("crates/muxa-cli") && s.contains("crates/muxad") {
        return true;
    }
    // Single-crate manifest: `name = "muxa-cli"` or `name = "muxa"`.
    s.lines().any(|l| {
        let l = l.trim();
        l == r#"name = "muxa-cli""# || l == r#"name = "muxa""#
    })
}

/// `cargo install --path <rel> --locked --force`, streaming output
/// to the user's terminal. The caller logs the "building X" step
/// banner — we just exec.
fn cargo_install(repo: &Path, rel_path: &str) -> Result<()> {
    let abs = repo.join(rel_path);
    run_streaming(
        Command::new("cargo")
            .arg("install")
            .arg("--path")
            .arg(&abs)
            .arg("--locked")
            .arg("--force"),
    )
}

/// Spawn `cmd` and wait for it. stdout/stderr stay attached to the
/// parent so cargo's progress bars and git's pull summary land in
/// the user's terminal directly — no buffering.
fn run_streaming(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().with_context(|| format!("spawning {cmd:?}"))?;
    if !status.success() {
        let program = cmd.get_program().to_string_lossy().into_owned();
        let exit = status
            .code()
            .map_or_else(|| "signal".into(), |c| c.to_string());
        return Err(anyhow!("{program} exited with {exit}"));
    }
    Ok(())
}

/// Build the per-OS daemon-restart command. Returns `Vec<String>`
/// (not a `Command`) so tests can inspect the planned invocation
/// without spawning anything. Empty vec means "no first-class
/// service manager on this OS — fall back to pkill/spawn".
pub(crate) fn restart_command_args(os: &str) -> Vec<String> {
    match os {
        "macos" => {
            // `launchctl kickstart -k gui/<uid>/dev.open330.muxad`
            // matches what `muxa init` already wires up. The `-k`
            // flag means "kill the running instance first" —
            // launchd's `KeepAlive` then re-spawns it with the new
            // binary on $PATH.
            let uid = uid_string();
            vec![
                "launchctl".into(),
                "kickstart".into(),
                "-k".into(),
                format!("gui/{uid}/dev.open330.muxad"),
            ]
        }
        "linux" => vec![
            "systemctl".into(),
            "--user".into(),
            "restart".into(),
            "muxad".into(),
        ],
        _ => Vec::new(),
    }
}

/// POSIX uid as a decimal string. Same fallback rationale as
/// `init::util::uid_string` — we only get here when `id -u` itself
/// is broken, which is exotic enough that "501" (typical macOS
/// user) is the right guess for the launchd code path that needs
/// it.
fn uid_string() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "501".into(), |s| s.trim().to_string())
}

/// Try, in order: the OS-native service manager, then a SIGUSR1
/// reload, then a hard kill+respawn. We don't error out on failure
/// here — the verify step polls the IPC socket and surfaces a
/// warning if nothing came back up, which is more actionable than
/// a stack of "exited with N" lines.
fn restart_daemon() {
    let args = restart_command_args(std::env::consts::OS);
    if !args.is_empty() {
        let mut iter = args.iter();
        let prog = iter.next().expect("non-empty args");
        let rest: Vec<&String> = iter.collect();
        // `launchctl` may not exist on macOS-without-launchd hosts
        // (rare — but `cargo` on FreeBSD-emulating-macos style
        // setups). Skip silently and let the fallback kick in.
        if which::which(prog).is_ok() {
            let status = Command::new(prog).args(&rest).status();
            if matches!(status, Ok(s) if s.success()) {
                return;
            }
        }
    }

    // Fallback: SIGUSR1 is muxad's reload signal in some builds; if
    // not handled, treat it as informational and fall through to the
    // hard restart. We deliberately use `sh -c` so the `||` chain
    // works as a single shell command.
    let _ = Command::new("sh")
        .arg("-c")
        .arg(
            "pkill -USR1 muxad 2>/dev/null \
             || (pkill muxad 2>/dev/null; nohup muxad > /tmp/muxad.log 2>&1 & disown 2>/dev/null || true)",
        )
        .status();
}

/// Short SHA of HEAD in `repo`. None when `git` is missing or the
/// directory isn't a git checkout (e.g. someone exported a tarball).
fn current_head(repo: &Path) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn find_repo_root_locates_workspace_manifest() {
        // Fixture: a workspace-shaped Cargo.toml with the marker
        // members, and `find_repo_root` walking up from a nested
        // subdirectory.
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["crates/muxa", "crates/muxa-cli", "crates/muxad"]
"#,
        )
        .unwrap();
        let nested = root.join("crates/muxa-cli/src");
        fs::create_dir_all(&nested).unwrap();

        let found = find_repo_root(&nested).expect("should find workspace root");
        // canonicalize both sides — tmpdirs on macOS are symlinked
        // (/var → /private/var) so `==` would spuriously fail.
        assert_eq!(
            fs::canonicalize(&found).unwrap(),
            fs::canonicalize(root).unwrap()
        );
    }

    #[test]
    fn find_repo_root_locates_single_crate_manifest() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "muxa-cli"
version = "0.1.0"
"#,
        )
        .unwrap();
        let found = find_repo_root(root).expect("should find crate root");
        assert_eq!(
            fs::canonicalize(&found).unwrap(),
            fs::canonicalize(root).unwrap()
        );
    }

    #[test]
    fn find_repo_root_returns_none_outside_repo() {
        // No Cargo.toml at all → walk hits the FS root and returns
        // None. We give it a fresh tmpdir so we don't accidentally
        // match the actual muxa repo above us.
        let dir = tempdir().unwrap();
        // Walk only within the tmpdir by passing it directly — its
        // parents won't have a muxa-shaped manifest either, but if
        // they did the test would still be correct: that's the
        // user's actual repo, which is a valid match for the real
        // `muxa upgrade` flow but not for this isolation test.
        let res = find_repo_root(dir.path());
        // Either None (clean test env) or Some pointing far above
        // the tmpdir (CI worker's own crate). Accept both shapes —
        // the only thing that would be a bug is matching *inside*
        // the tmpdir.
        if let Some(p) = res {
            assert!(
                !p.starts_with(dir.path()),
                "should not match inside fresh tmpdir, got {}",
                p.display()
            );
        }
    }

    #[test]
    fn find_repo_root_rejects_unrelated_manifest() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("Cargo.toml"),
            r#"[package]
name = "totally-unrelated"
version = "0.1.0"
"#,
        )
        .unwrap();
        // The walk may still match a parent dir's manifest (the
        // real muxa checkout in CI). What matters is that this
        // specific manifest is rejected.
        assert!(!is_muxa_manifest(&root.join("Cargo.toml")));
    }

    #[test]
    fn restart_command_for_macos() {
        let args = restart_command_args("macos");
        assert_eq!(args[0], "launchctl");
        assert_eq!(args[1], "kickstart");
        assert_eq!(args[2], "-k");
        // The 4th arg is `gui/<uid>/dev.open330.muxad` — uid is
        // host-dependent, so just shape-check.
        assert!(
            args[3].starts_with("gui/") && args[3].ends_with("/dev.open330.muxad"),
            "unexpected target: {}",
            args[3]
        );
    }

    #[test]
    fn restart_command_for_linux() {
        assert_eq!(
            restart_command_args("linux"),
            vec!["systemctl", "--user", "restart", "muxad"]
        );
    }

    #[test]
    fn restart_command_for_unknown_os_is_empty() {
        // Empty signals "fall back to pkill+spawn" to the live
        // path. Tested so a future OS rename doesn't silently
        // start emitting a wrong command.
        assert!(restart_command_args("plan9").is_empty());
    }

    #[test]
    fn dry_run_renders_plan() {
        let plan = Plan {
            repo: PathBuf::from("/tmp/fake-muxa"),
            do_pull: true,
            do_restart: true,
        };
        let s = render_plan(&plan);
        assert!(s.contains("/tmp/fake-muxa"));
        assert!(s.contains("git pull"));
        assert!(s.contains("cargo install --path crates/muxad --locked --force"));
        assert!(s.contains("cargo install --path crates/muxa-cli --locked --force"));
        // Restart line varies by OS — just assert *something*
        // restart-shaped is in there.
        assert!(s.contains("restart") || s.contains("muxad"));
    }

    #[test]
    fn dry_run_skips_pull_and_restart_when_flagged() {
        let plan = Plan {
            repo: PathBuf::from("/tmp/fake-muxa"),
            do_pull: false,
            do_restart: false,
        };
        let s = render_plan(&plan);
        assert!(s.contains("git pull (skipped)"));
        assert!(s.contains("restart (skipped)"));
    }
}
