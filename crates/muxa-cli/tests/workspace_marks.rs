//! Which session a workspace resolves to, and what muxa says when the two
//! sources of that answer disagree.
//!
//! A workspace's identity is a tmux session option (`@muxa_workspace_id`), and
//! the session *name* is the fallback used when nothing claims the workspace.
//! The mark outranks the name deliberately: that is what lets a renamed or
//! adopted session keep working. The cost is that a stale mark sends a launch
//! somewhere the operator did not name, and until now nothing said so.
//!
//! The tests drive the real `muxa` binary against a private tmux server, with
//! a stand-in `claude` on `PATH`. They skip when tmux is not installed.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn muxa() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_muxa"))
}

fn tmux_installed() -> bool {
    muxa::tmux::tmux_command()
        .arg("-V")
        .output()
        .is_ok_and(|out| out.status.success())
}

/// A tmux server of our own: private socket, and `-f /dev/null` so the
/// operator's `~/.tmux.conf` stays out of the test.
struct Server {
    socket: PathBuf,
    dir: tempfile::TempDir,
}

impl Server {
    fn start() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let socket = dir.path().join("tmux.sock");
        Self { socket, dir }
    }

    fn session(&self, name: &str) {
        self.tmux(&[
            "new-session",
            "-d",
            "-s",
            name,
            "-x",
            "80",
            "-y",
            "24",
            "-c",
            "/tmp",
        ])
        .unwrap_or_else(|error| panic!("start session {name}: {error}"));
    }

    fn tmux(&self, args: &[&str]) -> Result<String, String> {
        let out = muxa::tmux::tmux_command()
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

    /// Session name → workspace mark, for every session on the server.
    fn marks(&self) -> String {
        self.query(&[
            "list-sessions",
            "-F",
            "#{session_name} #{@muxa_workspace_id}",
        ])
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

fn run_muxa(server: &Server, stub_bin: &Path, args: &[&str]) -> std::process::Output {
    let path = format!(
        "{}:{}",
        stub_bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    Command::new(muxa())
        .args(args)
        .env("PATH", path)
        .env("MUXA_TMUX_SOCKET", &server.socket)
        .env_remove("RMUX")
        .env_remove("RMUX_PANE")
        // A socket no daemon answers: these paths mark tmux and report, and
        // must not reach the operator's running muxad.
        .env("MUXA_SOCKET", server.dir.path().join("muxad.sock"))
        .env_remove("RMUX")
        .env_remove("RMUX_PANE")
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .output()
        .expect("run muxa")
}

/// The shape that sent four agents to `callabo-set`: a stale mark on one
/// session while another carries the workspace's own name.
///
/// The mark winning is the intended order — this locks it so a later change
/// has to be deliberate — but it is also why the launch went somewhere nobody
/// asked for, which is what the doctor check below exists to say out loud.
#[test]
fn a_workspace_mark_outranks_a_session_named_after_the_workspace() {
    if !tmux_installed() {
        eprintln!("skipping: tmux is not installed");
        return;
    }
    let server = Server::start();
    server.session("wsmark");
    server.session("wsmark-set");
    let claimant = server.query(&["display-message", "-p", "-t", "wsmark-set", "#{session_id}"]);
    server
        .tmux(&[
            "set-option",
            "-t",
            &claimant,
            "@muxa_workspace_id",
            "wsmark",
        ])
        .expect("plant the stale mark");
    let stub = stub_agent_dir(server.dir.path());

    let out = run_muxa(
        &server,
        &stub,
        &[
            "work",
            "start",
            "probe",
            "--agent",
            "claude",
            "--workspace",
            "wsmark",
            "--cwd",
            "/tmp",
        ],
    );
    assert!(
        out.status.success(),
        "work start failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let windows = server.query(&[
        "list-windows",
        "-a",
        "-F",
        "#{session_name} #{@muxa_work_id}",
    ]);
    let owner = windows
        .lines()
        .find(|line| line.split_whitespace().nth(1) == Some("PROBE"))
        .and_then(|line| line.split_whitespace().next())
        .unwrap_or_else(|| panic!("no PROBE window anywhere; windows:\n{windows}"));
    assert_eq!(
        owner, "wsmark-set",
        "the mark decides, not the name; windows:\n{windows}"
    );
}

/// The reporting half. muxa still goes where the mark says; `doctor` is where
/// the disagreement stops being invisible.
#[test]
fn doctor_reports_a_workspace_claimed_by_another_session() {
    if !tmux_installed() {
        eprintln!("skipping: tmux is not installed");
        return;
    }
    let server = Server::start();
    server.session("wsmark");
    server.session("wsmark-set");
    let claimant = server.query(&["display-message", "-p", "-t", "wsmark-set", "#{session_id}"]);
    server
        .tmux(&[
            "set-option",
            "-t",
            &claimant,
            "@muxa_workspace_id",
            "wsmark",
        ])
        .expect("plant the stale mark");
    let stub = stub_agent_dir(server.dir.path());

    let out = run_muxa(&server, &stub, &["doctor"]);
    let report = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        report.contains("claimed by session wsmark-set"),
        "doctor should name the claiming session; report:\n{report}"
    );
    assert!(
        report.contains("named wsmark"),
        "doctor should name the session the workspace would otherwise resolve to; report:\n{report}"
    );
}

/// Why every write muxa makes to a session is addressed by id.
///
/// tmux resolves a bare name by exact match, then as a pattern, then as a
/// *unique prefix* — and the last of those is silent. So a name that named one
/// session when muxa looked it up can name a different one by the time the
/// write goes out. `=name` is not an escape: tmux 3.4 refuses that spelling
/// for `set-option`, even though `kill-session` accepts it.
#[test]
fn a_bare_session_name_can_resolve_to_a_neighbour() {
    if !tmux_installed() {
        eprintln!("skipping: tmux is not installed");
        return;
    }
    let server = Server::start();
    server.session("wsmark-set");

    // No session is named `wsmark`, and `wsmark-set` is the only one starting
    // with it, so tmux takes the neighbour and reports success.
    server
        .tmux(&["set-option", "-t", "wsmark", "@muxa_workspace_id", "wsmark"])
        .expect("tmux resolves the bare name to the only prefix match");
    assert_eq!(
        server.marks(),
        "wsmark-set wsmark",
        "a bare name landed on the neighbour"
    );

    // A second candidate makes it ambiguous, which tmux refuses — so this is
    // a hazard that shows up only sometimes, which is the worst kind.
    server.session("wsmark-other");
    let refused = server
        .tmux(&["set-option", "-t", "wsmark", "@muxa_probe", "x"])
        .expect_err("two prefix candidates are ambiguous");
    assert!(
        refused.contains("no such session"),
        "expected an ambiguity refusal, got {refused:?}"
    );
}
