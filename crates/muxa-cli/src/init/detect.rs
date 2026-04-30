//! Pre-flight environment probing.
//!
//! Used for two things:
//!
//! 1. **Hard gates.** Cargo missing or no tmux + zellij anywhere on
//!    PATH means the wizard exits early with a friendly message —
//!    there's nothing to wire.
//! 2. **Smart defaults.** When a user runs `muxa init` interactively,
//!    components are pre-checked based on what we found: Claude config
//!    file present → `claude-hooks` ✓; macOS → `muxad-systemd` greyed
//!    out; etc. The user can still toggle.

use crate::init::components::Component;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Default)]
pub struct Detection {
    pub tmux: Option<String>,
    pub zellij: Option<String>,
    pub cargo: Option<String>,
    pub claude_settings: Option<PathBuf>,
    pub codex_config: Option<PathBuf>,
    pub gemini_settings: Option<PathBuf>,
    pub muxad_running: bool,
    pub systemd_user_available: bool,
}

impl Detection {
    pub fn run() -> Detection {
        Detection {
            tmux: tool_version("tmux", &["-V"]),
            zellij: tool_version("zellij", &["--version"]),
            cargo: tool_version("cargo", &["--version"]),
            claude_settings: existing_file(home_join(".claude/settings.json")),
            codex_config: existing_file(home_join(".codex/config.toml")),
            gemini_settings: existing_file(home_join(".gemini/settings.json")),
            muxad_running: muxad_is_running(),
            systemd_user_available: super::files::systemd::systemd_available(),
        }
    }

    /// Components that should be pre-checked by default given this
    /// detection result.
    pub fn default_selection(&self) -> Vec<Component> {
        let mut out = Vec::new();
        if self.tmux.is_some() {
            out.push(Component::TmuxPopup);
            out.push(Component::TmuxStatusLine);
        }
        if self.claude_settings.is_some() {
            out.push(Component::ClaudeHooks);
        }
        if self.codex_config.is_some() {
            out.push(Component::CodexHooks);
        }
        if self.gemini_settings.is_some() {
            out.push(Component::GeminiHooks);
        }
        // Don't pre-check systemd / dashboard — they're opt-in.
        out
    }

    /// Hard-gate the wizard. Empty Vec means "go ahead".
    pub fn blockers(&self) -> Vec<&'static str> {
        let mut blockers = Vec::new();
        if self.cargo.is_none() {
            blockers.push("`cargo` not found on PATH (we need it to install muxa)");
        }
        if self.tmux.is_none() && self.zellij.is_none() {
            blockers.push("neither tmux nor zellij found on PATH — install one first");
        }
        blockers
    }

    /// Soft warnings — render but don't block.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if !self.systemd_user_available {
            warnings.push(
                "systemctl --user unavailable — `muxad-systemd` component will be skipped if selected".into(),
            );
        }
        if self.tmux.is_none() && self.zellij.is_some() {
            warnings.push("tmux not found — only zellij components will be considered".into());
        }
        warnings
    }
}

fn tool_version(name: &str, args: &[&str]) -> Option<String> {
    if which::which(name).is_err() {
        return None;
    }
    let out = Command::new(name).args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Some(if s.is_empty() { name.to_string() } else { s })
}

fn existing_file(p: PathBuf) -> Option<PathBuf> {
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

fn home_join(rel: &str) -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(rel)
}

/// Best-effort check for a running muxad. We just look for any
/// `muxad` process owned by the current uid via `pgrep`. False on
/// systems without `pgrep` (e.g. minimal containers); the verify
/// stage will retry against the daemon's IPC socket anyway.
fn muxad_is_running() -> bool {
    Command::new("pgrep")
        .args(["-u", &uid_string(), "-x", "muxad"])
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false)
}

fn uid_string() -> String {
    Command::new("id")
        .arg("-u")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map_or_else(|| "0".into(), |s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_runs_without_panicking() {
        // Just confirm the call shape — environment-dependent assertions
        // would be flaky in CI. We require `cargo` because cargo built us.
        let d = Detection::run();
        assert!(d.cargo.is_some());
    }

    #[test]
    fn default_selection_is_empty_when_nothing_detected() {
        let d = Detection::default();
        assert!(d.default_selection().is_empty());
    }

    #[test]
    fn blockers_fire_when_no_multiplexer() {
        let d = Detection::default();
        let blockers = d.blockers();
        assert!(blockers.iter().any(|m| m.contains("cargo")));
        assert!(blockers.iter().any(|m| m.contains("tmux")));
    }
}
