//! Native compatibility coverage for captures using observed identities.
#![cfg(unix)]
use muxa::{PaneBackend, RmuxBackend};
use std::path::PathBuf;
use std::process::Command;

struct Server(PathBuf);
impl Server {
    fn command(&self) -> Command {
        let mut command = Command::new("rmux");
        command.args(["-S"]).arg(&self.0);
        for key in ["RMUX", "RMUX_PANE", "TMUX", "TMUX_PANE"] {
            command.env_remove(key);
        }
        command
    }
    fn run(&self, args: &[&str]) {
        let result = self.command().args(args).output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.command().arg("kill-server").output();
    }
}

#[test]
#[ignore = "requires the rmux CLI; uses a private server and shell panes only"]
fn qualified_capture_handles_linked_windows_and_a_moved_pane() {
    let root = tempfile::tempdir().unwrap();
    let server = Server(root.path().join("socket"));
    server.run(&[
        "-f",
        "/dev/null",
        "new-session",
        "-d",
        "-s",
        "capture-test",
        "/bin/sh",
    ]);
    server.run(&[
        "new-session",
        "-d",
        "-t",
        "capture-test",
        "-s",
        "capture-view",
    ]);
    server.run(&[
        "send-keys",
        "-t",
        "capture-test:0",
        "printf 'CAPTURE_MOVED_OK\\n'",
        "Enter",
    ]);
    let backend = RmuxBackend::with_endpoint(server.0.to_string_lossy());
    let target = muxa::backend::rmux::qualified_pane_target("$0", "@0", "%0").unwrap();
    let mut ready = false;
    for _ in 0..30 {
        if backend
            .capture_pane_on(None, &target)
            .is_some_and(|text| text.contains("CAPTURE_MOVED_OK"))
        {
            ready = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(ready, "linked-window capture did not read the shell");
    server.run(&[
        "new-window",
        "-d",
        "-t",
        "capture-test",
        "-n",
        "other",
        "/bin/sh",
    ]);
    server.run(&["join-pane", "-s", "$0:@0.%0", "-t", "$0:@1"]);
    assert!(
        backend
            .capture_pane_on(None, &target)
            .unwrap()
            .contains("CAPTURE_MOVED_OK"),
        "a stale observed location must resolve the moved pane again"
    );
}
