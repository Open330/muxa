//! opencode adapter.
//!
//! opencode exposes broad plugin events and a server `/event` stream. The
//! first muxa integration path is plugin-first: a local TypeScript plugin
//! forwards one event object to `muxa hook opencode --event event`, and this
//! adapter extracts a conservative `AgentEvent` from the payload.

use crate::adapters::hook::{truncate, AdapterError, HookAdapter};
use crate::event::{AgentEvent, AgentId, AgentKind, NotificationLevel};
use serde_json::Value;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Event,
}

pub struct OpencodeAdapter;

impl HookAdapter for OpencodeAdapter {
    type Event = Event;
    type Input = Value;

    const KIND: AgentKind = AgentKind::Opencode;

    fn parse_event(flag: &str) -> Result<Self::Event, AdapterError> {
        match flag {
            "event" | "plugin" => Ok(Event::Event),
            other => Err(AdapterError::UnknownEvent(other.into())),
        }
    }

    fn normalize(_event: Self::Event, input: Self::Input, pane: Option<String>) -> AgentEvent {
        normalize_event(input, pane)
    }
}

pub fn normalize_event(input: Value, pane: Option<String>) -> AgentEvent {
    let ty = event_type(&input).unwrap_or("unknown");
    let at = OffsetDateTime::now_utc();
    let id = AgentId {
        kind: AgentKind::Opencode,
        session_id: session_id(&input).unwrap_or_else(|| "opencode-unknown".into()),
        surface: None,
        pane,
        tmux_socket: None,
        cwd: cwd(&input),
    };

    match ty {
        "session.created" | "session.updated" | "session.status" => AgentEvent::Started { id, at },
        "session.idle" => AgentEvent::TurnStopped {
            id,
            response: response_text(&input),
            at,
        },
        "session.error" => AgentEvent::NotificationFired {
            id,
            level: NotificationLevel::Error,
            message: truncate(
                message_text(&input).unwrap_or_else(|| "opencode error".into()),
                4000,
            ),
            at,
        },
        "permission.asked" => AgentEvent::NotificationFired {
            id,
            level: NotificationLevel::NeedsInput,
            message: truncate(
                message_text(&input).unwrap_or_else(|| "opencode permission requested".into()),
                4000,
            ),
            at,
        },
        "permission.replied" => AgentEvent::ToolCompleted {
            id,
            tool: tool_name(&input),
            success: true,
            at,
        },
        "tool.execute.before" => AgentEvent::ToolStarted {
            id,
            tool: tool_name(&input),
            subagent: None,
            at,
        },
        "tool.execute.after" => AgentEvent::ToolCompleted {
            id,
            tool: tool_name(&input),
            success: !has_error(&input),
            at,
        },
        "message.updated" => {
            if let Some(prompt) = prompt_text(&input) {
                AgentEvent::PromptSubmitted {
                    id,
                    prompt: truncate(prompt, 4000),
                    at,
                }
            } else {
                AgentEvent::Heartbeat {
                    id,
                    model: model(&input),
                    context_used_pct: None,
                    cost_usd: cost_usd(&input),
                    rate_limit_5h_pct: None,
                    rate_limit_5h_resets_at: None,
                    rate_limit_7d_pct: None,
                    rate_limit_7d_resets_at: None,
                    at,
                }
            }
        }
        _ => AgentEvent::Heartbeat {
            id,
            model: model(&input),
            context_used_pct: None,
            cost_usd: cost_usd(&input),
            rate_limit_5h_pct: None,
            rate_limit_5h_resets_at: None,
            rate_limit_7d_pct: None,
            rate_limit_7d_resets_at: None,
            at,
        },
    }
}

fn event_type(v: &Value) -> Option<&str> {
    v.get("type")
        .and_then(Value::as_str)
        .or_else(|| v.pointer("/event/type").and_then(Value::as_str))
}

fn session_id(v: &Value) -> Option<String> {
    first_string(v, &["sessionID", "session_id", "sessionId", "id"])
        .or_else(|| {
            v.pointer("/session/id")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            v.pointer("/properties/sessionID")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
}

fn cwd(v: &Value) -> Option<String> {
    first_string(v, &["cwd", "directory", "worktree"])
}

fn model(v: &Value) -> Option<String> {
    first_string(v, &["model", "modelID", "model_id"])
}

fn cost_usd(v: &Value) -> Option<f64> {
    first_number(v, &["cost", "costUSD", "cost_usd"])
}

fn tool_name(v: &Value) -> String {
    first_string(v, &["tool", "toolName", "tool_name"]).unwrap_or_else(|| "unknown".into())
}

fn prompt_text(v: &Value) -> Option<String> {
    let role = first_string(v, &["role"]);
    if role.as_deref().is_some_and(|role| role != "user") {
        return None;
    }
    first_string(v, &["prompt", "text", "content", "message"])
}

fn response_text(v: &Value) -> Option<String> {
    first_string(v, &["response", "assistant", "output", "content"])
}

fn message_text(v: &Value) -> Option<String> {
    first_string(v, &["message", "error", "text", "title"])
}

fn has_error(v: &Value) -> bool {
    v.get("error").is_some_and(|value| !value.is_null())
        || first_string(v, &["status"]).is_some_and(|s| s == "error" || s == "failed")
}

fn first_string(v: &Value, keys: &[&str]) -> Option<String> {
    find_value(v, keys).and_then(|value| match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    })
}

fn first_number(v: &Value, keys: &[&str]) -> Option<f64> {
    find_value(v, keys).and_then(Value::as_f64)
}

fn find_value<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    match v {
        Value::Object(map) => {
            for key in keys {
                if let Some(value) = map.get(*key) {
                    return Some(value);
                }
            }
            map.values().find_map(|value| find_value(value, keys))
        }
        Value::Array(items) => items.iter().find_map(|value| find_value(value, keys)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_asked_maps_to_waiting_input() {
        let ev = normalize_event(
            serde_json::json!({
                "type": "permission.asked",
                "sessionID": "s1",
                "tool": "bash",
                "message": "Run command?"
            }),
            None,
        );
        assert!(matches!(
            ev,
            AgentEvent::NotificationFired {
                level: NotificationLevel::NeedsInput,
                ..
            }
        ));
    }

    #[test]
    fn user_message_maps_to_prompt() {
        let ev = normalize_event(
            serde_json::json!({
                "type": "message.updated",
                "sessionID": "s1",
                "role": "user",
                "content": "hello"
            }),
            None,
        );
        assert!(matches!(ev, AgentEvent::PromptSubmitted { .. }));
    }
}
