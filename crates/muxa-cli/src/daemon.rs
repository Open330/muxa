//! Explicit `muxa daemon` lifecycle commands.
//!
//! The selected socket follows the ordinary CLI precedence
//! (`--socket`/`MUXA_SOCKET` -> config -> XDG default), so callers never need
//! to know that the fallback happens to contain their uid.

use crate::init::files::{launchd, systemd};
use anyhow::{anyhow, bail, Context, Result};
use clap::Subcommand;
use muxa::ipc::{Client, Hello, RuntimeError};
use std::fs::OpenOptions;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
// Shell startup invokes `start --quiet`; a wedged endpoint must not hold every
// new terminal for the full lifecycle-control timeout.
const START_PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const START_TIMEOUT: Duration = Duration::from_secs(5);
const TRANSITION_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, Subcommand)]
pub enum Action {
    /// Start muxad if the selected socket is not already serving.
    Start {
        /// Suppress the success/already-running message. Intended for shell startup hooks.
        #[arg(long)]
        quiet: bool,
    },
    /// Drain durable writers and stop muxad cleanly.
    Stop,
    /// Re-exec the running daemon, or start it when it is down.
    Restart,
    /// Report whether muxad is serving the selected socket.
    Status,
}

pub async fn run(
    action: Action,
    client: &Client,
    socket: &Path,
    config_path: Option<&Path>,
) -> Result<()> {
    match action {
        Action::Start { quiet } => start(client, socket, config_path, quiet).await,
        Action::Stop => stop(client, socket).await,
        Action::Restart => restart(client, socket, config_path).await,
        Action::Status => status(client, socket).await,
    }
}

async fn start(
    client: &Client,
    socket: &Path,
    config_path: Option<&Path>,
    quiet: bool,
) -> Result<()> {
    if let Ok(hello) = client.hello(START_PROBE_TIMEOUT).await {
        if !quiet {
            println!("muxad already running ({})", describe(socket, &hello));
        }
        return Ok(());
    }

    start_detached(socket, config_path, START_TIMEOUT)?;
    let hello = wait_for_serving(client, START_TIMEOUT)
        .await
        .ok_or_else(|| {
            anyhow!(
                "muxad did not become responsive at {}; check /tmp/muxad.log",
                socket.display()
            )
        })?;
    if !quiet {
        println!("muxad started ({})", describe(socket, &hello));
    }
    Ok(())
}

async fn stop(client: &Client, socket: &Path) -> Result<()> {
    let hello = match client.hello(CONTROL_TIMEOUT).await {
        Ok(hello) => hello,
        Err(RuntimeError::NotConnected(_)) => {
            println!("muxad is not running (socket: {})", socket.display());
            return Ok(());
        }
        Err(error) => return Err(error).context("checking muxad before stop"),
    };
    require_capability(&hello, "stop", "stop")?;

    // A committed stop may close the connection before its acknowledgement
    // reaches us. Socket disappearance below is the authoritative result.
    let request_error = client.stop(CONTROL_TIMEOUT).await.err();
    if wait_for_stopped(client, TRANSITION_TIMEOUT).await {
        println!("muxad stopped (socket: {})", socket.display());
        return Ok(());
    }
    match request_error {
        Some(error) => Err(error).context("requesting muxad stop"),
        None => bail!(
            "muxad accepted stop but is still responding at {}",
            socket.display()
        ),
    }
}

async fn restart(client: &Client, socket: &Path, config_path: Option<&Path>) -> Result<()> {
    let before = match client.hello(CONTROL_TIMEOUT).await {
        Ok(hello) => hello,
        Err(RuntimeError::NotConnected(_)) => {
            start_detached(socket, config_path, START_TIMEOUT)?;
            let hello = wait_for_serving(client, START_TIMEOUT)
                .await
                .ok_or_else(|| {
                    anyhow!(
                        "muxad did not become responsive at {}; check /tmp/muxad.log",
                        socket.display()
                    )
                })?;
            println!("muxad started ({})", describe(socket, &hello));
            return Ok(());
        }
        Err(error) => return Err(error).context("checking muxad before restart"),
    };
    require_capability(&before, "restart", "restart")?;
    let generation = before
        .generation
        .ok_or_else(|| anyhow!("muxad did not report an image generation"))?;

    let request_error = client.restart(CONTROL_TIMEOUT).await.err();
    if let Some(after) = wait_for_new_generation(client, generation, TRANSITION_TIMEOUT).await {
        println!("muxad restarted ({})", describe(socket, &after));
        return Ok(());
    }
    match request_error {
        Some(error) => Err(error).context("requesting muxad restart"),
        None => bail!(
            "muxad accepted restart but did not return at {}",
            socket.display()
        ),
    }
}

