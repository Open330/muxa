//! opencode adapter.
//!
//! opencode has no shell hooks; it has an in-process plugin API. `tend init`
//! installs a small plugin (`~/.config/opencode/plugins/tend.ts`, see
//! `tend-cli/src/init/files/opencode.rs`) that forwards a filtered subset of
//! opencode's own events, one JSON object per `tend hook opencode --event
//! event` call. This module maps those objects to [`AgentEvent`]s.
//!
//! Every field read here is one opencode documents in `@opencode-ai/sdk`'s
//! event types (checked against 1.19.5, both the v1 and v2 shapes): the
//! session id, session status, tool part state, permission and question
//! prompts. Nothing is searched for by key name at arbitrary depth, and an
//! event this module does not know maps to nothing rather than to a guess.
//!
//! The plugin does the stateful part, because it lives as long as opencode
//! and a hook process does not:
//!
//! - **Child sessions are dropped.** A subagent task runs in its own session
//!   (`Session.parentID` set); its events would otherwise register as a
//!   second agent on the same pane.
//! - **Tool status is de-duplicated.** `message.part.updated` fires on every
//!   streamed change; the plugin forwards a tool part only when its
//!   `state.status` changes, trimmed to tool name, call id and status.
//! - **The prompt comes from `chat.message`.** Its output carries the user
//!   message's parts; the plugin joins the non-synthetic text parts and sends
//!   them as `tend.prompt`.
//! - **The response comes from the assistant's text parts**, collected as
//!   they stream and attached to `session.idle` as `tend.response`.
//! - **The pane follows the active session.** When events start arriving for
//!   a different top-level session (the TUI switched to it, or resumed one),
//!   the plugin sends `tend.session.active` first, which becomes `Started` and
//!   moves the pane's row to that session.
//!
//! Payload envelope: `{ "type": "...", "properties": {...}, "tend": {
//! "directory"?, "response"? } }`.

use crate::adapters::hook::truncate;
use crate::event::{AgentEvent, AgentId, AgentKind, NotificationLevel};
use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;

/// What the tend plugin attaches beside opencode's own `properties`.
#[derive(Debug, Default, Deserialize)]
struct TendExtra {
    /// The plugin's `directory` (opencode's working directory).
    #[serde(default)]
    directory: Option<String>,
    /// Text of the turn's last assistant message, on `session.idle`.
    #[serde(default)]
    response: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(rename = "type")]
    ty: String,
    #[serde(default)]
    properties: Value,
    #[serde(default)]
    tend: TendExtra,
}

