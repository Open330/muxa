//! Best-effort process-tree enrichment for pane-backed agents.
//!
//! tmux tells us the pane's initial process (`pane_pid`) and foreground
//! command, but not the tree below the agent. Agent CLIs can spawn shells,
//! helper MCP servers, or nested agent sessions; this module walks the OS
//! process tree under a pane and reduces it to a small, argv-free summary
//! suitable for storing in the live registry.

use crate::discovery::classify_command;
use crate::event::AgentKind;
use crate::tmux::PaneInfo;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

const MAX_DEPTH: u8 = 10;
const MAX_NODES: usize = 256;
const MAX_PREVIEW: usize = 8;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadProcessKind {
    Shell,
    Subagent,
    Helper,
    Process,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadProcess {
    pub pid: u32,
    pub parent_pid: u32,
    pub depth: u8,
    pub kind: WorkloadProcessKind,
    pub command: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkloadSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_pid: Option<u32>,
    #[serde(default)]
    pub process_count: u16,
    #[serde(default)]
    pub shell_count: u16,
    #[serde(default)]
    pub subagent_count: u16,
    #[serde(default)]
    pub helper_count: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preview: Vec<WorkloadProcess>,
}

impl WorkloadSummary {
    pub fn is_empty(&self) -> bool {
        self.process_count == 0
            && self.shell_count == 0
            && self.subagent_count == 0
            && self.helper_count == 0
            && self.preview.is_empty()
    }

    pub fn visible_count(&self) -> u16 {
        self.process_count
    }
}

#[derive(Debug, Clone)]
struct ProcInfo {
    pid: u32,
    parent_pid: u32,
    depth: u8,
    comm: String,
    cmdline: String,
}

/// Scan every pane and return non-empty workload summaries keyed by pane id.
///
/// Empty summaries are omitted so callers can clear stale data by treating a
/// missing key as [`WorkloadSummary::default`].
pub fn scan_pane_workloads(panes: &[PaneInfo]) -> HashMap<String, WorkloadSummary> {
    let mut out = HashMap::new();
    for pane in panes {
        let summary = scan_pane_workload(pane);
        if !summary.is_empty() {
            out.insert(pane.pane_id.clone(), summary);
        }
    }
    out
}

pub fn scan_pane_workload(pane: &PaneInfo) -> WorkloadSummary {
    if pane.pane_pid == 0 {
        return WorkloadSummary::default();
    }
    summarize(read_descendants(pane.pane_pid))
}

fn summarize(procs: Vec<ProcInfo>) -> WorkloadSummary {
    let parent_by_pid: HashMap<u32, u32> = procs.iter().map(|p| (p.pid, p.parent_pid)).collect();
    let primary_pid = procs
        .iter()
        .filter(|p| agent_kind(p).is_some())
        .min_by_key(|p| (p.depth, p.pid))
        .map(|p| p.pid);

    let mut summary = WorkloadSummary {
        primary_pid,
        ..WorkloadSummary::default()
    };

    for proc in &procs {
        if Some(proc.pid) == primary_pid {
            continue;
        }
        if let Some(primary) = primary_pid {
            if !has_ancestor(proc.pid, primary, &parent_by_pid) {
                continue;
            }
        }

        let kind = classify_workload_process(proc);
        let command = display_command(proc, kind);
        if kind == WorkloadProcessKind::Helper {
            summary.helper_count = summary.helper_count.saturating_add(1);
            continue;
        }

        summary.process_count = summary.process_count.saturating_add(1);
        match kind {
            WorkloadProcessKind::Shell => {
                summary.shell_count = summary.shell_count.saturating_add(1);
            }
            WorkloadProcessKind::Subagent => {
                summary.subagent_count = summary.subagent_count.saturating_add(1);
            }
            WorkloadProcessKind::Helper | WorkloadProcessKind::Process => {}
        }
        if summary.preview.len() < MAX_PREVIEW {
            summary.preview.push(WorkloadProcess {
                pid: proc.pid,
                parent_pid: proc.parent_pid,
                depth: proc.depth,
                kind,
                command,
            });
        }
    }
    summary
}

fn has_ancestor(pid: u32, ancestor: u32, parent_by_pid: &HashMap<u32, u32>) -> bool {
    let mut cur = pid;
    for _ in 0..MAX_DEPTH {
        let Some(parent) = parent_by_pid.get(&cur).copied() else {
            return false;
        };
        if parent == ancestor {
            return true;
        }
        if parent <= 1 || parent == cur {
            return false;
        }
        cur = parent;
    }
    false
}

fn classify_workload_process(proc: &ProcInfo) -> WorkloadProcessKind {
    if is_helper(proc) {
        WorkloadProcessKind::Helper
    } else if agent_kind(proc).is_some() {
        WorkloadProcessKind::Subagent
    } else if is_shell(proc) {
        WorkloadProcessKind::Shell
    } else {
        WorkloadProcessKind::Process
    }
}

fn agent_kind(proc: &ProcInfo) -> Option<AgentKind> {
    classify_command(command_name(&proc.comm)).or_else(|| {
        if is_claude_fork_session(&proc.cmdline) {
            Some(AgentKind::ClaudeCode)
        } else {
            None
        }
    })
}

fn is_claude_fork_session(cmdline: &str) -> bool {
    cmdline.contains("--fork-session")
        && cmdline.contains("--session-id")
        && (cmdline.contains("/claude/versions/") || cmdline.contains(".claude"))
}

fn is_helper(proc: &ProcInfo) -> bool {
    let comm = command_name(&proc.comm).to_ascii_lowercase();
    let cmdline = proc.cmdline.to_ascii_lowercase();
    comm.ends_with("-mcp")
        || cmdline.contains("@playwright/mcp")
        || cmdline.contains("@modelcontextprotocol/")
        || cmdline.contains("playwright-mcp")
        || cmdline.contains("chrome-devtools-mcp")
        || cmdline.contains(" mcp")
        || cmdline.contains("-mcp")
        || comm.starts_with("pyright-langser")
        || cmdline.contains("pyright-langserver")
        || cmdline.contains("langserver.index.js")
        || cmdline.contains("language-server")
        || cmdline.contains("language_server")
        || cmdline.contains("rust-analyzer")
}

fn is_shell(proc: &ProcInfo) -> bool {
    matches!(
        command_name(&proc.comm).to_ascii_lowercase().as_str(),
        "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh" | "csh" | "tcsh" | "pwsh"
    )
}

fn display_command(proc: &ProcInfo, kind: WorkloadProcessKind) -> String {
    if kind == WorkloadProcessKind::Subagent {
        if let Some(agent) = agent_kind(proc) {
            return match agent {
                AgentKind::ClaudeCode => "claude".to_string(),
                AgentKind::Codex => "codex".to_string(),
                AgentKind::GeminiCli => "gemini".to_string(),
                AgentKind::Opencode => "opencode".to_string(),
                AgentKind::Pi => "pi".to_string(),
                AgentKind::Task | AgentKind::Unknown => command_name(&proc.comm).to_string(),
            };
        }
    }
    command_name(&proc.comm).to_string()
}

fn command_name(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s).trim()
}

