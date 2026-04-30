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
use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use time::OffsetDateTime;

/// Side-effect summary returned to the orchestrator so it can paint
/// the final outcome UI.
#[derive(Debug, Default)]
pub struct ApplyReport {
    pub edited: Vec<PathBuf>,
    pub deleted: Vec<PathBuf>,
    pub backups: Vec<PathBuf>,
    pub systemd_enabled: bool,
    pub systemd_disabled: bool,
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
            component: _,
            path,
            before,
            after,
            outcome,
        } => {
            if matches!(outcome, Outcome::Unchanged | Outcome::AlreadyAbsent) {
                return Ok(());
            }
            if dry_run {
                report.edited.push(path.clone());
                return Ok(());
            }
            if let Some(prev) = before {
                let backup = backup_path(path, stamp);
                fs::write(&backup, prev)
                    .with_context(|| format!("writing backup {}", backup.display()))?;
                report.backups.push(backup);
            }
            ensure_parent(path)?;
            atomic_write(path, after)?;
            report.edited.push(path.clone());
        }
        Action::DeleteFile { component: _, path } => {
            if dry_run {
                report.deleted.push(path.clone());
                return Ok(());
            }
            match fs::remove_file(path) {
                Ok(()) => {
                    report.deleted.push(path.clone());
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("removing {}", path.display())),
            }
        }
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
        }
        Action::DisableSystemdUnit => {
            if dry_run {
                return Ok(());
            }
            files::systemd::disable_service();
            report.systemd_disabled = true;
        }
        Action::SourceTmuxConf { path } => {
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
        }
        Action::PrintDashboard { token, bind } => {
            report.dashboard = Some(DashboardInfo {
                token: token.clone(),
                bind: bind.clone(),
            });
        }
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
        Action::SourceTmuxConf { path } => format!("sourcing {}", path.display()),
        Action::PrintDashboard { .. } => "rendering dashboard info".into(),
    }
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