/// Why an opencode payload produced no event.
#[derive(Debug, thiserror::Error)]
pub enum OpencodeError {
    #[error("unknown opencode hook event {0:?}; expected `event`")]
    UnknownFlag(String),
    #[error("opencode payload is not a forwarded event: {0}")]
    Payload(#[from] serde_json::Error),
}

/// Validate the `--event` flag. The plugin always passes `event`; `plugin`
/// is what an earlier tend plugin passed.
pub fn check_flag(flag: &str) -> Result<(), OpencodeError> {
    match flag {
        "event" | "plugin" => Ok(()),
        other => Err(OpencodeError::UnknownFlag(other.into())),
    }
}

/// Parse one forwarded payload. `Ok(None)` means a well-formed event that
/// carries no state change for tend (an unknown type, a child session's
/// event, or an aborted turn whose `session.idle` follows).
pub fn parse(text: &str, pane: Option<String>) -> Result<Option<AgentEvent>, OpencodeError> {
    let envelope: Envelope = serde_json::from_str(text)?;
    Ok(normalize(&envelope, pane, OffsetDateTime::now_utc()))
}

fn normalize(envelope: &Envelope, pane: Option<String>, at: OffsetDateTime) -> Option<AgentEvent> {
    let p = &envelope.properties;
    let session = str_at(p, "/sessionID")
        .or_else(|| str_at(p, "/info/sessionID"))
        .or_else(|| str_at(p, "/part/sessionID"))?;
    let id = AgentId {
        kind: AgentKind::Opencode,
        session_id: session.to_string(),
        surface: None,
        pane,
        tmux_socket: None,
        cwd: str_at(p, "/info/directory")
            .map(str::to_string)
            .or_else(|| envelope.tend.directory.clone()),
    };

    match envelope.ty.as_str() {
        "tend.session.active" | "session.created" => Some(AgentEvent::Started { id, at }),
        "session.deleted" => Some(AgentEvent::SessionEnded { id, at }),
        "tend.prompt" => Some(AgentEvent::PromptSubmitted {
            id,
            prompt: truncate(str_at(p, "/text").unwrap_or_default().to_string(), 4000),
            at,
        }),
        "session.idle" => Some(AgentEvent::TurnStopped {
            id,
            response: envelope
                .tend
                .response
                .clone()
                .filter(|text| !text.trim().is_empty())
                .map(|text| truncate(text, 8000)),
            recap: None,
            ai_title: None,
            idle_confirmed: false,
            at,
        }),
        "message.part.updated" => tool_event(p, id, at),
        "message.updated" => {
            (str_at(p, "/info/role") == Some("assistant")).then(|| AgentEvent::Heartbeat {
                id,
                model: str_at(p, "/info/modelID").map(str::to_string),
                context_used_pct: None,
                cost_usd: p.pointer("/info/cost").and_then(Value::as_f64),
                rate_limit_5h_pct: None,
                rate_limit_5h_resets_at: None,
                rate_limit_7d_pct: None,
                rate_limit_7d_resets_at: None,
                at,
            })
        }
        other => attention_event(other, p, id, at),
    }
}

/// Retries, errors, permission prompts, questions and their answers.
fn attention_event(ty: &str, p: &Value, id: AgentId, at: OffsetDateTime) -> Option<AgentEvent> {
    match ty {
        "session.status" => {
            (str_at(p, "/status/type") == Some("retry")).then(|| AgentEvent::NotificationFired {
                id,
                level: NotificationLevel::Warning,
                message: truncate(
                    str_at(p, "/status/message")
                        .unwrap_or("opencode is retrying the request")
                        .to_string(),
                    4000,
                ),
                at,
            })
        }
        "session.error" => {
            let name = str_at(p, "/error/name");
            // Esc in the TUI aborts the turn; `session.idle` follows and
            // carries the real state change. An abort is not a failure.
            if name == Some("MessageAbortedError") {
                return None;
            }
            let message = str_at(p, "/error/data/message")
                .or(name)
                .unwrap_or("opencode error");
            Some(AgentEvent::NotificationFired {
                id,
                level: NotificationLevel::Error,
                message: truncate(message.to_string(), 4000),
                at,
            })
        }
        // v1 `permission.updated` carries the flat `Permission`; v2 has
        // `permission.asked` and `permission.v2.asked`.
        "permission.updated" | "permission.asked" | "permission.v2.asked" => {
            Some(AgentEvent::NotificationFired {
                id,
                level: NotificationLevel::NeedsInput,
                message: truncate(permission_message(p), 4000),
                at,
            })
        }
        "question.asked" | "question.v2.asked" => Some(AgentEvent::NotificationFired {
            id,
            level: NotificationLevel::NeedsChoice,
            message: truncate(
                str_at(p, "/questions/0/question")
                    .or_else(|| str_at(p, "/questions/0/header"))
                    .unwrap_or("opencode asked a question")
                    .to_string(),
                4000,
            ),
            at,
        }),
        // The operator answered. `ToolCompleted` only moves a waiting row
        // back to working, which is exactly what an answer means.
        "permission.replied" | "permission.v2.replied" => {
            let reply = str_at(p, "/reply").or_else(|| str_at(p, "/response"));
            Some(AgentEvent::ToolCompleted {
                id,
                tool: "permission".into(),
                success: !matches!(reply, Some("reject")),
                at,
            })
        }
        "question.replied"
        | "question.v2.replied"
        | "question.rejected"
        | "question.v2.rejected" => Some(AgentEvent::ToolCompleted {
            id,
            tool: "question".into(),
            success: !ty.ends_with("rejected"),
            at,
        }),
        _ => None,
    }
}

/// A `ToolPart` whose `state.status` changed (the plugin forwards nothing
/// else). `pending` is not a start: the call has not been admitted yet.
fn tool_event(p: &Value, id: AgentId, at: OffsetDateTime) -> Option<AgentEvent> {
    if str_at(p, "/part/type") != Some("tool") {
        return None;
    }
    let tool = str_at(p, "/part/tool").unwrap_or("tool").to_string();
    match str_at(p, "/part/state/status")? {
        "running" => Some(AgentEvent::ToolStarted {
            id,
            command: (tool == "bash")
                .then(|| str_at(p, "/part/state/command").map(str::to_string))
                .flatten(),
            tool,
            subagent: None,
            at,
        }),
        status @ ("completed" | "error") => Some(AgentEvent::ToolCompleted {
            id,
            tool,
            success: status == "completed",
            at,
        }),
        _ => None,
    }
}

fn permission_message(p: &Value) -> String {
    // v1: `title` ("Run `rm -rf build`?"); v2: the permission name and the
    // patterns it covers; v2 rules engine: action and resources.
    if let Some(title) = str_at(p, "/title") {
        return title.to_string();
    }
    let (name, list) = match str_at(p, "/permission") {
        Some(name) => (Some(name), p.get("patterns")),
        None => (str_at(p, "/action"), p.get("resources")),
    };
    let targets: Vec<&str> = list
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    match (name, targets.is_empty()) {
        (Some(name), true) => format!("opencode asks permission: {name}"),
        (Some(name), false) => format!("opencode asks permission: {name} {}", targets.join(" ")),
        (None, _) => "opencode permission requested".into(),
    }
}

fn str_at<'a>(v: &'a Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(payload: Value) -> Option<AgentEvent> {
        parse(&payload.to_string(), Some("%7".into())).unwrap()
    }

