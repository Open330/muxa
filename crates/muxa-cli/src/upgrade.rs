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
//! **Channel awareness**: most users have no source clone, and telling
//! them to get one is a dead end. `upgrade` resolves its own install
//! channel and does the right thing per channel:
//!
//! - inside a muxa source checkout → the git-pull + cargo-install flow above;
//! - a Homebrew-managed binary → delegate to `brew upgrade muxa`;
//! - anything else (release-archive or hand-copied binary) → self-update
//!   from the GitHub release matching this platform: download the target
//!   archive and its `.sha256` sidecar, verify, atomically swap `muxa` and
//!   `muxad` in place (previous binaries kept as `.bak`), restart.
//!
//! The daemon restart + socket verification tail is shared by all three.
//! The running daemon is asked to drain and re-exec itself first; the native
//! service manager is retained as a compatibility fallback for an older or
//! absent daemon.

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
/// Async because the shared restart tail performs bounded IPC round trips.
/// Build/install commands remain blocking: this command is interactive and
/// has no other useful work to schedule while they run.
pub async fn run(args: Args, socket: PathBuf) -> Result<()> {
    let _ = cliclack::intro("muxa upgrade");

    // Locate the muxa source tree. We walk *up* from cwd so users can
    // run `muxa upgrade` from anywhere inside the checkout (a nested
    // crate dir, the docs/ folder, …) without having to cd to the
    // root first.
    let cwd = std::env::current_dir().context("getting current directory")?;
    let Some(repo) = find_repo_root(&cwd) else {
        // No checkout in sight — most users. Resolve the channel from the
        // running binary instead of sending them off to clone a repo.
        return run_without_repo(&args, &socket).await;
    };
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

    let restart = if plan.do_restart {
        let _ = cliclack::log::step("restarting daemon");
        restart_daemon(&socket).await
    } else {
        let _ = cliclack::log::info("daemon restart skipped (--no-restart)");
        RestartOutcome::Skipped
    };

    let head = current_head(&repo).unwrap_or_else(|| "HEAD".into());
    finish(&restart, &head, &socket)
}

