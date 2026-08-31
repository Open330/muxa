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
        // The collision under test: window 0 carries the session's own name.
        server
            .tmux(&["rename-window", "-t", "junia:0", "junia"])
            .expect("name window 0 after the session");
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

#[test]
fn window_placement_lands_in_the_target_session_beside_a_same_named_window() {
    if !tmux_installed() {
        eprintln!("skipping: tmux is not installed");
        return;
    }
    let dir = tempfile::tempdir().expect("tempdir");
    let stub_bin = stub_agent_dir(dir.path());
    let server = Server::start(dir);

    let session_id = server.query(&["display-message", "-p", "-t", "junia", "#{session_id}"]);
    let window_id = server.query(&["display-message", "-p", "-t", "junia:0", "#{window_id}"]);
    let pane_id = server.query(&["display-message", "-p", "-t", "junia:0", "#{pane_id}"]);
    let server_pid = server.query(&["display-message", "-p", "#{pid}"]);
    // What a caller inside that session has in its environment. It is what
    // gives tmux a "current session" to resolve an ambiguous target against,
    // so without it the collision cannot happen at all.
    let tmux_env = format!(
        "{},{},{}",
        server.socket.display(),
        server_pid,
        session_id.trim_start_matches('$'),
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
            .env("TMUX_PANE", &pane_id)
            .output()
            .expect("run muxa agent start");
        assert!(
            out.status.success(),
            "target {target:?} failed: {}",
            String::from_utf8_lossy(&out.stderr),
        );

        let placed = server.query(&[
            "list-windows",
            "-t",
            "junia",
            "-F",
            "#{window_index} #{window_name} #{pane_current_command}",
        ]);
        let row = placed
            .lines()
            .find(|line| line.split_whitespace().nth(1) == Some(name.as_str()))
            .unwrap_or_else(|| panic!("no window named {name} in junia; windows:\n{placed}"));
        let index: u32 = row
            .split_whitespace()
            .next()
            .and_then(|idx| idx.parse().ok())
            .expect("window index");
        assert_ne!(index, 0, "index 0 belongs to the session's first window");
        // The stub is what ran, so a machine with a real `claude` on PATH
        // cannot make this test pass for the wrong reason.
        assert!(
            row.ends_with("sleep"),
            "the stub agent should own the new pane: {row}",
        );
    }

    // Four launches, four windows, all in the session that was addressed.
    let windows = server.query(&["list-windows", "-a", "-F", "#{session_name}:#{window_name}"]);
    assert_eq!(
        windows
            .lines()
            .filter(|w| w.starts_with("junia:GH-13"))
            .count(),
        4,
        "windows:\n{windows}",
    );
}
