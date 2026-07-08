//! Reusable comment-fenced "managed block" editor.
//!
//! Used by any config file that takes a line-comment syntax —
//! shell-style `#` for `~/.tmux.conf`, C-style `//` for the opencode
//! `muxa.ts` plugin, and potentially others later. The format is:
//!
//! ```text
//! # >>> muxa managed (<id>) >>>
//! <body lines>
//! # <<< muxa managed (<id>) <<<
//! ```
//!
//! (The `#` lead is swapped for `//` when [`Style::Slash`] is used so
//! the fence lines are valid comments in JS/TS.)
//!
//! Each component owns its own block keyed by `id`, which means
//! `--uninstall` can surgically remove just the block we own without
//! touching anything the user added by hand.

use std::fmt::Write as _;

const OPEN_SUFFIX: &str = ") >>>";
const CLOSE_SUFFIX: &str = ") <<<";

/// Line-comment syntax used for the fence lines. Each managed file
/// picks the one that keeps its content syntactically valid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// `#`-prefixed fences — shell/tmux/toml-style configs.
    Hash,
    /// `//`-prefixed fences — JS/TS (e.g. the opencode plugin).
    Slash,
}

impl Style {
    /// The comment lead that starts each fence line.
    fn lead(self) -> &'static str {
        match self {
            Style::Hash => "#",
            Style::Slash => "//",
        }
    }

    fn open_prefix(self) -> String {
        format!("{} >>> muxa managed (", self.lead())
    }

    fn close_prefix(self) -> String {
        format!("{} <<< muxa managed (", self.lead())
    }
}

/// Result of a marker-block edit on a file's content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// File didn't have the block; appended one.
    Inserted,
    /// File had the block with different content; rewrote in place.
    Replaced,
    /// File had the block already with identical content; no-op.
    Unchanged,
    /// Asked to remove a block that wasn't there.
    AlreadyAbsent,
    /// Removed an existing block.
    Removed,
}

impl Outcome {
    /// Did this operation actually mutate the file content?
    pub fn changed(self) -> bool {
        matches!(
            self,
            Outcome::Inserted | Outcome::Replaced | Outcome::Removed
        )
    }
}

/// Insert or replace the block keyed by `id` with `body` in `original`,
/// using shell-style `#` fences. Convenience wrapper over
/// [`upsert_styled`] for the common case.
pub fn upsert(original: &str, id: &str, body: &str) -> (String, Outcome) {
    upsert_styled(original, id, body, Style::Hash)
}

/// Remove the block keyed by `id` from `original`, if present, using
/// shell-style `#` fences. Convenience wrapper over [`remove_styled`].
pub fn remove(original: &str, id: &str) -> (String, Outcome) {
    remove_styled(original, id, Style::Hash)
}

/// Insert or replace the block keyed by `id` with `body` in `original`.
/// Returns the new file content + an `Outcome` describing what happened.
///
/// `body` should be the inner lines without the fence comments. A
/// trailing newline on `body` is normalized away — the renderer adds
/// exactly one between body and close fence.
pub fn upsert_styled(original: &str, id: &str, body: &str, style: Style) -> (String, Outcome) {
    let body = body.trim_end_matches('\n');
    let rendered = render(id, body, style);
    match find_block(original, id, style) {
        Some((start, end)) => {
            let existing = &original[start..end];
            if existing == rendered {
                (original.to_string(), Outcome::Unchanged)
            } else {
                let mut out = String::with_capacity(original.len() + rendered.len());
                out.push_str(&original[..start]);
                out.push_str(&rendered);
                out.push_str(&original[end..]);
                (out, Outcome::Replaced)
            }
        }
        None => (append_block(original, &rendered), Outcome::Inserted),
    }
}

/// Remove the block keyed by `id` from `original`, if present, matching
/// the fence `style` that was used to write it.
pub fn remove_styled(original: &str, id: &str, style: Style) -> (String, Outcome) {
    let Some((start, end)) = find_block(original, id, style) else {
        return (original.to_string(), Outcome::AlreadyAbsent);
    };
    // Also eat one leading newline if there is one — keeps the file from
    // accumulating blank lines after repeated install/uninstall cycles.
    let cut_start = if start > 0 && original.as_bytes()[start - 1] == b'\n' {
        start - 1
    } else {
        start
    };
    let mut out = String::with_capacity(original.len());
    out.push_str(&original[..cut_start]);
    out.push_str(&original[end..]);
    (out, Outcome::Removed)
}

fn render(id: &str, body: &str, style: Style) -> String {
    let mut s = String::with_capacity(body.len() + 80);
    let _ = writeln!(s, "{}{id}{OPEN_SUFFIX}", style.open_prefix());
    s.push_str(body);
    s.push('\n');
    let _ = writeln!(s, "{}{id}{CLOSE_SUFFIX}", style.close_prefix());
    s
}

fn append_block(original: &str, rendered: &str) -> String {
    let mut out = String::with_capacity(original.len() + rendered.len() + 2);
    out.push_str(original);
    if !original.is_empty() && !original.ends_with('\n') {
        out.push('\n');
    }
    if !original.is_empty() && !original.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(rendered);
    out
}

