//! Whether an OS process id still names a live process.
//!
//! One answer for every caller: the registry's pid-tracked rows, launch
//! fences and the account swap's stop loop. None of these spawn a process,
//! but callers holding a lock should still check outside it.

/// Whether `pid` names a live process, including a zombie that has not been
/// reaped yet. Linux reads `/proc`, Windows the process table. Other Unix
/// hosts ask the kernel with signal 0: `EPERM` means the process belongs to
/// another user and is alive, and only `ESRCH` means it is gone. A pid no
/// single process can have (0, or past `i32::MAX`) is not alive.
pub(crate) fn pid_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(windows)]
    {
        crate::process_snapshot::pid_exists(pid)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        i32::try_from(pid).is_ok_and(|raw| raw > 0) && !pid_gone(pid)
    }
}

/// The kernel's own answer that no process has this pid. `ps` printing
/// nothing cannot tell an exited pid from a failed spawn; `ESRCH` can.
/// `EPERM` means the pid is alive under another user, so it is not an exit.
#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn pid_gone(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    // 0 and negatives address process groups, never a single process.
    raw > 0
        && matches!(
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw), None),
            Err(nix::errno::Errno::ESRCH)
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_is_alive_and_a_reaped_child_is_not() {
        assert!(pid_alive(std::process::id()));
        #[cfg(windows)]
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit"])
            .spawn()
            .unwrap();
        #[cfg(not(windows))]
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        assert!(!pid_alive(pid));
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    #[test]
    fn another_users_process_is_alive() {
        // pid 1 belongs to root: EPERM is a live process, not an exit.
        assert!(pid_alive(1));
        assert!(!pid_gone(1));
        // No process can have these; the fence check still calls them unknown.
        assert!(!pid_alive(0));
        assert!(!pid_alive(u32::MAX));
        assert!(!pid_gone(0));
        assert!(!pid_gone(u32::MAX));
    }
}
