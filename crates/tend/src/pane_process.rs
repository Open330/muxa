//! What runs in a pane, from its process tree: whether the provider agent is
//! there, whether a shell sits above it, and the processes that may hold a
//! conversation from outside it.
//!
//! **Internal:** `#[doc(hidden)]`, shared by worker stop/drain, collaboration
//! and the optional account module.

use crate::event::AgentKind;
use crate::process_snapshot::ProcessInfo;

/// Whether `process` is the interactive `kind` agent (not Codex's
/// app-server or a headless `exec` run).
pub fn is_agent_process(process: &ProcessInfo, kind: AgentKind) -> bool {
    let tokens: Vec<&str> = process.cmdline.split_whitespace().collect();
    // `node …/bin/codex` — the npm shim — names the agent in its second word.
    let Some(program) = tokens
        .iter()
        .take(2)
        .position(|token| crate::discovery::classify_command(token) == Some(kind))
    else {
        return false;
    };
    kind != AgentKind::Codex
        // Only the first positional word after `codex` names a subcommand;
        // later words belong to the prompt, and a worker prompt such as
        // "You review …" must not read as `codex review`.
        || !crate::adapters::proc_ancestry::codex_subcommand(&tokens[program..].join(" "))
            .is_some_and(|subcommand| {
                matches!(
                    subcommand,
                    "app-server" | "exec" | "review" | "mcp-server" | "exec-server"
                )
            })
}

/// The `kind` agent under the pane process `pane_pid`: the shallowest one,
/// provided every process between it and the pane is in `processes`. A pane
/// started as `exec claude …` has the agent as its own root.
#[must_use]
pub fn find_agent(
    processes: &[ProcessInfo],
    pane_pid: u32,
    kind: AgentKind,
) -> Option<&ProcessInfo> {
    let agent = processes
        .iter()
        .filter(|p| is_agent_process(p, kind))
        .min_by_key(|p| p.depth)?;
    let mut parent = agent.parent_pid;
    if agent.pid != pane_pid {
        while parent != pane_pid {
            parent = processes.iter().find(|p| p.pid == parent)?.parent_pid;
        }
    }
    Some(agent)
}

pub fn signal_pids(signal: &str, pids: &[u32]) {
    // Windows has no signals: `taskkill` asks a process to close, and `/F`
    // is the `KILL` that cannot be refused.
    #[cfg(windows)]
    let mut command = {
        let mut command = std::process::Command::new("taskkill");
        if signal == "KILL" {
            command.arg("/F");
        }
        for pid in pids {
            command.args(["/PID", &pid.to_string()]);
        }
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = std::process::Command::new("kill");
        command.arg(format!("-{signal}"));
        command.args(pids.iter().map(u32::to_string));
        command
    };
    let _ = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// Signal 0 / `/proc`, not a `ps` spawn: the stop loop polls this every
/// 100 ms, and an `EPERM` answer is a live process, not an exit.
pub fn is_alive(pid: u32) -> bool {
    crate::pid_liveness::pid_alive(pid)
}

/// The command line of `pid`, or `None` once it has exited.
#[cfg(windows)]
pub fn process_command(pid: u32) -> Option<String> {
    crate::process_snapshot::process_argv(pid).map(|argv| argv.join(" "))
}

/// The command line of `pid`, or `None` once it has exited.
#[cfg(not(windows))]
pub fn process_command(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-o", "args=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    let line = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (output.status.success() && !line.is_empty()).then_some(line)
}

/// Is `command` an interactive shell, one with a prompt a relaunch can be
/// typed at? `sh script` and `sh -c '…'` run a command and have none; any
/// other option (`--norc`, `-o vi`, `-NoLogo`) leaves a shell interactive.
pub fn is_shell(command: &str) -> bool {
    let mut words = command.split_whitespace();
    if !crate::shell::is_shell_program(words.next().unwrap_or_default()) {
        return false;
    }
    !words.any(|word| {
        let short_c = word.starts_with('-')
            && !word.starts_with("--")
            && word[1..].chars().all(|c| c.is_ascii_alphabetic())
            && word.contains('c');
        // A script: a path, not an option value like `vi` in `-o vi`.
        let script = !word.starts_with('-')
            && (word.contains('/')
                || std::path::Path::new(word)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("sh")));
        short_c || script
    })
}

/// Fresh provider observation including the pane root for direct-exec panes.
pub fn pane_has_agent_process(pane_pid: u32) -> Option<bool> {
    let processes = pane_processes(pane_pid);
    if processes.is_empty() {
        #[cfg(target_os = "linux")]
        {
            return std::path::Path::new(&format!("/proc/{pane_pid}"))
                .try_exists()
                .ok()
                .filter(|exists| !exists)
                .map(|_| false);
        }
        #[cfg(not(target_os = "linux"))]
        {
            return None;
        }
    }
    Some(processes.iter().any(|p| {
        [
            AgentKind::ClaudeCode,
            AgentKind::Codex,
            AgentKind::Opencode,
            AgentKind::Antigravity,
        ]
        .iter()
        .any(|kind| is_agent_process(p, *kind))
    }))
}