#[cfg(target_os = "linux")]
fn read_descendants(root: u32) -> Vec<ProcInfo> {
    let mut out = Vec::new();
    let mut seen: HashSet<u32> = HashSet::new();
    let mut q: VecDeque<(u32, u8)> = read_children(root)
        .into_iter()
        .map(|pid| (pid, 1))
        .collect();
    while let Some((pid, depth)) = q.pop_front() {
        if depth > MAX_DEPTH || !seen.insert(pid) || out.len() >= MAX_NODES {
            continue;
        }
        if let Some(proc) = read_proc(pid, depth) {
            for child in read_children(pid) {
                q.push_back((child, depth.saturating_add(1)));
            }
            out.push(proc);
        }
    }
    out
}

#[cfg(target_os = "linux")]
fn read_children(pid: u32) -> Vec<u32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect()
}

#[cfg(target_os = "linux")]
fn read_proc(pid: u32, depth: u8) -> Option<ProcInfo> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let parent_pid = parse_ppid(&status)?;
    let comm = std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map_or_else(|| pid.to_string(), |s| s.trim().to_string());
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline"))
        .ok()
        .map(|bytes| {
            bytes
                .split(|b| *b == 0)
                .filter(|part| !part.is_empty())
                .map(|part| String::from_utf8_lossy(part))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    Some(ProcInfo {
        pid,
        parent_pid,
        depth,
        comm,
        cmdline,
    })
}

#[cfg(target_os = "linux")]
fn parse_ppid(status: &str) -> Option<u32> {
    status.lines().find_map(|line| {
        line.strip_prefix("PPid:")
            .and_then(|rest| rest.trim().parse().ok())
    })
}

#[cfg(not(target_os = "linux"))]
fn read_descendants(root: u32) -> Vec<ProcInfo> {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,comm=,args="])
        .output()
        .ok();
    let Some(output) = output.filter(|o| o.status.success()) else {
        return Vec::new();
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut children: HashMap<u32, Vec<ProcInfo>> = HashMap::new();
    for line in stdout.lines() {
        let mut parts = line.split_whitespace();
        let (Some(pid), Some(parent_pid), Some(comm)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let (Ok(pid), Ok(parent_pid)) = (pid.parse(), parent_pid.parse()) else {
            continue;
        };
        let cmdline = parts.collect::<Vec<_>>().join(" ");
        children.entry(parent_pid).or_default().push(ProcInfo {
            pid,
            parent_pid,
            depth: 0,
            comm: comm.to_string(),
            cmdline,
        });
    }

    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut q: VecDeque<(u32, u8)> = children
        .get(&root)
        .into_iter()
        .flatten()
        .map(|p| (p.pid, 1))
        .collect();
    while let Some((pid, depth)) = q.pop_front() {
        if depth > MAX_DEPTH || !seen.insert(pid) || out.len() >= MAX_NODES {
            continue;
        }
        if let Some(mut proc) = children.values().flatten().find(|p| p.pid == pid).cloned() {
            proc.depth = depth;
            if let Some(kids) = children.get(&pid) {
                for child in kids {
                    q.push_back((child.pid, depth.saturating_add(1)));
                }
            }
            out.push(proc);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, parent_pid: u32, depth: u8, comm: &str, cmdline: &str) -> ProcInfo {
        ProcInfo {
            pid,
            parent_pid,
            depth,
            comm: comm.into(),
            cmdline: cmdline.into(),
        }
    }

    #[test]
    fn summarize_counts_shell_and_child_process_under_primary_agent() {
        let summary = summarize(vec![
            proc(20, 10, 1, "claude", "claude"),
            proc(30, 20, 2, "zsh", "zsh -c run"),
            proc(31, 30, 3, "python3", "python3 -m tests.manual"),
        ]);
        assert_eq!(summary.primary_pid, Some(20));
        assert_eq!(summary.shell_count, 1);
        assert_eq!(summary.subagent_count, 0);
        assert_eq!(summary.process_count, 2);
        assert_eq!(
            summary
                .preview
                .iter()
                .map(|p| p.command.as_str())
                .collect::<Vec<_>>(),
            vec!["zsh", "python3"]
        );
    }

    #[test]
    fn summarize_excludes_mcp_helpers_from_visible_workload() {
        let summary = summarize(vec![
            proc(20, 10, 1, "claude", "claude"),
            proc(
                30,
                20,
                2,
                "sh",
                "sh -c \"playwright-mcp\" --browser chromium",
            ),
        ]);
        assert_eq!(summary.primary_pid, Some(20));
        assert_eq!(summary.helper_count, 1);
        assert_eq!(summary.shell_count, 0);
        assert_eq!(summary.process_count, 0);
        assert!(summary.preview.is_empty());
    }

    #[test]
    fn summarize_counts_nested_agent_as_subagent() {
        let summary = summarize(vec![
            proc(20, 10, 1, "claude", "claude"),
            proc(
                40,
                20,
                2,
                "2.1.177",
                "/home/u/.local/share/claude/versions/2.1.177 --session-id s --fork-session",
            ),
        ]);
        assert_eq!(summary.primary_pid, Some(20));
        assert_eq!(summary.subagent_count, 1);
        assert_eq!(summary.process_count, 1);
        assert_eq!(summary.preview[0].command, "claude");
    }
}
