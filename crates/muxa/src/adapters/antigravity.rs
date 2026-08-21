//! Google Antigravity CLI (`agy`) adapter.
//!
//! agy is the successor to the Gemini CLI, and its hook system is **not** the
//! Claude-compatible one [`super::gemini`] targets. Three things differ, and
//! all three break the older adapter silently:
//!
//! 1. **Where.** Hooks live in their own `hooks.json` under a *customization
//!    root* — `~/.gemini/config/hooks.json` globally, or
//!    `<workspace>/.agents/hooks.json` for a trusted folder — not in the
//!    `hooks` key of `~/.gemini/settings.json`.
//! 2. **What.** The lifecycle is `SessionStart` / `PreInvocation` /
//!    `PostInvocation` / `PreToolUse` / `PostToolUse` / `Stop`. There is no
//!    `Notification` and no `SessionEnd`.
//! 3. **Shape.** Payloads are protojson, so every key is camelCase, and the
//!    session identifier is `conversationId`.
//!
//! Wire up in `~/.gemini/config/hooks.json` (what `muxa init` writes):
//!
//! ```json
//! {
//!   "muxa": {
//!     "SessionStart":   [{ "type": "command", "command": "muxa hook agy --event session_start" }],
//!     "PreInvocation":  [{ "type": "command", "command": "muxa hook agy --event pre_invocation" }],
//!     "PostInvocation": [{ "type": "command", "command": "muxa hook agy --event post_invocation" }],
//!     "Stop":           [{ "type": "command", "command": "muxa hook agy --event stop" }],
//!     "PreToolUse":  [{ "matcher": "*", "hooks": [{ "type": "command", "command": "muxa hook agy --event pre_tool_use" }] }],
//!     "PostToolUse": [{ "matcher": "*", "hooks": [{ "type": "command", "command": "muxa hook agy --event post_tool_use" }] }]
//!   }
//! }
//! ```
//!
//! ## `PreToolUse` MUST stay silent
//!
//! agy reads a hook's stdout as its verdict. An empty stdout means "no
//! opinion" and the tool proceeds under agy's own permission policy — but
//! printing *anything* JSON-shaped is read as a decision, and a `PreToolUse`
//! reply without a valid `decision` field **blocks the tool call**
//! (`tool call denied by pre-tool hook`). `muxa hook agy` therefore writes
//! nothing to stdout on any event, and observation stays observation.
//!
//! ## Prompts and responses come from the transcript
//!
//! No agy hook payload carries the user's prompt or the model's reply; they
//! carry a `transcriptPath` instead. `PreInvocation` and `Stop` read it via
//! [`super::antigravity_transcript`], the same arrangement Claude Code's
//! `Stop` hook uses.
//!
//! ## Turn boundaries
//!
//! `invocationNum` counts model calls **within one turn**, resetting to `0`
//! for each new user request (verified against agy 1.1.17 across a two-turn
//! session). `PreInvocation` with `invocationNum == 0` is therefore the turn
//! boundary and the only place a `PromptSubmitted` may be emitted; later
//! invocations in the same turn emit a [`AgentEvent::Heartbeat`] so the row
//! stays warm and its model label stays current without restating the prompt.
//!
//! ## What agy cannot tell us
//!
//! There is no permission/notification hook, so an agy row never reaches
//! `WaitingInput` from hooks alone. The bundled `agy` screen manifest covers
//! that case for panes with no hooks wired; where hooks *are* wired they take
//! precedence and the approval prompt is not observed. There is likewise no
//! session-end hook — `Stop` is a turn boundary, not a session boundary — so
//! agy rows are reaped by pane liveness like Codex's.

use super::hook::{truncate, AdapterError, HookAdapter};
use crate::event::{AgentEvent, AgentId, AgentKind, NotificationLevel};
use serde::Deserialize;
use std::path::Path;
use time::OffsetDateTime;

pub struct AntigravityAdapter;

