//! Reusable comment-fenced "managed block" editor.
//!
//! Used by any config file that takes shell-style `#`-prefix comments —
//! `~/.tmux.conf` today, potentially others later. The format is:
//!
//! ```text
//! # >>> muxa managed (<id>) >>>
//! <body lines>
//! # <<< muxa managed (<id>) <<<
//! ```
//!
//! Each component owns its own block keyed by `id`, which means
//! `--uninstall` can surgically remove just the block we own without
//! touching anything the user added by hand.

use std::fmt::Write as _;

const OPEN_PREFIX: &str = "# >>> muxa managed (";
const OPEN_SUFFIX: &str = ") >>>";
const CLOSE_PREFIX: &str = "# <<< muxa managed (";
const CLOSE_SUFFIX: &str = ") <<<";

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

/// Insert or replace the block keyed by `id` with `body` in `original`.
/// Returns the new file content + an `Outcome` describing what happened.
///
/// `body` should be the inner lines without the fence comments. A
/// trailing newline on `body` is normalized away — the renderer adds
/// exactly one between body and close fence.
pub fn upsert(original: &str, id: &str, body: &str) -> (String, Outcome) {
    let body = body.trim_end_matches('\n');
    let rendered = render(id, body);
    match find_block(original, id) {
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

/// Remove the block keyed by `id` from `original`, if present.
pub fn remove(original: &str, id: &str) -> (String, Outcome) {
    let Some((start, end)) = find_block(original, id) else {
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

fn render(id: &str, body: &str) -> String {
    let mut s = String::with_capacity(body.len() + 80);
    let _ = writeln!(s, "{OPEN_PREFIX}{id}{OPEN_SUFFIX}");
    s.push_str(body);
    s.push('\n');
    let _ = writeln!(s, "{CLOSE_PREFIX}{id}{CLOSE_SUFFIX}");
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
fn find_block(haystack: &str, id: &str) -> Option<(usize, usize)> {
    let open = format!("{OPEN_PREFIX}{id}{OPEN_SUFFIX}");
    let close = format!("{CLOSE_PREFIX}{id}{CLOSE_SUFFIX}");
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
        assert!(find_block(&s, "b").is_some());
        assert!(find_block(&s, "a").is_none());
    }

    #[test]
    fn does_not_match_substring_inside_a_quoted_line() {
        // Defensive: a status-right that quotes the literal must not
        // be confused for a fence line.
        let s = "set -g status-right \"# >>> muxa managed (x) >>> not a fence\"\n";
        assert!(find_block(s, "x").is_none());
    }
}
