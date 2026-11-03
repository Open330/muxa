//! Everything the swap executor does to the outside world, behind one seam:
//! the pane (read, type, keep open, respawn), the agent's processes and the
//! clock. [`LiveIo`] is the real thing; tests drive the executor through a
//! scripted fake instead, so key sequences and failure branches are checked
//! without a terminal, a process or a wall-clock wait.
//!
//! Mutations are not called on this trait directly by the executor: they go
//! through [`super::effects::Runner`], which records each one before and
//! after it happens.

use super::{is_agent_process, is_alive, pane_processes, signal_pids, Target};
use crate::backend::SharedBackend;
use std::time::{Duration, Instant};

/// An agent process and its arguments (`None`: unreadable).
pub(crate) type AgentArgv = (u32, Option<Vec<String>>);

/// Time, so a fake can advance it instead of waiting.
pub(super) trait Clock {
    fn now(&self) -> Instant;
    fn sleep(&self, duration: Duration);
}

/// The wall clock, for tests that drive a probe in real time.
#[cfg(test)]
pub(super) struct RealClock;

#[cfg(test)]
impl Clock for RealClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

/// One target's pane and processes.
pub(super) trait SwapIo: Clock {
    /// The pane's visible text, colour escapes stripped.
    fn capture(&self) -> Option<String>;
    /// Type raw keys or text into the pane.
    fn send_text(&self, text: &str) -> bool;
    /// Type a prompt the way the agent's composer expects it.
    fn send_prompt_text(&self, text: &str) -> bool;
    fn set_remain_on_exit(&self, on: bool) -> bool;
    /// Ask the multiplexer to respawn the pane running `line` in `cwd`.
    fn respawn(&self, cwd: &str, line: &str) -> bool;
    /// The pane's root process now, found by its own id and server.
    fn pane_pid_now(&self) -> Option<u32>;
    /// Whether the relaunched agent, on this conversation, runs under
    /// `pane_pid`. Presence only: it is not evidence of which conversation
    /// the agent resumed ([`Self::agent_argvs`] is).
    fn agent_running(&self, pane_pid: u32) -> bool;
    /// Every agent process of the target's kind under `pane_pid`, with its
    /// arguments as the process holds them (`None`: unreadable).
    fn agent_argvs(&self, pane_pid: u32) -> Vec<AgentArgv>;
    fn alive(&self, pid: u32) -> bool;
    /// Send `signal` to `pids`. Whether it arrived is not reported: the
    /// processes' liveness is observed separately.
    fn signal(&self, signal: &str, pids: &[u32]);
    /// Who else holds the Codex conversation open, for a blocked report.
    fn writer_owners(&self, pane_pid: u32) -> String;
    /// How long a composer needs between a prompt and its Enter.
    fn submit_grace(&self) -> Duration;
    /// Where the agents' prompt hooks stand now, when they can be read here.
    fn prompt_cursor(&self) -> Option<u64>;
    /// Whether this target's own prompt hook arrived after `cursor`.
    fn prompt_seen_after(&self, cursor: u64) -> bool;
    /// What is at `path` now, symlinks followed. Read-only.
    fn path_facts(&self, path: &str) -> PathFacts;
    /// The sha256 of the file at `path`, if it can be read.
    fn file_digest(&self, path: &str) -> Option<String>;
    /// The working directory of process `pid`, if it can be read.
    fn process_cwd(&self, pid: u32) -> Option<String>;
}

/// What a path names, as far as a launch cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PathFacts {
    Missing,
    Directory,
    File {
        /// Some execute bit is set. Not proof it runs (a `noexec` mount,
        /// another user's bit, a loader it lacks).
        executable: bool,
        /// A script's `#!` program.
        interpreter: Option<String>,
    },
    Unreadable,
}

/// [`SwapIo::path_facts`] on the real filesystem.
pub(super) fn path_facts(path: &str) -> PathFacts {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return PathFacts::Missing,
        Err(_) => return PathFacts::Unreadable,
    };
    if metadata.is_dir() {
        return PathFacts::Directory;
    }
    #[cfg(unix)]
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    };
    #[cfg(not(unix))]
    let executable = true;
    PathFacts::File {
        executable,
        interpreter: shebang(path),
    }
}