/// One agy hook payload. Every event shares the "common fields" block and
/// adds its own; modelling them as one optional-heavy struct matches how the
/// other adapters here handle their per-event supersets.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Input {
    /// agy's conversation id — stable across turns of one session, and the
    /// closest analog to the other adapters' `session_id`.
    pub conversation_id: String,
    /// Roots of the open workspace. Empty in print mode (`agy -p`) and
    /// whenever no folder is open, hence `cwd` is best-effort.
    #[serde(default)]
    pub workspace_paths: Vec<String>,
    #[serde(default)]
    pub transcript_path: Option<String>,
    #[serde(default)]
    pub model_name: Option<String>,
    /// `PreToolUse` / `PostToolUse`.
    #[serde(default)]
    pub tool_call: Option<ToolCall>,
    /// `PreInvocation` / `PostInvocation`: 0-based, per turn.
    #[serde(default)]
    pub invocation_num: Option<i64>,
    /// `PostToolUse` (tool failure) and `Stop` (turn failure). Empty string
    /// when nothing went wrong — agy sends the key either way.
    #[serde(default)]
    pub error: Option<String>,
    /// `Stop`: `NO_TOOL_CALL`, `ERROR`, `USER_CANCELED`, `MAX_INVOCATIONS`, …
    #[serde(default)]
    pub termination_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ToolCall {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    SessionStart,
    PreInvocation,
    PostInvocation,
    PreToolUse,
    PostToolUse,
    Stop,
}

impl Input {
    /// The workspace root, when agy knows one. `workspacePaths` is the only
    /// cwd agy exposes: a hook's own process cwd is the directory holding
    /// `hooks.json`, not the agent's, so there is nothing to fall back to.
    fn cwd(&self) -> Option<String> {
        self.workspace_paths.first().cloned()
    }

    fn tool_name(&self) -> String {
        self.tool_call
            .as_ref()
            .and_then(|t| t.name.clone())
            .unwrap_or_else(|| "unknown".into())
    }

    /// True when the payload reports a failure. agy sends `error: ""` on the
    /// happy path, so presence alone proves nothing.
    fn failed(&self) -> bool {
        self.error.as_deref().is_some_and(|e| !e.trim().is_empty())
    }

    fn transcript(&self) -> Option<&Path> {
        self.transcript_path
            .as_deref()
            .filter(|p| !p.is_empty())
            .map(Path::new)
    }

    /// Is this `PreInvocation` the first model call of a new turn?
    ///
    /// A missing `invocationNum` is treated as a turn start on purpose. If a
    /// future agy drops the field, the failure we want is a duplicated prompt
    /// record — not a row stuck at `Idle` through a whole turn because the
    /// only event that moves it to `Working` never fired.
    fn is_turn_start(&self) -> bool {
        self.invocation_num.is_none_or(|n| n <= 0)
    }
}

impl HookAdapter for AntigravityAdapter {
    type Event = Event;
    type Input = Input;
    const KIND: AgentKind = AgentKind::Antigravity;

    fn parse_event(flag: &str) -> Result<Event, AdapterError> {
        Ok(match flag {
            "session_start" => Event::SessionStart,
            "pre_invocation" => Event::PreInvocation,
            "post_invocation" => Event::PostInvocation,
            "pre_tool_use" => Event::PreToolUse,
            "post_tool_use" => Event::PostToolUse,
            "stop" => Event::Stop,
            other => return Err(AdapterError::UnknownEvent(other.into())),
        })
    }