/// Locate the byte range `[start, end)` of the block for `id`,
/// inclusive of the fence lines and trailing newline. Returns `None`
/// when the block is absent or malformed (open without matching close).
fn find_block(haystack: &str, id: &str, style: Style) -> Option<(usize, usize)> {
    let open = format!("{}{id}{OPEN_SUFFIX}", style.open_prefix());
    let close = format!("{}{id}{CLOSE_SUFFIX}", style.close_prefix());
    let open_idx = line_start_of(haystack, &open)?;
    // Search for the matching close *after* the open fence, scoped to
    // the same id — this gracefully tolerates other components' blocks
    // appearing between them in arbitrary order. The close must also be
    // at a line start; otherwise a literal close-fence string inside a
    // quoted body line would be misinterpreted as the real terminator.
    let after_open = open_idx + open.len();
    let close_line_start = line_start_of(&haystack[after_open..], &close)? + after_open;
    // Include the trailing newline after the close fence so removal
    // doesn't leave a dangling blank line.
    let line_end = haystack[close_line_start..]
        .find('\n')
        .map_or(haystack.len(), |off| close_line_start + off + 1);
    Some((open_idx, line_end))
}

/// Find `needle` in `haystack` only at line starts (begin-of-string or
/// just after a `\n`). Prevents matching a literal that happens to
/// appear inside a quoted string somewhere mid-line.
fn line_start_of(haystack: &str, needle: &str) -> Option<usize> {
    let mut search_from = 0;
    while let Some(rel) = haystack[search_from..].find(needle) {
        let abs = search_from + rel;
        if abs == 0 || haystack.as_bytes()[abs - 1] == b'\n' {
            return Some(abs);
        }
        search_from = abs + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_into_empty() {
        let (out, o) = upsert("", "x", "set -g foo bar");
        assert_eq!(o, Outcome::Inserted);
        assert!(out.contains("# >>> muxa managed (x) >>>"));
        assert!(out.contains("set -g foo bar"));
        assert!(out.contains("# <<< muxa managed (x) <<<"));
    }

    #[test]
    fn insert_appends_with_blank_separator() {
        let (out, _) = upsert("set -g status on\n", "x", "y");
        assert!(out.contains("set -g status on\n\n# >>> muxa managed (x) >>>"));
    }

    #[test]
    fn idempotent_same_content() {
        let (first, _) = upsert("", "x", "body");
        let (second, o) = upsert(&first, "x", "body");
        assert_eq!(o, Outcome::Unchanged);
        assert_eq!(first, second);
    }

    #[test]
    fn replace_in_place_keeps_surrounding_lines() {
        let (with, _) = upsert("before\n", "x", "old");
        let after = format!("{with}after\n");
        let (replaced, o) = upsert(&after, "x", "new");
        assert_eq!(o, Outcome::Replaced);
        assert!(replaced.starts_with("before\n"));
        assert!(replaced.ends_with("after\n"));
        assert!(replaced.contains("new"));
        assert!(!replaced.contains("old"));
    }

    #[test]
    fn remove_strips_block_and_preserves_neighbors() {
        let original = "set -g a 1\n";
        let (with, _) = upsert(original, "x", "managed line");
        let suffix = format!("{with}set -g b 2\n");
        let (removed, o) = remove(&suffix, "x");
        assert_eq!(o, Outcome::Removed);
        assert!(removed.contains("set -g a 1"));
        assert!(removed.contains("set -g b 2"));
        assert!(!removed.contains("muxa managed"));
    }

    #[test]
    fn remove_when_absent_is_noop() {
        let (out, o) = remove("just config\n", "x");
        assert_eq!(o, Outcome::AlreadyAbsent);
        assert_eq!(out, "just config\n");
    }

    #[test]
    fn multiple_components_coexist_in_any_order() {
        let s = String::new();
        let (s, _) = upsert(&s, "a", "AAA");
        let (s, _) = upsert(&s, "b", "BBB");
        // Removing the inner block must not corrupt the outer one.
        let (s, o) = remove(&s, "a");
        assert_eq!(o, Outcome::Removed);
        assert!(s.contains("BBB"));
        assert!(!s.contains("AAA"));
        assert!(find_block(&s, "b", Style::Hash).is_some());
        assert!(find_block(&s, "a", Style::Hash).is_none());
    }

    #[test]
    fn does_not_match_substring_inside_a_quoted_line() {
        // Defensive: a status-right that quotes the literal must not
        // be confused for a fence line.
        let s = "set -g status-right \"# >>> muxa managed (x) >>> not a fence\"\n";
        assert!(find_block(s, "x", Style::Hash).is_none());
    }

    #[test]
    fn slash_style_emits_valid_comment_fences() {
        // JS/TS files can't have `#`-prefixed lines. The Slash style
        // must fence with `//` so the result parses as a comment.
        let (out, o) = upsert_styled("", "opencode-plugin", "export const X = 1;", Style::Slash);
        assert_eq!(o, Outcome::Inserted);
        assert!(out.contains("// >>> muxa managed (opencode-plugin) >>>"));
        assert!(out.contains("// <<< muxa managed (opencode-plugin) <<<"));
        assert!(out.contains("export const X = 1;"));
        // No line may start with a bare `#` — that would be a TS syntax error.
        assert!(
            !out.lines().any(|l| l.starts_with('#')),
            "slash-styled output must not contain shell-comment fences"
        );
    }

    #[test]
    fn slash_style_round_trips_through_remove() {
        let (installed, _) = upsert_styled("", "opencode-plugin", "body", Style::Slash);
        let (removed, o) = remove_styled(&installed, "opencode-plugin", Style::Slash);
        assert_eq!(o, Outcome::Removed);
        assert!(removed.is_empty());
    }

    #[test]
    fn styles_do_not_match_each_others_fences() {
        // A `//`-fenced block must be invisible to a Hash-style remove
        // (and vice-versa) so we never half-strip a block.
        let (slash, _) = upsert_styled("", "x", "body", Style::Slash);
        let (out, o) = remove_styled(&slash, "x", Style::Hash);
        assert_eq!(o, Outcome::AlreadyAbsent);
        assert_eq!(out, slash);
    }
}
