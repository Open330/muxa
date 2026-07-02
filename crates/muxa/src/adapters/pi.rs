//! Pi coding agent adapter.
//!
//! Pi (`@earendil-works/pi-coding-agent`) doesn't expose a shell-hook
//! system like Claude Code / Codex / Gemini. Instead it ships an
//! in-process TypeScript extension API (`pi.on("agent_start", …)`).
//! The muxa integration is therefore plugin-first, mirroring the
//! opencode path: a local TypeScript extension forwards one event
//! object to `muxa hook pi --event event`, and this adapter extracts a
//! conservative `AgentEvent` from the payload.
//!
//! The shipped extension is written to
//! `~/.pi/agent/extensions/muxa/index.ts` by the `pi-hooks` init
//! component. The extension is responsible for filling in a stable
//! `session_id`, the `cwd`, the active `model`, and the pane id (read
//! from `$TMUX_PANE` / `$ZELLIJ_PANE_ID`). See
//! `crates/muxa-cli/src/init/files/pi.rs` for the exact payload schema
//! the extension emits.
//!
//! Wire format (stdin JSON forwarded by the extension):
//!
//! ```jsonc
//! { "type": "session_start", "session_id": "pi-…", "cwd": "/repo",
//!   "pane": "%5", "pid": 12345, "model": "claude-sonnet-4" }
//! ```

use crate::adapters::hook::{truncate, AdapterError, HookAdapter};
use crate::event::{AgentEvent, AgentId, AgentKind, NotificationLevel};
use serde_json::Value;
use time::OffsetDateTime;

/// Single event channel — like opencode, pi forwards one opaque JSON
/// blob per lifecycle signal and the adapter discriminates on
/// `type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Event,
}

pub struct PiAdapter;

impl HookAdapter for PiAdapter {
    type Event = Event;
    type Input = Value;

    const KIND: AgentKind = AgentKind::Pi;

    fn parse_event(flag: &str) -> Result<Self::Event, AdapterError> {
        match flag {
            "event" | "pi" => Ok(Event::Event),
            other => Err(AdapterError::UnknownEvent(other.into())),
        }
    }

    fn normalize(_event: Self::Event, input: Self::Input, pane: Option<String>) -> AgentEvent {
        normalize_event(input, pane)
    }
}