    fn normalize(event: Event, input: Input, pane: Option<String>) -> AgentEvent {
        let id = AgentId {
            kind: AgentKind::Antigravity,
            session_id: input.conversation_id.clone(),
            surface: None,
            pane,
            tmux_socket: None,
            cwd: input.cwd(),
        };
        let at = OffsetDateTime::now_utc();

        match event {
            Event::SessionStart => AgentEvent::Started { id, at },

            // Turn boundary: recover the prompt from the transcript. When it
            // can't be read we still emit `PromptSubmitted` — an empty prompt
            // costs a blank history cell, whereas skipping the event would
            // leave the row `Idle` for the whole turn.
            Event::PreInvocation if input.is_turn_start() => {
                let prompt = input
                    .transcript()
                    .and_then(super::antigravity_transcript::last_user_request)
                    .unwrap_or_default();
                AgentEvent::PromptSubmitted {
                    id,
                    prompt: truncate(prompt, 4_000),
                    at,
                }
            }
            // A later invocation of a turn already in flight, or the tail of
            // any invocation: keep the row warm and refresh the model label
            // without disturbing its state.
            Event::PreInvocation | Event::PostInvocation => heartbeat(id, input.model_name, at),

            Event::PreToolUse => AgentEvent::ToolStarted {
                id,
                tool: input.tool_name(),
                subagent: None,
                at,
            },
            Event::PostToolUse => AgentEvent::ToolCompleted {
                id,
                tool: input.tool_name(),
                success: !input.failed(),
                at,
            },

            // A failed turn surfaces as an error notification rather than a
            // plain stop, so the row goes red instead of quietly idling. The
            // next `PromptSubmitted` lifts it back out (see `state.rs`).
            Event::Stop
                if input.failed() || input.termination_reason.as_deref() == Some("ERROR") =>
            {
                let detail = input
                    .error
                    .as_deref()
                    .map(str::trim)
                    .filter(|e| !e.is_empty())
                    .or(input.termination_reason.as_deref())
                    .unwrap_or("turn failed");
                AgentEvent::NotificationFired {
                    id,
                    level: NotificationLevel::Error,
                    message: truncate(format!("agy: {detail}"), 1_000),
                    at,
                }
            }
            Event::Stop => AgentEvent::TurnStopped {
                id,
                response: input
                    .transcript()
                    .and_then(super::antigravity_transcript::last_assistant_text)
                    .map(|t| truncate(t, 4_000)),
                recap: None,
                ai_title: None,
                // agy's `Stop` payload carries `fullyIdle`, but `idle_confirmed`
                // is reserved for SYNTHETIC producers that OBSERVED the pane go
                // idle. A real hook must leave it false (see `state.rs`).
                idle_confirmed: false,
                at,
            },
        }
    }
}

