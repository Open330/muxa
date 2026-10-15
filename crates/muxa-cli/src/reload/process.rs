//! What each pane runs, read from the OS process table once per snapshot:
//! the command with its argument boundaries intact, and when the agent in it
//! started.
//!
//! macOS `ps` only reports a command line joined, so an initial prompt or a
//! `--append-system-prompt "…"` value came back split on its spaces, and a
//! replay either restarted the task or failed to parse. `sysinfo` reads the
//! real argv (`KERN_PROCARGS2`) without unsafe code in this workspace.

use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use super::{is_shell, ChildCommand};

pub(super) struct ProcessTable {
    system: System,
}

impl ProcessTable {
    pub(super) fn read() -> Self {
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing().with_cmd(UpdateKind::Always),
        );
        Self { system }
    }

    fn argv(process: &Process) -> Vec<String> {
        process
            .cmd()
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect()
    }

    fn command(process: &Process) -> Option<ChildCommand> {
        let argv = Self::argv(process);
        let command = argv.join(" ").trim().to_owned();
        (!command.is_empty() && !is_shell(&command)).then_some(ChildCommand {
            command,
            argv: Some(argv),
        })
    }

    fn children(&self, pid: Pid) -> Vec<&Process> {
        let mut children: Vec<&Process> = self
            .system
            .processes()
            .values()
            .filter(|process| process.parent() == Some(pid))
            .collect();
        children.sort_by_key(|process| process.pid().as_u32());
        children
    }

    /// What a pane runs: the pane's own process when it is not a shell (a
    /// pane started *with* its command keeps its wrapper, and with it the
    /// account a resume needs), else the shell's first non-shell child.
    pub(super) fn child_command(&self, pane_pid: u32) -> Option<ChildCommand> {
        let pane = Pid::from_u32(pane_pid);
        if let Some(command) = self.system.process(pane).and_then(Self::command) {
            return Some(command);
        }
        self.children(pane).into_iter().find_map(Self::command)
    }

    /// When the `kind` agent under `pane_pid` started, in Unix seconds. The
    /// agent is the shallowest process whose argv names the program.
    pub(super) fn agent_started(&self, pane_pid: u32, kind: &str) -> Option<u64> {
        let program = match kind {
            "codex" => "codex",
            "claude_code" => "claude",
            _ => return None,
        };
        let mut level = vec![Pid::from_u32(pane_pid)];
        for _ in 0..6 {
            let next: Vec<&Process> = level.iter().flat_map(|pid| self.children(*pid)).collect();
            if let Some(agent) = next.iter().find(|process| {
                Self::argv(process)
                    .iter()
                    .take(2)
                    .any(|argument| argument.rsplit('/').next() == Some(program))
            }) {
                return Some(agent.start_time());
            }
            level = next.iter().map(|process| process.pid()).collect();
        }
        None
    }
}
