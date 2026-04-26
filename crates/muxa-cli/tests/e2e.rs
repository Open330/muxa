//! End-to-end test: spawn the real `muxad` binary, drive it via the real
//! `muxa` CLI, assert observable state.
//!
//! Cargo sets `CARGO_BIN_EXE_<name>` for binaries in sibling packages of
//! the same workspace when `cargo test` is invoked.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn bin(name: &str) -> PathBuf {
    // `CARGO_BIN_EXE_*` is the canonical way to locate workspace binaries
    // from integration tests. Falls back to a dev-profile path if not set.
    if let Some(path) = option_env!("CARGO_BIN_EXE_muxad") {
        // `muxa` and `muxad` live in the same target/<profile>/ directory.
        let muxad = PathBuf::from(path);
        return muxad.with_file_name(name);
    }
    let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("target/debug");
    target.join(name)
}

fn wait_for_socket(path: &Path, deadline: Duration) {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!(
        "socket {} did not appear within {:?}",
        path.display(),
        deadline
    );
}

struct Daemon {
    child: Child,
    socket: PathBuf,
}

impl Daemon {
    fn spawn() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        // Leak tempdir intentionally — it's owned by the test's lifetime.
        let socket = dir.keep().join("muxa-e2e.sock");
        let child = Command::new(bin("muxad"))
            .arg("--socket")
            .arg(&socket)
            .env("RUST_LOG", "muxa=warn")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn muxad");
        wait_for_socket(&socket, Duration::from_secs(3));
        Self { child, socket }
    }

    fn cli(&self) -> Command {
        let mut c = Command::new(bin("muxa"));
        c.env("MUXA_SOCKET", &self.socket);
        c
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // Send SIGKILL — the test already asserted what it cared about, and
        // we only need cleanup here.
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

#[test]
fn claude_hook_round_trip() {
    let d = Daemon::spawn();

    // Invoke the hook as Claude Code would.
    let mut hook = d
        .cli()
        .args(["hook", "claude", "--event", "user_prompt_submit"])
        .env("TMUX_PANE", "%99")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn muxa hook");
    hook.stdin
        .as_mut()
        .unwrap()
        .write_all(br#"{"session_id":"sess-e2e","prompt":"hello e2e"}"#)
        .unwrap();
    let status = hook.wait().expect("hook exit");
    assert!(status.success(), "hook command failed");

    // Query via the CLI.
    let out = d.cli().arg("status").output().expect("run status");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("claude_code") && stdout.contains("%99") && stdout.contains("hello e2e"),
        "unexpected status output:\n{stdout}"
    );
}

#[test]
fn claude_statusline_forward_passes_stdin_to_command() {
    // Use `--forward cat` as a trivial passthrough: whatever we write to
    // muxa's stdin should appear verbatim on muxa's stdout. This exercises
    // the full forward path (spawn, feed stdin, relay stdout, propagate
    // exit code) without depending on the daemon or `npx`.
    let d = Daemon::spawn();

    let payload = br#"{"session_id":"sess-fwd","cwd":"/tmp","model":{"display_name":"Opus"}}"#;

    let mut child = d
        .cli()
        .args(["hook", "claude-statusline", "--forward", "cat"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn muxa hook claude-statusline --forward cat");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(payload)
        .expect("write stdin");
    // Drop stdin so `cat` sees EOF.
    drop(child.stdin.take());

    let out = child.wait_with_output().expect("wait forward");
    assert!(
        out.status.success(),
        "forward command failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        out.stdout,
        payload,
        "forwarded stdout should mirror stdin byte-for-byte; got {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

/// Variant of [`Daemon::spawn`] that turns the HTTP dashboard on,
/// binds it to port 0 (OS-picked), and reads stderr until it sees the
/// "dashboard listening" log line so the test gets the actual port.
/// Returns `None` if `curl` is unavailable on PATH — test functions
/// turn that into a `return` so dashboard tests are skipped without
/// failing the suite on minimal CI images.
struct DashboardDaemon {
    daemon: Daemon,
    port: u16,
}

fn curl_available() -> bool {
    Command::new("curl")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn spawn_dashboard(token: Option<&str>) -> Option<DashboardDaemon> {
    if !curl_available() {
        eprintln!("skipping dashboard test — curl not on PATH");
        return None;
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.keep().join("muxa-dash.sock");

    let mut cmd = Command::new(bin("muxad"));
    cmd.arg("--socket")
        .arg(&socket)
        .arg("--dashboard")
        .arg("--dashboard-bind")
        .arg("127.0.0.1:0")
        .env("RUST_LOG", "muxa=info")
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    if let Some(t) = token {
        cmd.arg("--dashboard-token").arg(t);
    }
    let mut child = cmd.spawn().expect("spawn muxad");

    // Read stderr in a thread; signal back the bound port via a channel.
    let stderr = child.stderr.take().expect("stderr piped");
    let (tx, rx) = mpsc::channel::<u16>();
    std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stderr);
        for line in reader.lines().map_while(Result::ok) {
            if line.contains("dashboard listening") {
                if let Some(port) = line
                    .split_whitespace()
                    .find_map(|tok| tok.strip_prefix("addr="))
                    .and_then(|a| a.rsplit(':').next())
                    .and_then(|p| p.parse::<u16>().ok())
                {
                    let _ = tx.send(port);
                    // Keep draining so the child's stderr buffer
                    // never fills.
                }
            }
        }
    });

    let port = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("dashboard never logged its bound port");
    wait_for_socket(&socket, Duration::from_secs(3));

    Some(DashboardDaemon {
        daemon: Daemon { child, socket },
        port,
    })
}

fn curl_status(url: &str, header: Option<&str>) -> u16 {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-s",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "--max-time",
        "2",
    ]);
    if let Some(h) = header {
        cmd.arg("-H").arg(h);
    }
    cmd.arg(url);
    let out = cmd.output().expect("curl");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

fn curl_body(url: &str, header: Option<&str>) -> String {
    let mut cmd = Command::new("curl");
    cmd.args(["-s", "--max-time", "2"]);
    if let Some(h) = header {
        cmd.arg("-H").arg(h);
    }
    cmd.arg(url);
    let out = cmd.output().expect("curl");
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn dashboard_health_endpoint_returns_ok() {
    let Some(dd) = spawn_dashboard(None) else {
        return;
    };
    let url = format!("http://127.0.0.1:{}/api/health", dd.port);
    assert_eq!(curl_status(&url, None), 200);
    let body = curl_body(&url, None);
    assert!(
        body.contains(r#""ok":true"#) && body.contains(r#""protocol":"#),
        "unexpected health body: {body}"
    );
    drop(dd.daemon);
}

#[test]
fn dashboard_token_gates_api() {
    let Some(dd) = spawn_dashboard(Some("e2e-token")) else {
        return;
    };
    let url = format!("http://127.0.0.1:{}/api/health", dd.port);

    // Without auth → 401.
    assert_eq!(curl_status(&url, None), 401);

    // With wrong token → 401.
    assert_eq!(
        curl_status(&url, Some("Authorization: Bearer wrong")),
        401
    );

    // With correct token → 200.
    assert_eq!(
        curl_status(&url, Some("Authorization: Bearer e2e-token")),
        200
    );

    drop(dd.daemon);
}

#[test]
fn dashboard_static_index_is_public() {
    // The HTML/CSS/JS bundle bootstraps the token, so it must be
    // reachable without auth even when /api/* is gated.
    let Some(dd) = spawn_dashboard(Some("e2e-token")) else {
        return;
    };
    let url = format!("http://127.0.0.1:{}/", dd.port);
    assert_eq!(curl_status(&url, None), 200);
    let body = curl_body(&url, None);
    assert!(
        body.contains("muxa dashboard"),
        "expected dashboard HTML; got: {body:.200}"
    );
    drop(dd.daemon);
}

#[test]
fn dashboard_panes_endpoint_is_well_formed_with_no_tmux() {
    let Some(dd) = spawn_dashboard(None) else {
        return;
    };
    let url = format!("http://127.0.0.1:{}/api/panes", dd.port);
    let body = curl_body(&url, None);
    assert!(body.contains(r#""panes":"#), "{body}");
    assert!(body.contains(r#""errors":"#), "{body}");
    assert!(body.contains(r#""fetched_at":"#), "{body}");
    drop(dd.daemon);
}

#[test]
fn rejects_unknown_hook_event() {
    let d = Daemon::spawn();

    let out = d
        .cli()
        .args(["hook", "claude", "--event", "does_not_exist"])
        .stdin(Stdio::piped())
        .output()
        .expect("spawn muxa hook");
    assert!(!out.status.success(), "should fail on unknown event");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("unknown") || err.contains("does_not_exist"),
        "unexpected stderr:\n{err}"
    );
}
