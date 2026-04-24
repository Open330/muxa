//! Claude Code adapter.
//!
//! Reads JSON from stdin (supplied by a Claude Code hook), normalizes it
//! into an `AgentEvent`, and sends it to the muxa daemon.
//!
//! Hook configuration in `~/.claude/settings.json`:
//!
//!   "hooks": {
//!     "SessionStart":     [{ "hooks": [{ "type":"command",
//!                             "command":"muxa hook claude --event session_start" }]}],
//!     "UserPromptSubmit": [{ "hooks": [{ "type":"command",
//!                             "command":"muxa hook claude --event user_prompt_submit" }]}],
//!     "PreToolUse":       [{ "hooks": [{ "type":"command",
//!                             "command":"muxa hook claude --event pre_tool_use" }]}],
//!     "PostToolUse":      [{ "hooks": [{ "type":"command",
//!                             "command":"muxa hook claude --event post_tool_use" }]}],
//!     "Notification":     [{ "hooks": [{ "type":"command",
//!                             "command":"muxa hook claude --event notification" }]}],
//!     "Stop":             [{ "hooks": [{ "type":"command",
//!                             "command":"muxa hook claude --event stop" }]}],
//!     "SessionEnd":       [{ "hooks": [{ "type":"command",
//!                             "command":"muxa hook claude --event session_end" }]}]
//!   }
//!
//! `$TMUX_PANE` is inherited from the shell that spawned `claude`; we read
//! it to correlate the hook invocation with a specific pane.

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
    // Notification matcher ends up here, e.g. "permission_prompt" / "idle_prompt".
    #[serde(default)]
    pub notification_type: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    // UserPromptSubmit carries the prompt text.
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum HookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Notification,
    Stop,
    SessionEnd,
}

impl HookEvent {
    pub fn from_flag(s: &str) -> Result<Self> {
        Ok(match s {
            "session_start" => Self::SessionStart,
            "user_prompt_submit" => Self::UserPromptSubmit,
            "pre_tool_use" => Self::PreToolUse,
            "post_tool_use" => Self::PostToolUse,
            "notification" => Self::Notification,
            "stop" => Self::Stop,
            "session_end" => Self::SessionEnd,
            other => anyhow::bail!("unknown Claude hook event: {other}"),
        })
    }
}

pub fn parse_stdin() -> Result<HookInput> {
    let mut buf = String::new();
    use std::io::Read;
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading hook JSON from stdin")?;
    serde_json::from_str(&buf).context("parsing hook JSON")
}

/// Shape of the JSON the Claude Code status line script receives on stdin.
/// We only pluck the fields we need for Heartbeat.
#[derive(Debug, Deserialize)]
pub struct StatusLineInput {
    pub session_id: String,
    pub cwd: Option<String>,
    #[serde(default)]
    pub model: Option<StatusModel>,
    #[serde(default)]
    pub context_window: Option<StatusContext>,
    #[serde(default)]
    pub cost: Option<StatusCost>,
}

#[derive(Debug, Deserialize)]
pub struct StatusModel {
    pub display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct StatusContext {
    pub used_percentage: Option<f32>,
}

#[derive(Debug, Deserialize)]
pub struct StatusCost {
    pub total_cost_usd: Option<f64>,
}

pub fn parse_statusline_stdin() -> Result<StatusLineInput> {
    let mut buf = String::new();
    use std::io::Read;
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading statusline JSON from stdin")?;
    serde_json::from_str(&buf).context("parsing statusline JSON")
}

pub fn statusline_to_heartbeat(input: StatusLineInput) -> AgentEvent {
    let pane = std::env::var("TMUX_PANE").ok();
    AgentEvent::Heartbeat {
        id: AgentId {
            kind: AgentKind::ClaudeCode,
            session_id: input.session_id,
            pane,
            cwd: input.cwd,
        },
        model: input.model.and_then(|m| m.display_name),
        context_used_pct: input.context_window.and_then(|c| c.used_percentage),
        cost_usd: input.cost.and_then(|c| c.total_cost_usd),
        at: OffsetDateTime::now_utc(),
    }
}

pub fn to_event(event: HookEvent, input: HookInput) -> AgentEvent {
    let pane = std::env::var("TMUX_PANE").ok();
    let id = AgentId {
        kind: AgentKind::ClaudeCode,
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
        HookEvent::Notification => {
            let kind = input.notification_type.unwrap_or_default();
            let level = match kind.as_str() {
                "permission_prompt" | "idle_prompt" | "elicitation_dialog" => {
                    NotificationLevel::NeedsInput
                }
                _ => NotificationLevel::Info,
            };
            AgentEvent::NotificationFired {
                id,
                level,
                message: input.message.unwrap_or(kind),
                at,
            }
        }
        HookEvent::Stop => AgentEvent::TurnStopped { id, at },
        HookEvent::SessionEnd => AgentEvent::SessionEnded { id, at },
    }
}

fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        s.truncate(max);
        s.push('…');
    }
    s
}
