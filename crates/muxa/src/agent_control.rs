//! Native `agent_start`: start one allowlisted coding agent from a client
//! that is not itself inside tmux (Muxa.app, a dashboard).
//!
//! Like [`crate::work_control`], the daemon does not grow a second launcher.
//! `muxa agent start` in the CLI crate owns the tmux transaction (split or
//! window or session, metadata stamping, rollback) and the native PTY path,
//! so muxad runs that exact binary with `--json` and hands its envelope back.
//! The same argv travels to a control-mode Fleet host over the `work_command`
//! transport, which is why [`validate_relay_argv`] exists: the relay on the
//! other side re-checks every argv it is asked to run.
//!
//! Every value is passed as one `--flag=value` word. A prompt that begins with
//! a dash, or a provider option such as `--model`, then cannot be read as a
//! flag of its own — by clap here, or by the relay's allowlist over there.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::work_control::{self, WorkCommandError, WorkCommandLimits, WorkCommandOutput};

/// Providers `muxa agent start --agent` accepts, by their CLI names.
pub const AGENT_PROGRAMS: &[&str] = &["claude", "codex", "gemini", "agy", "opencode"];

/// Environment a native launch may carry to the agent's PTY. The daemon's
/// own environment is usually launchd's minimal one, so the client sends the
/// login shell's `PATH` and the terminal identity a shell tab would get.
/// Nothing else: this is not a way to inject arbitrary variables.
pub const NATIVE_ENV_KEYS: &[&str] = &[
    "PATH",
    "TERM",
    "COLORTERM",
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
];

const MAX_AGENT_START_INPUT_BYTES: usize = 64 * 1024;

/// Value flags an `agent start` argv may carry over the Fleet transport.
/// Managed Work flags (`--work`, `--workspace`, `--alias`, `--task`) are
/// absent on purpose: a Work binding goes through `work up`.
const RELAY_VALUE_FLAGS: &[&str] = &[
    "agent",
    "host",
    "placement",
    "target",
    "cwd",
    "prompt",
    "name",
    "role",
    "direction",
    "option",
];

/// Where the new agent runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPlacement {
    /// Split the target tmux pane.
    Pane,
    /// A new window in the target pane's (or session's) tmux session.
    Window,
    /// A new detached tmux session named after the directory.
    Session,
    /// A muxa-owned PTY session in this daemon. Local host only.
    Native,
}