/// A model-label-only heartbeat. agy stamps `modelName` on every payload,
/// which is more than Codex or the Gemini CLI expose; the rate-limit fields
/// have no agy source and stay `None`.
fn heartbeat(id: AgentId, model: Option<String>, at: OffsetDateTime) -> AgentEvent {
    AgentEvent::Heartbeat {
        id,
        model: model.filter(|m| !m.is_empty()),
        context_used_pct: None,
        cost_usd: None,
        rate_limit_5h_pct: None,
        rate_limit_5h_resets_at: None,
        rate_limit_7d_pct: None,
        rate_limit_7d_resets_at: None,
        at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::run_hook;

    fn normalize(flag: &str, json: &str) -> AgentEvent {
        let event = AntigravityAdapter::parse_event(flag).unwrap();
        let input: Input = serde_json::from_str(json).unwrap();
        AntigravityAdapter::normalize(event, input, Some("%1".into()))
    }

    /// The real agy 1.1.17 `SessionStart` payload, captured from a live run.
    const SESSION_START: &str = r#"{
        "artifactDirectoryPath": "/Users/x/.gemini/antigravity-cli/brain/63a62ba4",
        "conversationId": "63a62ba4-d48d-4379-af4a-b70f78e3693d",
        "modelName": "gemini-3.7-flash-high",
        "transcriptPath": "/Users/x/.gemini/antigravity-cli/brain/63a62ba4/logs/transcript_full.jsonl",
        "workspacePaths": []
    }"#;

    #[test]
    fn parses_the_real_session_start_payload() {
        let ev = normalize("session_start", SESSION_START);
        let id = ev.id();
        assert!(matches!(ev, AgentEvent::Started { .. }));
        assert_eq!(id.kind, AgentKind::Antigravity);
        assert_eq!(id.session_id, "63a62ba4-d48d-4379-af4a-b70f78e3693d");
        assert_eq!(id.pane.as_deref(), Some("%1"));
        // Print mode reports no workspace; cwd must be absent, not "".
        assert_eq!(id.cwd, None);
    }

    /// protojson is camelCase throughout. A `snake_case` reader would fail on
    /// the required key and take the whole hook down, which is exactly how
    /// the Gemini CLI adapter fails against agy.
    #[test]
    fn requires_camel_case_conversation_id() {
        assert!(serde_json::from_str::<Input>(r#"{"conversation_id":"x"}"#).is_err());
        assert!(serde_json::from_str::<Input>(r#"{"conversationId":"x"}"#).is_ok());
    }

    #[test]
    fn workspace_root_becomes_cwd() {
        let ev = normalize(
            "session_start",
            r#"{"conversationId":"c","workspacePaths":["/repo/a","/repo/b"]}"#,
        );
        assert_eq!(ev.id().cwd.as_deref(), Some("/repo/a"));
    }

    #[test]
    fn first_invocation_of_a_turn_submits_the_prompt() {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            f.path(),
            "{\"source\":\"USER_EXPLICIT\",\"type\":\"USER_INPUT\",\"content\":\"<USER_REQUEST>\\nship it\\n</USER_REQUEST>\"}\n",
        )
        .unwrap();
        let json = format!(
            r#"{{"conversationId":"c","invocationNum":0,"transcriptPath":"{}"}}"#,
            f.path().display()
        );
        match normalize("pre_invocation", &json) {
            AgentEvent::PromptSubmitted { prompt, .. } => assert_eq!(prompt, "ship it"),
            other => panic!("expected PromptSubmitted, got {other:?}"),
        }
    }

    /// An unreadable transcript must still move the row to `Working`.
    #[test]
    fn turn_start_without_a_transcript_still_submits() {
        match normalize(
            "pre_invocation",
            r#"{"conversationId":"c","invocationNum":0}"#,
        ) {
            AgentEvent::PromptSubmitted { prompt, .. } => assert_eq!(prompt, ""),
            other => panic!("expected PromptSubmitted, got {other:?}"),
        }
    }

    /// Invocations 1..n belong to a turn already reported. Re-emitting
    /// `PromptSubmitted` would duplicate the prompt in history once per model
    /// call — agy made four in a single turn during capture.
    #[test]
    fn later_invocations_heartbeat_instead_of_resubmitting() {
        let ev = normalize(
            "pre_invocation",
            r#"{"conversationId":"c","invocationNum":3,"modelName":"gemini-3.7-flash-high"}"#,
        );
        match ev {
            AgentEvent::Heartbeat { model, .. } => {
                assert_eq!(model.as_deref(), Some("gemini-3.7-flash-high"));
            }
            other => panic!("expected Heartbeat, got {other:?}"),
        }
    }

    /// Defensive: a payload with no `invocationNum` counts as a turn start,
    /// trading a possible duplicate for a row that never leaves `Idle`.
    #[test]
    fn missing_invocation_num_counts_as_a_turn_start() {
        assert!(matches!(
            normalize("pre_invocation", r#"{"conversationId":"c"}"#),
            AgentEvent::PromptSubmitted { .. }
        ));
    }

    #[test]
    fn post_invocation_is_a_heartbeat() {
        assert!(matches!(
            normalize(
                "post_invocation",
                r#"{"conversationId":"c","invocationNum":0,"modelName":"m"}"#
            ),
            AgentEvent::Heartbeat { .. }
        ));
    }

    #[test]
    fn tool_events_carry_the_tool_call_name() {
        let payload = r#"{
            "conversationId":"c","stepIdx":7,
            "toolCall":{"name":"run_command","args":{"CommandLine":"echo hi"}}
        }"#;
        match normalize("pre_tool_use", payload) {
            AgentEvent::ToolStarted { tool, .. } => assert_eq!(tool, "run_command"),
            other => panic!("expected ToolStarted, got {other:?}"),
        }
        match normalize("post_tool_use", payload) {
            AgentEvent::ToolCompleted { tool, success, .. } => {
                assert_eq!(tool, "run_command");
                assert!(success, "empty/absent error means the tool succeeded");
            }
            other => panic!("expected ToolCompleted, got {other:?}"),
        }
    }

    /// agy sends `error: ""` on success, so only a non-empty string may flip
    /// `success` to false.
    #[test]
    fn empty_error_string_is_not_a_failure() {
        for payload in [
            r#"{"conversationId":"c","toolCall":{"name":"t"},"error":""}"#,
            r#"{"conversationId":"c","toolCall":{"name":"t"},"error":"  "}"#,
        ] {
            match normalize("post_tool_use", payload) {
                AgentEvent::ToolCompleted { success, .. } => assert!(success, "{payload}"),
                other => panic!("expected ToolCompleted, got {other:?}"),
            }
        }
        match normalize(
            "post_tool_use",
            r#"{"conversationId":"c","toolCall":{"name":"t"},"error":"exit status 1"}"#,
        ) {
            AgentEvent::ToolCompleted { success, .. } => assert!(!success),
            other => panic!("expected ToolCompleted, got {other:?}"),
        }
    }

    #[test]
    fn stop_reads_the_response_from_the_transcript() {
        let f = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            f.path(),
            "{\"source\":\"MODEL\",\"type\":\"PLANNER_RESPONSE\",\"content\":\"all done\"}\n",
        )
        .unwrap();
        let json = format!(
            r#"{{"conversationId":"c","terminationReason":"NO_TOOL_CALL","error":"","fullyIdle":true,"transcriptPath":"{}"}}"#,
            f.path().display()
        );
        match normalize("stop", &json) {
            AgentEvent::TurnStopped {
                response,
                idle_confirmed,
                ..
            } => {
                assert_eq!(response.as_deref(), Some("all done"));
                assert!(
                    !idle_confirmed,
                    "a real hook must never claim a synthetic idle observation",
                );
            }
            other => panic!("expected TurnStopped, got {other:?}"),
        }
    }

    #[test]
    fn a_failed_turn_raises_an_error_notification() {
        for payload in [
            r#"{"conversationId":"c","terminationReason":"ERROR","error":"model unavailable"}"#,
            r#"{"conversationId":"c","terminationReason":"ERROR","error":""}"#,
            r#"{"conversationId":"c","terminationReason":"NO_TOOL_CALL","error":"boom"}"#,
        ] {
            match normalize("stop", payload) {
                AgentEvent::NotificationFired { level, message, .. } => {
                    assert_eq!(level, NotificationLevel::Error, "{payload}");
                    assert!(message.starts_with("agy: "), "{message}");
                }
                other => panic!("expected NotificationFired for {payload}, got {other:?}"),
            }
        }
    }

    /// A cancelled or capped turn is not an error — it stops normally.
    #[test]
    fn a_cancelled_turn_is_an_ordinary_stop() {
        assert!(matches!(
            normalize(
                "stop",
                r#"{"conversationId":"c","terminationReason":"USER_CANCELED","error":""}"#
            ),
            AgentEvent::TurnStopped { .. }
        ));
    }

    #[test]
    fn unknown_event_flag_is_rejected() {
        assert!(matches!(
            AntigravityAdapter::parse_event("notification"),
            Err(AdapterError::UnknownEvent(_)),
        ));
    }

    /// Every flag `muxa init` writes into `hooks.json` must parse. A typo
    /// here would only surface as a silent hook failure inside agy.
    #[test]
    fn every_wired_event_flag_parses() {
        for flag in [
            "session_start",
            "pre_invocation",
            "post_invocation",
            "pre_tool_use",
            "post_tool_use",
            "stop",
        ] {
            assert!(
                AntigravityAdapter::parse_event(flag).is_ok(),
                "flag {flag} must parse",
            );
        }
    }

    #[test]
    fn run_hook_reads_a_payload_from_stdin() {
        let ev = run_hook::<AntigravityAdapter, _>("session_start", &mut SESSION_START.as_bytes())
            .unwrap();
        assert_eq!(ev.id().kind, AgentKind::Antigravity);
    }
}
