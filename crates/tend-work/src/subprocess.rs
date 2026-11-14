//! What every process `work up` starts inherits, when tendd runs it.
//!
//! The `tend` child tendd used to run `work up` in gave every process under
//! it three things: a closed stdin, `TEND_SOCKET` pinned to the daemon that
//! accepted the request, and a deadline after which the whole child was
//! killed. tendd's in-process `work up` runs on one dedicated thread, so
//! [`with_scope`] sets a thread-local [`Scope`] around it. The `git`, tmux and
//! `prepare` commands below then go through [`output`] / [`status`], which
//! apply that scope. Each command runs in its own process group, is killed
//! with it at the deadline (including descendants that keep its output
//! open), and is refused outright once the deadline has passed. The mux
//! helpers that run their own processes in `tend` check
//! [`ensure_time_left`] first. Together these keep a timed-out `work up` from
//! touching tmux, git or `prepare` after it has been reported as failed.
//! Daemon IPC calls are bounded by their own timeouts and are not refused.
//!
//! Without a scope (the CLI, the MCP server) both are plain
//! `Command::output` / `Command::status`.

use std::cell::RefCell;
use std::io;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Scope {
    /// The daemon a nested `tend` must reach.
    pub socket: PathBuf,
    /// When the operation's time is up.
    pub deadline: Instant,
    /// Set when the operation is abandoned (tendd shutting down): the
    /// running command is killed and no further one starts, as when the
    /// `tend` child used to be killed.
    pub cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

impl Scope {
    fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// Time left, or `None` once the deadline passed or the scope was
    /// cancelled.
    fn remaining(&self) -> Option<Duration> {
        if self.cancelled() {
            return None;
        }
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|left| !left.is_zero())
    }
}

thread_local! {
    static SCOPE: RefCell<Option<Scope>> = const { RefCell::new(None) };
}

/// Run `operation` with `scope` applied to every [`output`] / [`status`] on
/// this thread, restoring the previous scope afterwards (also on unwind).
pub fn with_scope<T>(scope: Scope, operation: impl FnOnce() -> T) -> T {
    struct Restore(Option<Scope>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SCOPE.with(|slot| {
                slot.replace(self.0.take());
            });
        }
    }
    let _restore = Restore(SCOPE.with(|slot| slot.replace(Some(scope))));
    operation()
}

fn current() -> Option<Scope> {
    SCOPE.with(|slot| slot.borrow().clone())
}

/// The scope this thread runs under, for handing to a worker thread it
/// starts: a scope is per thread, and work it farms out must stay inside it.
#[must_use]
pub fn current_scope() -> Option<Scope> {
    current()
}

/// Run `operation` under `scope` if there is one, else as is.
pub fn within<T>(scope: Option<Scope>, operation: impl FnOnce() -> T) -> T {
    match scope {
        Some(scope) => with_scope(scope, operation),
        None => operation(),
    }
}

/// Whether the thread's scope has run out of time or been cancelled. A
/// failure then is the operation being abandoned, not the step failing: the
/// `tend` child it replaces was killed at that point and reported nothing,
/// so callers must not record it as a durable failure either.
#[must_use]
pub fn abandoned() -> bool {
    current().is_some_and(|scope| scope.remaining().is_none())
}

/// `Err` once the thread's scope has run out of time. For mux operations
/// that do not start a process of their own here (`tend::tmux` and rmux
/// helpers, which carry their own per-call timeouts), checked before each
/// one, so none of them starts after the deadline.
pub fn ensure_time_left() -> io::Result<()> {
    match current() {
        Some(scope) if scope.remaining().is_none() => Err(timed_out()),
        _ => Ok(()),
    }
}

fn prepare(command: &mut Command, scope: &Scope) -> io::Result<()> {
    scope.remaining().ok_or_else(timed_out)?;
    command
        .stdin(Stdio::null())
        .env("TEND_SOCKET", &scope.socket);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    Ok(())
}

fn timed_out() -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        "stopped: the operation ran out of time, or tendd is shutting down",
    )
}

/// Kill the process group the command led (its own id, from
/// `process_group(0)`), which takes everything it started.
fn kill_group(leader: u32) {
    #[cfg(unix)]
    if let Ok(pid) = i32::try_from(leader) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    #[cfg(not(unix))]
    let _ = leader;
}

