//! UI layer — cliclack wrappers + non-interactive fallback.
//!
//! The interactive path is straightforward (cliclack does the heavy
//! lifting); the trick is that `--yes` / CI environments must produce
//! the same visible structure without ever blocking on a TTY. We
//! achieve that by routing every "ask" through a tiny trait that has
//! both an interactive and a print-only impl. Non-interactive runs
//! still emit `◇`-prefixed lines so log output is useful in CI.

use crate::init::components::Component;
use crate::init::detect::Detection;
use anyhow::Result;
use std::io::{self, IsTerminal, Write};

/// How user prompts should be sourced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Live wizard via cliclack.
    Interactive,
    /// Auto-confirm everything; emit progress lines only.
    NonInteractive,
}

impl Mode {
    pub fn detect(yes_flag: bool) -> Mode {
        if yes_flag
            || std::env::var_os("CI").is_some()
            || !io::stdout().is_terminal()
            || !io::stdin().is_terminal()
        {
            Mode::NonInteractive
        } else {
            Mode::Interactive
        }
    }
}

pub fn intro(mode: Mode) {
    match mode {
        Mode::Interactive => {
            let _ = cliclack::intro("muxa init");
        }
        Mode::NonInteractive => {
            println!("┌  muxa init");
            println!("│");
        }
    }
}

pub fn outro(mode: Mode, msg: &str) {
    match mode {
        Mode::Interactive => {
            let _ = cliclack::outro(msg);
        }
        Mode::NonInteractive => {
            println!("│");
            println!("└  {msg}");
        }
    }
}

pub fn note(mode: Mode, title: &str, body: &str) {
    match mode {
        Mode::Interactive => {
            let _ = cliclack::note(title, body);
        }
        Mode::NonInteractive => {
            println!("◇  {title}");
            for line in body.lines() {
                println!("│  {line}");
            }
            println!("│");
        }
    }
}

pub fn warn_line(mode: Mode, msg: &str) {
    match mode {
        Mode::Interactive => {
            let _ = cliclack::log::warning(msg);
        }
        Mode::NonInteractive => {
            println!("⚠  {msg}");
        }
    }
}

pub fn error_line(mode: Mode, msg: &str) {
    match mode {
        Mode::Interactive => {
            let _ = cliclack::log::error(msg);
        }
        Mode::NonInteractive => {
            eprintln!("✗  {msg}");
        }
    }
}

/// Render the pre-flight detection block.
pub fn render_detection(mode: Mode, d: &Detection) {
    let mut lines = Vec::new();
    lines.push(check("cargo", d.cargo.as_deref()));
    lines.push(check("tmux", d.tmux.as_deref()));
    if d.zellij.is_some() {
        lines.push(check("zellij", d.zellij.as_deref()));
    }
    lines.push(check_path("Claude Code config", d.claude_settings.as_ref()));
    lines.push(check_path("Codex config", d.codex_config.as_ref()));
    lines.push(check_path("Gemini CLI config", d.gemini_settings.as_ref()));
    lines.push(format!(
        "{} systemctl --user",
        if d.systemd_user_available {
            "✔"
        } else {
            "—"
        }
    ));
    // Distinct text per state — the previous "muxad already running"
    // string was shown with a `·` glyph even when muxad was NOT
    // running, which read as a contradiction at first glance.
    lines.push(if d.muxad_running {
        "✔ muxad responding".into()
    } else {
        "· muxad not running (will be started on apply)".into()
    });
    note(mode, "Pre-flight", &lines.join("\n"));
}

fn check(label: &str, version: Option<&str>) -> String {
    match version {
        Some(v) => format!("✔ {label}: {v}"),
        None => format!("✗ {label}: not found"),
    }
}

fn check_path(label: &str, p: Option<&std::path::PathBuf>) -> String {
    match p {
        Some(path) => format!("✔ {label}: {}", path.display()),
        None => format!("· {label}: not detected"),
    }
}

/// Multi-select component picker with auto-detected defaults. In
/// non-interactive mode this is bypassed (caller picks via preset).
/// Components that don't apply to this host (e.g. `MuxadSystemd` on
/// macOS, `MuxadLaunchd` on Linux) are filtered out so the picker
/// shows a clean, host-specific menu instead of forcing the user to
/// reason about which option is the right one for them.
pub fn pick_components(detect: &Detection) -> Result<Vec<Component>> {
    let defaults = detect.default_selection();
    let mut ms = cliclack::multiselect("What should I set up?").required(false);
    for c in Component::ALL.iter().filter(|c| c.applicable_here()) {
        ms = ms.item(*c, c.label(), c.hint());
    }
    let chosen = ms.initial_values(defaults).interact()?;
    Ok(chosen)
}

pub fn confirm_apply(mode: Mode, count: usize) -> Result<bool> {
    if count == 0 {
        return Ok(false);
    }
    match mode {
        Mode::NonInteractive => Ok(true),
        Mode::Interactive => {
            let prompt = if count == 1 {
                "Apply 1 change?".to_string()
            } else {
                format!("Apply {count} changes?")
            };
            Ok(cliclack::confirm(prompt).initial_value(true).interact()?)
        }
    }
}

/// Print a tagged line right under the most recent step. Used by
/// apply.rs's running output.
pub fn step(mode: Mode, msg: &str) {
    match mode {
        Mode::Interactive => {
            let _ = cliclack::log::step(msg);
        }
        Mode::NonInteractive => {
            println!("│  → {msg}");
            let _ = io::stdout().flush();
        }
    }
}

/// Render the next-step box after a successful run. Mirrors clack's
/// outro shape so we get a consistent landing screen.
pub fn final_summary(
    mode: Mode,
    edited: usize,
    backups: usize,
    dashboard: Option<(&str, &str)>,
    extra_lines: &[String],
) {
    let mut body = Vec::new();
    body.push(format!("{edited} files updated, {backups} backups written"));
    for l in extra_lines {
        body.push(l.clone());
    }
    if let Some((bind, token)) = dashboard {
        body.push(format!("Dashboard: {}", dashboard_url(bind, token)));
        body.push("Token also stored in your config.toml.".into());
    }
    body.push("Use `prefix+s` to watch and collaborate; `prefix+D` opens the dashboard.".into());
    body.push("Learn the workflow and shortcuts with `muxa onboard`.".into());
    body.push("Roll back with `muxa init --uninstall`.".into());
    note(mode, "Done", &body.join("\n"));
}

fn dashboard_url(bind: &str, token: &str) -> String {
    format!("http://{bind}/#token={token}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_bootstrap_url_uses_fragment() {
        let url = dashboard_url("127.0.0.1:7878", "secret");

        assert_eq!(url, "http://127.0.0.1:7878/#token=secret");
        assert!(!url.contains("?token="));
    }
}