    #[test]
    fn session_created_starts_the_session_with_its_directory() {
        let out = ev(json!({
            "type": "session.created",
            "properties": { "sessionID": "ses_1", "info": { "id": "ses_1", "directory": "/w/app" } },
            "tend": { "directory": "/w" }
        }));
        let Some(AgentEvent::Started { id, .. }) = out else {
            panic!("{out:?}")
        };
        assert_eq!(id.session_id, "ses_1");
        assert_eq!(id.kind, AgentKind::Opencode);
        assert_eq!(id.pane.as_deref(), Some("%7"));
        assert_eq!(id.cwd.as_deref(), Some("/w/app"));
    }

    #[test]
    fn active_session_switch_is_a_start_with_the_plugin_directory() {
        let out = ev(json!({
            "type": "tend.session.active",
            "properties": { "sessionID": "ses_2" },
            "tend": { "directory": "/w" }
        }));
        let Some(AgentEvent::Started { id, .. }) = out else {
            panic!("{out:?}")
        };
        assert_eq!(id.cwd.as_deref(), Some("/w"));
    }

    #[test]
    fn prompt_and_idle_carry_the_captured_text() {
        let prompt = ev(
            json!({ "type": "tend.prompt", "properties": { "sessionID": "s", "text": "fix it" } }),
        );
        assert!(
            matches!(prompt, Some(AgentEvent::PromptSubmitted { ref prompt, .. }) if prompt == "fix it")
        );

        let idle = ev(json!({
            "type": "session.idle",
            "properties": { "sessionID": "s" },
            "tend": { "response": "Fixed." }
        }));
        assert!(
            matches!(idle, Some(AgentEvent::TurnStopped { response: Some(ref r), .. }) if r == "Fixed.")
        );

        let bare = ev(
            json!({ "type": "session.idle", "properties": { "sessionID": "s" }, "tend": { "response": "  " } }),
        );
        assert!(matches!(
            bare,
            Some(AgentEvent::TurnStopped { response: None, .. })
        ));
    }

    #[test]
    fn permission_prompts_in_every_shape_wait_for_input() {
        for (payload, message) in [
            (
                json!({ "type": "permission.updated", "properties": {
                    "id": "p", "type": "bash", "sessionID": "s", "messageID": "m",
                    "title": "Run `make test`?", "metadata": {}, "time": { "created": 1 } } }),
                "Run `make test`?",
            ),
            (
                json!({ "type": "permission.asked", "properties": {
                    "id": "p", "sessionID": "s", "permission": "bash",
                    "patterns": ["make test"], "metadata": {}, "always": [] } }),
                "opencode asks permission: bash make test",
            ),
            (
                json!({ "type": "permission.v2.asked", "properties": {
                    "id": "p", "sessionID": "s", "action": "edit", "resources": ["src/a.rs"] } }),
                "opencode asks permission: edit src/a.rs",
            ),
        ] {
            let out = ev(payload);
            assert!(
                matches!(out, Some(AgentEvent::NotificationFired { level: NotificationLevel::NeedsInput, message: ref m, .. }) if m == message),
                "{out:?}"
            );
        }
    }

