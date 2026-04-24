//! Gemini CLI adapter.
//!
//! Gemini CLI ships a Claude-Code-compatible hook system (it even exports
//! `CLAUDE_PROJECT_DIR` alongside `GEMINI_PROJECT_DIR`). Event names differ,
//! and the "waiting for input" signal is `Notification` with
//! `notification_type == "ToolPermission"`.
//!
//! Config snippet for `~/.gemini/settings.json`:
//!
//!   {
//!     "hooks": {
//!       "SessionStart":  [{ "hooks": [{ "type":"command",
//!                           "command":"muxa hook gemini --event session_start" }]}],
//!       "BeforeAgent":   [{ "hooks": [{ "type":"command",
//!                           "command":"muxa hook gemini --event before_agent" }]}],
//!       "AfterAgent":    [{ "hooks": [{ "type":"command",
//!                           "command":"muxa hook gemini --event after_agent" }]}],
//!       "BeforeTool":    [{ "hooks": [{ "type":"command",
//!                           "command":"muxa hook gemini --event before_tool" }]}],
//!       "AfterTool":     [{ "hooks": [{ "type":"command",
//!                           "command":"muxa hook gemini --event after_tool" }]}],
//!       "Notification":  [{ "hooks": [{ "type":"command",
//!                           "command":"muxa hook gemini --event notification" }]}],
//!       "SessionEnd":    [{ "hooks": [{ "type":"command",
//!                           "command":"muxa hook gemini --event session_end" }]}]
//!     }
//!   }

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
    pub notification_type: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum HookEvent {
    SessionStart,
    BeforeAgent,
    AfterAgent,
    BeforeTool,
    AfterTool,
    Notification,
    SessionEnd,
}

impl HookEvent {
    pub fn from_flag(s: &str) -> Result<Self> {
        Ok(match s {
            "session_start" => Self::SessionStart,
            "before_agent" => Self::BeforeAgent,
            "after_agent" => Self::AfterAgent,
            "before_tool" => Self::BeforeTool,
            "after_tool" => Self::AfterTool,
            "notification" => Self::Notification,
            "session_end" => Self::SessionEnd,
            other => anyhow::bail!("unknown Gemini hook event: {other}"),
        })
    }
}

pub fn parse_stdin() -> Result<HookInput> {
    let mut buf = String::new();
    use std::io::Read;
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("reading Gemini hook JSON from stdin")?;
    serde_json::from_str(&buf).context("parsing Gemini hook JSON")
}

pub fn to_event(event: HookEvent, input: HookInput) -> AgentEvent {
    let pane = std::env::var("TMUX_PANE").ok();
    let id = AgentId {
        kind: AgentKind::GeminiCli,
        session_id: input.session_id,
        pane,
        cwd: input.cwd,
    };
    let at = OffsetDateTime::now_utc();

    match event {
        HookEvent::SessionStart => AgentEvent::Started { id, at },
        HookEvent::BeforeAgent => AgentEvent::PromptSubmitted {
            id,
            prompt: truncate(input.prompt.unwrap_or_default(), 4_000),
            at,
        },
        HookEvent::BeforeTool => AgentEvent::ToolStarted {
            id,
            tool: input.tool_name.unwrap_or_else(|| "unknown".into()),
            at,
        },
        HookEvent::AfterTool => AgentEvent::ToolCompleted {
            id,
            tool: input.tool_name.unwrap_or_else(|| "unknown".into()),
            success: true,
            at,
        },
        HookEvent::Notification => {
            let kind = input.notification_type.unwrap_or_default();
            let level = match kind.as_str() {
                "ToolPermission" | "elicitation" => NotificationLevel::NeedsInput,
                _ => NotificationLevel::Info,
            };
            AgentEvent::NotificationFired {
                id,
                level,
                message: input.message.unwrap_or(kind),
                at,
            }
        }
        HookEvent::AfterAgent => AgentEvent::TurnStopped { id, at },
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
