//! `muxa init` — interactive install wizard.
//!
//! Modular layout:
//!
//! - `components`  — the catalog of selectable items + presets
//! - `marker`      — generic comment-fenced "managed block" editor
//! - `files/*`     — per-target content layers (tmux, claude, codex, …)
//! - `detect`      — pre-flight environment probing
//! - `plan`/`apply`/`verify` — three phases of the install pipeline
//! - `ui`          — cliclack wrappers + non-interactive printer

pub mod apply;
pub mod components;
pub mod detect;
pub mod files;
pub mod marker;
pub mod plan;
pub mod ui;
pub mod util;
pub mod verify;

use crate::init::components::{Component, Preset};
use crate::init::detect::Detection;
use crate::init::plan::Direction;
use crate::init::ui::Mode;
use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser, Default)]
// Each bool is a distinct, well-known CLI flag. Collapsing them into a
// state-machine enum (clippy's suggestion) would be substantially less
// usable than the documented flag surface.
#[allow(clippy::struct_excessive_bools)]
pub struct Args {
    /// Apply a named preset instead of opening the wizard.
    #[arg(long, value_parser = parse_preset)]
    pub preset: Option<Preset>,

    /// Auto-confirm every prompt. CI environments (CI=true) imply this.
    #[arg(long, short = 'y', env = "MUXA_INIT_YES")]
    pub yes: bool,

    /// Compute and render the plan, but do not write or run anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Reverse a previous install — strip every muxa-managed block.
    #[arg(long)]
    pub uninstall: bool,

    /// Force the wizard to re-prompt even if the components look already
    /// configured.
    #[arg(long)]
    pub reconfigure: bool,

    /// Comma-separated component ids (`tmux-popup,claude-hooks,…`).
    /// When set, `--preset` and the wizard are bypassed.
    #[arg(long, value_delimiter = ',')]
    pub component: Vec<String>,

    /// Skip every component whose id starts with the given prefix.
    /// Repeatable (`--no tmux-popup --no claude-hooks`). Useful for
    /// preset+exclusion combos like `--preset standard --no muxad-systemd`.
    #[arg(long = "no", value_name = "ID")]
    pub no: Vec<String>,

    /// After applying, start `muxad` if it isn't already running.
    /// Default on — this is what makes the wizard "leave the system
    /// in a working state" on hosts where no service-manager component
    /// is selected (containers, BSDs, WSL1) or where the chosen
    /// manager hasn't kicked in yet. Pass `--start-daemon=false` to
    /// suppress (e.g. dotfile bootstrap that prefers to do it
    /// out-of-band).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub start_daemon: bool,
}

fn parse_preset(s: &str) -> Result<Preset, String> {
    Preset::parse(s).ok_or_else(|| format!("unknown preset '{s}' (try minimal | standard | full)"))
}

pub async fn run(args: Args, socket: PathBuf) -> Result<()> {
    let mode = Mode::detect(args.yes);
    ui::intro(mode);

    let detect = Detection::run();
    if !preflight_ok(mode, &detect, args.uninstall) {
        anyhow::bail!("pre-flight blockers");
    }

    let chosen = pick(args.uninstall, &args, &detect, mode)?;
    if chosen.is_empty() {
        ui::outro(mode, "Nothing to do.");
        return Ok(());
    }

    let direction = if args.uninstall {
        Direction::Uninstall
    } else {
        Direction::Install
    };
    let mut plan = plan::build(direction, &chosen, &detect)?;
    if matches!(direction, Direction::Install) && args.start_daemon && !manages_daemon(&chosen) {
        // Append last so disk edits + service enablement happen first;
        // by the time we try to start muxad the file/socket layout it
        // expects is fully in place.
        //
        // Suppressed when a real service manager (systemd / launchd)
        // was selected: those components own the spawn lifecycle, and
        // a parallel `nohup muxad &` here races their startup. The
        // orphan we'd spawn wins the socket bind, the manager's child
        // exits with EADDRINUSE, and `KeepAlive` / `Restart=on-failure`
        // is dead-on-arrival. Reported by a user on macOS — `muxa
        // status` worked but `pkill -9 muxad` left muxad gone with no
        // auto-restart.
        plan.actions.push(plan::Action::StartDaemonIfNeeded);
    }
    for w in &plan.warnings {
        ui::warn_line(mode, w);
    }

    // Always render the plan — interactive users see the diff before
    // confirm, --yes / dry-run see it as logged context.
    let dry = apply::render_dry_run(&plan);
    ui::note(mode, "Review changes", dry.trim_end());

    if args.dry_run {
        ui::outro(mode, "Dry run — no changes written.");
        return Ok(());
    }

    if !plan.has_changes() {
        ui::outro(mode, "Already in the desired state.");
        return Ok(());
    }

    if !ui::confirm_apply(mode, plan.actions.len())? {
        ui::outro(mode, "Cancelled.");
        return Ok(());
    }

    let report = apply::run(&plan, false).context("applying plan")?;
    render_apply_steps(mode, &report);

    // Propagate MUXA_SOCKET into the tmux server environment so that
    // every pane — including the one that runs `muxa status-line` in
    // `status-right` — uses the same socket path after a daemon restart.
    // Without this, tmux panes started before `muxad` was first spun up
    // inherit an empty MUXA_SOCKET and fall back to the default path,
    // which races when the daemon is restarted on a different socket.
    if report.tmux_sourced {
        if let Some(s) = socket.to_str() {
            let _ = std::process::Command::new("tmux")
                .args(["set-environment", "-g", "MUXA_SOCKET", s])
                .status();
            // Push the new value down to existing panes so the current
            // session's status-right (and any hook commands) picks it up
            // immediately without requiring a pane restart.
            let _ = std::process::Command::new("tmux")
                .args(["update-environment", "MUXA_SOCKET"])
                .status();
        }
    }

    let v = verify::run(&plan, socket).await?;
    let extra = summarize_verify(&v);
    let dashboard = report
        .dashboard
        .as_ref()
        .map(|d| (d.bind.as_str(), d.token.as_str()));
    ui::final_summary(
        mode,
        report.edited.len(),
        report.backups.len(),
        dashboard,
        &extra,
    );

    let outro_msg = if args.uninstall {
        "Uninstalled."
    } else {
        "Done. Try `prefix + s` for the muxa picker."
    };
    ui::outro(mode, outro_msg);
    Ok(())
}

