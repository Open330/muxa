//! Shared launcher for native/dashboard `muxa work up` control surfaces.
//!
//! The CLI crate owns the actual Work reconciliation implementation.  Native
//! clients therefore ask muxad to launch the exact bundled/installed `muxa`
//! binary instead of growing a second pipeline implementation in the daemon.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

pub const WORK_UP_TIMEOUT: Duration = Duration::from_secs(600);
const MAX_WORK_UP_INPUT_BYTES: usize = 64 * 1024;
const MAX_WORK_UP_OUTPUT_BYTES: usize = 2 * 1024 * 1024;

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
        let input_bytes = self.work.len()
            + optional_len(self.external.as_deref())
            + optional_len(self.pipeline.as_deref())
            + optional_len(self.workspace.as_deref())
            + self.cwd.as_deref().map_or(0, |path| path.as_os_str().len())
            + optional_len(self.skill.as_deref())
            + optional_len(self.body.as_deref())
            + optional_len(self.context.as_deref());
        if input_bytes > MAX_WORK_UP_INPUT_BYTES {
            return Err(WorkUpError::Invalid(format!(
                "request exceeds {MAX_WORK_UP_INPUT_BYTES} bytes"
            )));
        }
        Ok(())
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

/// Run the canonical CLI implementation. `socket_path` pins nested CLI IPC
/// back to the daemon that accepted the operation, including non-default test
/// and app sockets.
pub async fn execute_work_up(
    input: &WorkUpRequest,
    socket_path: Option<&Path>,
) -> Result<Value, WorkUpError> {
    input.validate()?;
    let mut command = tokio::process::Command::new(resolve_muxa_binary());
    command
        .args(input.arguments())
        .stdin(Stdio::null())
        .kill_on_drop(true);
    if let Some(socket_path) = socket_path {
        command.env("MUXA_SOCKET", socket_path);
    }
    let output = match tokio::time::timeout(WORK_UP_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => return Err(WorkUpError::Spawn(error)),
        Err(_) => return Err(WorkUpError::Timeout),
    };
    if output.stdout.len().saturating_add(output.stderr.len()) > MAX_WORK_UP_OUTPUT_BYTES {
        return Err(WorkUpError::OutputTooLarge);
    }
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim().lines().next_back().unwrap_or("no stderr");
        return Err(WorkUpError::Failed(detail.to_string()));
    }
    serde_json::from_slice(output.stdout.trim_ascii()).map_err(WorkUpError::InvalidJson)
}

fn resolve_muxa_binary() -> PathBuf {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_work_request_builds_non_interactive_canonical_cli_arguments() {
        let input = WorkUpRequest {
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
        };
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
        };
        assert!(matches!(input.validate(), Err(WorkUpError::Invalid(_))));
    }
}
