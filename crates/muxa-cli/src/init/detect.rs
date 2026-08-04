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
    pub opencode_config: Option<PathBuf>,
    pub muxad_running: bool,
    pub systemd_user_available: bool,
    pub launchctl_available: bool,
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
            opencode_config: existing_dir(opencode_config_dir()),
            muxad_running: muxad_is_running(),
            systemd_user_available: super::files::systemd::systemd_available(),
            launchctl_available: super::files::launchd::launchctl_available(),
        }
    }

    /// Components that should be pre-checked by default given this
    /// detection result.
    pub fn default_selection(&self) -> Vec<Component> {
        let mut out = Vec::new();
        if self.tmux.is_some() {
            out.push(Component::TmuxPopup);
            out.push(Component::TmuxStatusLine);
            out.push(Component::TmuxPeek);
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
        // Pre-check the daemon-manager that fits this host so the
        // wizard's default produces a working install. The picker
        // hides the others (filtered via `Component::applicable_here`).
        out.push(self.recommended_daemon_manager());
        // Dashboard stays opt-in — costs a port and a token.
        out
    }

    /// Whether an agent-hook component's config is present on disk.
    ///
    /// Returns `Some(true)`/`Some(false)` for the four agent hooks
    /// (Claude/Codex/Gemini/opencode) and `None` for every other
    /// component — the caller reads `None` as "not an agent hook, always
    /// keep". This is what lets the non-interactive/preset path skip
    /// wiring up an agent the user doesn't actually have installed,
    /// mirroring the pre-check logic in `default_selection`.
    pub fn agent_config_present(&self, c: Component) -> Option<bool> {
        match c {
            Component::ClaudeHooks => Some(self.claude_settings.is_some()),
            Component::CodexHooks => Some(self.codex_config.is_some()),
            Component::GeminiHooks => Some(self.gemini_settings.is_some()),
            Component::OpencodeHooks => Some(self.opencode_config.is_some()),
            _ => None,
        }
    }

    /// The OS-appropriate auto-start manager for this host. Defers to
    /// the static `Component::recommended_daemon_manager()` but degrades
    /// to `MuxadShellrc` when systemctl/launchctl is missing on a host
    /// that would normally support them (containers, CI sandboxes).
    pub fn recommended_daemon_manager(&self) -> Component {
        match Component::recommended_daemon_manager() {
            Component::MuxadSystemd if !self.systemd_user_available => Component::MuxadShellrc,
            Component::MuxadLaunchd if !self.launchctl_available => Component::MuxadShellrc,
            other => other,
        }
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
        // Only nag about a missing daemon-manager *if it's the one we
        // would have recommended for this host*. Linux without
        // systemctl is interesting; macOS without systemctl is normal.
        let dm = self.recommended_daemon_manager();
        if dm == Component::MuxadShellrc
            && Component::recommended_daemon_manager() != Component::MuxadShellrc
        {
            warnings.push(
                "no service manager available — falling back to shellrc autostart for muxad".into(),
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

fn existing_dir(p: Option<PathBuf>) -> Option<PathBuf> {
    p.filter(|p| p.is_dir())
}

/// opencode has no single well-known settings file the way the other
/// agents do — it keeps state under `~/.config/opencode/` (that's also
/// where its `plugins/` dir lives). Presence of that directory is our
/// "opencode is installed" signal.
fn opencode_config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("opencode"))
}

fn home_join(rel: &str) -> PathBuf {
    dirs::home_dir().unwrap_or_default().join(rel)
}

/// Is muxad actually serving requests? We probe its IPC socket
/// directly rather than asking `pgrep` whether *some* process named
/// `muxad` exists. The pgrep approach was misleading after a v0.4.0
/// incident: a stale muxad pid lingered with its socket gone, pgrep
/// said "running", IPC clients then couldn't connect. Socket-connect
/// is a strict superset of "the daemon you actually want to talk to
/// is up".
fn muxad_is_running() -> bool {
    super::util::muxad_responsive(&super::util::default_muxad_socket())
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
    fn default_selection_with_no_tools_only_picks_daemon_manager() {
        // `Detection::default()` represents a host with no tmux,
        // no agents, no service managers. We still pre-check the
        // OS-fallback daemon-manager (`MuxadShellrc`) so a default
        // install still produces a working muxad — that's the whole
        // point of the post-v0.4.1 wizard.
        let d = Detection::default();
        let sel = d.default_selection();
        assert_eq!(sel, vec![Component::MuxadShellrc]);
    }

    #[test]
    fn blockers_fire_when_no_multiplexer() {
        let d = Detection::default();
        let blockers = d.blockers();
        assert!(blockers.iter().any(|m| m.contains("cargo")));
        assert!(blockers.iter().any(|m| m.contains("tmux")));
    }
}