/// Upgrade without a source checkout: Homebrew delegation or GitHub
/// release self-update, chosen from where the running binary lives.
async fn run_without_repo(args: &Args, socket: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("resolving current executable")?;
    let exe = exe.canonicalize().unwrap_or(exe);

    if is_homebrew_managed(&exe) {
        let _ = cliclack::log::info(format!("channel: homebrew ({})", exe.display()));
        if args.dry_run {
            let _ = cliclack::note(
                "Plan",
                "brew upgrade muxa
ask muxad to re-exec itself (service-manager fallback for an older/stopped daemon)
verify IPC generation/socket",
            );
            let _ = cliclack::outro("Dry run — no changes made.");
            return Ok(());
        }
        let _ = cliclack::log::step("brew upgrade muxa");
        run_streaming(Command::new("brew").args(["upgrade", "muxa"]))
            .context("brew upgrade muxa")?;
        return finish_with_restart(args, socket, &format!("v{}", latest_installed_version()))
            .await;
    }

    let triple = release_target_triple(std::env::consts::OS, std::env::consts::ARCH)
        .ok_or_else(|| {
            anyhow!(
                "no prebuilt release for {}-{} — clone https://github.com/Open330/muxa and run `muxa upgrade` inside it",
                std::env::consts::OS,
                std::env::consts::ARCH
            )
        })?;
    let install_dir = exe
        .parent()
        .ok_or_else(|| anyhow!("executable has no parent directory"))?
        .to_path_buf();
    let _ = cliclack::log::info(format!(
        "channel: release binary ({} · {triple})",
        install_dir.display()
    ));

    let _ = cliclack::log::step("checking latest release");
    let latest = latest_release_tag().context("querying the latest GitHub release")?;
    let current = format!("v{}", env!("CARGO_PKG_VERSION"));
    if latest == current {
        let _ = cliclack::outro(format!("already up to date ({current})"));
        return Ok(());
    }
    let _ = cliclack::log::info(format!("{current} → {latest}"));

    if args.dry_run {
        let _ = cliclack::note(
            "Plan",
            format!(
                "download muxa-{latest}-{triple}.tar.gz + .sha256
verify checksum
replace muxa + muxad in {}
ask muxad to re-exec itself (service-manager fallback for an older/stopped daemon)
verify IPC generation/socket",
                install_dir.display()
            ),
        );
        let _ = cliclack::outro("Dry run — no changes made.");
        return Ok(());
    }

    let staging = std::env::temp_dir().join(format!("muxa-upgrade-{}", std::process::id()));
    std::fs::create_dir_all(&staging).context("creating staging dir")?;
    // Release asset naming, measured against v0.8.27: the archive is
    // `muxa-<tag>-<triple>.tar.gz`, its sidecar is `muxa-<tag>-<triple>.sha256`
    // (no `.tar.gz` in the sidecar name), and the archive unpacks into a
    // `muxa-<tag>-<triple>/` directory containing the binaries.
    let stem = format!("muxa-{latest}-{triple}");
    let archive = format!("{stem}.tar.gz");
    let base = format!("https://github.com/Open330/muxa/releases/download/{latest}");

    let _ = cliclack::log::step("downloading");
    download(&format!("{base}/{archive}"), &staging.join(&archive))?;
    download(
        &format!("{base}/{stem}.sha256"),
        &staging.join(format!("{stem}.sha256")),
    )?;

    let _ = cliclack::log::step("verifying checksum");
    verify_sha256(&staging, &archive, &format!("{stem}.sha256"))?;

    let _ = cliclack::log::step("extracting");
    run_streaming(
        Command::new("tar")
            .args(["-xzf", &archive])
            .current_dir(&staging),
    )
    .context("extracting release archive")?;

    let _ = cliclack::log::step(format!("installing into {}", install_dir.display()));
    for bin in ["muxa", "muxad"] {
        let fresh = staging.join(&stem).join(bin);
        if !fresh.is_file() {
            return Err(anyhow!("release archive is missing `{bin}`"));
        }
        replace_binary(&fresh, &install_dir.join(bin))
            .with_context(|| format!("installing {bin}"))?;
    }
    let _ = std::fs::remove_dir_all(&staging);

    finish_with_restart(args, socket, &latest).await
}

/// Shared tail: restart the daemon (unless opted out), verify the
/// socket, and close the flow with the version we ended on.
async fn finish_with_restart(args: &Args, socket: &Path, version: &str) -> Result<()> {
    let restart = if args.no_restart {
        let _ = cliclack::log::info("daemon restart skipped (--no-restart)");
        RestartOutcome::Skipped
    } else {
        let _ = cliclack::log::step("restarting daemon");
        restart_daemon(socket).await
    };
    finish(&restart, version, socket)
}

/// Convert the restart result into honest user messaging and an exit status.
fn finish(restart: &RestartOutcome, version: &str, socket: &Path) -> Result<()> {
    match restart {
        RestartOutcome::ManualRequired(reason) => {
            let _ = cliclack::log::warning(format!(
                "{reason}. Start muxad yourself to run the new build:\n    muxad --socket {}",
                socket.display()
            ));
            let _ = cliclack::outro(format!(
                "Upgraded to {version} — muxad needs a manual restart."
            ));
            Ok(())
        }
        RestartOutcome::Failed(reason) => {
            let _ = cliclack::outro(format!("Upgraded to {version} — muxad is down."));
            Err(anyhow!("{reason}"))
        }
        RestartOutcome::Restarted => {
            let _ = cliclack::log::success(format!("muxad responsive on {}", socket.display()));
            let _ = cliclack::outro(format!(
                "Upgraded to {version} — try `muxa doctor` for a health check."
            ));
            Ok(())
        }
        RestartOutcome::Skipped => {
            let _ = cliclack::outro(format!(
                "Upgraded to {version} — try `muxa doctor` for a health check."
            ));
            Ok(())
        }
    }
}

