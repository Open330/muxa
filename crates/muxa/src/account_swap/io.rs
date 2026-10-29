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
    /// `pane_pid`.
    fn agent_running(&self, pane_pid: u32) -> bool;
    fn alive(&self, pid: u32) -> bool;
    /// Send `signal` to `pids`. Whether it arrived is not reported: the
    /// processes' liveness is observed separately.
    fn signal(&self, signal: &str, pids: &[u32]);
    /// Who else holds the Codex conversation open, for a blocked report.
    fn writer_owners(&self, pane_pid: u32) -> String;
    /// How long a composer needs between a prompt and its Enter.
    fn submit_grace(&self) -> Duration;
}

/// The pane and processes of a real target.
pub(super) struct LiveIo<'a> {
    pub(super) backend: &'a SharedBackend,
    pub(super) target: &'a Target,
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
}