async fn status(client: &Client, socket: &Path) -> Result<()> {
    match client.hello(CONTROL_TIMEOUT).await {
        Ok(hello) => {
            println!("muxad running ({})", describe(socket, &hello));
            Ok(())
        }
        Err(RuntimeError::NotConnected(_)) => {
            bail!("muxad is not running (socket: {})", socket.display())
        }
        Err(error) => Err(error).context("checking muxad status"),
    }
}

fn require_capability(hello: &Hello, capability: &str, operation: &str) -> Result<()> {
    if hello.capabilities.iter().any(|value| value == capability) {
        return Ok(());
    }
    bail!("the running muxad does not support `{operation}`; upgrade it before retrying")
}

fn describe(socket: &Path, hello: &Hello) -> String {
    hello.generation.map_or_else(
        || format!("socket: {}", socket.display()),
        |generation| format!("generation: {generation}, socket: {}", socket.display()),
    )
}

/// Start muxad for `socket`: through the installed `LaunchAgent` / systemd unit
/// when that service owns the socket, otherwise as a background process with
/// stdio detached from the caller. Kept crate-visible so `muxa init` and `muxa daemon start` cannot
/// drift into different socket probing or process-launch behaviour.
pub(crate) fn start_detached(
    socket: &Path,
    config_path: Option<&Path>,
    timeout: Duration,
) -> Result<bool> {
    if socket_responding(socket) {
        return Ok(false);
    }

    // A loaded LaunchAgent / enabled systemd unit owns this socket. Spawning a
    // second, unmanaged muxad here wins the bind race at login (the shellrc
    // hook runs before launchd's copy binds), and the KeepAlive service then
    // crash-loops on "socket already in use" for as long as the orphan lives.
    if let Some(manager) = ServiceManager::owning(socket, config_path) {
        manager.start()?;
        let started = Instant::now();
        while started.elapsed() < timeout {
            if socket_responding(socket) {
                return Ok(true);
            }
            std::thread::sleep(POLL_INTERVAL);
        }
        bail!(
            "{} did not bind {} within {}s; {}",
            manager.describe(),
            socket.display(),
            timeout.as_secs(),
            manager.hint()
        );
    }

    let muxad = which::which("muxad").context("finding muxad on PATH")?;
    let log_path = std::env::temp_dir().join("muxad.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;
    let stderr = log.try_clone().context("cloning muxad log handle")?;

    let mut command = Command::new("nohup");
    command
        .arg(muxad)
        .arg("--socket")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .env_remove("MUXA_RESTART_GENERATION");
    if let Some(path) = config_path {
        command.arg("--config").arg(path);
    }
    let mut child = command.spawn().context("spawning detached muxad")?;

    let started = Instant::now();
    while started.elapsed() < timeout {
        if socket_responding(socket) {
            drop(child);
            return Ok(true);
        }
        if let Some(status) = child.try_wait().context("polling detached muxad")? {
            bail!(
                "muxad exited with {status} before binding {}; check {}",
                socket.display(),
                log_path.display()
            );
        }
        std::thread::sleep(POLL_INTERVAL);
    }
    bail!(
        "muxad started but did not bind {} within {}s; check {}",
        socket.display(),
        timeout.as_secs(),
        log_path.display()
    )
}

/// The per-user service `muxa init` installs, when it is the one that would
/// serve `socket`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServiceManager {
    Launchd,
    Systemd,
}

impl ServiceManager {
    fn owning(socket: &Path, config_path: Option<&Path>) -> Option<Self> {
        if !Self::serves(socket, config_path) {
            return None;
        }
        match std::env::consts::OS {
            "macos" if launchd_agent_loaded() => Some(Self::Launchd),
            "linux" if systemd_unit_enabled() => Some(Self::Systemd),
            _ => None,
        }
    }

