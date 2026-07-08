//! Lightweight process-table snapshot shared by OS process-tree scanners.
//!
//! On non-Linux hosts the cheapest portable primitive is a single `ps` pass
//! over the whole process table. Callers then walk this in memory instead of
//! spawning `ps`/`pgrep` once per pane.

use std::collections::{HashMap, HashSet, VecDeque};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProcessInfo {
    pub pid: u32,
    pub parent_pid: u32,
    pub depth: u8,
    pub comm: String,
    pub cmdline: String,
}

// The whole-table snapshot is only walked on non-Linux hosts (Linux reads
// `/proc` directly), so on Linux every method here is legitimately unused.
// Suppress dead_code there rather than duplicate the type behind cfgs.
#[cfg_attr(target_os = "linux", allow(dead_code))]
#[derive(Debug, Clone, Default)]
pub(crate) struct ProcessTable {
    by_pid: HashMap<u32, ProcessInfo>,
    children_by_parent: HashMap<u32, Vec<u32>>,
}

#[cfg_attr(target_os = "linux", allow(dead_code))]
impl ProcessTable {
    pub(crate) fn from_processes(processes: impl IntoIterator<Item = ProcessInfo>) -> Self {
        let mut table = Self::default();
        for mut process in processes {
            process.depth = 0;
            table
                .children_by_parent
                .entry(process.parent_pid)
                .or_default()
                .push(process.pid);
            table.by_pid.insert(process.pid, process);
        }
        table
    }

    pub(crate) fn children(&self, pid: u32) -> &[u32] {
        self.children_by_parent.get(&pid).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn comm(&self, pid: u32) -> Option<&str> {
        self.by_pid.get(&pid).map(|process| process.comm.as_str())
    }

    pub(crate) fn descendants(
        &self,
        root: u32,
        max_depth: u8,
        max_nodes: usize,
    ) -> Vec<ProcessInfo> {
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut queue: VecDeque<(u32, u8)> = self
            .children(root)
            .iter()
            .copied()
            .map(|pid| (pid, 1))
            .collect();

        while let Some((pid, depth)) = queue.pop_front() {
            if depth > max_depth || !seen.insert(pid) || out.len() >= max_nodes {
                continue;
            }
            let Some(process) = self.by_pid.get(&pid) else {
                continue;
            };
            for child in self.children(pid) {
                queue.push_back((*child, depth.saturating_add(1)));
            }
            let mut process = process.clone();
            process.depth = depth;
            out.push(process);
        }
        out
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn read_current_process_table() -> ProcessTable {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,args="])
        .output()
        .ok();
    let Some(output) = output.filter(|o| o.status.success()) else {
        // A failed/killed `ps` here silently degrades discovery and workload
        // detection (agents/workloads appear to vanish with nothing logged),
        // so leave a breadcrumb for `muxa logs`/RUST_LOG debugging.
        tracing::debug!("process-table snapshot unavailable: `ps -axo` did not succeed");
        return ProcessTable::default();
    };
    parse_ps_table(&String::from_utf8_lossy(&output.stdout))
}

#[cfg(not(target_os = "linux"))]
fn parse_ps_table(stdout: &str) -> ProcessTable {
    let processes = stdout.lines().filter_map(|line| {
        let mut parts = line.split_whitespace();
        let (Some(pid), Some(parent_pid), Some(argv0)) = (parts.next(), parts.next(), parts.next())
        else {
            return None;
        };
        let (Ok(pid), Ok(parent_pid)) = (pid.parse(), parent_pid.parse()) else {
            return None;
        };
        let cmdline = std::iter::once(argv0)
            .chain(parts)
            .collect::<Vec<_>>()
            .join(" ");
        Some(ProcessInfo {
            pid,
            parent_pid,
            depth: 0,
            comm: argv0.to_string(),
            cmdline,
        })
    });
    ProcessTable::from_processes(processes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, parent_pid: u32, comm: &str) -> ProcessInfo {
        ProcessInfo {
            pid,
            parent_pid,
            depth: 99,
            comm: comm.into(),
            cmdline: comm.into(),
        }
    }

    #[test]
    fn descendants_walks_breadth_first_and_assigns_depth() {
        let table = ProcessTable::from_processes(vec![
            proc(10, 1, "node"),
            proc(11, 1, "ruby"),
            proc(20, 10, "codex"),
            proc(21, 10, "zsh"),
            proc(30, 20, "helper"),
        ]);

        let out = table.descendants(1, 3, 100);

        assert_eq!(
            out.iter().map(|p| (p.pid, p.depth)).collect::<Vec<_>>(),
            vec![(10, 1), (11, 1), (20, 2), (21, 2), (30, 3)]
        );
    }

    #[test]
    fn descendants_respects_limits() {
        let table = ProcessTable::from_processes(vec![proc(10, 1, "a"), proc(20, 10, "b")]);

        assert_eq!(table.descendants(1, 1, 100).len(), 1);
        assert_eq!(table.descendants(1, 3, 1).len(), 1);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn parses_ps_table_rows() {
        let table = parse_ps_table(
            "  10   1 /opt/homebrew/bin/node /tmp/shim.js\n\
               20  10 /usr/local/bin/codex --resume\n",
        );

        assert_eq!(table.children(1), &[10]);
        assert_eq!(table.children(10), &[20]);
        assert_eq!(table.comm(20), Some("/usr/local/bin/codex"));
        assert_eq!(
            table
                .descendants(1, 4, 10)
                .into_iter()
                .map(|p| (p.pid, p.cmdline))
                .collect::<Vec<_>>(),
            vec![
                (10, "/opt/homebrew/bin/node /tmp/shim.js".to_string()),
                (20, "/usr/local/bin/codex --resume".to_string()),
            ]
        );
    }
}
