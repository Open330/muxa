//! Claude Code adapter — reads hook JSON on stdin, emits `AgentEvent`.
//!
//! Wire up in `~/.claude/settings.json`:
//!
//! ```json
//! "hooks": {
//!   "SessionStart":     [{ "hooks": [{ "type":"command", "command":"muxa hook claude --event session_start"      }]}],
//!   "UserPromptSubmit": [{ "hooks": [{ "type":"command", "command":"muxa hook claude --event user_prompt_submit" }]}],
//!   "PreToolUse":       [{ "hooks": [{ "type":"command", "command":"muxa hook claude --event pre_tool_use"       }]}],
//!   "PostToolUse":      [{ "hooks": [{ "type":"command", "command":"muxa hook claude --event post_tool_use"      }]}],
//!   "Notification":     [{ "hooks": [{ "type":"command", "command":"muxa hook claude --event notification"       }]}],
//!   "Stop":             [{ "hooks": [{ "type":"command", "command":"muxa hook claude --event stop"               }]}],
//!   "SessionEnd":       [{ "hooks": [{ "type":"command", "command":"muxa hook claude --event session_end"        }]}]
//! }
//! ```

use crate::hook::{truncate, AdapterError, HookAdapter};
use muxa_core::event::{AgentEvent, AgentId, AgentKind, NotificationLevel};
use serde::Deserialize;
use time::OffsetDateTime;

pub struct ClaudeAdapter;

#[derive(Debug, Deserialize)]
pub struct Input {
    pub session_id: String,
    pub cwd: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub notification_type: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum Event {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Notification,
    Stop,
    SessionEnd,
}

impl HookAdapter for ClaudeAdapter {
    type Event = Event;
    type Input = Input;
    const KIND: AgentKind = AgentKind::ClaudeCode;

    fn parse_event(flag: &str) -> Result<Event, AdapterError> {
        Ok(match flag {
            "session_start" => Event::SessionStart,
            "user_prompt_submit" => Event::UserPromptSubmit,
            "pre_tool_use" => Event::PreToolUse,
            "post_tool_use" => Event::PostToolUse,
            "notification" => Event::Notification,
            "stop" => Event::Stop,
            "session_end" => Event::SessionEnd,
            other => return Err(AdapterError::UnknownEvent(other.into())),
        })
    }

    fn normalize(event: Event, input: Input, pane: Option<String>) -> AgentEvent {
        let id = AgentId {
            kind: AgentKind::ClaudeCode,
            session_id: input.session_id,
            pane,
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
                at,
            },
            Event::PostToolUse => AgentEvent::ToolCompleted {
                id,
                tool: input.tool_name.unwrap_or_else(|| "unknown".into()),
                success: true,
                at,
            },
            Event::Notification => {
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
            Event::Stop => AgentEvent::TurnStopped { id, at },
            Event::SessionEnd => AgentEvent::SessionEnded { id, at },
        }
    }
}

// ---------------------------------------------------------------------------
// Status line (not a HookAdapter — it reads a different payload and emits a
// Heartbeat rather than a hook-triggered event).

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

/// Parse the Claude Code status-line JSON from stdin and build a Heartbeat.
pub fn parse_statusline<R: std::io::Read>(r: &mut R) -> Result<StatusLineInput, AdapterError> {
    let mut buf = String::new();
    r.read_to_string(&mut buf)?;
    Ok(serde_json::from_str(&buf)?)
}

pub fn statusline_heartbeat(input: StatusLineInput, pane: Option<String>) -> AgentEvent {
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