/// Render pre-flight, surface warnings, and signal whether we should
/// proceed. `false` means a hard blocker fired (caller bails).
fn preflight_ok(mode: Mode, detect: &Detection, uninstall: bool) -> bool {
    let blockers = detect.blockers();
    if !blockers.is_empty() && !uninstall {
        for b in &blockers {
            ui::error_line(mode, b);
        }
        ui::outro(mode, "Aborting — install the missing tools and try again.");
        return false;
    }
    ui::render_detection(mode, detect);
    for w in detect.warnings() {
        ui::warn_line(mode, &w);
    }
    true
}

fn render_apply_steps(mode: Mode, report: &apply::ApplyReport) {
    for path in &report.edited {
        ui::step(mode, &format!("wrote {}", path.display()));
    }
    for path in &report.deleted {
        ui::step(mode, &format!("removed {}", path.display()));
    }
    for backup in &report.backups {
        ui::step(mode, &format!("backup → {}", backup.display()));
    }
    if report.systemd_enabled {
        ui::step(mode, "systemctl --user enable --now muxad.service");
    }
    if report.systemd_disabled {
        ui::step(mode, "systemctl --user disable --now muxad.service");
    }
    if report.launchd_enabled {
        ui::step(mode, "launchctl bootstrap dev.open330.muxad");
    }
    if report.launchd_disabled {
        ui::step(mode, "launchctl bootout dev.open330.muxad");
    }
    if report.daemon_started {
        ui::step(mode, "started muxad");
    }
    if report.tmux_sourced {
        ui::step(mode, "tmux source-file (config reloaded live)");
    }
    for w in &report.warnings {
        ui::warn_line(mode, w);
    }
}

fn summarize_verify(v: &verify::VerifyReport) -> Vec<String> {
    let mut extra = Vec::new();
    match v.muxad_responsive {
        Some(true) => extra.push("✔ muxad responding".into()),
        Some(false) => extra.push("⚠ muxad not responding (try `muxad &`)".into()),
        None => {}
    }
    if v.current_pane_seen == Some(true) {
        extra.push("✔ current pane registered with muxad".into());
    }
    for n in &v.notes {
        extra.push(n.clone());
    }
    extra
}

/// Resolve which components to install/uninstall. Precedence:
///
/// 1. `--component` (explicit list — wins over everything)
/// 2. `--preset`
/// 3. Interactive multi-select (only on `Mode::Interactive`)
/// 4. Detection's default selection (in non-interactive mode without preset)
fn pick(uninstall: bool, args: &Args, detect: &Detection, mode: Mode) -> Result<Vec<Component>> {
    if !args.component.is_empty() {
        let mut picked = Vec::new();
        for id in &args.component {
            let id = id.trim();
            if id.is_empty() {
                continue;
            }
            let c = Component::parse(id).with_context(|| format!("unknown component: {id}"))?;
            picked.push(c);
        }
        return Ok(filter_excluded(picked, &args.no));
    }

    if let Some(p) = args.preset {
        let picked = Component::preset(p);
        return Ok(filter_excluded(picked, &args.no));
    }

    // Uninstall without a preset means "remove everything we can detect"
    // — but only blocks/edits we actually own (the file editors are
    // idempotent on already-clean files, so this is safe).
    if uninstall {
        return Ok(Component::ALL.to_vec());
    }

    match mode {
        Mode::Interactive => Ok(filter_excluded(ui::pick_components(detect)?, &args.no)),
        Mode::NonInteractive => {
            // No preset, no flags, can't prompt → use detection defaults.
            Ok(filter_excluded(detect.default_selection(), &args.no))
        }
    }
}

/// True iff one of the components owns the muxad spawn lifecycle.
/// `MuxadShellrc` *does* start muxad lazily on the next interactive
/// shell, but it doesn't run during `muxa init` itself — we still
/// want `--start-daemon` to fire so the user has a working muxad in
/// the same session.
fn manages_daemon(components: &[Component]) -> bool {
    components
        .iter()
        .any(|c| matches!(c, Component::MuxadSystemd | Component::MuxadLaunchd))
}

fn filter_excluded(components: Vec<Component>, no: &[String]) -> Vec<Component> {
    if no.is_empty() {
        return components;
    }
    components
        .into_iter()
        .filter(|c| !no.iter().any(|n| n == c.id()))
        .collect()
}
