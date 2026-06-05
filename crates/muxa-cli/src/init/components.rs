//! The catalog of installable components.
//!
//! Each variant is one user-selectable item in the wizard. A component
//! is the unit of: marker-block id, default selection, file-edit
//! target, and uninstall scope. Adding a new component means adding a
//! variant here and a matching arm in `apply.rs` / `uninstall.rs`.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Component {
    /// `prefix + s` → `display-popup -E muxa watch`
    TmuxPopup,
    /// per-pane glyph in `status-right`
    TmuxStatusLine,
    /// Claude Code shell hooks + statusLine
    ClaudeHooks,
    /// Codex shell hooks
    CodexHooks,
    /// Gemini CLI shell hooks
    GeminiHooks,
    /// opencode plugin event bridge
    OpencodeHooks,
    /// `muxad` user-level systemd service (Linux only)
    MuxadSystemd,
    /// `muxad` `launchd` `LaunchAgent` (macOS only)
    MuxadLaunchd,
    /// `muxad` autostart hook in `~/.zshrc` / `~/.bashrc`
    /// (fallback for systems without systemd or launchd —
    /// containers, BSD, WSL1, minimal Linux)
    MuxadShellrc,
    /// Web dashboard: generate token, enable in config
    Dashboard,
}

impl Component {
    pub const ALL: &'static [Component] = &[
        Component::TmuxPopup,
        Component::TmuxStatusLine,
        Component::ClaudeHooks,
        Component::CodexHooks,
        Component::GeminiHooks,
        Component::OpencodeHooks,
        Component::MuxadSystemd,
        Component::MuxadLaunchd,
        Component::MuxadShellrc,
        Component::Dashboard,
    ];

    /// Stable kebab-case id used in CLI flags and marker blocks.
    pub fn id(self) -> &'static str {
        match self {
            Component::TmuxPopup => "tmux-popup",
            Component::TmuxStatusLine => "tmux-statusline",
            Component::ClaudeHooks => "claude-hooks",
            Component::CodexHooks => "codex-hooks",
            Component::GeminiHooks => "gemini-hooks",
            Component::OpencodeHooks => "opencode-hooks",
            Component::MuxadSystemd => "muxad-systemd",
            Component::MuxadLaunchd => "muxad-launchd",
            Component::MuxadShellrc => "muxad-shellrc",
            Component::Dashboard => "dashboard",
        }
    }

    pub fn parse(s: &str) -> Option<Component> {
        Component::ALL.iter().copied().find(|c| c.id() == s)
    }

    /// One-line label shown in the multi-select.
    pub fn label(self) -> &'static str {
        match self {
            Component::TmuxPopup => "tmux: replace prefix+s with muxa watch popup",
            Component::TmuxStatusLine => "tmux: per-pane agent glyphs in status-right",
            Component::ClaudeHooks => "Claude Code: shell hooks + statusLine",
            Component::CodexHooks => "OpenAI Codex: shell hooks",
            Component::GeminiHooks => "Gemini CLI: shell hooks",
            Component::OpencodeHooks => "opencode: plugin event bridge",
            Component::MuxadSystemd => "muxad: systemd user service (auto-start on login)",
            Component::MuxadLaunchd => "muxad: launchd LaunchAgent (auto-start on login)",
            Component::MuxadShellrc => "muxad: shellrc autostart hook (no service manager)",
            Component::Dashboard => "Web dashboard: generate token + enable",
        }
    }

    /// Longer help text shown next to the label.
    pub fn hint(self) -> &'static str {
        match self {
            Component::TmuxPopup => "drop-in replacement for tmux's choose-tree",
            Component::TmuxStatusLine => "⚙ / · / ! per pane, refreshed every 2s",
            Component::ClaudeHooks => "auto-detect when ~/.claude/settings.json exists",
            Component::CodexHooks => "auto-detect when ~/.codex/config.toml exists",
            Component::GeminiHooks => "auto-detect when ~/.gemini/settings.json exists",
            Component::OpencodeHooks => "installs ~/.config/opencode/plugins/muxa.ts",
            Component::MuxadSystemd => "Linux only; skipped on macOS / launchd hosts",
            Component::MuxadLaunchd => "macOS only; skipped on Linux / systemd hosts",
            Component::MuxadShellrc => "appends to ~/.zshrc or ~/.bashrc; cross-platform",
            Component::Dashboard => "loopback :7878 by default; token in config",
        }
    }

    /// The OS-appropriate "auto-start muxad" component for this host.
    /// Returned at runtime so the *same* binary running under a Linux
    /// container vs. on macOS metal picks correctly without rebuild.
    pub fn recommended_daemon_manager() -> Component {
        match std::env::consts::OS {
            "macos" => Component::MuxadLaunchd,
            "linux" => Component::MuxadSystemd,
            // BSDs, illumos, WSL1, anything else — fall back to the
            // shellrc hook since it has no platform prereqs.
            _ => Component::MuxadShellrc,
        }
    }

    /// True if this component makes sense on this host. Used by the
    /// wizard to hide cross-platform-irrelevant options (e.g. don't
    /// offer `MuxadSystemd` on macOS).
    pub fn applicable_here(self) -> bool {
        match self {
            Component::MuxadSystemd => std::env::consts::OS == "linux",
            Component::MuxadLaunchd => std::env::consts::OS == "macos",
            // shellrc + everything else is OS-agnostic.
            _ => true,
        }
    }

    /// Components shipped by each preset.
    pub fn preset(p: Preset) -> Vec<Component> {
        let dm = Component::recommended_daemon_manager();
        match p {
            Preset::Minimal => vec![Component::TmuxPopup, Component::TmuxStatusLine],
            Preset::Standard => vec![
                Component::TmuxPopup,
                Component::TmuxStatusLine,
                Component::ClaudeHooks,
                Component::CodexHooks,
                Component::GeminiHooks,
                Component::OpencodeHooks,
                dm,
            ],
            Preset::Full => {
                let mut v = Component::preset(Preset::Standard);
                v.push(Component::Dashboard);
                v
            }
        }
    }
}