/// A brew-owned binary lives under the Cellar (the `bin/` entries are
/// symlinks into it, which canonicalization resolves). Managing it
/// ourselves would fight the package manager — delegate instead.
fn is_homebrew_managed(exe: &Path) -> bool {
    exe.components()
        .any(|c| c.as_os_str().eq_ignore_ascii_case("Cellar"))
}

/// The release-archive target triple for this host, `None` when the
/// release matrix does not build for it.
pub(crate) fn release_target_triple(os: &str, arch: &str) -> Option<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        _ => None,
    }
}

/// `vX.Y.Z` of the latest published release, straight from the GitHub
/// API. curl keeps us off an HTTP client dependency; it is present on
/// every platform the release matrix builds for.
fn latest_release_tag() -> Result<String> {
    let out = Command::new("curl")
        .args([
            "-fsSL",
            "--retry",
            "2",
            "https://api.github.com/repos/Open330/muxa/releases/latest",
        ])
        .output()
        .context("spawning curl (is curl installed?)")?;
    if !out.status.success() {
        return Err(anyhow!(
            "GitHub API request failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let body: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("parsing GitHub release JSON")?;
    body.get("tag_name")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("release JSON has no tag_name"))
}

fn download(url: &str, dest: &Path) -> Result<()> {
    run_streaming(Command::new("curl").args([
        "-fL",
        "--retry",
        "2",
        "-o",
        &dest.display().to_string(),
        url,
    ]))
    .with_context(|| format!("downloading {url}"))
}

/// Check the archive against its sidecar. The sidecar's first field is
/// the digest; the local digest comes from whichever of `sha256sum` /
/// `shasum -a 256` this host has.
fn verify_sha256(dir: &Path, archive: &str, sidecar_name: &str) -> Result<()> {
    let sidecar =
        std::fs::read_to_string(dir.join(sidecar_name)).context("reading .sha256 sidecar")?;
    let expected = sidecar
        .split_whitespace()
        .next()
        .ok_or_else(|| anyhow!("empty .sha256 sidecar"))?
        .to_ascii_lowercase();

    let out = Command::new("sha256sum")
        .arg(archive)
        .current_dir(dir)
        .output()
        .or_else(|_| {
            Command::new("shasum")
                .args(["-a", "256", archive])
                .current_dir(dir)
                .output()
        })
        .context("spawning sha256sum/shasum")?;
    if !out.status.success() {
        return Err(anyhow!("checksum tool failed"));
    }
    let actual = String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if actual != expected {
        return Err(anyhow!(
            "checksum mismatch: expected {expected}, downloaded file hashes to {actual}"
        ));
    }
    Ok(())
}

/// Swap `fresh` into `target`'s place: previous binary parked as
/// `.bak`, new one renamed in atomically. A running process keeps its
/// old inode, so replacing a live `muxa`/`muxad` is safe on unix.
fn replace_binary(fresh: &Path, target: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(fresh, std::fs::Permissions::from_mode(0o755))
            .context("chmod new binary")?;
    }
    let backup = target.with_extension("bak");
    if target.exists() {
        std::fs::rename(target, &backup).context("parking previous binary as .bak")?;
    }
    if let Err(e) = std::fs::rename(fresh, target) {
        // Cross-device rename (temp on another filesystem) falls back to
        // copy; restore the backup if even that fails.
        if std::fs::copy(fresh, target).is_err() {
            let _ = std::fs::rename(&backup, target);
            return Err(anyhow!("installing binary: {e}"));
        }
    }
    Ok(())
}

fn latest_installed_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
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
    render_plan_for_os(plan, std::env::consts::OS)
}

