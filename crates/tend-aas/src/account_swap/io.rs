//! Everything the swap executor does to the outside world, behind one seam:
//! the pane (read, type, keep open, respawn), the agent's processes and the
//! clock. [`LiveIo`] is the real thing; tests drive the executor through a
//! scripted fake instead, so key sequences and failure branches are checked
//! without a terminal, a process or a wall-clock wait.
//!
//! Mutations are not called on this trait directly by the executor: they go
//! through [`super::effects::Runner`], which records each one before and
//! after it happens.

use super::Target;
use std::time::{Duration, Instant};
pub(crate) use tend::agent_tui::Clock;
use tend::backend::SharedBackend;
use tend::pane_process::{is_agent_process, is_alive, pane_processes, signal_pids};

/// An agent process and its arguments (`None`: unreadable).
pub(crate) type AgentArgv = (u32, Option<Vec<String>>);

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
    /// Register the follow-up `text` with the daemon before typing it.
    fn expect_prompt(&self, text: &str) -> Option<u64> {
        let _ = text;
        None
    }
    /// Drop a registration whose prompt was not submitted.
    fn forget_prompt(&self, token: u64) {
        let _ = token;
    }
    /// Whether a hook consumed the follow-up's registration `token`.
    fn registration_consumed(&self, token: u64) -> bool {
        let _ = token;
        false
    }
    /// What is at `path` now, symlinks followed. Read-only, and never
    /// blocks: only a regular file's contents are read, and boundedly.
    fn path_facts(&self, path: &str) -> PathFacts;
    /// The sha256 of the regular file at `path`, if it can be read.
    fn file_digest(&self, path: &str) -> Option<String>;
    /// The working directory process `pid` is actually in.
    fn process_cwd(&self, pid: u32) -> Option<ShellCwd>;
    /// The `TEND_SWAP_LAUNCH_ID` in process `pid`'s own environment.
    fn launch_id_of(&self, pid: u32) -> super::LaunchMark {
        let _ = pid;
        super::LaunchMark::Unreadable
    }
}

/// What a path names, as far as a launch cares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum PathFacts {
    Missing,
    Directory,
    /// A regular file.
    File {
        /// Some execute bit is set. Not proof it runs (a `noexec` mount,
        /// another user's bit, a loader it lacks).
        executable: bool,
        /// A script's `#!` program.
        interpreter: Option<String>,
    },
    /// Neither: a FIFO, socket or device. Never read.
    Other,
    Unreadable,
}

/// A process's working directory, read from the directory itself rather
/// than from its name: one removed since keeps the process in it, whatever
/// now has its name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ShellCwd {
    /// Its name as the kernel renders it, for the report only.
    pub(super) path: String,
    /// It no longer has a name: the process is in a removed directory.
    pub(super) removed: bool,
}

/// The most of a file read for its digest.
const DIGEST_LIMIT: u64 = 1024 * 1024;

#[cfg(target_os = "linux")]
const O_NONBLOCK: i32 = 0o4000;
#[cfg(any(target_os = "macos", target_os = "ios", target_os = "freebsd"))]
const O_NONBLOCK: i32 = 0x0004;

/// `path` opened for reading only if it is a regular file, checked on the
/// open handle: a FIFO put in its place never blocks the open or a read.
fn open_regular(path: &str) -> Option<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd"
    ))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(O_NONBLOCK);
    }
    let file = options.open(path).ok()?;
    file.metadata().ok().filter(std::fs::Metadata::is_file)?;
    Some(file)
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
    if !metadata.is_file() {
        return PathFacts::Other;
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
    let read = open_regular(path)?.read(&mut head).ok()?;
    let line = head[..read].strip_prefix(b"#!")?;
    let line = line.split(|byte| *byte == b'\n').next()?;
    let text = std::str::from_utf8(line).ok()?;
    text.split_whitespace().next().map(str::to_owned)
}

/// [`SwapIo::file_digest`] on the real filesystem; `None` for anything but
/// a readable regular file of at most [`DIGEST_LIMIT`] bytes.
pub(super) fn file_digest(path: &str) -> Option<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut bytes = Vec::new();
    open_regular(path)?
        .take(DIGEST_LIMIT + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > DIGEST_LIMIT {
        return None;
    }
    Some(format!("{:x}", Sha256::digest(bytes)))
}

/// [`SwapIo::process_cwd`] from `/proc`: the directory itself, followed
/// through the link, says whether it was removed.
pub(super) fn process_cwd(pid: u32) -> Option<ShellCwd> {
    let link = format!("/proc/{pid}/cwd");
    let path = std::fs::read_link(&link).ok()?.to_str()?.to_owned();
    let metadata = std::fs::metadata(&link).ok()?;
    #[cfg(unix)]
    let removed = {
        use std::os::unix::fs::MetadataExt;
        metadata.nlink() == 0
    };
    #[cfg(not(unix))]
    let removed = false;
    Some(ShellCwd {
        path,
        removed: removed || !metadata.is_dir(),
    })
}

/// [`SwapIo::launch_id_of`] from a process's environment entries; an empty
/// one could not be read (a running agent always has some).
pub(super) fn launch_id_of(environ: &[(String, String)]) -> super::LaunchMark {
    if environ.is_empty() {
        return super::LaunchMark::Unreadable;
    }
    environ
        .iter()
        .find(|(key, _)| key == "TEND_SWAP_LAUNCH_ID")
        .map_or(super::LaunchMark::Absent, |(_, value)| {
            super::LaunchMark::Carries(value.clone())
        })
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

    fn process_cwd(&self, pid: u32) -> Option<ShellCwd> {
        process_cwd(pid)
    }

    fn launch_id_of(&self, pid: u32) -> super::LaunchMark {
        launch_id_of(&tend::accounts::process_environ(pid))
    }

    fn capture(&self) -> Option<String> {
        self.backend
            .capture_pane_on(self.target.socket.as_deref(), &self.target.pane)
            .map(|screen| tend::screen::strip_ansi(&screen))
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
            .map(|p| (p.pid, tend::process_snapshot::process_argv(p.pid)))
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
        tend::backend::prompt_submit_grace(&self.target.pane)
    }

    fn prompt_cursor(&self) -> Option<u64> {
        self.probe.map(super::TurnProbe::cursor)
    }

    fn expect_prompt(&self, text: &str) -> Option<u64> {
        self.probe?
            .expect_prompt(&self.target.pane, self.target.socket.as_deref(), text)
    }

    fn forget_prompt(&self, token: u64) {
        if let Some(probe) = self.probe {
            probe.forget_prompt(token);
        }
    }

    fn registration_consumed(&self, token: u64) -> bool {
        self.probe
            .is_some_and(|probe| probe.registration_consumed(token))
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
