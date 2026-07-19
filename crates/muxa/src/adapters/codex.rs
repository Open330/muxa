//! `OpenAI` Codex CLI adapter.
//!
//! Codex's hook engine is a verbatim port of Claude Code's (it's literally
//! named `ClaudeHooksEngine` upstream). Wire up in `~/.codex/config.toml`:
//!
//! ```toml
//! [[hooks.SessionStart]]
//!   [[hooks.SessionStart.hooks]]
//!   type = "command"
//!   command = "muxa hook codex --event session_start"
//! [[hooks.UserPromptSubmit]]
//!   [[hooks.UserPromptSubmit.hooks]]
//!   type = "command"
//!   command = "muxa hook codex --event user_prompt_submit"
//! # ... (PreToolUse / PostToolUse / PermissionRequest / Stop)
//! ```

use super::hook::{truncate, AdapterError, HookAdapter};
use crate::event::{AgentEvent, AgentId, AgentKind, NotificationLevel};
use serde::Deserialize;
use time::OffsetDateTime;

pub struct CodexAdapter;

#[derive(Debug, Deserialize)]
pub struct Input {
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
pub enum Event {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PermissionRequest,
    Stop,
}

impl HookAdapter for CodexAdapter {
    type Event = Event;
    type Input = Input;
    const KIND: AgentKind = AgentKind::Codex;

    fn parse_event(flag: &str) -> Result<Event, AdapterError> {
        Ok(match flag {
            "session_start" => Event::SessionStart,
            "user_prompt_submit" => Event::UserPromptSubmit,
            "pre_tool_use" => Event::PreToolUse,
            "post_tool_use" => Event::PostToolUse,
            "permission_request" => Event::PermissionRequest,
            "stop" => Event::Stop,
            other => return Err(AdapterError::UnknownEvent(other.into())),
        })
    }

    fn normalize(event: Event, input: Input, pane: Option<String>) -> AgentEvent {
        let id = AgentId {
            kind: AgentKind::Codex,
            session_id: input.session_id,
            surface: None,
            pane,
            tmux_socket: None,
            cwd: input.cwd,
        };
        let at = OffsetDateTime::now_utc();

        match event {
            Event::SessionStart => AgentEvent::Started { id, at },
            Event::UserPromptSubmit => AgentEvent::PromptSubmitted {
                id,
                prompt: truncate(input.prompt.unwrap_or_default(), 4_000),
                at,
            },
            Event::PreToolUse => AgentEvent::ToolStarted {
                id,
                tool: input.tool_name.unwrap_or_else(|| "unknown".into()),
                subagent: None,
                at,
            },
            Event::PostToolUse => AgentEvent::ToolCompleted {
                id,
                tool: input.tool_name.unwrap_or_else(|| "unknown".into()),
                success: true,
                at,
            },
            Event::PermissionRequest => AgentEvent::NotificationFired {
                id,
                level: NotificationLevel::NeedsInput,
                message: format!(
                    "codex permission: {}",
                    input.tool_name.unwrap_or_else(|| "tool".into())
                ),
                at,
            },
            Event::Stop => AgentEvent::TurnStopped {
                id,
                response: None,
                at,
            },
        }
    }
}
