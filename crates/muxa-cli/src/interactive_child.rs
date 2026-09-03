//! Waits for an interactive child (a `tmux attach-session` client or the
//! `ssh` carrying a remote attach) while forwarding hang-up signals to it.
//!
//! `muxa fleet attach --fit` temporarily changes the target window's sizing
//! policy, dimensions, active pane, and zoom state, and restores them from a
//! `Drop` guard once the client exits. When the terminal around this process
//! goes away instead — Muxa.app's Live Pane **Stop** asks muxad to end the
//! PTY, which delivers SIGHUP — the default signal disposition kills the
//! process outright and no destructor runs. The window would then stay
//! zoomed and sized to a viewport that no longer exists. Recording the
//! signal, handing it to the child, and returning normally lets every guard
//! restore its state before the process ends.

use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use signal_hook::consts::{SIGHUP, SIGTERM};

/// Zero while no signal is pending; otherwise the signal number.
fn pending_signal() -> &'static Arc<AtomicUsize> {
    static PENDING: OnceLock<Arc<AtomicUsize>> = OnceLock::new();
    PENDING.get_or_init(|| {
        let flag = Arc::new(AtomicUsize::new(0));
        for signal in [SIGHUP, SIGTERM] {
            // Registration only fails for signals that cannot be handled;
            // SIGHUP and SIGTERM are always allowed.
            let value = usize::try_from(signal).unwrap_or(0);
            let _ = signal_hook::flag::register_usize(signal, Arc::clone(&flag), value);
        }
        flag
    })
}

/// `Child::wait` would block straight through the signal, so the wait polls
/// `try_wait` at this interval and checks for a recorded signal in between.
const POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Outcome of an interactive child.
#[derive(Debug)]
pub(crate) struct InteractiveExit {
    pub(crate) status: ExitStatus,
    /// The signal this process received and forwarded to the child, if any.
    /// Callers treat a forwarded hang-up as a normal detach rather than an
    /// error: the surrounding terminal went away and the child was told so.
    pub(crate) forwarded_signal: Option<Signal>,
}

impl InteractiveExit {
    pub(crate) fn is_clean_detach(&self) -> bool {
        self.status.success() || self.forwarded_signal.is_some()
    }
}

/// Spawn `command` and wait for it, forwarding SIGHUP/SIGTERM received by
/// this process to the child so its own cleanup and this process's `Drop`
/// guards both run.
pub(crate) fn run_interactive(command: &mut Command) -> std::io::Result<InteractiveExit> {
    let child = command.spawn()?;
    wait_forwarding_hangup(child)
}

pub(crate) fn wait_forwarding_hangup(mut child: Child) -> std::io::Result<InteractiveExit> {
    let pending = pending_signal();
    pending.store(0, Ordering::SeqCst);
    let mut forwarded_signal = None;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(InteractiveExit {
                status,
                forwarded_signal,
            });
        }
        let received = pending.swap(0, Ordering::SeqCst);
        if received != 0 && forwarded_signal.is_none() {
            let signal = i32::try_from(received)
                .ok()
                .and_then(|number| Signal::try_from(number).ok())
                .unwrap_or(Signal::SIGHUP);
            let pid = i32::try_from(child.id()).map(Pid::from_raw);
            if let Ok(pid) = pid {
                // A child that already exited is reaped by the next
                // `try_wait`; a failed kill is not worth aborting the wait.
                let _ = kill(pid, signal);
            }
            forwarded_signal = Some(signal);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn hangup_is_forwarded_to_the_child_and_the_wait_returns() {
        let child = Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        // Register the handler before the signal can arrive.
        let _ = pending_signal();
        let raiser = std::thread::spawn(|| {
            std::thread::sleep(Duration::from_millis(200));
            signal_hook::low_level::raise(SIGHUP).expect("raise SIGHUP");
        });

        let exit = wait_forwarding_hangup(child).expect("wait");
        raiser.join().expect("raiser thread");

        assert_eq!(exit.forwarded_signal, Some(Signal::SIGHUP));
        assert_eq!(exit.status.signal(), Some(SIGHUP));
        assert!(exit.is_clean_detach());
    }

    #[test]
    fn normal_exit_reports_no_forwarded_signal() {
        let exit = run_interactive(&mut Command::new("/usr/bin/true")).expect("run true");
        assert!(exit.status.success());
        assert_eq!(exit.forwarded_signal, None);
    }
}
