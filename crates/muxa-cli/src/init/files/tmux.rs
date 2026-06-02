//! `~/.tmux.conf` content layer.
//!
//! Pure functions: take the existing file content + which components
//! are selected, return the new content. All I/O (read, backup, write,
//! `tmux source-file`) is handled by `apply.rs`.

use crate::init::components::Component;
use crate::init::marker::{self, Outcome};
use std::path::Path;

/// Marker-block id reserved for the (component-less) `MUXA_SOCKET` pin.
/// Not exposed in the user-facing component catalog — it's auto-managed
/// alongside any other tmux block so users can't get the popup or
/// statusline without socket propagation.
pub const ENV_BLOCK_ID: &str = "tmux-env";

/// The body that goes inside the `tmux-popup` marker block.
pub const POPUP_BODY: &str = r#"# Replace tmux's stock prefix+s (choose-tree) with muxa watch in a popup.
# Enter opens a prompt composer; empty Enter again attaches.
bind-key s display-popup -E -w 90% -h 85% "muxa watch""#;

/// The body that goes inside the `tmux-statusline` marker block.
pub const STATUSLINE_BODY: &str = r##"# Per-pane agent glyph (⚙ working / · idle / ! waiting / ✗ error)
set -g status-interval 2
set -g status-right-length 140
set -g status-right "#(muxa status-line --pane #{pane_id}) | #[fg=white]%H:%M""##;

/// Render the body of the `tmux-env` marker block. We pin the socket
/// path inside `~/.tmux.conf` so it survives `tmux kill-server` and
/// reboots — the runtime-only `tmux set-environment` we issue at init
/// time is otherwise lost the moment the tmux server dies, which leaves
/// every freshly-spawned pane unable to find muxad.
pub fn env_body(socket: &std::path::Path) -> String {
    // Quote the path so a shell-special character in $XDG_RUNTIME_DIR
    // (rare but possible) doesn't break tmux's parser. Embedded `"` in
    // a path is exotic enough that we just escape it conservatively.
    let escaped = socket.display().to_string().replace('"', r#"\""#);
    format!(
        "# Pin the muxad IPC socket path so panes started in any tmux\n\
         # session can reach the daemon regardless of when their parent\n\
         # shell was launched. If you change MUXA_SOCKET in muxad's\n\
         # config, re-run `muxa init` to update this line.\n\
         set-environment -g MUXA_SOCKET \"{escaped}\""
    )
}

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

/// Upsert the auto-managed `tmux-env` block (pins `MUXA_SOCKET`).
pub fn upsert_env(original: &str, socket: &Path) -> (String, Outcome) {
    marker::upsert(original, ENV_BLOCK_ID, &env_body(socket))
}

/// Remove the auto-managed `tmux-env` block.
pub fn remove_env(original: &str) -> (String, Outcome) {
    marker::remove(original, ENV_BLOCK_ID)
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

    #[test]
    fn env_block_round_trip() {
        use std::path::PathBuf;
        let p = PathBuf::from("/tmp/muxa-501.sock");
        let (after, o1) = upsert_env("", &p);
        assert_eq!(o1, Outcome::Inserted);
        assert!(after.contains("# >>> muxa managed (tmux-env) >>>"));
        assert!(after.contains(r#"set-environment -g MUXA_SOCKET "/tmp/muxa-501.sock""#));

        // Idempotent on identical body.
        let (after2, o2) = upsert_env(&after, &p);
        assert_eq!(o2, Outcome::Unchanged);
        assert_eq!(after, after2);

        // Path change → Replaced (covers a daemon socket migration).
        let p2 = PathBuf::from("/run/user/501/muxa.sock");
        let (after3, o3) = upsert_env(&after, &p2);
        assert_eq!(o3, Outcome::Replaced);
        assert!(after3.contains("/run/user/501/muxa.sock"));
        assert!(!after3.contains("/tmp/muxa-501.sock"));

        let (after4, o4) = remove_env(&after3);
        assert_eq!(o4, Outcome::Removed);
        assert!(!after4.contains("tmux-env"));
    }

    #[test]
    fn env_block_coexists_with_popup_and_statusline() {
        use std::path::PathBuf;
        let p = PathBuf::from("/tmp/muxa-501.sock");
        let (s, _) = upsert("", Component::TmuxPopup);
        let (s, _) = upsert(&s, Component::TmuxStatusLine);
        let (s, _) = upsert_env(&s, &p);
        assert!(s.contains("display-popup"));
        assert!(s.contains("status-right"));
        assert!(s.contains("MUXA_SOCKET"));

        // Removing one block leaves the others alone.
        let (s, _) = remove(&s, Component::TmuxPopup);
        assert!(!s.contains("display-popup"));
        assert!(s.contains("status-right"));
        assert!(s.contains("MUXA_SOCKET"));
    }

    #[test]
    fn env_body_quotes_path() {
        use std::path::PathBuf;
        let p = PathBuf::from(r#"/tmp/with"quote.sock"#);
        let body = env_body(&p);
        assert!(body.contains(r#"set-environment -g MUXA_SOCKET "/tmp/with\"quote.sock""#));
    }
}