impl AgentPlacement {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pane => "pane",
            Self::Window => "window",
            Self::Session => "session",
            Self::Native => "native",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStartRequest {
    /// One of [`AGENT_PROGRAMS`].
    pub agent: String,
    /// Fleet host alias. Absent or `"local"` runs on the daemon's own host.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub placement: AgentPlacement,
    /// tmux pane or session the pane/window placement is relative to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Short socket name (`default`) of the tmux server `target` lives on,
    /// as a pane row records it. Pane ids repeat across servers, so a local
    /// pane/window launch pins the server rather than trusting the default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tmux_socket: Option<String>,
    /// `auto`, `right`, or `down`; pane placement only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub direction: Option<String>,
    /// Absolute working directory, on the host that runs the agent.
    pub cwd: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// Provider arguments for this launch. Absent (or empty) uses the
    /// `[agent.<program>]` defaults, the same rule as `--option`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<String>>,
    /// Native placement only; keys limited to [`NATIVE_ENV_KEYS`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// The CLI's `schema_version: 1` envelope, reduced to what a client acts on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStartResult {
    /// Launch host kind the CLI resolved: `tmux` or `native`.
    pub host: String,
    pub agent: String,
    pub placement: String,
    #[serde(default)]
    pub pane: Option<String>,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub window: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub prompt_supplied: bool,
    /// Fleet host alias that ran the launch; `None` for the daemon's own
    /// host. Filled in by the daemon, not the CLI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fleet_host: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AgentStartError {
    #[error("invalid agent start request: {0}")]
    Invalid(String),
    #[error("{0}")]
    Command(#[from] WorkCommandError),
    #[error("muxa agent start failed: {0}")]
    Failed(String),
    #[error("muxa agent start answered with unparseable JSON: {0}")]
    InvalidJson(#[source] serde_json::Error),
}

fn present(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

impl AgentStartRequest {
    /// The Fleet alias that must run this request, or `None` for this host.
    #[must_use]
    pub fn remote_host(&self) -> Option<&str> {
        work_control::remote_host_alias(self.host.as_deref())
    }

    pub fn validate(&self) -> Result<(), AgentStartError> {
        let invalid = |message: &str| Err(AgentStartError::Invalid(message.into()));
        if !AGENT_PROGRAMS.contains(&self.agent.as_str()) {
            return Err(AgentStartError::Invalid(format!(
                "unknown agent {:?}; expected {}",
                self.agent,
                AGENT_PROGRAMS.join(", ")
            )));
        }
        if self
            .host
            .as_deref()
            .is_some_and(|host| host.trim().is_empty())
        {
            return invalid("host alias is empty");
        }
        if !self.cwd.is_absolute() {
            return invalid("cwd must be an absolute path");
        }
        let target = present(self.target.as_deref());
        match self.placement {
            AgentPlacement::Native => {
                if self.remote_host().is_some() {
                    return invalid("a native agent can only start on this Mac");
                }
                if target.is_some() || self.tmux_socket.is_some() || self.direction.is_some() {
                    return invalid("target, tmux_socket, and direction need a tmux placement");
                }
                if present(self.role.as_deref()).is_some() {
                    return invalid("role needs a tmux placement");
                }
            }
            AgentPlacement::Pane | AgentPlacement::Window => {
                if target.is_none() {
                    return Err(AgentStartError::Invalid(format!(
                        "{} placement needs a target pane or session",
                        self.placement.as_str()
                    )));
                }
            }
            AgentPlacement::Session => {
                if target.is_some() {
                    return invalid("session placement does not take a target");
                }
            }
        }
        if self.placement != AgentPlacement::Native && !self.env.is_empty() {
            return invalid("env is only carried by a native placement");
        }
        if let Some(key) = self
            .env
            .keys()
            .find(|key| !NATIVE_ENV_KEYS.contains(&key.as_str()))
        {
            return Err(AgentStartError::Invalid(format!(
                "env key {key:?} is not allowed"
            )));
        }
        if let Some(direction) = present(self.direction.as_deref()) {
            if self.placement != AgentPlacement::Pane {
                return invalid("direction only applies to pane placement");
            }
            if !["auto", "right", "down"].contains(&direction) {
                return Err(AgentStartError::Invalid(format!(
                    "unknown direction {direction:?}; expected auto, right, or down"
                )));
            }
        }
        if self.tmux_socket.is_some() && self.remote_host().is_some() {
            return invalid("tmux_socket only applies to this Mac's tmux servers");
        }
        let arguments = self.arguments();
        if arguments.iter().any(|argument| argument.contains('\0'))
            || self.env.values().any(|value| value.contains('\0'))
        {
            return invalid("values cannot contain NUL bytes");
        }
        let bytes = arguments.iter().map(String::len).sum::<usize>()
            + self
                .env
                .iter()
                .map(|(k, v)| k.len() + v.len())
                .sum::<usize>();
        if bytes > MAX_AGENT_START_INPUT_BYTES {
            return Err(AgentStartError::Invalid(format!(
                "request exceeds {MAX_AGENT_START_INPUT_BYTES} bytes"
            )));
        }
        Ok(())
    }

    /// The `muxa agent start` argv for this request.
    #[must_use]
    pub fn arguments(&self) -> Vec<String> {
        let native = self.placement == AgentPlacement::Native;
        let mut args = vec![
            "agent".to_string(),
            "start".to_string(),
            "--json".to_string(),
            format!("--agent={}", self.agent),
            format!("--host={}", if native { "native" } else { "tmux" }),
        ];
        if native {
            // Explicit for CLIs older than the fix that made the flag
            // optional: their `right` default refused every native launch.
            args.push("--direction=auto".into());
        } else {
            args.push(format!("--placement={}", self.placement.as_str()));
            if let Some(target) = present(self.target.as_deref()) {
                args.push(format!("--target={target}"));
            }
            if let Some(direction) = present(self.direction.as_deref()) {
                args.push(format!("--direction={direction}"));
            }
        }
        args.push(format!("--cwd={}", self.cwd.display()));
        if let Some(prompt) = self
            .prompt
            .as_deref()
            .filter(|prompt| !prompt.trim().is_empty())
        {
            args.push(format!("--prompt={prompt}"));
        }
        if let Some(name) = present(self.name.as_deref()) {
            args.push(format!("--name={name}"));
        }
        if !native {
            if let Some(role) = present(self.role.as_deref()) {
                args.push(format!("--role={role}"));
            }
        }
        for option in self.options.iter().flatten() {
            args.push(format!("--option={option}"));
        }
        args
    }
}

/// Whether `args` is an `agent start` argv (as opposed to `work …`).
#[must_use]
pub fn is_agent_start(args: &[String]) -> bool {
    args.first().is_some_and(|command| command == "agent")
        && args.get(1).is_some_and(|subcommand| subcommand == "start")
}

/// Check an `agent start` argv arriving over the Fleet transport: JSON
/// output, the tmux host, and only the allowlisted `--flag=value` words.
/// Native launches never cross hosts, and the daemon-owned `--socket` /
/// `--config` fall outside the allowlist.
pub fn validate_relay_argv(args: &[String]) -> Result<(), String> {
    if !is_agent_start(args) {
        return Err("expected `agent start …`".into());
    }
    let rest = &args[2..];
    if !rest.iter().any(|arg| arg == "--json") {
        return Err("`agent start` over Fleet must pass --json".into());
    }
    if !rest.iter().any(|arg| arg == "--host=tmux") {
        return Err("`agent start` over Fleet must pass --host=tmux".into());
    }
    for arg in rest {
        if arg == "--json" {
            continue;
        }
        let allowed = arg
            .strip_prefix("--")
            .and_then(|flag| flag.split_once('='))
            .is_some_and(|(name, _)| RELAY_VALUE_FLAGS.contains(&name));
        if !allowed {
            let shown: String = arg.chars().take(40).collect();
            return Err(format!("`agent start` does not accept {shown:?} here"));
        }
        if arg.starts_with("--host=") && arg != "--host=tmux" {
            return Err("`agent start` over Fleet runs on the tmux host only".into());
        }
    }
    Ok(())
}

/// Run `binary agent start …` on this host with `MUXA_SOCKET` pinned to the
/// accepting daemon, the target's tmux server pinned when known, and the
/// native environment allowlist applied.
pub async fn execute_local(
    binary: &Path,
    request: &AgentStartRequest,
    socket_path: Option<&Path>,
) -> Result<AgentStartResult, AgentStartError> {
    request.validate()?;
    let args = request.arguments();
    let mut command = tokio::process::Command::new(binary);
    command.args(&args);
    if let Some(socket_path) = socket_path {
        command.env("MUXA_SOCKET", socket_path);
    }
    if let Some(socket) = present(request.tmux_socket.as_deref()) {
        // A full path is taken as is; a short name is looked up among the
        // live servers, failing closed rather than landing on the default
        // server where the same `%N` may exist.
        let path = if socket.starts_with('/') {
            PathBuf::from(socket)
        } else {
            crate::tmux::resolve_socket_path(socket).ok_or_else(|| {
                AgentStartError::Failed(format!("tmux server {socket:?} is not running"))
            })?
        };
        command.env("MUXA_TMUX_SOCKET", path);
    }
    for (key, value) in &request.env {
        command.env(key, value);
    }
    let output =
        work_control::run_bounded(command, &args, None, WorkCommandLimits::COMMAND).await?;
    parse_output(&output, None)
}

/// Interpret a finished `muxa agent start --json` child, wherever it ran.
pub fn parse_output(
    output: &WorkCommandOutput,
    fleet_host: Option<&str>,
) -> Result<AgentStartResult, AgentStartError> {
    if output.exit_code != 0 {
        let detail = output
            .stderr
            .trim()
            .lines()
            .next_back()
            .unwrap_or("no stderr")
            .trim_start_matches("Error: ");
        return Err(AgentStartError::Failed(detail.to_string()));
    }
    let mut result: AgentStartResult =
        serde_json::from_str(output.stdout.trim()).map_err(AgentStartError::InvalidJson)?;
    result.fleet_host = fleet_host.map(str::to_string);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(placement: AgentPlacement) -> AgentStartRequest {
        AgentStartRequest {
            agent: "claude".into(),
            host: None,
            placement,
            target: None,
            tmux_socket: None,
            direction: None,
            cwd: PathBuf::from("/srv/app"),
            prompt: None,
            name: None,
            role: None,
            options: None,
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn arguments_pass_every_value_as_one_word() {
        let mut pane = request(AgentPlacement::Pane);
        pane.target = Some("%12".into());
        pane.direction = Some("down".into());
        pane.prompt = Some("-v means verbose; review it".into());
        pane.role = Some("reviewer".into());
        pane.options = Some(vec!["--model".into(), "opus".into()]);
        pane.validate().unwrap();
        assert_eq!(
            pane.arguments(),
            [
                "agent",
                "start",
                "--json",
                "--agent=claude",
                "--host=tmux",
                "--placement=pane",
                "--target=%12",
                "--direction=down",
                "--cwd=/srv/app",
                "--prompt=-v means verbose; review it",
                "--role=reviewer",
                "--option=--model",
                "--option=opus",
            ]
        );
        validate_relay_argv(&pane.arguments()).unwrap();
    }

    #[test]
    fn native_arguments_carry_auto_direction_and_no_tmux_flags() {
        let mut native = request(AgentPlacement::Native);
        native.name = Some("claude".into());
        native
            .env
            .insert("PATH".into(), "/opt/homebrew/bin:/usr/bin".into());
        native.validate().unwrap();
        assert_eq!(
            native.arguments(),
            [
                "agent",
                "start",
                "--json",
                "--agent=claude",
                "--host=native",
                "--direction=auto",
                "--cwd=/srv/app",
                "--name=claude",
            ]
        );
        assert!(validate_relay_argv(&native.arguments()).is_err());
    }

    #[test]
    fn validation_refuses_what_the_cli_would_refuse_later() {
        let refuse = |request: AgentStartRequest| request.validate().unwrap_err().to_string();

        let mut unknown = request(AgentPlacement::Session);
        unknown.agent = "bash".into();
        assert!(refuse(unknown).contains("unknown agent"));

        let mut relative = request(AgentPlacement::Session);
        relative.cwd = PathBuf::from("src");
        assert!(refuse(relative).contains("absolute"));

        assert!(refuse(request(AgentPlacement::Window)).contains("needs a target"));

        let mut session_target = request(AgentPlacement::Session);
        session_target.target = Some("$1".into());
        assert!(refuse(session_target).contains("does not take a target"));

        let mut remote_native = request(AgentPlacement::Native);
        remote_native.host = Some("devbox".into());
        assert!(refuse(remote_native).contains("this Mac"));

        let mut native_role = request(AgentPlacement::Native);
        native_role.role = Some("reviewer".into());
        assert!(refuse(native_role).contains("role"));

        let mut tmux_env = request(AgentPlacement::Session);
        tmux_env.env.insert("PATH".into(), "/bin".into());
        assert!(refuse(tmux_env).contains("native"));

        let mut foreign_env = request(AgentPlacement::Native);
        foreign_env
            .env
            .insert("LD_PRELOAD".into(), "/tmp/x.dylib".into());
        assert!(refuse(foreign_env).contains("not allowed"));

        let mut window_direction = request(AgentPlacement::Window);
        window_direction.target = Some("%1".into());
        window_direction.direction = Some("right".into());
        assert!(refuse(window_direction).contains("pane placement"));

        let mut remote_socket = request(AgentPlacement::Pane);
        remote_socket.target = Some("%1".into());
        remote_socket.host = Some("devbox".into());
        remote_socket.tmux_socket = Some("default".into());
        assert!(refuse(remote_socket).contains("tmux_socket"));

        let mut huge = request(AgentPlacement::Session);
        huge.prompt = Some("x".repeat(MAX_AGENT_START_INPUT_BYTES + 1));
        assert!(refuse(huge).contains("exceeds"));

        let mut local = request(AgentPlacement::Session);
        local.host = Some("local".into());
        local.validate().unwrap();
        assert_eq!(local.remote_host(), None);
    }

    #[test]
    fn relay_allowlist_accepts_only_tmux_json_value_words() {
        let words = |parts: &[&str]| parts.iter().map(|p| (*p).to_string()).collect::<Vec<_>>();
        let base = ["agent", "start", "--json", "--agent=codex", "--host=tmux"];
        validate_relay_argv(&words(&base)).unwrap();
        for extra in [
            "--socket=/tmp/other.sock",
            "--config=/tmp/c.toml",
            "--work=W-1",
            "--alias=impl",
            "--agent",
            "codex",
        ] {
            let mut args = words(&base);
            args.push(extra.into());
            assert!(validate_relay_argv(&args).is_err(), "{extra} accepted");
        }
        assert!(
            validate_relay_argv(&words(&["agent", "start", "--agent=codex", "--host=tmux"]))
                .is_err()
        );
        assert!(
            validate_relay_argv(&words(&["agent", "start", "--json", "--agent=codex"])).is_err()
        );
        assert!(validate_relay_argv(&words(&["work", "up", "W-1"])).is_err());
    }

    #[test]
    fn output_parses_both_envelopes_and_reports_the_last_stderr_line() {
        let tmux = WorkCommandOutput {
            exit_code: 0,
            stdout: r#"{"schema_version":1,"host":"tmux","agent":"codex","placement":"window","pane":"%42","session":null,"window":null,"name":null,"workspace":null,"work":null,"created_workspace":false,"created_work":false,"role":null,"task":null,"alias":null,"cwd":"/srv/app","prompt_supplied":true}"#.into(),
            stderr: String::new(),
        };
        let result = parse_output(&tmux, Some("devbox")).unwrap();
        assert_eq!(result.pane.as_deref(), Some("%42"));
        assert_eq!(result.fleet_host.as_deref(), Some("devbox"));
        assert!(result.prompt_supplied);

        let native = WorkCommandOutput {
            exit_code: 0,
            stdout: r#"{"schema_version":1,"host":"native","agent":"claude","placement":"session","pane":null,"session":"s-7","window":null,"name":"claude","cwd":"/srv/app","prompt_supplied":false}"#.into(),
            stderr: String::new(),
        };
        let result = parse_output(&native, None).unwrap();
        assert_eq!(result.session.as_deref(), Some("s-7"));
        assert_eq!(result.pane, None);

        let failed = WorkCommandOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: "warning: noise\nError: resolve cwd /nope\n".into(),
        };
        assert_eq!(
            parse_output(&failed, None).unwrap_err().to_string(),
            "muxa agent start failed: resolve cwd /nope"
        );
    }

    /// The local path runs the given binary with the argv, the daemon socket,
    /// and only the allowlisted environment.
    #[tokio::test]
    async fn local_execution_runs_the_binary_with_pinned_socket_and_env() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("muxa");
        std::fs::write(
            &script,
            "#!/bin/sh\nprintf '{\"host\":\"native\",\"agent\":\"claude\",\"placement\":\"session\",\"session\":\"%s|%s|%s\",\"cwd\":\"/srv/app\"}' \"$MUXA_SOCKET\" \"$TERM\" \"$*\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut native = request(AgentPlacement::Native);
        native.env.insert("TERM".into(), "xterm-256color".into());
        let result = execute_local(&script, &native, Some(Path::new("/tmp/app.sock")))
            .await
            .unwrap();
        assert_eq!(
            result.session.as_deref(),
            Some("/tmp/app.sock|xterm-256color|agent start --json --agent=claude --host=native --direction=auto --cwd=/srv/app")
        );
    }
}
