//! OpenAI codex CLI adapter.
//!
//! Codex's hook system is a verbatim port of the Claude Code protocol
//! (the engine in codex-rs is literally named `ClaudeHooksEngine`), so the
//! stdin-JSON shape is near-identical.
//!
//! Config snippet for `~/.codex/config.toml`:
//!
//!   [[hooks.SessionStart]]
//!     [[hooks.SessionStart.hooks]]
//!     type = "command"
//!     command = "muxa hook codex --event session_start"
//!   [[hooks.UserPromptSubmit]]
//!     [[hooks.UserPromptSubmit.hooks]]
//!     type = "command"
//!     command = "muxa hook codex --event user_prompt_submit"
//!   [[hooks.PreToolUse]]
//!     [[hooks.PreToolUse.hooks]]
//!     type = "command"
//!     command = "muxa hook codex --event pre_tool_use"
//!   [[hooks.PostToolUse]]
//!     [[hooks.PostToolUse.hooks]]
//!     type = "command"
//!     command = "muxa hook codex --event post_tool_use"
//!   [[hooks.PermissionRequest]]
//!     [[hooks.PermissionRequest.hooks]]
//!     type = "command"
//!     command = "muxa hook codex --event permission_request"
//!   [[hooks.Stop]]
//!     [[hooks.Stop.hooks]]
//!     type = "command"
//!     command = "muxa hook codex --event stop"
//!
//! Codex injects `CODEX_THREAD_ID` into every shell tool invocation, but the
//! stdin JSON already carries `session_id` / `turn_id`, so we use those.

use crate::event::{AgentEvent, AgentId, AgentKind, NotificationLevel};
use anyhow::{Context, Result};
use serde::Deserialize;
use time::OffsetDateTime;

#[derive(Debug, Deserialize)]
pub struct HookInput {
    pub session_id: String,
    pub cwd: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub last_assistant_message: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PermissionRequest,
    Stop,
}

impl HookEvent {
    pub fn from_flag(s: &str) -> Result<Self> {
        Ok(match s {
            "session_start" => Self::SessionStart,
            "user_prompt_submit" => Self::UserPromptSubmit,
            "pre_tool_use" => Self::PreToolUse,
            "post_tool_use" => Self::PostToolUse,
            "permission_request" => Self::PermissionRequest,
            "stop" => Self::Stop,
            other => anyhow::bail!("unknown Codex hook event: {other}"),
        })
    }
}

pub fn parse_stdin() -> Result<HookInput> {
    let mut buf = String::new();
    use std::io::Read;
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading Codex hook JSON from stdin")?;
    serde_json::from_str(&buf).context("parsing Codex hook JSON")
}

pub fn to_event(event: HookEvent, input: HookInput) -> AgentEvent {
    let pane = std::env::var("TMUX_PANE").ok();
    let id = AgentId {
        kind: AgentKind::Codex,
        session_id: input.session_id,
        pane,
        cwd: input.cwd,
    };
    let at = OffsetDateTime::now_utc();

    match event {
        HookEvent::SessionStart => AgentEvent::Started { id, at },
        HookEvent::UserPromptSubmit => AgentEvent::PromptSubmitted {
            id,
            prompt: truncate(input.prompt.unwrap_or_default(), 4_000),
            at,
        },
        HookEvent::PreToolUse => AgentEvent::ToolStarted {
            id,
            tool: input.tool_name.unwrap_or_else(|| "unknown".into()),
            at,
        },
        HookEvent::PostToolUse => AgentEvent::ToolCompleted {
            id,
            tool: input.tool_name.unwrap_or_else(|| "unknown".into()),
            success: true,
            at,
        },
        HookEvent::PermissionRequest => AgentEvent::NotificationFired {
            id,
            level: NotificationLevel::NeedsInput,
            message: format!(
                "codex permission: {}",
                input.tool_name.unwrap_or_else(|| "tool".into())
            ),
            at,
        },
        HookEvent::Stop => AgentEvent::TurnStopped { id, at },
    }
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        s.truncate(max);
        s.push('…');
    }
    s
}
