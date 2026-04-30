//! Execute a `Plan` against the disk + system.
//!
//! Three knobs:
//!
//! * `dry_run` — render diffs / next steps without writing anything.
//! * Backup before write — every existing file gets `<path>.muxa-backup-<unix_ts>`
//!   exactly once per run, so the user can roll back by hand if something
//!   downstream goes sideways.
//! * Atomic-ish write — we write to a sibling tempfile and rename. This
//!   keeps the destination either fully old or fully new.

use crate::init::components::Component;
use crate::init::files;
use crate::init::marker::Outcome;
use crate::init::plan::{Action, Direction, Plan};
use anyhow::{anyhow, Context, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use time::OffsetDateTime;

/// Side-effect summary returned to the orchestrator so it can paint
/// the final outcome UI.
// Each bool is a distinct, well-known summary flag rendered as one
// step line. Collapsing them into a state-machine enum (clippy's
// suggestion) would just push the same fan-out into pattern matches.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default)]
pub struct ApplyReport {
    pub edited: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
    pub backups: Vec<PathBuf>,
    pub systemd_enabled: bool,
    pub systemd_disabled: bool,
    pub launchd_enabled: bool,
    pub launchd_disabled: bool,
    pub daemon_started: bool,
    pub tmux_sourced: bool,
    pub dashboard: Option<DashboardInfo>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct DashboardInfo {
    pub token: String,
    pub bind: String,
}

/// Run every action in order. On the first hard failure we stop and
/// return the partial report alongside the error — the caller can
/// surface what was done before the break.
pub fn run(plan: &Plan, dry_run: bool) -> Result<ApplyReport> {
    let stamp = OffsetDateTime::now_utc().unix_timestamp();
    let mut report = ApplyReport::default();
    for action in &plan.actions {
        apply_one(action, dry_run, stamp, &mut report).with_context(|| describe(action))?;
    }
    Ok(report)
}

fn apply_one(action: &Action, dry_run: bool, stamp: i64, report: &mut ApplyReport) -> Result<()> {
    match action {
        Action::EditFile {
            path,
            before,
            after,
            outcome,
            ..
        } => apply_edit(
            path,
            before.as_deref(),
            after,
            *outcome,
            dry_run,
            stamp,
            report,
        ),
        Action::DeleteFile { path, .. } => apply_delete(path, dry_run, report),
        Action::EnableSystemdUnit => {
            if dry_run {
                return Ok(());
            }
            match files::systemd::enable_service() {
                Ok(()) => report.systemd_enabled = true,
                Err(e) => report
                    .warnings
                    .push(format!("systemd enable failed: {e}; service NOT started")),
            }
            Ok(())
        }
        Action::DisableSystemdUnit => {
            if !dry_run {
                files::systemd::disable_service();
                report.systemd_disabled = true;
            }
            Ok(())
        }
        Action::EnableLaunchdUnit { plist_path } => {
            if dry_run {
                return Ok(());
            }
            match files::launchd::enable_service(plist_path) {
                Ok(()) => report.launchd_enabled = true,
                Err(e) => report
                    .warnings
                    .push(format!("launchd bootstrap failed: {e}; agent NOT loaded")),
            }
            Ok(())
        }
        Action::DisableLaunchdUnit => {
            if !dry_run {
                files::launchd::disable_service();
                report.launchd_disabled = true;
            }
            Ok(())
        }
        Action::StartDaemonIfNeeded => {
            apply_start_daemon(dry_run, report);
            Ok(())
        }
        Action::SourceTmuxConf { path } => apply_source_tmux(path, dry_run, report),
        Action::PrintDashboard { token, bind } => {
            report.dashboard = Some(DashboardInfo {
                token: token.clone(),
                bind: bind.clone(),
            });
            Ok(())
        }
    }
}

fn apply_edit(
    path: &Path,
    before: Option<&str>,
    after: &str,
    outcome: Outcome,
    dry_run: bool,
    stamp: i64,
    report: &mut ApplyReport,
) -> Result<()> {
    if matches!(outcome, Outcome::Unchanged | Outcome::AlreadyAbsent) {
        return Ok(());
    }
    if dry_run {
        report.edited.push(path.to_path_buf());
        return Ok(());
    }
    if let Some(prev) = before {
        let backup = backup_path(path, stamp);
        fs::write(&backup, prev).with_context(|| format!("writing backup {}", backup.display()))?;
        report.backups.push(backup);
    }
    ensure_parent(path)?;
    atomic_write(path, after)?;
    report.edited.push(path.to_path_buf());
    Ok(())
}

fn apply_delete(path: &Path, dry_run: bool, report: &mut ApplyReport) -> Result<()> {
    if dry_run {
        report.deleted.push(path.to_path_buf());
        return Ok(());
    }
    match fs::remove_file(path) {
        Ok(()) => {
            report.deleted.push(path.to_path_buf());
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

fn apply_start_daemon(dry_run: bool, report: &mut ApplyReport) {
    if dry_run {
        return;
    }
    // The orchestrator skips this action when --start-daemon=false,
    // so reaching here means the user wants us to ensure muxad is up.
    match start_muxad_detached() {
        Ok(true) => report.daemon_started = true,
        Ok(false) => {} // already running
        Err(e) => report.warnings.push(format!("could not start muxad: {e}")),
    }
}

fn apply_source_tmux(path: &Path, dry_run: bool, report: &mut ApplyReport) -> Result<()> {
    if dry_run {
        return Ok(());
    }
    // Only attempt if there's a tmux server running; otherwise
    // `tmux source-file` errors with "no server running".
    let server_up = Command::new("tmux")
        .arg("info")
        .output()
        .is_ok_and(|o| o.status.success());
    if !server_up {
        return Ok(());
    }
    let status = Command::new("tmux")
        .arg("source-file")
        .arg(path)
        .status()
        .with_context(|| format!("running tmux source-file {}", path.display()))?;
    if status.success() {
        report.tmux_sourced = true;
    } else {
        report
            .warnings
            .push(format!("tmux source-file exited with {status}"));
    }
    Ok(())
}

fn backup_path(p: &Path, stamp: i64) -> PathBuf {
    let mut name = p.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".muxa-backup-{stamp}"));
    p.with_file_name(name)
}

fn ensure_parent(p: &Path) -> Result<()> {
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
    }
    Ok(())
}

fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension(format!(
        "{}.muxa-tmp",
        path.extension().and_then(|s| s.to_str()).unwrap_or("")
    ));
    {
        let mut f =
            fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
        f.write_all(contents.as_bytes())
            .with_context(|| format!("writing {}", tmp.display()))?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn describe(a: &Action) -> String {
    match a {
        Action::EditFile { path, .. } => format!("editing {}", path.display()),
        Action::DeleteFile { path, .. } => format!("removing {}", path.display()),
        Action::EnableSystemdUnit => "enabling muxad.service".into(),
        Action::DisableSystemdUnit => "disabling muxad.service".into(),
        Action::EnableLaunchdUnit { .. } => "loading launchd LaunchAgent".into(),
        Action::DisableLaunchdUnit => "unloading launchd LaunchAgent".into(),
        Action::StartDaemonIfNeeded => "starting muxad if needed".into(),
        Action::SourceTmuxConf { path } => format!("sourcing {}", path.display()),
        Action::PrintDashboard { .. } => "rendering dashboard info".into(),
    }
}

/// Spawn `muxad` as a detached background process. Returns `Ok(true)`
/// when we actually launched a new one, `Ok(false)` when the daemon
/// was already serving requests on its IPC socket.
///
/// We probe the socket directly rather than `pgrep -x muxad`. The
/// pgrep approach was the source of the v0.4.0 confusion: a stale
/// muxad pid lingered with its socket gone, pgrep said "already
/// running", we skipped the spawn, and the user's next `muxa status`
/// still failed. Socket-connect captures the only thing that actually
/// matters — "is the daemon answering" — and on a true cold-start
/// (no muxad anywhere) it errors out in microseconds anyway.
///
/// After spawn we *poll* for the socket to come up rather than
/// sleeping a flat 300 ms. Hot path returns in 20-40 ms; slow
/// hardware / VMs / CI runners get up to a generous 3 s grace before
/// we give up and surface the failure as a warning to the caller.
fn start_muxad_detached() -> Result<bool> {
    use std::process::Stdio;
    use std::time::Duration;

    let socket = super::util::default_muxad_socket();
    if super::util::muxad_responsive(&socket) {
        return Ok(false);
    }
    // Detach: fork-and-forget via the shell so we don't keep the
    // current process tied to the daemon's stdio. Output goes to
    // `/tmp/muxad.log` for debugging.
    let status = Command::new("sh")
        .arg("-c")
        .arg("nohup muxad >/tmp/muxad.log 2>&1 & disown 2>/dev/null || true")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("spawning muxad")?;
    if !status.success() {
        return Err(anyhow!(
            "muxad spawn shell exited with {}",
            status
                .code()
                .map_or_else(|| "signal".into(), |c| c.to_string())
        ));
    }
    // Wait for the socket to appear so a follow-up `muxa status`
    // doesn't race the daemon's startup. Returns false if the
    // process didn't bind in time, in which case the orchestrator's
    // verify step will surface a "muxad not responding" warning.
    if !super::util::wait_for_muxad(&socket, Duration::from_secs(3)) {
        return Err(anyhow!(
            "muxad started but did not bind {} within 3s; check /tmp/muxad.log",
            socket.display()
        ));
    }
    Ok(true)
}

/// Render a plan as dry-run diff lines. One line per action — the
/// caller (ui.rs) wraps this in a cliclack note.
pub fn render_dry_run(plan: &Plan) -> String {
    use std::fmt::Write as _;
    let mut s = String::new();
    let verb = match plan.direction {
        Direction::Install => "install",
        Direction::Uninstall => "uninstall",
    };
    let _ = writeln!(s, "Plan ({verb}):");
    for a in &plan.actions {
        match a {
            Action::EditFile {
                component,
                path,
                before,
                after,
                outcome,
            } => {
                let tag = outcome_tag(*outcome);
                let summary = match outcome {
                    Outcome::Unchanged | Outcome::AlreadyAbsent => "[no change]".into(),
                    _ => diff_summary(before.as_deref().unwrap_or(""), after),
                };
                let _ = writeln!(
                    s,
                    "  {tag} edit  {} ({}) {summary}",
                    path.display(),
                    label(*component),
                );
            }
            Action::DeleteFile { component, path } => {
                let _ = writeln!(s, "  ✗ remove {} ({})", path.display(), label(*component));
            }
            Action::EnableSystemdUnit => {
                let _ = writeln!(s, "  ⚙ systemctl --user enable --now muxad.service");
            }
            Action::DisableSystemdUnit => {
                let _ = writeln!(s, "  ⚙ systemctl --user disable --now muxad.service");
            }
            Action::EnableLaunchdUnit { .. } => {
                let _ = writeln!(
                    s,
                    "  ⚙ launchctl bootstrap gui/<uid> dev.open330.muxad.plist"
                );
            }
            Action::DisableLaunchdUnit => {
                let _ = writeln!(s, "  ⚙ launchctl bootout gui/<uid>/dev.open330.muxad");
            }
            Action::StartDaemonIfNeeded => {
                let _ = writeln!(s, "  ⚙ start muxad if not running");
            }
            Action::SourceTmuxConf { path } => {
                let _ = writeln!(s, "  ⟳ tmux source-file {}", path.display());
            }
            Action::PrintDashboard { bind, .. } => {
                let _ = writeln!(s, "  ✓ dashboard token generated; URL: http://{bind}/");
            }
        }
    }
    if !plan.has_changes() {
        s.push_str("  (no changes)\n");
    }
    s
}

fn outcome_tag(o: Outcome) -> &'static str {
    match o {
        Outcome::Inserted => "+",
        Outcome::Replaced => "~",
        Outcome::Removed => "-",
        Outcome::Unchanged | Outcome::AlreadyAbsent => "·",
    }
}

fn label(c: Component) -> &'static str {
    c.id()
}

fn diff_summary(before: &str, after: &str) -> String {
    use std::cmp::Ordering;
    let bl = before.lines().count();
    let al = after.lines().count();
    match al.cmp(&bl) {
        Ordering::Equal => format!("[~{} lines edited]", al.min(bl)),
        Ordering::Greater => format!("[+{} lines]", al - bl),
        Ordering::Less => format!("[-{} lines]", bl - al),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_path_appends_stamp() {
        let p = PathBuf::from("/tmp/foo.conf");
        let b = backup_path(&p, 1_234_567_890);
        assert_eq!(b.to_string_lossy(), "/tmp/foo.conf.muxa-backup-1234567890");
    }
}