/// Does this pane preserve an interactive shell above its provider process?
pub fn pane_has_shell(pane_pid: u32, agent_pid: u32) -> Option<bool> {
    shell_above(&pane_processes(pane_pid), agent_pid, pane_pid)
}

pub fn shell_above(processes: &[ProcessInfo], agent_pid: u32, pane_pid: u32) -> Option<bool> {
    let by_pid = |pid: u32| processes.iter().find(|p| p.pid == pid);
    let root = by_pid(pane_pid)?;
    if agent_pid == pane_pid {
        return Some(false);
    }
    let mut pid = by_pid(agent_pid)?.parent_pid;
    while pid != pane_pid {
        let process = by_pid(pid)?;
        if is_shell(&process.cmdline) {
            return Some(true);
        }
        pid = process.parent_pid;
    }
    Some(is_shell(&root.cmdline))
}

/// Every process in a pane: its root, then the root's descendants. The root
/// is the agent itself in a pane started as `exec claude …`.
pub fn pane_processes(pane_pid: u32) -> Vec<ProcessInfo> {
    let mut processes = crate::process_tree::read_descendants(pane_pid);
    if let Some(cmdline) = process_command(pane_pid) {
        processes.insert(
            0,
            ProcessInfo {
                pid: pane_pid,
                parent_pid: 0,
                depth: 0,
                comm: cmdline
                    .split_whitespace()
                    .next()
                    .unwrap_or_default()
                    .to_owned(),
                cmdline,
            },
        );
    }
    processes
}

/// Codex app-servers outside this pane, which can keep writer ownership of a
/// conversation after the pane's own agent exits. Named, never stopped: one
/// shared server serves unrelated agents.
pub fn codex_writer_owners(pane_pid: u32) -> String {
    let processes = all_processes();
    let in_pane: std::collections::HashSet<u32> = crate::process_tree::read_descendants(pane_pid)
        .iter()
        .map(|p| p.pid)
        .collect();
    let owners = outside_codex_servers(processes.iter(), pane_pid, &in_pane);
    if owners.is_empty() {
        String::new()
    } else {
        format!(
            " — Codex app-server(s) outside the pane that may hold it: {}; \
             close the other client or restart that server when it is safe",
            owners.join(", ")
        )
    }
}

/// Every process's pid and command line. Linux reads `/proc` (there is no
/// whole-table snapshot there); elsewhere the shared process table.
#[cfg(target_os = "linux")]
pub fn all_processes() -> Vec<ProcessInfo> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let raw = std::fs::read(entry.path().join("cmdline")).ok()?;
            let cmdline = String::from_utf8_lossy(&raw)
                .split('\0')
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            Some(ProcessInfo {
                pid,
                parent_pid: 0,
                depth: 0,
                comm: String::new(),
                cmdline,
            })
        })
        .collect()
}

#[cfg(not(target_os = "linux"))]
pub fn all_processes() -> Vec<ProcessInfo> {
    crate::process_snapshot::read_current_process_table()
        .processes()
        .cloned()
        .collect()
}

#[allow(clippy::implicit_hasher)] // one caller, one set
pub fn outside_codex_servers<'a>(
    processes: impl Iterator<Item = &'a ProcessInfo>,
    pane_pid: u32,
    in_pane: &std::collections::HashSet<u32>,
) -> Vec<String> {
    let mut owners: Vec<(u32, String)> = processes
        .filter(|p| p.pid != pane_pid && !in_pane.contains(&p.pid))
        .filter(|p| {
            let mut words = p.cmdline.split_whitespace();
            words.any(|w| w.rsplit('/').next() == Some("codex"))
                && p.cmdline.split_whitespace().any(|w| w == "app-server")
        })
        .map(|p| (p.pid, format!("pid {}", p.pid)))
        .collect();
    owners.sort();
    owners.into_iter().map(|(_, label)| label).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, parent_pid: u32, depth: u8, cmdline: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid,
            depth,
            comm: cmdline.split_whitespace().next().unwrap_or_default().into(),
            cmdline: cmdline.into(),
        }
    }

    #[test]
    fn the_agent_is_found_only_through_an_unbroken_chain_to_the_pane() {
        let shell = [
            proc(11, 10, 1, "aas e work@claude claude"),
            proc(12, 11, 2, "claude --resume x"),
        ];
        assert_eq!(
            find_agent(&shell, 10, AgentKind::ClaudeCode).map(|p| p.pid),
            Some(12)
        );
        // A missing link between agent and pane proves nothing.
        assert!(find_agent(&shell[1..], 10, AgentKind::ClaudeCode).is_none());
        // `exec claude …`: the agent is the pane's own root.
        let exec = [proc(10, 1, 0, "claude")];
        assert_eq!(
            find_agent(&exec, 10, AgentKind::ClaudeCode).map(|p| p.pid),
            Some(10)
        );
        // Codex's app-server is not the interactive agent.
        let server = [proc(11, 10, 1, "codex app-server")];
        assert!(find_agent(&server, 10, AgentKind::Codex).is_none());
    }
}
