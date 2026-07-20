//! Google Gemini CLI adapter.
//!
//! Gemini ships a Claude-Code-compatible hook system (it even exports
//! `CLAUDE_PROJECT_DIR` for compat). Wire up in `~/.gemini/settings.json`:
//!
//! ```json
//! "hooks": {
//!   "SessionStart": [{ "hooks": [{ "type":"command", "command":"muxa hook gemini --event session_start" }]}],
//!   "BeforeAgent":  [{ "hooks": [{ "type":"command", "command":"muxa hook gemini --event before_agent"  }]}],
//!   "AfterAgent":   [{ "hooks": [{ "type":"command", "command":"muxa hook gemini --event after_agent"   }]}],
//!   "BeforeTool":   [{ "hooks": [{ "type":"command", "command":"muxa hook gemini --event before_tool"   }]}],
//!   "AfterTool":    [{ "hooks": [{ "type":"command", "command":"muxa hook gemini --event after_tool"    }]}],
//!   "Notification": [{ "hooks": [{ "type":"command", "command":"muxa hook gemini --event notification"  }]}],
//!   "SessionEnd":   [{ "hooks": [{ "type":"command", "command":"muxa hook gemini --event session_end"   }]}]
//! }
//! ```

use super::hook::{truncate, AdapterError, HookAdapter};
use crate::event::{AgentEvent, AgentId, AgentKind, NotificationLevel};
use serde::Deserialize;
use time::OffsetDateTime;

pub struct GeminiAdapter;

#[derive(Debug, Deserialize)]
pub struct Input {
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
pub enum Event {
    SessionStart,
    BeforeAgent,
    AfterAgent,
    BeforeTool,
    AfterTool,
    Notification,
    SessionEnd,
}

impl HookAdapter for GeminiAdapter {
    type Event = Event;
    type Input = Input;
    const KIND: AgentKind = AgentKind::GeminiCli;

    fn parse_event(flag: &str) -> Result<Event, AdapterError> {
        Ok(match flag {
            "session_start" => Event::SessionStart,
            "before_agent" => Event::BeforeAgent,
            "after_agent" => Event::AfterAgent,
            "before_tool" => Event::BeforeTool,
            "after_tool" => Event::AfterTool,
            "notification" => Event::Notification,
            "session_end" => Event::SessionEnd,
            other => return Err(AdapterError::UnknownEvent(other.into())),
        })
    }

    fn normalize(event: Event, input: Input, pane: Option<String>) -> AgentEvent {
        let id = AgentId {
            kind: AgentKind::GeminiCli,
            session_id: input.session_id,
            surface: None,
            pane,
            tmux_socket: None,
            cwd: input.cwd,
        };
        let at = OffsetDateTime::now_utc();

        match event {
            Event::SessionStart => AgentEvent::Started { id, at },
            Event::BeforeAgent => AgentEvent::PromptSubmitted {
                id,
                prompt: truncate(input.prompt.unwrap_or_default(), 4_000),
                at,
            },
            Event::BeforeTool => AgentEvent::ToolStarted {
                id,
                tool: input.tool_name.unwrap_or_else(|| "unknown".into()),
                subagent: None,
                at,
            },
            Event::AfterTool => AgentEvent::ToolCompleted {
                id,
                tool: input.tool_name.unwrap_or_else(|| "unknown".into()),
                success: true,
                at,
            },
            Event::Notification => {
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
            Event::AfterAgent => AgentEvent::TurnStopped {
                id,
                response: None,
                recap: None,
                ai_title: None,
                idle_confirmed: false,
                at,
            },
            Event::SessionEnd => AgentEvent::SessionEnded { id, at },
        }
    }
}
