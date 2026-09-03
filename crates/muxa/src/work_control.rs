//! Shared launcher for native/dashboard `muxa work …` control surfaces.
//!
//! The CLI crate owns the actual Work reconciliation implementation.  Native
//! clients therefore ask muxad to launch the exact bundled/installed `muxa`
//! binary instead of growing a second pipeline implementation in the daemon.
//!
//! Two operations share this module:
//!
//! - `work_up` runs `muxa work up --json --yes …` as a bounded asynchronous
//!   operation, on this host or on a control-mode Fleet host.
//! - `work_command` runs one read/edit subcommand (`muxa work
//!   options|preset|pipeline|route …`) synchronously and returns its exit
//!   code, stdout, and stderr.
//!
//! Remote execution goes through a [`RemoteWorkRunner`]. muxad implements it
//! over the Fleet manager, which prefers the SSH relay's `work_command`
//! operation and falls back to a one-shot OpenSSH command
//! ([`ssh_work_command_argv`]) for hosts whose `muxa` predates that operation.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::fleet::{drain_bounded, HostAccessMode, LOCAL_HOST_ALIAS};

pub const WORK_UP_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_WORK_UP_INPUT_BYTES: usize = 64 * 1024;
const MAX_WORK_UP_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

/// Wall-clock budget for one `work_command` child.
pub const WORK_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
/// Combined stdout + stderr a `work_command` child may produce.
pub const MAX_WORK_COMMAND_OUTPUT_BYTES: usize = 1024 * 1024;
/// Combined argv + stdin bytes one `work_command` may carry.
pub const MAX_WORK_COMMAND_INPUT_BYTES: usize = 1024 * 1024;
/// `muxa work` subcommands the IPC `work_command` kind may run.
pub const WORK_COMMAND_SUBCOMMANDS: &[&str] = &["options", "preset", "pipeline", "route"];
/// Subcommands that only read configuration. Observe-mode hosts run these
/// and nothing else.
const WORK_COMMAND_READ_ONLY_SUBCOMMANDS: &[&str] = &["options"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkUpRequest {
    /// Stable Muxa Work id.
    pub work: String,
    /// Optional external issue reference, kept separate from the Work id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pipeline: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// Working directory for the launched agents. For a remote `host` this is
    /// a path on that host and is passed through untouched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default)]
    pub no_ticket: bool,
    #[serde(default)]
    pub dry_run: bool,
    /// Fleet host alias that runs the CLI. Absent or `"local"` runs it next
    /// to the daemon that accepted the request; any other alias must be a
    /// configured host in `mode = "control"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkUpError {
    #[error("invalid work request: {0}")]
    Invalid(String),
    #[error("spawning muxa work up: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("muxa work up exceeded {}s", WORK_UP_TIMEOUT.as_secs())]
    Timeout,
    #[error("muxa work up failed: {0}")]
    Failed(String),
    #[error("muxa work up returned too much output")]
    OutputTooLarge,
    #[error("muxa work up answered with unparseable JSON: {0}")]
    InvalidJson(#[source] serde_json::Error),
}

impl From<WorkCommandError> for WorkUpError {
    fn from(error: WorkCommandError) -> Self {
        match error {
            WorkCommandError::Spawn(error) => Self::Spawn(error),
            WorkCommandError::Timeout { .. } => Self::Timeout,
            WorkCommandError::OutputTooLarge { .. } => Self::OutputTooLarge,
            WorkCommandError::Invalid(message)
            | WorkCommandError::Forbidden(message)
            | WorkCommandError::Failed(message) => Self::Failed(message),
        }
    }
}

impl WorkUpRequest {
    pub fn validate(&self) -> Result<(), WorkUpError> {
        if self.work.trim().is_empty() {
            return Err(WorkUpError::Invalid("work id is empty".into()));
        }
        if self.no_ticket
            && self
                .external
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
        {
            return Err(WorkUpError::Invalid(
                "external cannot be combined with no_ticket".into(),
            ));
        }
        if self.cwd.as_deref().is_some_and(|path| !path.is_absolute()) {
            return Err(WorkUpError::Invalid("cwd must be an absolute path".into()));
        }
        if self
            .host
            .as_deref()
            .is_some_and(|host| host.trim().is_empty())
        {
            return Err(WorkUpError::Invalid("host alias is empty".into()));
        }
        let input_bytes = self.work.len()
            + optional_len(self.external.as_deref())
            + optional_len(self.pipeline.as_deref())
            + optional_len(self.workspace.as_deref())
            + self.cwd.as_deref().map_or(0, |path| path.as_os_str().len())
            + optional_len(self.skill.as_deref())
            + optional_len(self.body.as_deref())
            + optional_len(self.context.as_deref())
            + optional_len(self.host.as_deref());
        if input_bytes > MAX_WORK_UP_INPUT_BYTES {
            return Err(WorkUpError::Invalid(format!(
                "request exceeds {MAX_WORK_UP_INPUT_BYTES} bytes"
            )));
        }
        Ok(())
    }

    /// The Fleet alias that must run this request, or `None` when it runs on
    /// the daemon's own host.
    #[must_use]
    pub fn remote_host(&self) -> Option<&str> {
        remote_host_alias(self.host.as_deref())
    }

    #[must_use]
    pub fn arguments(&self) -> Vec<String> {
        let mut args = vec![
            "work".to_string(),
            "up".to_string(),
            self.work.trim().to_string(),
            "--json".to_string(),
            // The native/dashboard confirmation is the authority boundary;
            // never leave a subprocess waiting on an invisible stdin prompt.
            "--yes".to_string(),
        ];
        push_option(&mut args, "--pipeline", self.pipeline.as_deref());
        push_option(&mut args, "--workspace", self.workspace.as_deref());
        push_option(
            &mut args,
            "--cwd",
            self.cwd.as_ref().and_then(|p| p.to_str()),
        );
        push_option(&mut args, "--external", self.external.as_deref());
        push_option(&mut args, "--skill", self.skill.as_deref());
        push_option(&mut args, "--body", self.body.as_deref());
        push_option(&mut args, "--context", self.context.as_deref());
        if self.no_ticket
            || self
                .external
                .as_deref()
                .is_none_or(|value| value.trim().is_empty())
        {
            args.push("--no-ticket".into());
        }
        if self.dry_run {
            args.push("--dry-run".into());
        }
        args
    }
}

/// Normalize an optional host field: absent, blank, or `"local"` means the
/// daemon's own host.
#[must_use]
pub fn remote_host_alias(host: Option<&str>) -> Option<&str> {
    host.map(str::trim)
        .filter(|host| !host.is_empty() && *host != LOCAL_HOST_ALIAS)
}

/// Run the canonical CLI implementation. `socket_path` pins nested CLI IPC
/// back to the daemon that accepted the operation, including non-default test
/// and app sockets.
pub async fn execute_work_up(
    input: &WorkUpRequest,
    socket_path: Option<&Path>,
) -> Result<Value, WorkUpError> {
    input.validate()?;
    let output = execute_work_command(
        &resolve_muxa_binary(),
        &input.arguments(),
        None,
        socket_path,
        WorkCommandLimits::WORK_UP,
    )
    .await?;
    work_up_result(&output)
}

/// Interpret a finished `muxa work up --json` child, wherever it ran.
pub fn work_up_result(output: &WorkCommandOutput) -> Result<Value, WorkUpError> {
    if output.exit_code != 0 {
        let detail = output
            .stderr
            .trim()
            .lines()
            .next_back()
            .unwrap_or("no stderr");
        return Err(WorkUpError::Failed(detail.to_string()));
    }
    serde_json::from_str(output.stdout.trim()).map_err(WorkUpError::InvalidJson)
}

/// The `muxa` binary the daemon launches: `$MUXA_CLI`, else the sibling of
/// the running executable, else `muxa` on `PATH`.
#[must_use]
pub fn resolve_muxa_binary() -> PathBuf {
    if let Some(path) = std::env::var_os("MUXA_CLI").filter(|value| !value.is_empty()) {
        return PathBuf::from(path);
    }
    if let Ok(current) = std::env::current_exe() {
        if let Some(parent) = current.parent() {
            let sibling = parent.join("muxa");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    PathBuf::from("muxa")
}

fn optional_len(value: Option<&str>) -> usize {
    value.map_or(0, str::len)
}

fn push_option(args: &mut Vec<String>, flag: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        args.push(flag.to_string());
        args.push(value.to_string());
    }
}

// ---------------------------------------------------------------------------
// Bounded `muxa work <subcommand>` execution.
// ---------------------------------------------------------------------------

/// Which surface an argv arrived on. The surfaces share one allowlist except
/// that only the Fleet transport carries `work up`, because a remote
/// `work_up` operation travels as a `work_command` relay frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkCommandSurface {
    /// The IPC `work_command` kind. `work up` has its own asynchronous
    /// operation and is refused here.
    Ipc,
    /// The Fleet relay / OpenSSH transport between two muxa installations.
    Relay,
}

/// Time and output budget for one child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkCommandLimits {
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

impl WorkCommandLimits {
    /// Budget for `work options|preset|pipeline|route`.
    pub const COMMAND: Self = Self {
        timeout: WORK_COMMAND_TIMEOUT,
        max_output_bytes: MAX_WORK_COMMAND_OUTPUT_BYTES,
    };
    /// Budget for `work up`, which may wait on a ticket tracker and agent
    /// launches.
    pub const WORK_UP: Self = Self {
        timeout: WORK_UP_TIMEOUT,
        max_output_bytes: MAX_WORK_UP_OUTPUT_BYTES,
    };

    /// The budget a validated argv is entitled to.
    #[must_use]
    pub fn for_args(args: &[String]) -> Self {
        if is_work_up(args) {
            Self::WORK_UP
        } else {
            Self::COMMAND
        }
    }
}

/// Exit status and captured streams of one finished child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkCommandOutput {
    /// The child's exit code, or `-1` when a signal ended it.
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkCommandError {
    #[error("invalid work command: {0}")]
    Invalid(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("spawning muxa: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("muxa {command} exceeded {seconds}s")]
    Timeout { command: String, seconds: u64 },
    #[error("muxa {command} returned more than {limit} bytes of output")]
    OutputTooLarge { command: String, limit: usize },
    /// Transport failure between the daemon and the host that ran the command.
    #[error("{0}")]
    Failed(String),
}

fn is_work_up(args: &[String]) -> bool {
    args.first().is_some_and(|command| command == "work")
        && args.get(1).is_some_and(|subcommand| subcommand == "up")
}

fn is_daemon_flag(arg: &str) -> bool {
    arg == "--socket"
        || arg == "--config"
        || arg.starts_with("--socket=")
        || arg.starts_with("--config=")
}

/// Refuse anything but an allowlisted `work <subcommand> …` argv before a
/// process exists. Flags before `work`, other top-level commands, and the
/// daemon-owned `--socket`/`--config` flags are all rejected; shell
/// metacharacters need no special handling because the argv is never
/// re-parsed by a shell on this host.
pub fn validate_work_command(
    args: &[String],
    stdin: Option<&str>,
    surface: WorkCommandSurface,
) -> Result<(), WorkCommandError> {
    let [command, subcommand, rest @ ..] = args else {
        return Err(WorkCommandError::Invalid(
            "expected `work <subcommand> …`".into(),
        ));
    };
    if command != "work" {
        return Err(WorkCommandError::Invalid(format!(
            "only `muxa work …` may run here, not `{command}`"
        )));
    }
    let allowed = WORK_COMMAND_SUBCOMMANDS.contains(&subcommand.as_str())
        || (surface == WorkCommandSurface::Relay && subcommand == "up");
    if !allowed {
        let hint = if subcommand == "up" {
            "; use the `work_up` operation instead".to_string()
        } else {
            format!("; allowed: {}", WORK_COMMAND_SUBCOMMANDS.join(", "))
        };
        return Err(WorkCommandError::Invalid(format!(
            "`work {subcommand}` is not allowed{hint}"
        )));
    }
    if let Some(flag) = rest.iter().find(|arg| is_daemon_flag(arg)) {
        return Err(WorkCommandError::Invalid(format!(
            "`{flag}` is chosen by the daemon and cannot be passed"
        )));
    }
    let input_bytes = args.iter().map(String::len).sum::<usize>() + optional_len(stdin);
    if input_bytes > MAX_WORK_COMMAND_INPUT_BYTES {
        return Err(WorkCommandError::Invalid(format!(
            "request exceeds {MAX_WORK_COMMAND_INPUT_BYTES} bytes"
        )));
    }
    Ok(())
}

/// Per-host authorization. Control hosts run every allowlisted subcommand;
/// observe-only hosts run read-only ones (`work options`).
pub fn authorize_work_command(
    host: &str,
    mode: HostAccessMode,
    args: &[String],
) -> Result<(), WorkCommandError> {
    if mode == HostAccessMode::Control {
        return Ok(());
    }
    let read_only = !is_work_up(args)
        && args.get(1).is_some_and(|subcommand| {
            WORK_COMMAND_READ_ONLY_SUBCOMMANDS.contains(&subcommand.as_str())
        });
    if read_only {
        return Ok(());
    }
    Err(WorkCommandError::Forbidden(format!(
        "host '{host}' is observe-only; set mode = \"control\" in [fleet.hosts.{host}] to run `{}`",
        args.iter().take(2).cloned().collect::<Vec<_>>().join(" ")
    )))
}

/// `work up` needs a control-mode host.
pub fn authorize_work_up(host: &str, mode: HostAccessMode) -> Result<(), WorkCommandError> {
    authorize_work_command(host, mode, &["work".to_string(), "up".to_string()])
}

/// Run `binary args…` with `stdin` written to the child, `MUXA_SOCKET` pinned
/// to `socket_path`, and both streams captured within `limits`. Output past
/// the cap is drained so the child never blocks on a full pipe, then the whole
/// result is rejected. A child still running at the deadline is killed.
pub async fn execute_work_command(
    binary: &Path,
    args: &[String],
    stdin: Option<&str>,
    socket_path: Option<&Path>,
    limits: WorkCommandLimits,
) -> Result<WorkCommandOutput, WorkCommandError> {
    let mut command = tokio::process::Command::new(binary);
    command.args(args);
    if let Some(socket_path) = socket_path {
        command.env("MUXA_SOCKET", socket_path);
    }
    run_bounded(command, args, stdin, limits).await
}

async fn run_bounded(
    mut command: tokio::process::Command,
    args: &[String],
    stdin: Option<&str>,
    limits: WorkCommandLimits,
) -> Result<WorkCommandOutput, WorkCommandError> {
    let label = args.iter().take(2).cloned().collect::<Vec<_>>().join(" ");
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(WorkCommandError::Spawn)?;
    let stdin_pipe = child.stdin.take();
    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| WorkCommandError::Spawn(std::io::Error::other("stdout is unavailable")))?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| WorkCommandError::Spawn(std::io::Error::other("stderr is unavailable")))?;
    // One byte past the cap is enough to prove the child exceeded it; the
    // rest is discarded so the child can still exit on its own.
    let read_limit = limits.max_output_bytes.saturating_add(1);
    let run = async {
        let write = async {
            if let (Some(mut pipe), Some(input)) = (stdin_pipe, stdin) {
                if let Err(error) = pipe.write_all(input.as_bytes()).await {
                    // A child that exits without reading stdin is not an error
                    // of ours; its exit code and stderr tell the story.
                    if error.kind() != std::io::ErrorKind::BrokenPipe {
                        return Err(error);
                    }
                }
                let _ = pipe.shutdown().await;
            }
            Ok(())
        };
        let (written, stdout, stderr, status) = tokio::join!(
            write,
            drain_bounded(&mut stdout_pipe, read_limit),
            drain_bounded(&mut stderr_pipe, read_limit),
            child.wait(),
        );
        written?;
        Ok::<_, std::io::Error>((status?, stdout?, stderr?))
    };
    let (status, stdout, stderr) = match tokio::time::timeout(limits.timeout, run).await {
        Ok(Ok(finished)) => finished,
        Ok(Err(error)) => return Err(WorkCommandError::Spawn(error)),
        Err(_) => {
            let _ = child.kill().await;
            return Err(WorkCommandError::Timeout {
                command: label,
                seconds: limits.timeout.as_secs(),
            });
        }
    };
    if stdout.len().saturating_add(stderr.len()) > limits.max_output_bytes {
        return Err(WorkCommandError::OutputTooLarge {
            command: label,
            limit: limits.max_output_bytes,
        });
    }
    Ok(WorkCommandOutput {
        exit_code: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

// ---------------------------------------------------------------------------
// Remote execution.
// ---------------------------------------------------------------------------

/// Future returned by [`RemoteWorkRunner::run`].
pub type RemoteWorkFuture<'a> =
    Pin<Box<dyn Future<Output = Result<WorkCommandOutput, WorkCommandError>> + Send + 'a>>;
/// Future returned by [`RemoteWorkRunner::host_mode`].
pub type HostModeFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HostAccessMode, WorkCommandError>> + Send + 'a>>;

/// Runs an already validated `muxa work …` argv on a Fleet host. muxad backs
/// it with the Fleet manager; tests inject fakes.
pub trait RemoteWorkRunner: Send + Sync {
    /// The configured access mode of `host`, or an error naming an unknown
    /// alias.
    fn host_mode<'a>(&'a self, host: &'a str) -> HostModeFuture<'a>;
    /// Run `args` on `host` with `stdin` forwarded, within `limits`.
    fn run<'a>(
        &'a self,
        host: &'a str,
        args: Vec<String>,
        stdin: Option<String>,
        limits: WorkCommandLimits,
    ) -> RemoteWorkFuture<'a>;
}

/// One-shot OpenSSH target for a host whose relay predates `work_command`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshWorkTarget {
    /// OpenSSH destination or `Host` alias (`fleet.hosts.<alias>.ssh`).
    pub ssh: String,
    /// Remote `muxa` binary (`fleet.hosts.<alias>.muxa_path`), already
    /// validated as a shell-token-safe word by config loading.
    pub muxa_path: String,
    /// Remote daemon socket (`fleet.hosts.<alias>.remote_socket`), already
    /// validated as a shell-token-safe word by config loading.
    pub remote_socket: Option<PathBuf>,
    pub connect_timeout: Option<Duration>,
}

/// Quote one word for a POSIX shell using single quotes.
#[must_use]
pub fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// `ssh -T -o BatchMode=yes -o ClearAllForwardings=yes [-o ConnectTimeout=N]
/// -- <ssh> <muxa_path> [--socket <remote_socket>] <args…>` as an argv.
///
/// OpenSSH joins everything after the destination with spaces and hands it to
/// the remote login shell, so each `work …` argument is single-quoted. The
/// binary and socket words are passed exactly as the persistent relay passes
/// them (config validation already restricts them to shell-safe characters,
/// and quoting would defeat a `~` in `remote_socket`).
#[must_use]
pub fn ssh_work_command_argv(target: &SshWorkTarget, args: &[String]) -> Vec<String> {
    let mut command_line = vec![
        "ssh".to_string(),
        "-T".to_string(),
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ClearAllForwardings=yes".to_string(),
    ];
    if let Some(timeout) = target.connect_timeout {
        command_line.push("-o".to_string());
        command_line.push(format!("ConnectTimeout={}", timeout.as_secs().max(1)));
    }
    command_line.push("--".to_string());
    command_line.push(target.ssh.clone());
    command_line.push(target.muxa_path.clone());
    if let Some(socket) = &target.remote_socket {
        command_line.push("--socket".to_string());
        command_line.push(socket.to_string_lossy().into_owned());
    }
    command_line.extend(args.iter().map(|arg| shell_quote(arg)));
    command_line
}

/// Run `args` on `target` over a one-shot OpenSSH command with `stdin`
/// forwarded. OpenSSH's own failures (exit 255) become a transport error;
/// any other exit code is the remote `muxa` speaking.
pub async fn execute_ssh_work_command(
    target: &SshWorkTarget,
    args: &[String],
    stdin: Option<&str>,
    limits: WorkCommandLimits,
) -> Result<WorkCommandOutput, WorkCommandError> {
    let command_line = ssh_work_command_argv(target, args);
    let mut command = tokio::process::Command::new(&command_line[0]);
    command.args(&command_line[1..]);
    let output = run_bounded(command, args, stdin, limits).await?;
    if output.exit_code == 255 {
        let detail = output
            .stderr
            .trim()
            .lines()
            .next_back()
            .unwrap_or("no diagnostic");
        return Err(WorkCommandError::Failed(format!(
            "OpenSSH to '{}' failed: {detail}",
            target.ssh
        )));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| (*part).to_string()).collect()
    }

    fn work_request(host: Option<&str>) -> WorkUpRequest {
        WorkUpRequest {
            work: " native-app ".into(),
            external: None,
            pipeline: Some("implement-review".into()),
            workspace: Some("muxa".into()),
            cwd: Some(PathBuf::from("/tmp/muxa")),
            skill: None,
            body: Some("Build it".into()),
            context: None,
            no_ticket: false,
            dry_run: false,
            host: host.map(str::to_string),
        }
    }

    #[test]
    fn native_work_request_builds_non_interactive_canonical_cli_arguments() {
        let input = work_request(None);
        input.validate().unwrap();
        assert_eq!(
            input.arguments(),
            vec![
                "work",
                "up",
                "native-app",
                "--json",
                "--yes",
                "--pipeline",
                "implement-review",
                "--workspace",
                "muxa",
                "--cwd",
                "/tmp/muxa",
                "--body",
                "Build it",
                "--no-ticket",
            ]
        );
    }

    #[test]
    fn native_work_request_rejects_conflicting_external_modes() {
        let input = WorkUpRequest {
            work: "W-1".into(),
            external: Some("CAL-1".into()),
            pipeline: None,
            workspace: None,
            cwd: None,
            skill: None,
            body: None,
            context: None,
            no_ticket: true,
            dry_run: false,
            host: None,
        };
        assert!(matches!(input.validate(), Err(WorkUpError::Invalid(_))));
    }

    #[test]
    fn host_field_is_optional_and_local_means_no_remote() {
        let input = work_request(None);
        assert_eq!(input.remote_host(), None);
        assert!(!serde_json::to_string(&input).unwrap().contains("host"));
        assert_eq!(work_request(Some("local")).remote_host(), None);
        assert_eq!(work_request(Some(" dev ")).remote_host(), Some("dev"));
        assert!(matches!(
            work_request(Some("  ")).validate(),
            Err(WorkUpError::Invalid(_))
        ));
        // The host never leaks into the CLI argv; the transport picks the host.
        assert_eq!(
            work_request(Some("dev")).arguments(),
            work_request(None).arguments()
        );
        let decoded: WorkUpRequest =
            serde_json::from_str(r#"{"work":"W-1","host":"dev"}"#).unwrap();
        assert_eq!(decoded.remote_host(), Some("dev"));
    }

    #[test]
    fn allowlist_accepts_only_work_read_edit_subcommands() {
        for subcommand in WORK_COMMAND_SUBCOMMANDS {
            validate_work_command(
                &argv(&["work", subcommand, "--json"]),
                None,
                WorkCommandSurface::Ipc,
            )
            .unwrap();
        }
        validate_work_command(
            &argv(&["work", "pipeline", "set", "--from-json", "-"]),
            Some("{}"),
            WorkCommandSurface::Ipc,
        )
        .unwrap();
        // Arguments after the subcommand are an argv, never a shell string.
        validate_work_command(
            &argv(&["work", "route", "set", "$(rm -rf /)", "; echo"]),
            None,
            WorkCommandSurface::Ipc,
        )
        .unwrap();
    }

    #[test]
    fn allowlist_rejects_everything_else_before_anything_runs() {
        let rejected: &[&[&str]] = &[
            &[],
            &["work"],
            &["work", "up", "W-1", "--json"],
            &["work", "start"],
            &["work", "close"],
            &["fleet", "options"],
            &["--socket", "/tmp/x.sock", "work", "options"],
            &["--json", "work", "options"],
            &["work", "options", "--socket", "/tmp/x.sock"],
            &["work", "options", "--config=/tmp/other.toml"],
            &["Work", "options"],
        ];
        for args in rejected {
            let error = validate_work_command(&argv(args), None, WorkCommandSurface::Ipc)
                .expect_err(&format!("{args:?} must be rejected"));
            assert!(matches!(error, WorkCommandError::Invalid(_)), "{args:?}");
        }
        let error =
            validate_work_command(&argv(&["work", "up", "W-1"]), None, WorkCommandSurface::Ipc)
                .unwrap_err();
        assert!(error.to_string().contains("work_up"));
        let oversized = "x".repeat(MAX_WORK_COMMAND_INPUT_BYTES);
        assert!(validate_work_command(
            &argv(&["work", "pipeline", "set", "--from-json", "-"]),
            Some(&oversized),
            WorkCommandSurface::Ipc,
        )
        .is_err());
    }

    #[test]
    fn relay_surface_also_carries_work_up() {
        validate_work_command(
            &argv(&["work", "up", "W-1", "--json", "--yes"]),
            None,
            WorkCommandSurface::Relay,
        )
        .unwrap();
        assert!(validate_work_command(
            &argv(&["work", "start", "W-1"]),
            None,
            WorkCommandSurface::Relay,
        )
        .is_err());
        assert_eq!(
            WorkCommandLimits::for_args(&argv(&["work", "up", "W-1"])),
            WorkCommandLimits::WORK_UP
        );
        assert_eq!(
            WorkCommandLimits::for_args(&argv(&["work", "options"])),
            WorkCommandLimits::COMMAND
        );
    }

    #[test]
    fn observe_hosts_may_only_read_options() {
        authorize_work_command("dev", HostAccessMode::Observe, &argv(&["work", "options"]))
            .unwrap();
        for subcommand in ["preset", "pipeline", "route", "up"] {
            let error = authorize_work_command(
                "dev",
                HostAccessMode::Observe,
                &argv(&["work", subcommand, "set"]),
            )
            .unwrap_err();
            assert!(matches!(error, WorkCommandError::Forbidden(_)));
            assert!(error.to_string().contains("observe-only"));
            assert!(error.to_string().contains("[fleet.hosts.dev]"));
        }
        for subcommand in ["options", "preset", "pipeline", "route", "up"] {
            authorize_work_command("dev", HostAccessMode::Control, &argv(&["work", subcommand]))
                .unwrap();
        }
        assert!(authorize_work_up("dev", HostAccessMode::Observe).is_err());
        authorize_work_up("dev", HostAccessMode::Control).unwrap();
    }

    #[test]
    fn ssh_fallback_argv_quotes_every_work_argument() {
        let target = SshWorkTarget {
            ssh: "dev.example".into(),
            muxa_path: "/opt/muxa/bin/muxa".into(),
            remote_socket: Some(PathBuf::from("/run/user/1000/muxa.sock")),
            connect_timeout: Some(Duration::from_secs(10)),
        };
        let args = argv(&[
            "work",
            "route",
            "set",
            "has space",
            "it's",
            "$HOME",
            "--json",
        ]);
        assert_eq!(
            ssh_work_command_argv(&target, &args),
            vec![
                "ssh",
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "ConnectTimeout=10",
                "--",
                "dev.example",
                "/opt/muxa/bin/muxa",
                "--socket",
                "/run/user/1000/muxa.sock",
                "'work'",
                "'route'",
                "'set'",
                "'has space'",
                "'it'\\''s'",
                "'$HOME'",
                "'--json'",
            ]
        );
        let bare = SshWorkTarget {
            ssh: "dev".into(),
            muxa_path: "muxa".into(),
            remote_socket: None,
            connect_timeout: None,
        };
        assert_eq!(
            ssh_work_command_argv(&bare, &argv(&["work", "options", "--json"])),
            vec![
                "ssh",
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ClearAllForwardings=yes",
                "--",
                "dev",
                "muxa",
                "'work'",
                "'options'",
                "'--json'",
            ]
        );
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("a'b'c"), "'a'\\''b'\\''c'");
    }

    #[test]
    fn work_up_result_reads_json_or_last_stderr_line() {
        let ok = WorkCommandOutput {
            exit_code: 0,
            stdout: "{\"work\":\"W-1\"}\n".into(),
            stderr: String::new(),
        };
        assert_eq!(work_up_result(&ok).unwrap()["work"], "W-1");
        let failed = WorkCommandOutput {
            exit_code: 2,
            stdout: String::new(),
            stderr: "warning: first\nerror: no pipeline matched\n".into(),
        };
        match work_up_result(&failed).unwrap_err() {
            WorkUpError::Failed(detail) => assert_eq!(detail, "error: no pipeline matched"),
            other => panic!("unexpected error {other:?}"),
        }
        let garbage = WorkCommandOutput {
            exit_code: 0,
            stdout: "not json".into(),
            stderr: String::new(),
        };
        assert!(matches!(
            work_up_result(&garbage),
            Err(WorkUpError::InvalidJson(_))
        ));
    }

    #[cfg(unix)]
    mod child {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn script(dir: &Path, body: &str) -> PathBuf {
            let path = dir.join("fake-muxa");
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            path
        }

        #[tokio::test]
        async fn child_receives_argv_stdin_and_socket_and_reports_exit_code() {
            let dir = tempfile::tempdir().unwrap();
            let binary = script(
                dir.path(),
                "printf 'args=%s socket=%s\\n' \"$*\" \"$MUXA_SOCKET\"; cat; echo oops >&2; exit 3",
            );
            let output = execute_work_command(
                &binary,
                &argv(&["work", "pipeline", "set", "--from-json", "-"]),
                Some("{\"name\":\"solo\"}"),
                Some(Path::new("/tmp/muxa-test.sock")),
                WorkCommandLimits::COMMAND,
            )
            .await
            .unwrap();
            assert_eq!(output.exit_code, 3);
            assert_eq!(
                output.stdout,
                "args=work pipeline set --from-json - socket=/tmp/muxa-test.sock\n{\"name\":\"solo\"}"
            );
            assert_eq!(output.stderr, "oops\n");
        }

        #[tokio::test]
        async fn child_without_stdin_sees_eof_immediately() {
            let dir = tempfile::tempdir().unwrap();
            let binary = script(dir.path(), "cat; echo done");
            let output = execute_work_command(
                &binary,
                &argv(&["work", "options"]),
                None,
                None,
                WorkCommandLimits::COMMAND,
            )
            .await
            .unwrap();
            assert_eq!(output.exit_code, 0);
            assert_eq!(output.stdout, "done\n");
        }

        #[tokio::test]
        async fn child_past_the_deadline_is_killed() {
            let dir = tempfile::tempdir().unwrap();
            let binary = script(dir.path(), "sleep 30");
            let error = execute_work_command(
                &binary,
                &argv(&["work", "options"]),
                None,
                None,
                WorkCommandLimits {
                    timeout: Duration::from_millis(200),
                    max_output_bytes: 1024,
                },
            )
            .await
            .unwrap_err();
            assert!(matches!(error, WorkCommandError::Timeout { .. }), "{error}");
            assert!(error.to_string().starts_with("muxa work options exceeded"));
        }

        #[tokio::test]
        async fn child_output_past_the_cap_is_rejected_without_blocking() {
            let dir = tempfile::tempdir().unwrap();
            let binary = script(
                dir.path(),
                "i=0; while [ $i -lt 400 ]; do echo 0123456789; i=$((i+1)); done",
            );
            let error = execute_work_command(
                &binary,
                &argv(&["work", "options"]),
                None,
                None,
                WorkCommandLimits {
                    timeout: Duration::from_secs(10),
                    max_output_bytes: 1024,
                },
            )
            .await
            .unwrap_err();
            assert!(
                matches!(error, WorkCommandError::OutputTooLarge { limit: 1024, .. }),
                "{error}"
            );
        }

        #[tokio::test]
        async fn missing_binary_is_a_spawn_error() {
            let error = execute_work_command(
                Path::new("/nonexistent/muxa-binary"),
                &argv(&["work", "options"]),
                None,
                None,
                WorkCommandLimits::COMMAND,
            )
            .await
            .unwrap_err();
            assert!(matches!(error, WorkCommandError::Spawn(_)));
        }
    }
}