fn render_plan_for_os(plan: &Plan, os: &str) -> String {
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
        let cmd = restart_command_args(os).join(" ");
        if cmd.is_empty() {
            lines.push(
                "restart: ask muxad to re-exec itself (older/stopped daemon requires a manual restart)"
                    .into(),
            );
        } else {
            lines.push(format!(
                "restart: ask muxad to re-exec itself (fallback: `{cmd}` for an older/stopped daemon)"
            ));
        }
    } else {
        lines.push("restart (skipped)".into());
    }
    lines.join("\n")
}

/// Walk upward from `start` until we find a `Cargo.toml` whose
/// workspace members include the muxa crates. That's our marker for
/// "this is the muxa source tree". Member-crate manifests are not
/// accepted because installs are resolved relative to the workspace.
///
/// Returns the directory containing that `Cargo.toml` so the caller
/// can pass it to `git -C` / `cargo install --path`.
pub(crate) fn find_repo_root(start: &Path) -> Option<PathBuf> {
    let mut cur = Some(start.to_path_buf());
    while let Some(dir) = cur {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file() && is_muxa_workspace_manifest(&manifest) {
            return Some(dir);
        }
        cur = dir.parent().map(Path::to_path_buf);
    }
    None
}

/// True iff `manifest` looks like the muxa workspace. We deliberately
/// *don't* parse TOML — a substring match is good enough for a
/// shape-check, faster, and keeps us off the toml dep graph for one
/// read.
fn is_muxa_workspace_manifest(manifest: &Path) -> bool {
    let Ok(s) = std::fs::read_to_string(manifest) else {
        return false;
    };
    // Workspace root: `members = ["crates/muxa", "crates/muxa-cli", "crates/muxad"]`
    s.contains("[workspace]") && s.contains("crates/muxa-cli") && s.contains("crates/muxad")
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
/// without spawning anything. Empty vec means there is no supported
/// service manager on this OS and a manual restart is required.
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

enum RestartOutcome {
    Restarted,
    /// Nothing was stopped. The new binary is installed, but an older daemon
    /// (or no daemon) could not be started automatically.
    ManualRequired(String),
    /// A restart was accepted or a service manager claimed success, but no
    /// replacement daemon became responsive.
    Failed(String),
    /// `--no-restart`: preserve the caller's daemon state exactly.
    Skipped,
}

/// Prefer an in-place daemon re-exec and prove it by observing a higher image
/// generation. The service manager remains the compatibility path for a daemon
/// that predates the restart capability, or when no daemon is running.
async fn restart_daemon(socket: &Path) -> RestartOutcome {
    let client = muxa::ipc::Client::new(socket.to_path_buf());
    let before = match client.hello(Duration::from_secs(5)).await {
        Ok(hello)
            if hello.capabilities.iter().any(|cap| cap == "restart")
                && hello.generation.is_some() =>
        {
            hello.generation.expect("checked above")
        }
        Ok(_) => {
            let _ = cliclack::log::info(
                "the running daemon predates self-restart; trying the service manager",
            );
            return via_service_manager(socket).await;
        }
        Err(muxa::ipc::RuntimeError::NotConnected(_)) => {
            let _ = cliclack::log::info("no daemon was running on that socket");
            return via_service_manager(socket).await;
        }
        Err(error) => {
            let _ = cliclack::log::info(format!(
                "could not identify the daemon on {} ({error}); trying the service manager",
                socket.display()
            ));
            return via_service_manager(socket).await;
        }
    };

    if let Err(error) = client.restart(Duration::from_secs(5)).await {
        // The daemon commits to restart before replying, so a lost response is
        // ambiguous. Falling back here could race the re-exec for the socket;
        // the generation check below is the authoritative outcome.
        let _ = cliclack::log::info(format!(
            "no answer to the restart request ({error}); waiting for a new generation"
        ));
    }

    match wait_for_new_generation(socket, before, Duration::from_secs(30)).await {
        Some(after) => {
            let _ = cliclack::log::info(format!("muxad came back as generation {after}"));
            RestartOutcome::Restarted
        }
        None => RestartOutcome::Failed(format!(
            "muxad did not come back on {} — check /tmp/muxad.log and start it manually",
            socket.display()
        )),
    }
}

/// Wait until a daemon with an image identity newer than `before` answers.
/// Socket connectability and pid are insufficient: a draining listener can
/// still accept one more request, while `exec` deliberately preserves pid.
async fn wait_for_new_generation(socket: &Path, before: u64, deadline: Duration) -> Option<u64> {
    let client = muxa::ipc::Client::new(socket.to_path_buf());
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        if let Ok(hello) = client.hello(Duration::from_secs(1)).await {
            if let Some(now) = hello.generation.filter(|now| *now > before) {
                return Some(now);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    None
}

/// Use the OS service manager for an older or absent daemon, then require a
/// real IPC round trip before reporting success.
async fn via_service_manager(socket: &Path) -> RestartOutcome {
    match restart_via_service_manager() {
        Ok(()) if wait_for_daemon_serving(socket, Duration::from_secs(30)).await => {
            RestartOutcome::Restarted
        }
        Ok(()) => RestartOutcome::Failed(format!(
            "the service manager reported success but muxad is not answering on {} — check /tmp/muxad.log",
            socket.display()
        )),
        Err(reason) => RestartOutcome::ManualRequired(reason),
    }
}

fn restart_via_service_manager() -> std::result::Result<(), String> {
    let args = restart_command_args(std::env::consts::OS);
    let Some((prog, rest)) = args.split_first() else {
        return Err("no supported service manager for this operating system".into());
    };

    if which::which(prog).is_err() {
        return Err(format!("`{prog}` is not available"));
    }

    match Command::new(prog).args(rest).status() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("`{prog}` exited with {status}")),
        Err(error) => Err(format!("could not run `{prog}`: {error}")),
    }
}

/// A serving daemon must complete a `hello` round trip; a connect-only probe
/// is fooled by a bound Unix socket whose process has stopped accepting.
async fn wait_for_daemon_serving(socket: &Path, deadline: Duration) -> bool {
    let client = muxa::ipc::Client::new(socket.to_path_buf());
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        if client.hello(Duration::from_secs(1)).await.is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
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

    #[test]
    fn release_triples_cover_the_build_matrix_and_nothing_else() {
        assert_eq!(
            release_target_triple("macos", "aarch64"),
            Some("aarch64-apple-darwin")
        );
        assert_eq!(
            release_target_triple("linux", "x86_64"),
            Some("x86_64-unknown-linux-gnu")
        );
        assert_eq!(release_target_triple("windows", "x86_64"), None);
        assert_eq!(release_target_triple("linux", "riscv64"), None);
    }

    #[test]
    fn homebrew_detection_keys_on_the_cellar() {
        assert!(is_homebrew_managed(Path::new(
            "/opt/homebrew/Cellar/muxa/0.8.27/bin/muxa"
        )));
        assert!(!is_homebrew_managed(Path::new(
            "/home/june/.cargo/bin/muxa"
        )));
    }

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
    fn find_repo_root_skips_member_crate_manifests() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("Cargo.toml"),
            r#"[workspace]
members = ["crates/muxa", "crates/muxa-cli", "crates/muxad"]
"#,
        )
        .unwrap();

        for crate_name in ["muxa-cli", "muxa"] {
            let crate_root = root.join("crates").join(crate_name);
            let nested = crate_root.join("src");
            fs::create_dir_all(&nested).unwrap();
            fs::write(
                crate_root.join("Cargo.toml"),
                format!(
                    r#"[package]
name = "{crate_name}"
version = "0.1.0"
"#
                ),
            )
            .unwrap();

            let found = find_repo_root(&nested).expect("should find workspace root");
            assert_eq!(
                fs::canonicalize(&found).unwrap(),
                fs::canonicalize(root).unwrap(),
                "started inside {crate_name}"
            );
        }
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
        assert!(!is_muxa_workspace_manifest(&root.join("Cargo.toml")));
    }

    #[test]
    fn find_repo_root_rejects_member_manifest_without_workspace() {
        let dir = tempdir().unwrap();
        let manifest = dir.path().join("Cargo.toml");
        fs::write(
            &manifest,
            r#"[package]
name = "muxa-cli"
version = "0.1.0"
"#,
        )
        .unwrap();

        assert!(!is_muxa_workspace_manifest(&manifest));
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
        // Empty signals that a manual restart is required. Tested so
        // a future OS rename doesn't silently emit a wrong command.
        assert!(restart_command_args("plan9").is_empty());
    }

    #[test]
    fn dry_run_renders_plan() {
        let plan = Plan {
            repo: PathBuf::from("/tmp/fake-muxa"),
            do_pull: true,
            do_restart: true,
        };
        let s = render_plan_for_os(&plan, "linux");
        assert!(s.contains("/tmp/fake-muxa"));
        assert!(s.contains("git pull"));
        assert!(s.contains("cargo install --path crates/muxad --locked --force"));
        assert!(s.contains("cargo install --path crates/muxa-cli --locked --force"));
        assert!(s.contains("re-exec"));
        assert!(s.contains("systemctl --user restart muxad"));
        assert!(!s.contains("pkill"));
        assert!(!s.contains("SIGUSR1"));
    }

    #[test]
    fn dry_run_uses_self_restart_without_service_manager() {
        let plan = Plan {
            repo: PathBuf::from("/tmp/fake-muxa"),
            do_pull: true,
            do_restart: true,
        };
        let s = render_plan_for_os(&plan, "plan9");
        assert!(s.contains("re-exec"));
        assert!(s.contains("older/stopped daemon requires a manual restart"));
        assert!(!s.contains("pkill"));
        assert!(!s.contains("SIGUSR1"));
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

    async fn serve_at_generation(
        socket: &Path,
        generation: u64,
    ) -> (
        tokio::sync::broadcast::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let (shutdown, receiver) = tokio::sync::broadcast::channel(1);
        let restart = std::sync::Arc::new(muxa::ipc::RestartController::new(
            generation,
            shutdown.clone(),
        ));
        let server = muxa::ipc::Server::new(socket.to_path_buf(), muxa::Store::shared())
            .with_restart_controller(restart);
        let handle = tokio::spawn(async move {
            let _ = server.run(receiver).await;
        });
        for _ in 0..100 {
            if muxa::ipc::Client::new(socket.to_path_buf())
                .hello(Duration::from_millis(100))
                .await
                .is_ok()
            {
                return (shutdown, handle);
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("test server never came up on {}", socket.display());
    }

    #[tokio::test]
    async fn same_generation_does_not_satisfy_restart_verification() {
        let dir = tempdir().unwrap();
        let socket = dir.path().join("same.sock");
        let (shutdown, handle) = serve_at_generation(&socket, 4).await;

        let observed = wait_for_new_generation(&socket, 4, Duration::from_millis(300)).await;
        assert_eq!(observed, None);

        let _ = shutdown.send(());
        let _ = handle.await;
    }

    #[tokio::test]
    async fn newer_generation_satisfies_restart_verification() {
        let dir = tempdir().unwrap();
        let socket = dir.path().join("newer.sock");
        let (shutdown, handle) = serve_at_generation(&socket, 5).await;

        let observed = wait_for_new_generation(&socket, 4, Duration::from_secs(2)).await;
        assert_eq!(observed, Some(5));

        let _ = shutdown.send(());
        let _ = handle.await;
    }

    #[tokio::test]
    async fn serving_probe_requires_an_ipc_answer() {
        let dir = tempdir().unwrap();
        let socket = dir.path().join("bound-but-silent.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

        assert!(
            !wait_for_daemon_serving(&socket, Duration::from_millis(300)).await,
            "a listener with nobody accepting must not count as muxad",
        );
    }
}
