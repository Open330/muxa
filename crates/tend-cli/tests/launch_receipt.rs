//! Uses only a disposable -L tmux server, private tendd and isolated HOME/XDG.
#![cfg(target_os = "linux")]
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Scratch {
    root: tempfile::TempDir,
    label: String,
    socket: PathBuf,
    daemon: Option<Child>,
}
impl Scratch {
    fn command(&self, executable: impl AsRef<std::ffi::OsStr>) -> Command {
        let mut command = Command::new(executable);
        for key in [
            "RMUX",
            "RMUX_PANE",
            "TMUX",
            "TMUX_PANE",
            "TEND_SESSION_ID",
            "TEND_SESSION_BACKEND",
            "TEND_ASK",
            "TEND_ASK_ENTRY",
            "TEND_HOSTS",
            "CODEX_THREAD_ID",
        ] {
            command.env_remove(key);
        }
        let root = self.root.path();
        command
            .env("HOME", root)
            .env("SHELL", "/bin/sh")
            .env("TEND_HOST", "tmux")
            .env("TMUX_TMPDIR", root.join("tmux"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("TEND_CONFIG", root.join("config.toml"))
            .env("TEND_SOCKET", root.join("daemon.sock"))
            .env("CLAUDE_CONFIG_DIR", root.join("claude"))
            .env("TEND_TMUX_SOCKET", &self.socket)
            .env("TEND_LAUNCH_ID", "inherited-id-must-be-replaced")
            .env(
                "PATH",
                format!(
                    "{}:{}",
                    root.join("bin").display(),
                    std::env::var("PATH").unwrap_or_default()
                ),
            )
            .env("RUST_LOG", "tend=warn");
        command.current_dir(root);
        command
    }
    fn tmux(&self, args: &[&str]) -> String {
        let out = self
            .command(tend::tmux::tmux_binary())
            .args(["-L", &self.label, "-f", "/dev/null"])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "tmux failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().into()
    }
    fn cli(&self, args: &[&str]) -> serde_json::Value {
        self.cli_with_stderr(args).0
    }
    fn cli_with_stderr(&self, args: &[&str]) -> (serde_json::Value, String) {
        let out = self
            .command(env!("CARGO_BIN_EXE_tend"))
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "CLI {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stdout)));
        (value, String::from_utf8_lossy(&out.stderr).into_owned())
    }
    #[allow(clippy::unnecessary_debug_formatting)] // Path Debug emits quoted TOML strings for isolated temp paths.
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for directory in [
            "tmux",
            "config",
            "data",
            "state",
            "cache",
            "bin",
            "claude/sessions",
        ] {
            std::fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        std::fs::write(root.path().join("config.toml"),format!("[reconciler]\nenabled = false\n[state]\npath = {:?}\n[history]\npath = {:?}\n[activity]\npath = {:?}\n",root.path().join("state/state.json"),root.path().join("state/prompts.ndjson"),root.path().join("state/activity.ndjson"))).unwrap();
        let mut fixture = Self {
            root,
            label: format!("tend-launch-{}", uuid::Uuid::new_v4()),
            socket: PathBuf::from("/dev/null"),
            daemon: None,
        };
        fixture.tmux(&["new-session", "-d", "-s", "seed"]);
        fixture.socket = PathBuf::from(fixture.tmux(&["display-message", "-p", "#{socket_path}"]));
        assert!(fixture.socket.is_absolute() && fixture.socket.starts_with(fixture.root.path()));
        fixture.tmux(&["set-option", "-g", "default-shell", "/bin/sh"]);
        fixture.tmux(&["set-option", "-g", "remain-on-exit", "on"]);
        let provider = fixture.root.path().join("bin/claude");
        std::fs::write(&provider,r"#!/usr/bin/env python3
import json, os, subprocess, sys, time
session = sys.argv[sys.argv.index('--session-id')+1]
pid = os.getpid()
start = open('/proc/%s/stat' % pid).read().rsplit(')',1)[1].split()[19]
root = os.environ['CLAUDE_CONFIG_DIR']
with open('%s/sessions/%s.json' % (root,pid),'w') as f:
    json.dump({'pid':pid,'procStart':start,'sessionId':session},f)
if not os.environ.get('TEST_SKIP_HOOK'): subprocess.run([os.environ['TEST_TEND'],'hook','claude','--event','session_start'], input=json.dumps({'session_id':session,'cwd':os.getcwd()}), text=True, check=True)
while True: time.sleep(1)
").unwrap();
        std::fs::set_permissions(&provider, std::fs::Permissions::from_mode(0o755)).unwrap();
        let daemon = PathBuf::from(env!("CARGO_BIN_EXE_tend")).with_file_name("tendd");
        let log = std::fs::File::create(fixture.root.path().join("daemon.log")).unwrap();
        fixture.daemon = Some(
            fixture
                .command(daemon)
                .stdout(Stdio::null())
                .stderr(log)
                .spawn()
                .expect("build tendd before the integration smoke"),
        );
        let until = Instant::now() + Duration::from_secs(5);
        while !fixture.root.path().join("daemon.sock").exists() {
            assert!(Instant::now() < until, "private daemon did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        // The private server passes this only to its own provider processes.
        fixture.tmux(&[
            "set-environment",
            "-g",
            "TEST_TEND",
            env!("CARGO_BIN_EXE_tend"),
        ]);
        fixture
    }
    fn await_identity(&self, session: &str, expected: &str) -> serde_json::Value {
        let until = Instant::now() + Duration::from_secs(8);
        loop {
            let snapshot = self.cli(&["agents", "--json"]);
            if let Some(row) = snapshot
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["agent_session_id"] == session)
            {
                if row["launch_identity"]["status"] == expected {
                    return row.clone();
                }
            }
            if Instant::now() >= until {
                let panes = self.tmux(&["list-panes", "-a", "-F", "#{pane_id}"]);
                let captures = panes
                    .lines()
                    .map(|pane| self.tmux(&["capture-pane", "-p", "-t", pane]))
                    .collect::<Vec<_>>();
                panic!("no {expected} identity for {session}: {snapshot}; captures={captures:?}; daemon={}", std::fs::read_to_string(self.root.path().join("daemon.log")).unwrap_or_default());
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(mut daemon) = self.daemon.take() {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        let _ = self
            .command(tend::tmux::tmux_binary())
            .args(["-L", &self.label, "-f", "/dev/null", "kill-server"])
            .output();
    }
}

#[test]
#[allow(clippy::too_many_lines, clippy::unnecessary_debug_formatting)] // Three create paths and a malformed-reply smoke, with quoted Python paths.
fn scratch_tmux_launch_receipts_verify_all_create_shapes_without_consumers() {
    let scratch = Scratch::new();
    for placement in ["session", "window", "pane"] {
        let target = scratch.tmux(&["display-message", "-p", "-t", "seed:", "#{pane_id}"]);
        let mut args = vec![
            "agent",
            "start",
            "--host",
            "tmux",
            "--agent",
            "claude",
            "--placement",
            placement,
            "--cwd",
            scratch.root.path().to_str().unwrap(),
            "--json",
        ];
        if placement != "session" {
            args.extend(["--target", &target]);
        }
        let result = scratch.cli(&args);
        let launch = result["launch_id"].as_str().unwrap();
        assert_ne!(launch, "inherited-id-must-be-replaced");
        let archive = scratch
            .root
            .path()
            .join(format!("data/tend/spawns/{launch}.json"));
        let intent: serde_json::Value =
            serde_json::from_slice(&std::fs::read(archive).unwrap()).unwrap();
        let session = intent["expected_session"].as_str().unwrap();
        let row = scratch.await_identity(session, "verified");
        assert_eq!(row["launch_identity"]["launch_id"], launch);
        let pane = result["pane"].as_str().unwrap();
        assert_eq!(
            scratch.tmux(&["show-options", "-p", "-v", "-t", pane, "@tend_launch_id"]),
            launch
        );
        let pid = result["launch_receipt"]["process"]["pid"].as_u64().unwrap();
        let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
        assert!(environ
            .split(|b| *b == 0)
            .any(|entry| entry == format!("TEND_LAUNCH_ID={launch}").as_bytes()));
        let status = scratch.cli(&["status", "--json"]);
        assert!(!status.to_string().contains("launch_identity"));
    }
    // A malformed four-field response still represents one physical create, not a retry.
    let real = tend::tmux::tmux_binary().to_str().unwrap();
    let wrapper = scratch.root.path().join("bin/tmux");
    let count = scratch.root.path().join("creates");
    std::fs::write(
        &wrapper,
        format!(
            r"#!/usr/bin/env python3
import os, subprocess, sys
real={real:?}
args=sys.argv[1:]
if any(a in ('new-window','new-session','split-window') for a in args):
    out=subprocess.run([real]+args, capture_output=True)
    with open({count:?},'a') as f: f.write('create\n')
    sys.stdout.buffer.write(out.stdout.split(b'\t')[0]+b'\n')
    sys.stderr.buffer.write(out.stderr)
    sys.exit(out.returncode)
os.execv(real,[real]+args)
",
            count = count.to_str().unwrap()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let result = scratch.cli(&[
        "agent",
        "start",
        "--host",
        "tmux",
        "--agent",
        "claude",
        "--placement",
        "session",
        "--cwd",
        scratch.root.path().to_str().unwrap(),
        "--json",
    ]);
    let launch = result["launch_id"].as_str().unwrap();
    let archive: serde_json::Value = serde_json::from_slice(
        &std::fs::read(
            scratch
                .root
                .path()
                .join(format!("data/tend/spawns/{launch}.json")),
        )
        .unwrap(),
    )
    .unwrap();
    let row = scratch.await_identity(archive["expected_session"].as_str().unwrap(), "unknown");
    assert_eq!(row["launch_identity"]["reason"], "unsupported_receipt");
    assert_eq!(std::fs::read_to_string(count).unwrap().lines().count(), 1);
}

#[test]
fn post_create_archive_failure_preserves_marks_registration_and_success() {
    archive_failure_still_launches(true);
}

#[test]
fn inaccessible_archive_directory_still_launches() {
    archive_failure_still_launches(false);
}

#[allow(clippy::unnecessary_debug_formatting)] // Quoted Python paths in the private tmux shim.
fn archive_failure_still_launches(corrupt_after_create: bool) {
    let scratch = Scratch::new();
    // Synthetic registrations use a short socket name. Give the daemon's
    // existing canonical-path scope check an alias to this private socket.
    std::os::unix::fs::symlink(&scratch.socket, scratch.root.path().join(&scratch.label)).unwrap();
    // Disable hooks to prove the launcher itself registers the observed provider.
    scratch.tmux(&["set-environment", "-g", "TEST_SKIP_HOOK", "1"]);
    let archives = scratch.root.path().join("data/tend/spawns");
    if !corrupt_after_create {
        std::fs::create_dir_all(archives.parent().unwrap()).unwrap();
        // ENOTDIR is deterministic even when the test runner has root privileges.
        std::fs::write(&archives, "archive directory unavailable").unwrap();
    }
    let wrapper = scratch.root.path().join("bin/tmux");
    let real = tend::tmux::tmux_binary().to_str().unwrap();
    let count = scratch.root.path().join("creates");
    std::fs::write(&wrapper, format!(r"#!/usr/bin/env python3
import os, pathlib, subprocess, sys
args=sys.argv[1:]
out=subprocess.run([{real:?}]+args, capture_output=True)
if any(a in ('new-window','new-session','split-window') for a in args):
    with open({count:?},'a') as f: f.write('create\n')
    if {corrupt}:
        for path in pathlib.Path({archives:?}).glob('*.json'): path.write_text('corrupt receipt archive')
# The stand-in is Python; report Claude only in discovery to exercise registration.
stdout=out.stdout
if 'list-panes' in args and '-F' in args:
    columns=args[args.index('-F')+1].split('\t')
    if '#{{pane_current_command}}' in columns:
        index=columns.index('#{{pane_current_command}}')
        rows=[]
        for line in stdout.splitlines():
            fields=line.split(b'\t')
            fields[index]=b'claude'
            rows.append(b'\t'.join(fields))
        stdout=b'\n'.join(rows)+b'\n'
sys.stdout.buffer.write(stdout)
sys.stderr.buffer.write(out.stderr)
sys.exit(out.returncode)
", count=count.to_str().unwrap(), archives=archives.to_str().unwrap(), corrupt=if corrupt_after_create { "True" } else { "False" })).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (result, stderr) = scratch.cli_with_stderr(&[
        "agent",
        "start",
        "--host",
        "tmux",
        "--agent",
        "claude",
        "--placement",
        "session",
        "--cwd",
        scratch.root.path().to_str().unwrap(),
        "--json",
    ]);
    assert!(
        stderr.contains(if corrupt_after_create {
            "warning: cannot archive launch receipt"
        } else {
            "warning: cannot prepare launch identity"
        }),
        "{stderr}"
    );
    let pane = result["pane"].as_str().unwrap();
    for (option, expected) in [
        ("@tend_launch_id", result["launch_id"].as_str().unwrap()),
        ("@tend_agent", "claude"),
        ("@tend_managed_agent", "1"),
    ] {
        assert_eq!(
            scratch.tmux(&["show-options", "-p", "-v", "-t", pane, option]),
            expected
        );
    }
    let rows = scratch.cli(&["agents", "--json"]);
    assert!(
        rows.as_array().unwrap().iter().any(|row| {
            row["pane"] == pane
                && row["agent_session_id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with("synthetic-"))
        }),
        "launcher did not register the pane: {rows}; stderr={stderr}"
    );
    assert_eq!(std::fs::read_to_string(count).unwrap().lines().count(), 1);
}
