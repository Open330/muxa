//! `~/.tmux.conf` content layer.
//!
//! Pure functions: take the existing file content + which components
//! are selected, return the new content. All I/O (read, backup, write,
//! `tmux source-file`) is handled by `apply.rs`.

use crate::init::components::Component;
use crate::init::marker::{self, Outcome};

/// The body that goes inside the `tmux-popup` marker block.
pub const POPUP_BODY: &str = r#"# Replace tmux's stock prefix+s (choose-tree) with muxa watch in a popup.
# Enter on a row attaches to that pane and closes the popup.
bind-key s display-popup -E -w 90% -h 85% "muxa watch""#;

/// The body that goes inside the `tmux-statusline` marker block.
pub const STATUSLINE_BODY: &str = r##"# Per-pane agent glyph (⚙ working / · idle / ! waiting / ✗ error)
#
# NOTE: `muxa init` sets MUXA_SOCKET in the tmux server environment so
# that every pane — including the one that runs this status-right
# command — uses the same socket path after a daemon restart. Without
# it, heartbeats can silently miss the daemon and rows get stuck in
# `Starting` indefinitely.
set -g status-interval 2
set -g status-right-length 140
set -g status-right "#(muxa status-line --pane #{pane_id}) | #[fg=white]%H:%M""##;

/// Apply (insert or replace) the given component to the supplied tmux
/// config text. Other components' blocks are left untouched. Components
/// that don't write to tmux.conf return `Outcome::Unchanged`.
pub fn upsert(original: &str, component: Component) -> (String, Outcome) {
    match component {
        Component::TmuxPopup => marker::upsert(original, component.id(), POPUP_BODY),
        Component::TmuxStatusLine => marker::upsert(original, component.id(), STATUSLINE_BODY),
        _ => (original.to_string(), Outcome::Unchanged),
    }
}

/// Reverse a previous `upsert`. No-op if the block is already absent.
pub fn remove(original: &str, component: Component) -> (String, Outcome) {
    match component {
        Component::TmuxPopup | Component::TmuxStatusLine => {
            marker::remove(original, component.id())
        }
        _ => (original.to_string(), Outcome::Unchanged),
    }
}

/// Default path for `~/.tmux.conf`. Returns `None` only if `$HOME` is
/// unset, which is exotic enough that we let the caller decide how to
/// surface the error.
pub fn default_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".tmux.conf"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn popup_round_trip() {
        let (after, o1) = upsert("", Component::TmuxPopup);
        assert_eq!(o1, Outcome::Inserted);
        assert!(after.contains("display-popup"));
        assert!(after.contains("muxa watch"));

        let (after2, o2) = upsert(&after, Component::TmuxPopup);
        assert_eq!(o2, Outcome::Unchanged);
        assert_eq!(after, after2);

        let (after3, o3) = remove(&after2, Component::TmuxPopup);
        assert_eq!(o3, Outcome::Removed);
        assert!(!after3.contains("display-popup"));
    }

    #[test]
    fn statusline_and_popup_coexist() {
        let (s, _) = upsert("", Component::TmuxPopup);
        let (s, _) = upsert(&s, Component::TmuxStatusLine);
        assert!(s.contains("display-popup"));
        assert!(s.contains("status-right"));

        // Removing one should leave the other alone.
        let (s, _) = remove(&s, Component::TmuxPopup);
        assert!(!s.contains("display-popup"));
        assert!(s.contains("status-right"));
    }

    #[test]
    fn upsert_preserves_user_config_around_block() {
        let user = "set -g mouse on\nbind r source-file ~/.tmux.conf\n";
        let (after, _) = upsert(user, Component::TmuxPopup);
        assert!(after.contains("set -g mouse on"));
        assert!(after.contains("bind r source-file ~/.tmux.conf"));
        assert!(after.contains("display-popup"));
    }

    #[test]
    fn unrelated_component_is_noop() {
        let (out, o) = upsert("set -g a 1\n", Component::ClaudeHooks);
        assert_eq!(o, Outcome::Unchanged);
        assert_eq!(out, "set -g a 1\n");
    }
}
