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
//! Every channel first runs the *new* `muxa config check` against the live
//! config and stops (old binaries in place, daemon untouched) when the new
//! build cannot load it (#300). Homebrew replaces files in place, so there the
//! check can only hold back the restart.
//!
//! The daemon restart + socket verification tail is shared by all three.
//! The running daemon is asked to drain and re-exec itself first; the native
//! service manager is retained as a compatibility fallback for an older or
//! absent daemon.

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
pub async fn run(args: Args, socket: PathBuf, config: Option<PathBuf>) -> Result<()> {
    let _ = cliclack::intro("muxa upgrade");

    // Locate the muxa source tree. We walk *up* from cwd so users can
    // run `muxa upgrade` from anywhere inside the checkout (a nested
    // crate dir, the docs/ folder, …) without having to cd to the
    // root first.
    let cwd = std::env::current_dir().context("getting current directory")?;
    let Some(repo) = find_repo_root(&cwd) else {
        // No checkout in sight — most users. Resolve the channel from the
        // running binary instead of sending them off to clone a repo.
        return run_without_repo(&args, &socket, config.as_deref()).await;
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

    let before = snapshot_before_upgrade(&socket).await;

    if plan.do_pull {
        let _ = cliclack::log::step("git pull");
        run_streaming(Command::new("git").arg("pull").current_dir(&repo)).context("git pull")?;
    } else {
        let _ = cliclack::log::info("git pull skipped (--no-pull)");
    }

    let staging = std::env::temp_dir().join(format!("muxa-upgrade-build-{}", std::process::id()));
    build_check_install(&staging, config.as_deref(), |rel, root| {
        cargo_install(&repo, rel, root)
    })?;

    let restart = if plan.do_restart {
        let _ = cliclack::log::step("restarting daemon");
        restart_daemon(&socket).await
    } else {
        let _ = cliclack::log::info("daemon restart skipped (--no-restart)");
        RestartOutcome::Skipped
    };

    let head = current_head(&repo).unwrap_or_else(|| "HEAD".into());
    report_after_upgrade(before.as_deref());
    finish(&restart, &head, &socket)
}

/// Snapshot the workspace muxa tracks before anything is replaced, so a
/// restart of the agents (or the multiplexer) can be undone with one
/// `muxa restore`. Best-effort: an upgrade never fails for want of one.
async fn snapshot_before_upgrade(socket: &Path) -> Option<PathBuf> {
    let client = muxa::ipc::Client::new(socket.to_path_buf());
    match crate::reload::snapshot_before_upgrade(&client).await {
        Ok((dir, summary)) => {
            let _ = cliclack::log::info(format!(
                "pre-upgrade snapshot: {} ({summary})",
                dir.display()
            ));
            Some(dir)
        }
        Err(error) => {
            let _ = cliclack::log::info(format!("pre-upgrade snapshot skipped: {error}"));
            None
        }
    }
}

/// After the swap: name the agent-side processes still on the old binary
/// and how to replace them, plus the way back to the pre-upgrade workspace.
fn report_after_upgrade(snapshot: Option<&Path>) {
    let stale = crate::stale_agents::installed_muxa()
        .as_deref()
        .map(crate::stale_agents::scan)
        .unwrap_or_default();
    let mut note = stale.guidance().unwrap_or_else(|| {
        "Every running agent tool server (`muxa mcp`) already uses the new build.".to_owned()
    });
    if let Some(dir) = snapshot {
        let _ = write!(
            note,
            "\n\nIf restarting agents or the multiplexer loses a pane, rebuild the \
             pre-upgrade workspace (agents relaunch with their resume command):\n    \
             muxa restore {}          # review the plan\n    \
             muxa restore {} --run    # recreate what is missing",
            dir.display(),
            dir.display()
        );
    }
    let _ = cliclack::note("After the upgrade", note);
}

/// Upgrade without a source checkout: Homebrew delegation or GitHub
/// release self-update, chosen from where the running binary lives.
async fn run_without_repo(args: &Args, socket: &Path, config: Option<&Path>) -> Result<()> {
    let exe = std::env::current_exe().context("resolving current executable")?;
    let exe = exe.canonicalize().unwrap_or(exe);

    if is_homebrew_managed(&exe) {
        let _ = cliclack::log::info(format!("channel: homebrew ({})", exe.display()));
        if args.dry_run {
            let _ = cliclack::note(
                "Plan",
                "brew upgrade muxa
check the live config with the new muxa (no restart if it fails)
ask muxad to re-exec itself (service-manager fallback for an older/stopped daemon)
verify IPC generation/socket",
            );
            let _ = cliclack::outro("Dry run — no changes made.");
            return Ok(());
        }
        let before = snapshot_before_upgrade(socket).await;
        let _ = cliclack::log::step("brew upgrade muxa");
        run_streaming(Command::new("brew").args(["upgrade", "muxa"]))
            .context("brew upgrade muxa")?;
        // brew has already swapped the files; what is left to protect is the
        // running daemon, which must not be restarted onto this build.
        check_config_with(
            &homebrew_bin(&exe).unwrap_or(exe),
            config,
            "brew has installed the new build, but muxad was NOT restarted and keeps \
             running the previous one. Fix the config, then run `muxa daemon restart`.",
        )?;
        return finish_with_restart(
            args,
            socket,
            &format!("v{}", latest_installed_version()),
            before.as_deref(),
        )
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
    if !is_newer_release(&latest, &current) {
        let _ = cliclack::outro(format!("up to date ({current}; latest release {latest})"));
        return Ok(());
    }
    let _ = cliclack::log::info(format!("{current} → {latest}"));

    if args.dry_run {
        let _ = cliclack::note(
            "Plan",
            format!(
                "download muxa-{latest}-{triple}.tar.gz + .sha256
verify checksum
check the live config with the new muxa (abort before replacing if it fails)
replace muxa + muxad in {}
ask muxad to re-exec itself (service-manager fallback for an older/stopped daemon)
verify IPC generation/socket",
                install_dir.display()
            ),
        );
        let _ = cliclack::outro("Dry run — no changes made.");
        return Ok(());
    }
    let before = snapshot_before_upgrade(socket).await;

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

    let installed = install_release(&staging.join(&stem), &install_dir, config);
    let _ = std::fs::remove_dir_all(&staging);
    installed?;

    finish_with_restart(args, socket, &latest, before.as_deref()).await
}

/// Whether release `latest` is strictly newer than `current` (both `vX.Y.Z`).
/// A build ahead of the newest published release — a source or combined
/// install of an unreleased version — must not be "upgraded" back down to it:
/// that is how #300 put a v0.9.1 binary under a v0.9.2 config. Tags that do
/// not parse fall back to "different means newer".
fn is_newer_release(latest: &str, current: &str) -> bool {
    fn parse(tag: &str) -> Option<Vec<u64>> {
        tag.trim_start_matches('v')
            .split('.')
            .map(|part| part.parse().ok())
            .collect()
    }
    match (parse(latest), parse(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => latest != current,
    }
}

/// Swap the unpacked release binaries in `staged` into `install_dir`, but only
/// once the new `muxa` has accepted the live config. On refusal nothing is
/// replaced, and the caller's `?` keeps the daemon restart from running.
fn install_release(staged: &Path, install_dir: &Path, config: Option<&Path>) -> Result<()> {
    for bin in ["muxa", "muxad"] {
        if !staged.join(bin).is_file() {
            return Err(anyhow!("release archive is missing `{bin}`"));
        }
    }
    check_config_with(&staged.join("muxa"), config, NOTHING_CHANGED)?;

    let _ = cliclack::log::step(format!("installing into {}", install_dir.display()));
    for bin in ["muxa", "muxad"] {
        replace_binary(&staged.join(bin), &install_dir.join(bin))
            .with_context(|| format!("installing {bin}"))?;
    }
    Ok(())
}

/// Source channel: `cargo install` writes straight over the live binaries, so
/// build into the `staging` root first and check the config there. The real
/// install reruns `cargo install` on the same workspace target dir, so it
/// reuses that build instead of compiling again. `install(rel, root)` is
/// `cargo install --path <rel> [--root <root>]`.
fn build_check_install(
    staging: &Path,
    config: Option<&Path>,
    mut install: impl FnMut(&str, Option<&Path>) -> Result<()>,
) -> Result<()> {
    let mut staged = || -> Result<()> {
        let _ = cliclack::log::step("building muxad");
        install("crates/muxad", Some(staging)).context("cargo install muxad")?;
        let _ = cliclack::log::step("building muxa-cli");
        install("crates/muxa-cli", Some(staging)).context("cargo install muxa-cli")?;
        let muxa = staging
            .join("bin")
            .join(format!("muxa{}", std::env::consts::EXE_SUFFIX));
        check_config_with(&muxa, config, NOTHING_CHANGED)
    };
    let staged = staged();
    let _ = std::fs::remove_dir_all(staging);
    staged?;

    let _ = cliclack::log::step("installing muxad");
    install("crates/muxad", None).context("cargo install muxad")?;
    let _ = cliclack::log::step("installing muxa-cli");
    install("crates/muxa-cli", None).context("cargo install muxa-cli")
}

const NOTHING_CHANGED: &str = "Nothing was replaced and muxad was not restarted. Update the \
     config so the new build accepts it (or drop the key it rejects), then rerun `muxa upgrade`.";

/// Ask the new build's own parser whether it can load the live config:
/// `<new_muxa> --config <path> config check --from <path>`. A missing file
/// means the daemon runs on defaults, so there is nothing to check. On refusal
/// the error carries the new build's diagnostic plus `consequence`.
fn check_config_with(new_muxa: &Path, config: Option<&Path>, consequence: &str) -> Result<()> {
    let Some(config) = config.filter(|path| path.exists()) else {
        let _ = cliclack::log::info("no config file; nothing to check");
        return Ok(());
    };
    let _ = cliclack::log::step(format!("checking {} with the new build", config.display()));
    let out = Command::new(new_muxa)
        .arg("--config")
        .arg(config)
        .args(["config", "check", "--from"])
        .arg(config)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("running `{} config check`", new_muxa.display()))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let detail = [stderr.trim(), stdout.trim()]
        .into_iter()
        .find(|text| !text.is_empty())
        .map_or_else(|| format!("exited with {}", out.status), str::to_owned);
    Err(anyhow!(
        "the new build cannot load {}:\n{detail}\n\n{consequence}",
        config.display()
    ))
}

/// `<prefix>/bin/muxa` for a binary under `<prefix>/Cellar/…`: the link brew
/// repoints at the version it just installed.
fn homebrew_bin(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .find(|dir| {
            dir.file_name()
                .is_some_and(|name| name.eq_ignore_ascii_case("Cellar"))
        })
        .and_then(Path::parent)
        .map(|prefix| prefix.join("bin").join("muxa"))
}

/// Shared tail: restart the daemon (unless opted out), verify the
/// socket, and close the flow with the version we ended on.
async fn finish_with_restart(
    args: &Args,
    socket: &Path,
    version: &str,
    before: Option<&Path>,
) -> Result<()> {
    let restart = if args.no_restart {
        let _ = cliclack::log::info("daemon restart skipped (--no-restart)");
        RestartOutcome::Skipped
    } else {
        let _ = cliclack::log::step("restarting daemon");
        restart_daemon(socket).await
    };
    report_after_upgrade(before);
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
    lines.push("cargo install --path crates/muxad --locked --force --root <staging>".into());
    lines.push("cargo install --path crates/muxa-cli --locked --force --root <staging>".into());
    lines.push("check the live config with <staging>/bin/muxa (abort if it fails)".into());
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

/// `cargo install --path <rel> --locked --force [--root <root>]`,
/// streaming output to the user's terminal. The caller logs the
/// "building X" step banner — we just exec.
fn cargo_install(repo: &Path, rel_path: &str, root: Option<&Path>) -> Result<()> {
    let abs = repo.join(rel_path);
    // Only the real install overwrites the binaries that may be running.
    if cfg!(windows) && root.is_none() {
        let program = if rel_path.ends_with("muxad") {
            "muxad"
        } else {
            "muxa"
        };
        park_running_binary(program);
    }
    let mut cmd = Command::new("cargo");
    cmd.arg("install")
        .arg("--path")
        .arg(&abs)
        .arg("--locked")
        .arg("--force");
    if let Some(root) = root {
        cmd.arg("--root").arg(root);
    }
    run_streaming(&mut cmd)
}

/// Windows refuses to overwrite an executable while it runs — this very
/// `muxa`, or the daemon — but lets it be renamed. Move the installed copy
/// aside so `cargo install` can write a fresh one in its place; the running
/// process keeps its image, and a restart launches the new file from the
/// original path. Copies parked by an earlier upgrade go once nothing runs
/// them. Best-effort: when this fails, cargo reports the real error.
fn park_running_binary(program: &str) {
    let Ok(installed) = which::which(program) else {
        return;
    };
    let Some(dir) = installed.parent() else {
        return;
    };
    if let Ok(entries) = std::fs::read_dir(dir) {
        let prefix = format!("{program}.parked-");
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(&prefix) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let parked = dir.join(format!("{program}.parked-{}.exe", std::process::id()));
    let _ = std::fs::rename(&installed, parked);
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

    /// A stand-in `muxa` that accepts exactly the preflight invocation
    /// (`--config <p> config check --from <p>`) and then exits with `code`.
    fn fake_muxa(path: &Path, code: i32) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(
            path,
            format!(
                "#!/bin/sh\n[ \"$1 $3 $4 $5\" = \"--config config check --from\" ] && [ \"$2\" = \"$6\" ] || exit 99\n\
                 [ {code} -eq 0 ] || echo \"unknown field \\`account\\`, expected \\`options\\`\" >&2\nexit {code}\n"
            ),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn config_check_reports_the_new_builds_refusal() {
        let dir = tempdir().unwrap();
        let config = dir.path().join("config.toml");
        fs::write(&config, "[agent.claude]\naccount = \"auto\"\n").unwrap();
        let good = dir.path().join("good");
        let bad = dir.path().join("bad");
        fake_muxa(&good, 0);
        fake_muxa(&bad, 1);

        check_config_with(&good, Some(&config), "HINT").unwrap();
        let error = check_config_with(&bad, Some(&config), "HINT")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown field `account`"), "{error}");
        assert!(error.contains("HINT"), "{error}");
        assert!(error.contains(&config.display().to_string()), "{error}");
    }

    #[test]
    fn config_check_is_skipped_without_a_config_file() {
        let dir = tempdir().unwrap();
        let bad = dir.path().join("bad");
        fake_muxa(&bad, 1);
        check_config_with(&bad, None, "HINT").unwrap();
        check_config_with(&bad, Some(&dir.path().join("absent.toml")), "HINT").unwrap();
    }

    fn release_fixture(new_muxa_exit: i32) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let dir = tempdir().unwrap();
        let staged = dir.path().join("staged");
        let install = dir.path().join("bin");
        fs::create_dir_all(&staged).unwrap();
        fs::create_dir_all(&install).unwrap();
        fake_muxa(&staged.join("muxa"), new_muxa_exit);
        fs::write(staged.join("muxad"), "new muxad").unwrap();
        fs::write(install.join("muxa"), "old muxa").unwrap();
        fs::write(install.join("muxad"), "old muxad").unwrap();
        let config = dir.path().join("config.toml");
        fs::write(&config, "[agent.claude]\naccount = \"auto\"\n").unwrap();
        (dir, staged, install, config)
    }

    #[test]
    fn release_install_keeps_the_old_binaries_when_the_config_is_refused() {
        let (_dir, staged, install, config) = release_fixture(1);

        let error = install_release(&staged, &install, Some(&config)).unwrap_err();
        assert!(
            error.to_string().contains("Nothing was replaced"),
            "{error}"
        );
        assert_eq!(
            fs::read_to_string(install.join("muxa")).unwrap(),
            "old muxa"
        );
        assert_eq!(
            fs::read_to_string(install.join("muxad")).unwrap(),
            "old muxad"
        );
        assert!(!install.join("muxa.bak").exists());
        assert!(!install.join("muxad.bak").exists());
    }

    #[test]
    fn release_install_replaces_once_the_config_is_accepted() {
        let (_dir, staged, install, config) = release_fixture(0);

        install_release(&staged, &install, Some(&config)).unwrap();
        assert_eq!(
            fs::read_to_string(install.join("muxad")).unwrap(),
            "new muxad"
        );
        assert_eq!(
            fs::read_to_string(install.join("muxad.bak")).unwrap(),
            "old muxad"
        );
    }

    /// Drives `build_check_install` with a fake `cargo install` that drops a
    /// `muxa` exiting `new_muxa_exit` into the staging root, and returns the
    /// root of every install it was asked for (`None` = the live install).
    fn source_install(new_muxa_exit: i32) -> (Result<()>, Vec<Option<PathBuf>>, PathBuf) {
        let dir = tempdir().unwrap();
        let staging = dir.path().join("staging");
        let config = dir.path().join("config.toml");
        fs::write(&config, "[agent.claude]\naccount = \"auto\"\n").unwrap();
        let mut roots = Vec::new();
        let result = build_check_install(&staging, Some(&config), |rel, root| {
            roots.push(root.map(Path::to_path_buf));
            if let Some(root) = root.filter(|_| rel == "crates/muxa-cli") {
                fs::create_dir_all(root.join("bin")).unwrap();
                fake_muxa(&root.join("bin").join("muxa"), new_muxa_exit);
            }
            Ok(())
        });
        (result, roots, staging)
    }

    #[test]
    fn source_install_never_touches_the_live_binaries_when_the_config_is_refused() {
        let (result, roots, staging) = source_install(1);
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("unknown field `account`"),
            "{error}"
        );
        assert!(
            roots.iter().all(Option::is_some),
            "only staging installs may run: {roots:?}"
        );
        assert_eq!(roots.len(), 2);
        assert!(!staging.exists(), "staging root is cleaned up");
    }

    #[test]
    fn source_install_stages_checks_then_installs() {
        let (result, roots, staging) = source_install(0);
        result.unwrap();
        assert_eq!(
            roots,
            vec![Some(staging.clone()), Some(staging.clone()), None, None]
        );
        assert!(!staging.exists(), "staging root is cleaned up");
    }

    #[test]
    fn release_channel_never_downgrades() {
        assert!(is_newer_release("v0.9.4", "v0.9.3"));
        assert!(is_newer_release("v0.10.0", "v0.9.9"));
        assert!(!is_newer_release("v0.9.4", "v0.9.4"));
        // #300: an unreleased 0.9.2 build against a latest release of v0.9.1.
        assert!(!is_newer_release("v0.9.1", "v0.9.2"));
        assert!(is_newer_release("v1.0.0-rc1", "v0.9.2"));
    }

    #[test]
    fn homebrew_bin_is_the_prefix_link() {
        assert_eq!(
            homebrew_bin(Path::new("/opt/homebrew/Cellar/muxa/0.9.1/bin/muxa")),
            Some(PathBuf::from("/opt/homebrew/bin/muxa"))
        );
        assert_eq!(homebrew_bin(Path::new("/home/june/.cargo/bin/muxa")), None);
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