    #[test]
    fn replies_release_the_wait_and_questions_ask_for_a_choice() {
        let q = ev(json!({ "type": "question.asked", "properties": {
            "id": "q", "sessionID": "s",
            "questions": [{ "question": "Which database?", "header": "DB", "options": [] }] } }));
        assert!(
            matches!(q, Some(AgentEvent::NotificationFired { level: NotificationLevel::NeedsChoice, message: ref m, .. }) if m == "Which database?")
        );

        let v2 = ev(
            json!({ "type": "permission.replied", "properties": { "sessionID": "s", "requestID": "p", "reply": "reject" } }),
        );
        assert!(matches!(
            v2,
            Some(AgentEvent::ToolCompleted { success: false, .. })
        ));
        let v1 = ev(
            json!({ "type": "permission.replied", "properties": { "sessionID": "s", "permissionID": "p", "response": "once" } }),
        );
        assert!(matches!(
            v1,
            Some(AgentEvent::ToolCompleted { success: true, .. })
        ));
        let answered = ev(
            json!({ "type": "question.replied", "properties": { "sessionID": "s", "requestID": "q", "answers": [] } }),
        );
        assert!(matches!(
            answered,
            Some(AgentEvent::ToolCompleted { success: true, .. })
        ));
    }

    #[test]
    fn tool_status_changes_map_to_start_and_completion() {
        let part = |status: &str| {
            json!({ "type": "message.part.updated", "properties": { "sessionID": "s", "part": {
                "type": "tool", "tool": "bash", "callID": "c1",
                "state": { "status": status, "command": "cargo test" } } } })
        };
        assert!(
            matches!(ev(part("running")), Some(AgentEvent::ToolStarted { ref tool, command: Some(ref c), .. }) if tool == "bash" && c == "cargo test")
        );
        assert!(matches!(
            ev(part("completed")),
            Some(AgentEvent::ToolCompleted { success: true, .. })
        ));
        assert!(matches!(
            ev(part("error")),
            Some(AgentEvent::ToolCompleted { success: false, .. })
        ));
        assert!(ev(part("pending")).is_none());
    }

    #[test]
    fn errors_report_but_an_abort_does_not() {
        let failed = ev(
            json!({ "type": "session.error", "properties": { "sessionID": "s",
            "error": { "name": "APIError", "data": { "message": "overloaded", "isRetryable": true } } } }),
        );
        assert!(
            matches!(failed, Some(AgentEvent::NotificationFired { level: NotificationLevel::Error, message: ref m, .. }) if m == "overloaded")
        );
        let aborted = ev(
            json!({ "type": "session.error", "properties": { "sessionID": "s",
            "error": { "name": "MessageAbortedError", "data": { "message": "aborted" } } } }),
        );
        assert!(aborted.is_none());
        // `session.error.sessionID` is optional upstream; no session, no row.
        assert!(ev(json!({ "type": "session.error", "properties": { "error": { "name": "UnknownError" } } })).is_none());
    }

    #[test]
    fn assistant_completion_reports_model_and_cost() {
        let out = ev(
            json!({ "type": "message.updated", "properties": { "sessionID": "s", "info": {
            "id": "m", "sessionID": "s", "role": "assistant", "modelID": "claude-sonnet-5-5",
            "providerID": "anthropic", "cost": 0.25 } } }),
        );
        assert!(
            matches!(out, Some(AgentEvent::Heartbeat { model: Some(ref m), cost_usd: Some(c), .. }) if m == "claude-sonnet-5-5" && (c - 0.25).abs() < f64::EPSILON)
        );
    }

    #[test]
    fn unknown_events_and_unknown_flags_change_nothing() {
        assert!(ev(json!({ "type": "file.edited", "properties": { "file": "a.rs" } })).is_none());
        assert!(ev(
            json!({ "type": "todo.updated", "properties": { "sessionID": "s", "todos": [] } })
        )
        .is_none());
        assert!(check_flag("event").is_ok());
        assert!(check_flag("session_start").is_err());
        assert!(parse("not json", None).is_err());
    }

    /// The previous adapter searched every nested object for keys such as
    /// `id` and `content`, so a message id could become the session id. An
    /// id that is not at a documented location is never used.
    #[test]
    fn a_nested_id_is_never_taken_for_the_session() {
        assert!(
            ev(json!({ "type": "session.idle", "properties": { "info": { "id": "msg_1" } } }))
                .is_none()
        );
    }
}
