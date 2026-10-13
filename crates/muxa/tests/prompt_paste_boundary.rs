//! Isolated terminal protocol test; never touches the user's running servers.
use muxa::{PaneBackend, RmuxBackend, TmuxBackend};
use std::process::Command;
use std::time::{Duration, Instant};

struct Server {
    binary: String,
    socket: String,
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = Command::new(&self.binary)
            .args(["-S", &self.socket, "kill-server"])
            .output();
    }
}

fn wait_file(path: &std::path::Path) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(bytes) = std::fs::read(path) {
            return bytes;
        }
        assert!(
            Instant::now() < deadline,
            "fixture did not write {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn check(binary: String, rmux: bool) {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("server.sock").to_str().unwrap().to_owned();
    let script = dir.path().join("input.py");
    let ready = dir.path().join("ready");
    let received = dir.path().join("received");
    // Advertise bracketed paste as Codex does, and record the exact PTY stream.
    std::fs::write(
        &script,
        r"import os, sys, tty, time
tty.setraw(0)
os.write(1, b'\x1b[?2004h')
open(sys.argv[1], 'wb').write(b'ready')
data = b''
while not data.endswith(b'\r'):
    data += os.read(0, 1)
open(sys.argv[2], 'wb').write(data)
time.sleep(30)
",
    )
    .unwrap();
    let server = Server { binary, socket };
    let command = format!(
        "python3 {} {} {}",
        script.display(),
        ready.display(),
        received.display()
    );
    let output = Command::new(&server.binary)
        .args([
            "-S",
            &server.socket,
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            "prompt-test",
            "-P",
            "-F",
            "#{pane_id}",
            &command,
        ])
        .env_remove("TMUX")
        .env_remove("RMUX")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pane = String::from_utf8(output.stdout).unwrap().trim().to_owned();
    let backend: Box<dyn PaneBackend> = if rmux {
        Box::new(RmuxBackend::with_endpoint(&server.socket))
    } else {
        Box::new(TmuxBackend::new())
    };
    let pane = if rmux { format!("rmux:{pane}") } else { pane };
    wait_file(&ready);
    assert!(backend.send_prompt_text_on(Some(&server.socket), &pane, "continue"));
    // No sleep: the explicit end boundary must precede the separate submit.
    assert!(backend.send_text_on(Some(&server.socket), &pane, "\r"));
    assert_eq!(wait_file(&received), b"\x1b[200~continue\x1b[201~\r");
}

#[test]
#[ignore = "requires native tmux and python3; creates an isolated test server"]
fn tmux_short_prompt_has_explicit_boundary_before_enter() {
    check("/usr/bin/tmux".into(), false);
}

#[test]
#[ignore = "requires rmux and python3; creates an isolated test server"]
fn rmux_short_prompt_has_explicit_boundary_before_enter() {
    check(
        std::env::var("MUXA_TEST_RMUX_BINARY").unwrap_or_else(|_| "rmux".into()),
        true,
    );
}
