//! Process ancestry helpers — walk parent PID chain to recover information
//! the immediate caller's environment didn't carry.
//!
//! The motivating case: Claude Code's SDK (`claude --dangerously-skip-permissions`)
//! spawns sub-process Claude sessions whose environment **does not inherit
//! `TMUX_PANE`**. When their `Stop` / `UserPromptSubmit` hooks fire the
//! `muxa hook claude` shell-out doesn't see a pane id, so the agent is
//! recorded with `pane: None` — invisible to `muxa watch`'s attach action.
//!
//! Walking the ancestor chain almost always finds the SDK's parent, which is
//! the interactive `zsh` (or other shell) running inside a real tmux pane.
//! Matching that PID against `tmux list-panes`' `pane_pid` recovers the
//! correct attachment without changing how the SDK invokes itself.
//!
//! Linux reads one parent at a time via `/proc/<pid>/status`. macOS/BSD use
//! the crate's shared one-shot `ps` process snapshot from the hook adapter,
//! then feed its in-memory parent lookup into [`ancestor_in_set`].

use std::collections::HashSet;
use std::hash::BuildHasher;

/// Maximum number of parent links to follow when walking ancestry.
///
/// Real-world depth from a hook command up to the pane shell is 3–5
/// hops; 32 is well above any plausible working tree and bounds CPU /
/// I/O even if `/proc` returns garbage in a loop.
const MAX_DEPTH: usize = 32;

/// Read the parent PID of `pid` from `/proc/<pid>/status` on Linux.
/// Returns `None` for any failure (file missing, permission denied,
/// malformed content, non-Linux target).
#[cfg(target_os = "linux")]
pub fn parent_pid(pid: u32) -> Option<u32> {
    let content = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    parse_ppid_from_status(&content)
}

/// Portable fallback for hosts without Linux `/proc`. This path is only used
/// for the MCP ancestry recovery; hook reconciliation already takes a shared
/// one-shot process snapshot on macOS/BSD. A failed or unavailable `ps`
/// degrades cleanly to no ancestry match.
#[cfg(not(target_os = "linux"))]
pub fn parent_pid(pid: u32) -> Option<u32> {
    let output = std::process::Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()?.trim().parse().ok()
}

/// Parse the `PPid:` field out of `/proc/<pid>/status` content.
///
/// Pulled out for direct testing — `parent_pid` itself is hard to unit
/// test because it touches the real filesystem.
#[cfg(target_os = "linux")]
pub(crate) fn parse_ppid_from_status(content: &str) -> Option<u32> {
    for line in content.lines() {
        if let Some(rest) = line.strip_prefix("PPid:") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// Walk the parent chain starting at `start_pid` and return the first
/// ancestor PID present in `pids`. Returns `None` if nothing matches
/// within [`MAX_DEPTH`] hops or the chain terminates first.
///
/// `parent_of` is injected so tests can simulate arbitrary process trees
/// without touching `/proc`. Production code passes [`parent_pid`].
pub fn ancestor_in_set<F, S>(start_pid: u32, pids: &HashSet<u32, S>, parent_of: F) -> Option<u32>
where
    F: Fn(u32) -> Option<u32>,
    S: BuildHasher,
{
    let mut cur = start_pid;
    for _ in 0..MAX_DEPTH {
        let parent = parent_of(cur)?;
        // PID 1 is init; nothing to learn beyond that. Also guards
        // against pathological loops that report self-parent.
        if parent <= 1 || parent == cur {
            return None;
        }
        if pids.contains(&parent) {
            return Some(parent);
        }
        cur = parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_ppid_from_realistic_status_blob() {
        let blob = "\
Name:\tbash
Umask:\t0022
State:\tS (sleeping)
Tgid:\t12345
Ngid:\t0
Pid:\t12345
PPid:\t12340
TracerPid:\t0
";
        assert_eq!(parse_ppid_from_status(blob), Some(12340));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn missing_ppid_field_returns_none() {
        let blob = "Name:\tno-ppid-here\nPid:\t1\n";
        assert_eq!(parse_ppid_from_status(blob), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn malformed_ppid_value_returns_none() {
        let blob = "PPid:\tnot-a-number\n";
        assert_eq!(parse_ppid_from_status(blob), None);
    }

    /// Walk a synthetic 5-deep chain and confirm we stop at the first
    /// ancestor inside `pids`. Mirrors the SDK case: hook → sh → claude →
    /// zsh (pane shell, the match) → tmux server.
    #[test]
    fn ancestor_in_set_finds_first_match_in_chain() {
        let chain = [(100, 99), (99, 98), (98, 97), (97, 96), (96, 1)];
        let parent_of = |pid: u32| {
            chain
                .iter()
                .find(|(child, _)| *child == pid)
                .map(|(_, p)| *p)
        };
        let pids: HashSet<u32> = [97].into_iter().collect();
        assert_eq!(ancestor_in_set(100, &pids, parent_of), Some(97));
    }

    #[test]
    fn ancestor_in_set_returns_none_when_no_match() {
        let chain = [(50, 49), (49, 48), (48, 1)];
        let parent_of = |pid: u32| {
            chain
                .iter()
                .find(|(child, _)| *child == pid)
                .map(|(_, p)| *p)
        };
        let pids: HashSet<u32> = [999].into_iter().collect();
        assert_eq!(ancestor_in_set(50, &pids, parent_of), None);
    }

    #[test]
    fn ancestor_in_set_stops_at_init() {
        // Even if PID 1 is in the set, treat it as the chain terminator.
        let parent_of = |pid: u32| if pid == 5 { Some(1) } else { None };
        let pids: HashSet<u32> = [1].into_iter().collect();
        assert_eq!(ancestor_in_set(5, &pids, parent_of), None);
    }

    #[test]
    fn ancestor_in_set_breaks_self_parent_loops() {
        // A pathological /proc state where a pid reports itself as its
        // own parent must not spin forever or panic.
        let parent_of = |pid: u32| Some(pid);
        let pids: HashSet<u32> = [42].into_iter().collect();
        assert_eq!(ancestor_in_set(42, &pids, parent_of), None);
    }

    #[test]
    fn ancestor_in_set_caps_at_max_depth() {
        // A long unmatched chain should give up rather than hammer
        // `/proc` indefinitely. Each hop returns `prev - 1`.
        let parent_of = |pid: u32| if pid > 1 { Some(pid - 1) } else { None };
        let pids: HashSet<u32> = [0].into_iter().collect();
        assert_eq!(ancestor_in_set(10_000, &pids, parent_of), None);
    }
}
