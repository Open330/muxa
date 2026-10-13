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
            if !bytes.is_empty() {
                return bytes;
            }
        }
        assert!(
            Instant::now() < deadline,
            "fixture did not write {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn check(binary: &str, rmux: bool, text: &str) {
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
while data != b'\r' and not data.endswith(b'\x1b[201~\r'):
    data += os.read(0, 1)
open(sys.argv[2], 'wb').write(data)
time.sleep(30)
",
    )
    .unwrap();
    let server = Server {
        binary: binary.into(),
        socket,
    };
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
    assert!(backend.send_prompt_text_on(Some(&server.socket), &pane, text));
    // No sleep: the explicit end boundary must precede the separate submit.
    assert!(backend.send_text_on(Some(&server.socket), &pane, "\r"));
    let expected = if text.is_empty() {
        b"\r".to_vec()
    } else {
        [
            b"\x1b[200~".as_slice(),
            text.as_bytes(),
            b"\x1b[201~\r".as_slice(),
        ]
        .concat()
    };
    assert_eq!(wait_file(&received), expected);
}

#[test]
#[ignore = "requires native tmux and python3; creates an isolated test server"]
fn tmux_prompts_preserve_text_and_end_boundary_before_enter() {
    for text in [
        "continue",
        "한국어 입력 확인;",
        "first line\nsecond line",
        "",
    ] {
        check("/usr/bin/tmux", false, text);
    }
}

#[test]
#[ignore = "requires rmux and python3; creates an isolated test server"]
fn rmux_prompts_preserve_text_and_end_boundary_before_enter() {
    let binary = std::env::var("MUXA_TEST_RMUX_BINARY").unwrap_or_else(|_| "rmux".into());
    for text in [
        "continue",
        "한국어 입력 확인;",
        "first line\nsecond line",
        "",
    ] {
        check(&binary, true, text);
    }
}
