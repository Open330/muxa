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

/// Whether `start_pid` runs beneath Codex's **shared** `app-server`.
///
/// Codex 0.160+ attaches every TUI to one background `codex app-server
/// --managed-daemon` (`features.daemon_auto_start`). That server runs the
/// hooks, MCP servers, and shell commands of *every* attached thread, and all
/// of them inherit the environment of whichever pane happened to start the
/// server first. Its `$TMUX_PANE`/`$RMUX_PANE` therefore name an unrelated
/// pane, and trusting them makes one Codex speak as another. Callers use this
/// to distrust the inherited pane and identify the thread by its session id.
///
/// An `app-server` whose parent is an ordinary `codex` invocation is private
/// to that TUI (it lives in the TUI's pane), so its environment stays valid.
pub fn under_shared_codex_app_server(start_pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        shared_codex_app_server_in_ancestry(start_pid, parent_pid, |pid| {
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .ok()
                .map(|raw| String::from_utf8_lossy(&raw).into_owned())
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let table = crate::process_snapshot::read_current_process_table();
        shared_codex_app_server_in_ancestry(
            start_pid,
            |pid| table.get(pid).map(|p| p.parent_pid),
            |pid| table.get(pid).map(|p| p.cmdline.clone()),
        )
    }
}

fn shared_codex_app_server_in_ancestry<P, A>(start_pid: u32, parent_of: P, argv_of: A) -> bool
where
    P: Fn(u32) -> Option<u32>,
    A: Fn(u32) -> Option<String>,
{
    let mut cur = start_pid;
    for _ in 0..MAX_DEPTH {
        let Some(parent) = parent_of(cur) else {
            return false;
        };
        if parent <= 1 || parent == cur {
            return false;
        }
        if argv_of(parent).is_some_and(|argv| codex_subcommand(&argv) == Some("app-server")) {
            // Owned by a TUI (`codex`, `codex resume …`) → private server.
            let owner = parent_of(parent).and_then(&argv_of);
            return !owner.is_some_and(|argv| {
                codex_subcommand(&argv).is_some_and(|sub| sub != "app-server")
            });
        }
        cur = parent;
    }
    false
}

/// For a `codex …` command line (NUL- or space-separated), the first argument
/// after the program, or `""` when there is none. `None` when the program is
/// not `codex`.
fn codex_subcommand(command_line: &str) -> Option<&str> {
    let mut args = command_line
        .split(|c: char| c == '\0' || c.is_whitespace())
        .filter(|arg| !arg.is_empty());
    let program = args.next()?;
    (program.rsplit('/').next() == Some("codex")).then(|| args.next().unwrap_or(""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

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

    fn tree(entries: &[(u32, u32, &str)]) -> (HashMap<u32, u32>, HashMap<u32, String>) {
        (
            entries.iter().map(|(pid, ppid, _)| (*pid, *ppid)).collect(),
            entries
                .iter()
                .map(|(pid, _, argv)| (*pid, (*argv).to_string()))
                .collect(),
        )
    }

    fn shared(entries: &[(u32, u32, &str)], start: u32) -> bool {
        let (parents, argv) = tree(entries);
        shared_codex_app_server_in_ancestry(
            start,
            |pid| parents.get(&pid).copied(),
            |pid| argv.get(&pid).cloned(),
        )
    }

    /// The live layout that made every Codex speak as `%9`: the managed
    /// daemon is a child of the daemon's update loop, not of any TUI.
    #[test]
    fn managed_app_server_children_are_shared() {
        let entries = [
            (
                100,
                1,
                "/opt/codex/bin/codex\0app-server\0daemon\0pid-update-loop",
            ),
            (
                200,
                100,
                "/opt/codex/bin/codex\0app-server\0--listen\0unix://\0--managed-daemon",
            ),
            (300, 200, "muxa\0mcp"),
            (310, 200, "/bin/sh\0-c\0muxa hook codex --event Stop"),
            (311, 310, "muxa\0hook\0codex"),
        ];
        assert!(shared(&entries, 300));
        assert!(shared(&entries, 311));
    }

    #[test]
    fn app_server_reparented_to_init_is_shared() {
        let entries = [
            (200, 1, "codex app-server --listen unix://"),
            (300, 200, "muxa mcp"),
        ];
        assert!(shared(&entries, 300));
    }

    #[test]
    fn tui_owned_or_pane_processes_are_not_shared() {
        let entries = [
            (10, 1, "-zsh"),
            (20, 10, "node /usr/bin/codex --yolo"),
            (
                21,
                20,
                "/vendor/bin/codex --yolo resume 01a0f037-0aec-77b0-8782-647a69e14f08",
            ),
            (22, 21, "codex app-server"),
            (30, 22, "muxa mcp"),
            (40, 21, "muxa mcp"),
            (50, 10, "claude"),
            (51, 50, "muxa mcp"),
        ];
        assert!(
            !shared(&entries, 30),
            "a TUI's private server keeps its pane env"
        );
        assert!(!shared(&entries, 40));
        assert!(!shared(&entries, 51));
    }

    #[test]
    fn codex_subcommand_requires_the_codex_program() {
        assert_eq!(
            codex_subcommand("/x/bin/codex\0app-server\0--listen"),
            Some("app-server")
        );
        assert_eq!(codex_subcommand("codex"), Some(""));
        assert_eq!(codex_subcommand("muxa app-server"), None);
        assert_eq!(codex_subcommand("/x/codex-code-mode-host app-server"), None);
        assert_eq!(codex_subcommand(""), None);
    }
}