impl fmt::Display for Component {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Minimal,
    Standard,
    Full,
}

impl Preset {
    pub fn parse(s: &str) -> Option<Preset> {
        match s {
            "minimal" => Some(Preset::Minimal),
            "standard" => Some(Preset::Standard),
            "full" => Some(Preset::Full),
            _ => None,
        }
    }
}

impl fmt::Display for Preset {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Preset::Minimal => "minimal",
            Preset::Standard => "standard",
            Preset::Full => "full",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_round_trip() {
        for c in Component::ALL {
            assert_eq!(Component::parse(c.id()), Some(*c));
        }
    }

    #[test]
    fn preset_parses() {
        assert_eq!(Preset::parse("minimal"), Some(Preset::Minimal));
        assert_eq!(Preset::parse("standard"), Some(Preset::Standard));
        assert_eq!(Preset::parse("full"), Some(Preset::Full));
        assert_eq!(Preset::parse("bogus"), None);
    }

    #[test]
    fn presets_grow_monotonically() {
        let m = Component::preset(Preset::Minimal);
        let s = Component::preset(Preset::Standard);
        let f = Component::preset(Preset::Full);
        for c in &m {
            assert!(s.contains(c), "standard should include all of minimal");
        }
        for c in &s {
            assert!(f.contains(c), "full should include all of standard");
        }
    }

    #[test]
    fn standard_preset_includes_one_daemon_manager() {
        let s = Component::preset(Preset::Standard);
        let dm_count = s
            .iter()
            .filter(|c| {
                matches!(
                    c,
                    Component::MuxadSystemd | Component::MuxadLaunchd | Component::MuxadShellrc
                )
            })
            .count();
        assert_eq!(
            dm_count, 1,
            "standard should pick exactly one daemon-manager"
        );
    }

    #[test]
    fn applicable_here_filters_per_os() {
        // The two OS-locked managers can't both be true on the same host.
        let systemd_ok = Component::MuxadSystemd.applicable_here();
        let launchd_ok = Component::MuxadLaunchd.applicable_here();
        assert!(
            !(systemd_ok && launchd_ok),
            "systemd and launchd are mutually exclusive per host"
        );
        // The OS-agnostic shellrc fallback is always applicable.
        assert!(Component::MuxadShellrc.applicable_here());
    }
}