pub fn normalize_event(input: Value, fallback_pane: Option<String>) -> AgentEvent {
    let ty = event_type(&input).unwrap_or("unknown");
    let at = OffsetDateTime::now_utc();
    let pane = first_string(&input, &["pane"]).or(fallback_pane);
    let id = AgentId {
        kind: AgentKind::Pi,
        session_id: session_id(&input).unwrap_or_else(|| "pi-unknown".into()),
        surface: None,
        pane,
        cwd: first_string(&input, &["cwd", "directory"]),
    };

    match ty {
        "session_start" => AgentEvent::Started { id, at },
        "session_shutdown" => AgentEvent::SessionEnded { id, at },
        "before_agent_start" => AgentEvent::PromptSubmitted {
            id,
            prompt: truncate(
                first_string(&input, &["prompt", "text"]).unwrap_or_default(),
                4_000,
            ),
            at,
        },
        // tool_execution_* is Pi's observation layer (vs the preflight
        // `tool_call`/`tool_result`), and carries `isError` + `result`.
        "tool_execution_start" => AgentEvent::ToolStarted {
            id,
            tool: tool_name(&input),
            at,
        },
        "tool_execution_end" => AgentEvent::ToolCompleted {
            id,
            tool: tool_name(&input),
            success: !has_error(&input),
            at,
        },
        "agent_end" => AgentEvent::TurnStopped {
            id,
            response: first_string(&input, &["response", "content"]),
            at,
        },
        // Pi surfaces a permission/confirm request as a notification.
        "permission_asked" | "waiting_input" => AgentEvent::NotificationFired {
            id,
            level: NotificationLevel::NeedsInput,
            message: truncate(
                first_string(&input, &["message", "text"])
                    .unwrap_or_else(|| "pi needs input".into()),
                4_000,
            ),
            at,
        },
        // Everything else — including `turn_end`, which the extension
        // attaches model + cost to once per turn instead of per streamed
        // message to keep daemon churn low — collapses to a Heartbeat so
        // the daemon refreshes `last_activity_at` without polluting the
        // state machine.
        _ => AgentEvent::Heartbeat {
            id,
            model: first_string(&input, &["model", "model_id"]),
            context_used_pct: first_percent(&input, &["context_used_pct", "context_used"]),
            cost_usd: first_number(&input, &["cost_usd", "cost"]),
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
    first_string(v, &["session_id", "sessionId", "sessionID", "id"])
}

fn tool_name(v: &Value) -> String {
    first_string(v, &["tool", "tool_name", "toolName"]).unwrap_or_else(|| "unknown".into())
}

fn has_error(v: &Value) -> bool {
    v.get("error").is_some_and(|value| !value.is_null())
        || first_string(v, &["success"]).is_some_and(|s| s == "false")
        || first_string(v, &["status"]).is_some_and(|s| s == "error" || s == "failed")
}

fn first_string(v: &Value, keys: &[&str]) -> Option<String> {
    find_value(v, keys).and_then(|value| match value {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    })
}

fn first_number(v: &Value, keys: &[&str]) -> Option<f64> {
    find_value(v, keys).and_then(Value::as_f64)
}

/// Like `first_number` but narrowed to `f32` for percentage fields such
/// as `context_used_pct`. The f64→f32 narrowing is semantically lossless
/// for a 0–100 percentage, so the cast is allowed here only.
#[expect(clippy::cast_possible_truncation)]
fn first_percent(v: &Value, keys: &[&str]) -> Option<f32> {
    first_number(v, keys).map(|n| n as f32)
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
    fn session_start_maps_to_started() {
        let ev = normalize_event(
            serde_json::json!({
                "type": "session_start",
                "session_id": "pi-1",
                "cwd": "/repo",
                "pane": "%5"
            }),
            None,
        );
        assert!(matches!(ev, AgentEvent::Started { .. }));
    }

    #[test]
    fn prompt_maps_to_prompt_submitted() {
        let ev = normalize_event(
            serde_json::json!({
                "type": "before_agent_start",
                "session_id": "pi-1",
                "prompt": "ship it"
            }),
            None,
        );
        assert!(matches!(ev, AgentEvent::PromptSubmitted { .. }));
    }

    #[test]
    fn unknown_type_falls_back_to_heartbeat() {
        let ev = normalize_event(
            serde_json::json!({
                "type": "turn_start",
                "session_id": "pi-1",
                "model": "claude-sonnet-4"
            }),
            None,
        );
        assert!(matches!(ev, AgentEvent::Heartbeat { .. }));
    }

    #[test]
    fn pane_falls_back_to_env_when_payload_omits_it() {
        let ev = normalize_event(
            serde_json::json!({ "type": "session_start", "session_id": "pi-1" }),
            Some("%9".into()),
        );
        match ev {
            AgentEvent::Started { id, .. } => assert_eq!(id.pane.as_deref(), Some("%9")),
            _ => panic!("expected Started"),
        }
    }

    #[test]
    fn tool_execution_end_success_maps_to_completed() {
        let ev = normalize_event(
            serde_json::json!({
                "type": "tool_execution_end",
                "session_id": "pi-1",
                "tool": "bash",
                "success": true
            }),
            None,
        );
        match ev {
            AgentEvent::ToolCompleted { tool, success, .. } => {
                assert_eq!(tool, "bash");
                assert!(success);
            }
            _ => panic!("expected ToolCompleted"),
        }
    }

    #[test]
    fn tool_execution_end_error_marks_unsuccessful() {
        let ev = normalize_event(
            serde_json::json!({
                "type": "tool_execution_end",
                "session_id": "pi-1",
                "tool": "bash",
                "success": false
            }),
            None,
        );
        match ev {
            AgentEvent::ToolCompleted { success, .. } => assert!(!success),
            _ => panic!("expected ToolCompleted"),
        }
    }

    #[test]
    fn turn_end_carries_model_and_cost_as_heartbeat() {
        let ev = normalize_event(
            serde_json::json!({
                "type": "turn_end",
                "session_id": "pi-1",
                "model": "claude-sonnet-4",
                "cost_usd": 0.42
            }),
            None,
        );
        match ev {
            AgentEvent::Heartbeat {
                model, cost_usd, ..
            } => {
                assert_eq!(model.as_deref(), Some("claude-sonnet-4"));
                assert_eq!(cost_usd, Some(0.42));
            }
            _ => panic!("expected Heartbeat"),
        }
    }
}