/// Kill the command and everything it started.
fn kill(child: &mut std::process::Child) {
    kill_group(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

fn wait(child: &mut std::process::Child, scope: &Scope) -> io::Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if scope.remaining().is_none() {
            kill(child);
            return Err(timed_out());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `command.output()`, under the thread's scope when one is set.
pub fn output(command: &mut Command) -> io::Result<Output> {
    let Some(scope) = current() else {
        return command.output();
    };
    prepare(command, &scope)?;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let drain = |pipe: Option<Box<dyn io::Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(child.stdout.take().map(|p| Box::new(p) as _));
    let stderr = drain(child.stderr.take().map(|p| Box::new(p) as _));
    let status = wait(&mut child, &scope)?;
    // A descendant can keep the pipes open after the command itself exits,
    // even one that left the process group (`setsid`), which killing the
    // group cannot reach. The deadline covers reading them to the end too:
    // past it the group is killed and the readers are left behind (they end
    // when the last holder closes the pipe) rather than waited on.
    while !(stdout.is_finished() && stderr.is_finished()) {
        if scope.remaining().is_none() {
            kill_group(child.id());
            return Err(timed_out());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

/// `command.status()`, under the thread's scope when one is set. Output is
/// inherited, as `status` does.
pub fn status(command: &mut Command) -> io::Result<ExitStatus> {
    let Some(scope) = current() else {
        return command.status();
    };
    prepare(command, &scope)?;
    let mut child = command.spawn()?;
    wait(&mut child, &scope)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn scope(socket: &str, left: Duration) -> Scope {
        Scope {
            socket: socket.into(),
            deadline: Instant::now() + left,
            cancel: None,
        }
    }

    fn sh(script: &str) -> Command {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }

    #[test]
    fn outside_a_scope_commands_run_as_usual() {
        let out = output(&mut sh("echo \"${TEND_SOCKET:-unset}\"")).unwrap();
        assert!(out.status.success());
    }

    #[test]
    fn a_scoped_command_gets_a_closed_stdin_and_the_daemon_socket() {
        let out = with_scope(scope("/tmp/tendd.sock", Duration::from_secs(10)), || {
            output(&mut sh("read line; echo \"[$line]$TEND_SOCKET\"")).unwrap()
        });
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "[]/tmp/tendd.sock"
        );
    }

    #[test]
    fn the_deadline_kills_the_command_and_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("late");
        let started = Instant::now();
        let error = with_scope(scope("/s", Duration::from_millis(300)), || {
            // The grandchild would write the marker after 1 s if it survived.
            status(&mut sh(&format!(
                "(sleep 1; touch '{}') & sleep 30",
                marker.display()
            )))
            .unwrap_err()
        });
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(1500));
        assert!(!marker.exists(), "the process group outlived the deadline");
    }

    #[test]
    fn a_descendant_holding_the_pipes_does_not_outlast_the_deadline() {
        let started = Instant::now();
        let error = with_scope(scope("/s", Duration::from_millis(300)), || {
            // The command exits at once; its background child keeps stdout.
            output(&mut sh("sleep 30 & exit 0")).unwrap_err()
        });
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_detached_descendant_holding_the_pipes_does_not_outlast_the_deadline() {
        // The grandchild leaves the process group (`setsid`), so killing the
        // group cannot close the pipes it inherited.
        let started = Instant::now();
        let error = with_scope(scope("/s", Duration::from_millis(300)), || {
            output(&mut sh(
                "perl -MPOSIX -e 'if (fork() == 0) { POSIX::setsid(); sleep 3; exit 0 } exit 0'",
            ))
            .unwrap_err()
        });
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn cancelling_kills_the_running_command_and_refuses_the_next() {
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let scope = Scope {
            socket: "/s".into(),
            deadline: Instant::now() + Duration::from_secs(60),
            cancel: Some(flag.clone()),
        };
        let setter = {
            let flag = flag.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
            })
        };
        let started = Instant::now();
        let (running, next) = with_scope(scope, || {
            (
                status(&mut sh("sleep 30")).unwrap_err(),
                ensure_time_left().unwrap_err(),
            )
        });
        setter.join().unwrap();
        assert_eq!(running.kind(), io::ErrorKind::TimedOut);
        assert_eq!(next.kind(), io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn nothing_starts_once_the_deadline_has_passed() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let error = with_scope(scope("/s", Duration::ZERO), || {
            output(&mut sh(&format!("touch '{}'", marker.display()))).unwrap_err()
        });
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(!marker.exists());
        assert!(with_scope(scope("/s", Duration::ZERO), ensure_time_left).is_err());
        assert!(ensure_time_left().is_ok(), "no scope, no deadline");
    }
}
