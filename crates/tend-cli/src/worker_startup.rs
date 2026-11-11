//! No model turn is spent preparing a dedicated worker's input composer.
use crate::mux_control::Control;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tend::account_swap::worker_startup::{self, Report};

#[derive(Debug, PartialEq, Eq)]
struct Fence {
    process: tend::collaboration::ProcessFence,
    cwd: PathBuf,
    launch: String,
    kind: tend::event::AgentKind,
}

fn observe(control: &Control, pane: &str) -> Option<Fence> {
    let value = control
        .output(&[
            "display-message",
            "-p",
            "-t",
            pane,
            "#{pane_pid}\t#{pane_current_path}\t#{@tend_launch_id}\t#{@tend_agent}",
        ])
        .ok()?;
    let mut parts = value.trim_end().splitn(4, '\t');
    let pid = parts.next()?.parse().ok()?;
    Some(Fence {
        process: tend::collaboration::ProcessFence::capture(pid)?,
        cwd: tend::paths::canonicalize(Path::new(parts.next()?)).ok()?,
        launch: parts.next()?.to_owned(),
        kind: tend::discovery::classify_command(parts.next()?)?,
    })
}

/// Only the exact new launch in its requested directory may be prepared.
/// Capture joined logical lines: narrow panes wrap both labels and footers.
pub fn prepare(pane: &str, cwd: &Path, launch_id: &str, bypass: bool) -> Result<Report> {
    prepare_inner(pane, cwd, launch_id, bypass, false)
}

/// After a person answers setup, recover readiness without pressing any key
/// or accepting another folder, hook, settings or permission dialog.
pub fn probe(pane: &str, cwd: &Path, launch_id: &str) -> Result<Report> {
    prepare_inner(pane, cwd, launch_id, false, true)
}

fn prepare_inner(
    pane: &str,
    cwd: &Path,
    launch_id: &str,
    bypass: bool,
    passive: bool,
) -> Result<Report> {
    let control = Control::Ambient;
    let fence = observe(&control, pane).context("cannot fence worker startup")?;
    anyhow::ensure!(
        fence.cwd == tend::paths::canonicalize(cwd)? && fence.launch == launch_id,
        "worker pane {pane} no longer belongs to this launch and directory"
    );
    let matches = || observe(&control, pane).as_ref() == Some(&fence);
    control.run(&[
        "set-option",
        "-p",
        "-t",
        pane,
        "@tend_input_readiness",
        "pending",
    ])?;
    let report = worker_startup::prepare(
        fence.kind,
        || {
            if !matches() {
                return None;
            }
            let raw = control
                .output(&["capture-pane", "-p", "-J", "-t", pane])
                .ok()?;
            Some(tend::screen::prepare_capture(&raw, 120))
        },
        &matches,
        |key| {
            if passive || !matches() {
                return false;
            }
            let key = match key {
                "\x1b[B" => "Down",
                "\x1b[A" => "Up",
                "\r" => "Enter",
                "\x1b" => "Escape",
                _ => return false,
            };
            control.run(&["send-keys", "-t", pane, key]).is_ok()
        },
        bypass,
        Duration::from_secs(if passive { 3 } else { 30 }),
    );
    if !matches() {
        anyhow::bail!("worker changed before readiness could be recorded");
    }
    let state = if report.state == worker_startup::State::Ready {
        "ready"
    } else {
        "blocked"
    };
    control.run(&[
        "set-option",
        "-p",
        "-t",
        pane,
        "@tend_input_readiness",
        state,
    ])?;
    Ok(report)
}

#[derive(Debug, Clone, clap::Args)]
pub struct Args {
    /// Exact pane returned by a worker launch.
    #[arg(long)]
    pub pane: String,
    /// Launch receipt id; refuses a different or recycled pane.
    #[arg(long)]
    pub launch_id: String,
    /// Explicit directory authorized for this worker.
    #[arg(long)]
    pub cwd: PathBuf,
    /// Explicit tmux server name/path; never inferred from the caller pane.
    #[arg(long)]
    pub tmux_socket: String,
}

pub fn run(args: Args) -> Result<()> {
    let endpoint = tend::BackendEndpoint {
        host: tend::HostKind::Tmux,
        socket: args.tmux_socket,
    };
    crate::mux_control::with_endpoint(Some(endpoint), || {
        let control = Control::Ambient;
        let marker = control.output(&[
            "show-options",
            "-p",
            "-v",
            "-t",
            &args.pane,
            "@tend_worker_launch",
        ])?;
        anyhow::ensure!(
            marker.trim() == args.launch_id,
            "only the exact dedicated worker launch may be prepared"
        );
        // Recovery never grants bypass permission. A worker explicitly launched
        // with it can finish that dialog in its original launch transaction.
        let report = prepare(&args.pane, &args.cwd, &args.launch_id, false)?;
        println!("{}", serde_json::to_string(&report)?);
        anyhow::ensure!(
            report.state == worker_startup::State::Ready,
            "worker is not input-ready"
        );
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Cleanup(Control);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.0.run(&["kill-server"]);
        }
    }
    #[test]
    #[ignore = "live isolated tmux fixture; no coding agent or model is started"]
    fn trust_to_input_is_prepared_without_an_agent_turn() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = tend::BackendEndpoint {
            host: tend::HostKind::Tmux,
            socket: dir
                .path()
                .join("server.sock")
                .to_string_lossy()
                .into_owned(),
        };
        let control = Control::Endpoint(endpoint.clone());
        let _cleanup = Cleanup(control.clone());
        let script = dir.path().join("startup.py");
        std::fs::write(&script, r"import os,tty,time
os.chdir(os.path.dirname(__file__))
tty.setraw(0)
print('\x1b[2J\x1b[H ❯ No, exit\r\n   Yes, I trust this folder\r\n Enter to confirm · Esc to cancel',end='',flush=True)
data=b''
while not data.endswith(b'\r'): data+=os.read(0,1)
open('keys.hex','w').write(data.hex())
print('\x1b[2J\x1b[HClaude Code\r\n❯ \r\n ? for shortcuts',end='',flush=True)
while True:
    if not os.read(0,1): break
").unwrap();
        let line = tend::shell::Dialect::Posix.command_line(&["python3", script.to_str().unwrap()]);
        let out = control
            .output(&[
                "new-session",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-x",
                "120",
                "-y",
                "40",
                "-s",
                "fixture",
                "-c",
                dir.path().to_str().unwrap(),
                &line,
            ])
            .unwrap();
        let pane = out.trim();
        control
            .run(&[
                "set-option",
                "-p",
                "-t",
                pane,
                "@tend_launch_id",
                "fixture-launch",
            ])
            .unwrap();
        control
            .run(&["set-option", "-p", "-t", pane, "@tend_agent", "claude"])
            .unwrap();
        // A wrong launch id is rejected before any key can be sent.
        crate::mux_control::with_endpoint(Some(endpoint.clone()), || {
            assert!(prepare(pane, dir.path(), "wrong-launch", false).is_err());
        });
        let report = crate::mux_control::with_endpoint(Some(endpoint), || {
            prepare(pane, dir.path(), "fixture-launch", false)
        })
        .unwrap();
        assert_eq!(report.state, worker_startup::State::Ready);
        assert_eq!(report.answered, ["trusted the folder"]);
        assert_eq!(
            control
                .output(&[
                    "show-options",
                    "-p",
                    "-v",
                    "-t",
                    pane,
                    "@tend_input_readiness"
                ])
                .unwrap()
                .trim(),
            "ready"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("keys.hex")).unwrap(),
            "1b5b420d"
        );
    }
}