    /// The service runs muxad with no `--socket`/`--config`, so it binds the
    /// socket named by the default config file, else the XDG default. A
    /// caller aimed anywhere else (tests, a second instance) keeps the
    /// detached spawn.
    fn serves(socket: &Path, config_path: Option<&Path>) -> bool {
        let default_config = muxa::paths::default_config_file();
        if config_path.is_some_and(|path| Some(path) != default_config.as_deref()) {
            return false;
        }
        let service_socket = muxa::config::Config::load_or_default(default_config.as_deref())
            .ok()
            .and_then(|cfg| cfg.socket)
            .unwrap_or_else(muxa::paths::default_socket);
        service_socket == socket
    }

    fn start(self) -> Result<()> {
        // Plain `kickstart` (no `-k`) is a no-op for a job that is already
        // running and skips the throttle wait for one that is not.
        let (program, args): (&str, Vec<String>) = match self {
            Self::Launchd => ("launchctl", vec!["kickstart".into(), launchd_target()]),
            Self::Systemd => (
                "systemctl",
                vec![
                    "--user".into(),
                    "start".into(),
                    systemd::UNIT_FILENAME.into(),
                ],
            ),
        };
        let status = Command::new(program)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .with_context(|| format!("running {program}"))?;
        if !status.success() {
            bail!("{program} {} exited with {status}", args.join(" "));
        }
        Ok(())
    }

    fn describe(self) -> &'static str {
        match self {
            Self::Launchd => "the muxad LaunchAgent",
            Self::Systemd => "muxad.service",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Launchd => {
                "check /tmp/muxad.err and `launchctl print gui/$(id -u)/dev.open330.muxad`"
            }
            Self::Systemd => "check `journalctl --user -u muxad`",
        }
    }
}

fn launchd_target() -> String {
    format!("gui/{}/{}", crate::init::util::uid_string(), launchd::LABEL)
}

fn launchd_agent_loaded() -> bool {
    Command::new("launchctl")
        .args(["print", &launchd_target()])
        .stdin(Stdio::null())
        .output()
        .is_ok_and(|out| out.status.success())
}

fn systemd_unit_enabled() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-enabled", "--quiet", systemd::UNIT_FILENAME])
        .stdin(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub(crate) fn socket_responding(socket: &Path) -> bool {
    std::os::unix::net::UnixStream::connect(socket).is_ok()
}

async fn wait_for_serving(client: &Client, timeout: Duration) -> Option<Hello> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if let Ok(hello) = client.hello(Duration::from_secs(1)).await {
            return Some(hello);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    None
}

async fn wait_for_stopped(client: &Client, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if matches!(
            client.hello(Duration::from_secs(1)).await,
            Err(RuntimeError::NotConnected(_))
        ) {
            return true;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    false
}

async fn wait_for_new_generation(client: &Client, before: u64, timeout: Duration) -> Option<Hello> {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if let Ok(hello) = client.hello(Duration::from_secs(1)).await {
            if hello
                .generation
                .is_some_and(|generation| generation > before)
            {
                return Some(hello);
            }
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
    None
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    #[test]
    fn daemon_subcommands_parse_at_the_expected_surface() {
        for verb in ["start", "stop", "restart", "status"] {
            crate::Args::try_parse_from(["muxa", "daemon", verb])
                .unwrap_or_else(|error| panic!("muxa daemon {verb} did not parse: {error}"));
        }
    }

    #[test]
    fn a_non_service_socket_keeps_the_detached_spawn() {
        let socket = std::path::Path::new("/nonexistent/muxa-test.sock");
        assert_eq!(super::ServiceManager::owning(socket, None), None);
        let config = std::path::Path::new("/nonexistent/config.toml");
        assert!(!super::ServiceManager::serves(
            &muxa::paths::default_socket(),
            Some(config)
        ));
    }

    #[test]
    fn start_quiet_flag_is_scoped_to_start() {
        crate::Args::try_parse_from(["muxa", "daemon", "start", "--quiet"])
            .expect("start accepts --quiet");
        assert!(crate::Args::try_parse_from(["muxa", "daemon", "status", "--quiet"]).is_err());
    }
}