/// The program a `#!` line names, read from the first bytes only.
fn shebang(path: &str) -> Option<String> {
    use std::io::Read;
    let mut head = [0_u8; 256];
    let read = std::fs::File::open(path).ok()?.read(&mut head).ok()?;
    let line = head[..read].strip_prefix(b"#!")?;
    let line = line.split(|byte| *byte == b'\n').next()?;
    let text = std::str::from_utf8(line).ok()?;
    text.split_whitespace().next().map(str::to_owned)
}

/// [`SwapIo::file_digest`] on the real filesystem.
pub(super) fn file_digest(path: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).ok()?;
    Some(format!("{:x}", Sha256::digest(bytes)))
}

/// [`SwapIo::process_cwd`] from `/proc`.
pub(super) fn process_cwd(pid: u32) -> Option<String> {
    let link = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    let path = link.to_str()?;
    // A removed directory still reads, marked; it is not there to run in.
    Some(path.strip_suffix(" (deleted)").unwrap_or(path).to_owned())
}

/// The pane and processes of a real target.
pub(super) struct LiveIo<'a> {
    pub(super) backend: &'a SharedBackend,
    pub(super) target: &'a Target,
    pub(super) probe: Option<&'a dyn super::TurnProbe>,
}

impl Clock for LiveIo<'_> {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

impl SwapIo for LiveIo<'_> {
    fn path_facts(&self, path: &str) -> PathFacts {
        path_facts(path)
    }

    fn file_digest(&self, path: &str) -> Option<String> {
        file_digest(path)
    }

    fn process_cwd(&self, pid: u32) -> Option<String> {
        process_cwd(pid)
    }

    fn capture(&self) -> Option<String> {
        self.backend
            .capture_pane_on(self.target.socket.as_deref(), &self.target.pane)
            .map(|screen| crate::screen::strip_ansi(&screen))
    }

    fn send_text(&self, text: &str) -> bool {
        self.backend
            .send_text_on(self.target.socket.as_deref(), &self.target.pane, text)
    }

    fn send_prompt_text(&self, text: &str) -> bool {
        self.backend
            .send_prompt_text_on(self.target.socket.as_deref(), &self.target.pane, text)
    }

    fn set_remain_on_exit(&self, on: bool) -> bool {
        self.backend
            .set_remain_on_exit_on(self.target.socket.as_deref(), &self.target.pane, on)
    }

    fn respawn(&self, cwd: &str, line: &str) -> bool {
        self.backend
            .respawn_pane_on(self.target.socket.as_deref(), &self.target.pane, cwd, line)
    }

    fn pane_pid_now(&self) -> Option<u32> {
        // `%5` exists on every server: match the pane's own.
        let socket = self.target.socket.as_deref();
        self.backend
            .list_panes()
            .into_iter()
            .find(|pane| {
                pane.pane_id == self.target.pane
                    && socket
                        .is_none_or(|socket| pane.socket.as_deref().is_none_or(|s| s == socket))
            })
            .map(|pane| pane.pane_pid)
    }

    fn agent_running(&self, pane_pid: u32) -> bool {
        pane_processes(pane_pid).iter().any(|p| {
            is_agent_process(p, self.target.kind)
                && p.cmdline.contains(self.target.session_id.as_str())
        })
    }

    fn agent_argvs(&self, pane_pid: u32) -> Vec<AgentArgv> {
        pane_processes(pane_pid)
            .into_iter()
            .filter(|p| is_agent_process(p, self.target.kind))
            .map(|p| (p.pid, crate::process_snapshot::process_argv(p.pid)))
            .collect()
    }

    fn alive(&self, pid: u32) -> bool {
        is_alive(pid)
    }

    fn signal(&self, signal: &str, pids: &[u32]) {
        signal_pids(signal, pids);
    }

    fn writer_owners(&self, pane_pid: u32) -> String {
        super::codex_writer_owners(pane_pid)
    }

    fn submit_grace(&self) -> Duration {
        crate::backend::prompt_submit_grace(&self.target.pane)
    }

    fn prompt_cursor(&self) -> Option<u64> {
        self.probe.map(super::TurnProbe::cursor)
    }

    fn prompt_seen_after(&self, cursor: u64) -> bool {
        self.probe.is_some_and(|probe| {
            probe.prompt_after(
                cursor,
                self.target.kind,
                &self.target.session_id,
                &super::endpoint_key(&self.target.pane, self.target.socket.as_deref()),
            )
        })
    }
}
