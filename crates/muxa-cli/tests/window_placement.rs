//! `--placement window` must land in the session the target belongs to, at an
//! index tmux picks, even when a window is named after that session.
//!
//! This is the shape muxa's own topology produces — one session per workspace,
//! whose first window is commonly named after it — and it used to fail with
//! `create window failed: index 0 in use`: `new-window -t <name>` takes a
//! *window* target, so a colonless string is looked up as a window in the
//! caller's current session before it is tried as a session.
//!
//! The test drives the real `muxa` binary against a private tmux server, with
//! a stand-in `claude` on `PATH` so no agent CLI has to exist on the machine.
//! It skips when tmux is not installed.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn muxa() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_muxa"))
}

fn tmux_installed() -> bool {
    Command::new("tmux")
        .arg("-V")
        .output()
        .is_ok_and(|out| out.status.success())
}

/// A tmux server of our own: private socket, and `-f /dev/null` so the
/// operator's `~/.tmux.conf` (hooks included) stays out of the test.
struct Server {
    socket: PathBuf,
    _dir: tempfile::TempDir,
}

impl Server {
    fn start(dir: tempfile::TempDir) -> Self {
        let socket = dir.path().join("tmux.sock");
        let server = Self { socket, _dir: dir };
        server
            .tmux(&[
                "new-session",
                "-d",
                "-s",
                "junia",
                "-x",
                "80",
                "-y",
                "24",
                "-c",
                "/tmp",
            ])
            .expect("start the test tmux server");
        // A second session, standing in for the pane a caller runs from, whose
        // window carries the *target* session's name. That is the collision:
        // tmux reads a colonless `junia` as this window before it tries the
        // session, which is both how `index 0 in use` happened and how a
        // window could land in the caller's session instead.
        server
            .tmux(&[
                "new-session",
                "-d",
                "-s",
                "caller",
                "-x",
                "80",
                "-y",
                "24",
                "-c",
                "/tmp",
            ])
            .expect("start the caller session");
        server
            .tmux(&["rename-window", "-t", "caller:0", "junia"])
            .expect("name the caller's window after the target session");
        server
    }

    fn tmux(&self, args: &[&str]) -> Result<String, String> {
        let out = Command::new("tmux")
            .arg("-f")
            .arg("/dev/null")
            .arg("-S")
            .arg(&self.socket)
            .args(args)
            .output()
            .expect("run tmux");
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() {
            Ok(stdout)
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    fn query(&self, args: &[&str]) -> String {
        self.tmux(args).expect("tmux query")
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.tmux(&["kill-server"]);
    }
}

/// A `claude` that stays in its pane and needs nothing installed.
fn stub_agent_dir(root: &Path) -> PathBuf {
    let bin = root.join("bin");
    std::fs::create_dir_all(&bin).expect("create stub bin dir");
    let claude = bin.join("claude");
    std::fs::write(&claude, "#!/bin/sh\nexec sleep 600\n").expect("write stub claude");
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755))
        .expect("chmod stub claude");
    bin
}

/// tmux runs the launch through `sh -c`, which then execs the stub, so the
/// foreground command reported the instant the window appears may still be the
/// shell. Poll briefly rather than race it.
fn wait_for_pane_command(server: &Server, window: &str) -> String {
    let mut command = String::new();
    for _ in 0..50 {
        command = server.query(&[
            "display-message",
            "-p",
            "-t",
            window,
            "#{pane_current_command}",
        ]);
        if command == "sleep" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    command
}

#[test]
fn window_placement_lands_in_the_target_session_beside_a_same_named_window() {
    if !tmux_installed() {
        eprintln!("skipping: tmux is not installed");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let stub_bin = stub_agent_dir(dir.path());
    let server = Server::start(dir);

    let session_id = server.query(&["display-message", "-p", "-t", "junia:", "#{session_id}"]);
    let window_id = server.query(&["display-message", "-p", "-t", "junia:0", "#{window_id}"]);
    let pane_id = server.query(&["display-message", "-p", "-t", "junia:0", "#{pane_id}"]);
    let caller_session = server.query(&["display-message", "-p", "-t", "caller:", "#{session_id}"]);
    let caller_pane = server.query(&["display-message", "-p", "-t", "caller:0", "#{pane_id}"]);
    let server_pid = server.query(&["display-message", "-p", "#{pid}"]);
    // What a caller running inside `caller` has in its environment. It is what
    // gives tmux a "current session" to resolve an ambiguous target against,
    // so without it the collision cannot happen at all.
    let tmux_env = format!(
        "{},{},{}",
        server.socket.display(),
        server_pid,
        caller_session.trim_start_matches('$'),
    );

    // Every address muxa accepts for the same session.
    for (nth, target) in [
        session_id.as_str(),
        "junia",
        window_id.as_str(),
        pane_id.as_str(),
    ]
    .iter()
    .enumerate()
    {
        let name = format!("GH-13{nth}");
        let path = format!(
            "{}:{}",
            stub_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new(muxa())
            .args([
                "agent",
                "start",
                "--agent",
                "claude",
                "--placement",
                "window",
                "--target",
                target,
                "--name",
                &name,
                "--cwd",
                "/tmp",
                "--json",
            ])
            .env("PATH", path)
            .env("MUXA_TMUX_SOCKET", &server.socket)
            .env("TMUX", &tmux_env)
            .env("TMUX_PANE", &caller_pane)
            .output()
            .expect("run muxa agent start");
        assert!(
            out.status.success(),
            "target {target:?} failed: {}",
            String::from_utf8_lossy(&out.stderr),
        );

        // Where it landed, across every session on the server: a window in
        // `caller` would be the silent half of this bug.
        let placed = server.query(&[
            "list-windows",
            "-a",
            "-F",
            "#{session_name} #{window_index} #{window_name}",
        ]);
        let row = placed
            .lines()
            .find(|line| line.split_whitespace().nth(2) == Some(name.as_str()))
            .unwrap_or_else(|| panic!("no window named {name} anywhere; windows:\n{placed}"));
        let mut fields = row.split_whitespace();
        let session = fields.next().expect("session name");
        let index: u32 = fields
            .next()
            .and_then(|idx| idx.parse().ok())
            .expect("window index");
        assert_eq!(
            session, "junia",
            "target {target:?} addressed junia; windows:\n{placed}",
        );
        assert_ne!(index, 0, "index 0 belongs to the session's first window");
        // The stub is what ran, so a machine with a real `claude` on PATH
        // cannot make this test pass for the wrong reason. The pane goes
        // through `sh -c` first, so give the exec a moment to land.
        let command = wait_for_pane_command(&server, &format!("junia:{index}"));
        assert_eq!(command, "sleep", "the stub agent should own the new pane");
    }

    // Four launches, four windows, every one of them in the addressed
    // session and none in the caller's.
    let windows = server.query(&["list-windows", "-a", "-F", "#{session_name}:#{window_name}"]);
    assert_eq!(
        windows
            .lines()
            .filter(|w| w.starts_with("junia:GH-13"))
            .count(),
        4,
        "windows:\n{windows}",
    );
    assert!(
        !windows.lines().any(|w| w.starts_with("caller:GH-13")),
        "nothing may be created in the caller's session; windows:\n{windows}",
    );
}
