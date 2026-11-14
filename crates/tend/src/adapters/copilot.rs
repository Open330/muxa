//! GitHub Copilot CLI (`copilot`) adapter.
//!
//! Copilot runs every `*.json` under `$COPILOT_HOME/hooks/` (default
//! `~/.copilot/hooks/`); `tend init` writes `tend.json` there. Each hook
//! gets one JSON object on stdin. The camelCase event names are used
//! (`sessionStart`, `userPromptSubmitted`, …): their payloads are the
//! documented native shape, camelCase fields with `sessionId` and `cwd`.
//! The `PascalCase` names switch to a Claude-compatible `snake_case` shape and
//! are not used.
//!
//! Checked against Copilot CLI 1.0.110 (see
//! `docs/proposals/AGENT_CLI_SURVEY.md` and the replay in
//! `testdata/copilot-1.0.110-hooks.jsonl`):
//!
//! | Hook | Event |
//! | --- | --- |
//! | `sessionStart` | `Started` |
//! | `userPromptSubmitted` | `PromptSubmitted` (`prompt`) |
//! | `preToolUse` | `ToolStarted` (`toolName`, `toolArgs.command`) |
//! | `postToolUse` / `postToolUseFailure` | `ToolCompleted` |
//! | `notification` (`permission_prompt`) | `NotificationFired` waiting |
//! | `agentStop` | `TurnStopped`, answer read from `transcriptPath` |
//! | `errorOccurred` | `NotificationFired` error |
//! | `sessionEnd` | `SessionEnded` |
//!
//! `permissionRequest` is **not** a wait: on 1.0.110 it also fires for a
//! call auto-approved 30 ms later. Only `notification` with
//! `notification_type: "permission_prompt"` means the prompt is on screen,
//! so `permissionRequest` maps to nothing.
//!
//! ## stdout stays empty
//!
//! Copilot reads a `preToolUse` or `permissionRequest` hook's stdout as a
//! decision. `tend hook copilot` prints nothing on any event and exits 0
//! whatever happens, so observing can never deny a tool.
//!
//! ## What hooks miss
//!
//! Esc on a permission prompt cancels the turn and fires no hook at all:
//! no `postToolUse`, no `agentStop`. The row would stay waiting. Copilot
//! records the cancel only in its session log (`abort`,
//! `assistant.turn_end` in `session-state/<id>/events.jsonl`); reading
//! that is [`super::copilot_session`]'s job, not a hook's.

use super::hook::{truncate, truncate_response};
use crate::event::{AgentEvent, AgentId, AgentKind, NotificationLevel};
use serde::Deserialize;
use serde_json::Value;
use std::path::Path;
use time::OffsetDateTime;

/// Starts the message of a Copilot permission prompt.
pub const PERMISSION_PREFIX: &str = "copilot permission: ";

/// The `--event` flags `tend init` wires, one per Copilot hook.
pub const EVENTS: &[(&str, &str)] = &[
    ("sessionStart", "session_start"),
    ("userPromptSubmitted", "user_prompt_submitted"),
    ("preToolUse", "pre_tool_use"),
    ("postToolUse", "post_tool_use"),
    ("postToolUseFailure", "post_tool_use_failure"),
    ("notification", "notification"),
    ("agentStop", "agent_stop"),
    ("errorOccurred", "error_occurred"),
    ("sessionEnd", "session_end"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    SessionStart,
    UserPromptSubmitted,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    Notification,
    AgentStop,
    ErrorOccurred,
    SessionEnd,
    /// A hook tend accepts but takes nothing from (`permission_request`,
    /// subagent and compaction hooks), so a hooks file written by a newer
    /// tend never makes an older one fail.
    Ignored,
}

impl Event {
    /// The typed event for an `--event` flag. Unknown flags are an error so
    /// a typo in a hand-written hooks file shows up in the debug log.
    pub fn parse(flag: &str) -> Result<Self, super::AdapterError> {
        Ok(match flag {
            "session_start" => Self::SessionStart,
            "user_prompt_submitted" => Self::UserPromptSubmitted,
            "pre_tool_use" => Self::PreToolUse,
            "post_tool_use" => Self::PostToolUse,
            "post_tool_use_failure" => Self::PostToolUseFailure,
            "notification" => Self::Notification,
            "agent_stop" => Self::AgentStop,
            "error_occurred" => Self::ErrorOccurred,
            "session_end" => Self::SessionEnd,
            "permission_request" | "subagent_start" | "subagent_stop" | "pre_compact" => {
                Self::Ignored
            }
            other => return Err(super::AdapterError::UnknownEvent(other.into())),
        })
    }
}

/// One Copilot hook payload: the shared fields plus every event's own, all
/// optional.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Input {
    pub session_id: String,
    #[serde(default)]
    pub cwd: Option<String>,
    /// `userPromptSubmitted`.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Tool hooks.
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub tool_args: Option<Value>,
    /// `postToolUse`: `{"resultType": "success", …}`.
    #[serde(default)]
    pub tool_result: Option<Value>,
    /// `notification`. Copilot sends this one field in `snake_case` even on
    /// the camelCase event.
    #[serde(default, rename = "notification_type")]
    pub notification_type: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    /// `agentStop`: the session's `events.jsonl`.
    #[serde(default)]
    pub transcript_path: Option<String>,
    /// When Copilot fired the hook (Unix milliseconds). Copilot's own log
    /// record of an `agentStop` carries the same value.
    #[serde(default)]
    pub timestamp: Option<i64>,
    /// `agentStop`: `end_turn` on a clean turn.
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// `errorOccurred`: a string or an object with `message`.
    #[serde(default)]
    pub error: Option<Value>,
}

impl Input {
    fn tool(&self) -> String {
        self.tool_name
            .clone()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "unknown".into())
    }

    fn command(&self) -> Option<String> {
        self.tool_args
            .as_ref()?
            .get("command")?
            .as_str()
            .map(|command| truncate(command.to_string(), 1_000))
    }

    /// `postToolUse` reports its outcome in `toolResult.resultType`; a
    /// payload without one is taken as success, as Claude's is.
    fn tool_succeeded(&self) -> bool {
        self.tool_result
            .as_ref()
            .and_then(|result| result.get("resultType"))
            .and_then(Value::as_str)
            .is_none_or(|kind| kind == "success")
    }

    fn error_text(&self) -> String {
        match &self.error {
            Some(Value::String(text)) => text.clone(),
            Some(value) => value
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| value.to_string(), str::to_string),
            None => self.message.clone().unwrap_or_else(|| "error".into()),
        }
    }
}

/// Translate one hook payload, or `None` for a hook tend takes nothing
/// from.
pub fn parse(
    flag: &str,
    text: &str,
    pane: Option<String>,
) -> Result<Option<AgentEvent>, super::AdapterError> {
    let event = Event::parse(flag)?;
    if event == Event::Ignored {
        return Ok(None);
    }
    let input: Input = serde_json::from_str(text)?;
    if input.session_id.is_empty() {
        return Ok(None);
    }
    Ok(normalize(event, input, pane))
}

fn normalize(event: Event, input: Input, pane: Option<String>) -> Option<AgentEvent> {
    let id = AgentId {
        kind: AgentKind::Copilot,
        session_id: input.session_id.clone(),
        surface: None,
        pane,
        tmux_socket: None,
        cwd: input.cwd.clone().filter(|cwd| !cwd.is_empty()),
    };
    let at = OffsetDateTime::now_utc();
    Some(match event {
        Event::SessionStart => AgentEvent::Started { id, at },
        Event::UserPromptSubmitted => AgentEvent::PromptSubmitted {
            id,
            prompt: truncate(input.prompt.unwrap_or_default(), 4_000),
            at,
        },
        Event::PreToolUse => AgentEvent::ToolStarted {
            tool: input.tool(),
            command: input.command(),
            subagent: None,
            id,
            at,
        },
        Event::PostToolUse | Event::PostToolUseFailure => AgentEvent::ToolCompleted {
            tool: input.tool(),
            success: event == Event::PostToolUse && input.tool_succeeded(),
            id,
            at,
        },
        Event::Notification => {
            let waiting = input.notification_type.as_deref() == Some("permission_prompt");
            let message = input
                .message
                .clone()
                .or(input.title.clone())
                .unwrap_or_else(|| "Copilot".into());
            AgentEvent::NotificationFired {
                id,
                level: if waiting {
                    NotificationLevel::NeedsInput
                } else {
                    NotificationLevel::Info
                },
                message: truncate(
                    if waiting {
                        format!("{PERMISSION_PREFIX}{message}")
                    } else {
                        message
                    },
                    1_000,
                ),
                at,
            }
        }
        Event::AgentStop => AgentEvent::TurnStopped {
            response: input
                .transcript_path
                .as_deref()
                .filter(|path| !path.is_empty())
                .zip(input.timestamp)
                .and_then(|(path, at)| super::copilot_session::answer_at_stop(Path::new(path), at))
                .map(truncate_response),
            recap: None,
            ai_title: None,
            // Reserved for producers that observed the pane go idle.
            idle_confirmed: false,
            id,
            at,
        },
        Event::ErrorOccurred => AgentEvent::NotificationFired {
            level: NotificationLevel::Error,
            message: truncate(format!("copilot: {}", input.error_text()), 1_000),
            id,
            at,
        },
        Event::SessionEnd => AgentEvent::SessionEnded { id, at },
        Event::Ignored => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every hook a real Copilot CLI 1.0.110 session ran through
    /// `tend init`'s hooks file, with the stdin each received
    /// (`{"event": FLAG, "input": PAYLOAD}` per line): a turn that runs
    /// `cat a.txt` and answers, one whose permission prompt is cancelled
    /// with Esc, the same prompt approved, then `/exit`.
    const REPLAY: &str = include_str!("testdata/copilot-1.0.110-hooks.jsonl");

    fn replay() -> Vec<(String, Option<AgentEvent>)> {
        REPLAY
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let record: Value = serde_json::from_str(line).unwrap();
                let flag = record["event"].as_str().unwrap().to_string();
                let event = parse(&flag, &record["input"].to_string(), Some("%7".into())).unwrap();
                (flag, event)
            })
            .collect()
    }

    fn one(flag: &str, json: &str) -> AgentEvent {
        parse(flag, json, Some("%1".into())).unwrap().unwrap()
    }

    #[test]
    fn a_real_session_replays_into_the_row_tend_expects() {
        let events = replay();
        let names: Vec<String> = events
            .iter()
            .map(|(flag, event)| match event {
                Some(event) => format!(
                    "{flag}:{}",
                    serde_json::to_value(event).unwrap()["type"]
                        .as_str()
                        .unwrap()
                ),
                None => format!("{flag}:-"),
            })
            .collect();
        insta_like(&names);
        for (_, event) in events
            .iter()
            .filter_map(|(f, e)| e.as_ref().map(|e| (f, e)))
        {
            let id = event.id();
            assert_eq!(id.kind, AgentKind::Copilot);
            assert_eq!(id.pane.as_deref(), Some("%7"));
            assert!(!id.session_id.is_empty());
            assert!(id.cwd.as_deref().is_some_and(|cwd| cwd.ends_with("/work")));
        }
    }

    /// The expected sequence, spelled out so a change in mapping reads as a
    /// diff of the session.
    fn insta_like(names: &[String]) {
        let expected = [
            "session_start:started",
            // Turn 1: a read-only command, auto-approved, then the answer.
            "user_prompt_submitted:prompt_submitted",
            "pre_tool_use:tool_started",
            "post_tool_use:tool_completed",
            "agent_stop:turn_stopped",
            // Turn 2: the permission prompt, cancelled with Esc. Copilot
            // fires nothing after it.
            "user_prompt_submitted:prompt_submitted",
            "pre_tool_use:tool_started",
            "notification:notification_fired",
            // Turn 3: the same prompt, approved.
            "user_prompt_submitted:prompt_submitted",
            "pre_tool_use:tool_started",
            "notification:notification_fired",
            "post_tool_use:tool_completed",
            "agent_stop:turn_stopped",
            "session_end:session_ended",
        ];
        assert_eq!(names, expected);
    }

    #[test]
    fn the_prompt_tool_and_permission_carry_their_text() {
        let events = replay();
        let find = |flag: &str| {
            events
                .iter()
                .find(|(f, _)| f == flag)
                .and_then(|(_, e)| e.clone())
                .unwrap()
        };
        match find("user_prompt_submitted") {
            AgentEvent::PromptSubmitted { prompt, .. } => {
                assert_eq!(
                    prompt,
                    "Run the shell command 'cat a.txt' and reply with only its content."
                );
            }
            other => panic!("{other:?}"),
        }
        match find("pre_tool_use") {
            AgentEvent::ToolStarted { tool, command, .. } => {
                assert_eq!(tool, "bash");
                assert_eq!(command.as_deref(), Some("cat a.txt"));
            }
            other => panic!("{other:?}"),
        }
        match find("notification") {
            AgentEvent::NotificationFired { level, message, .. } => {
                assert_eq!(level, NotificationLevel::NeedsInput);
                assert_eq!(
                    message,
                    format!("{PERMISSION_PREFIX}Run command: touch b.txt")
                );
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn agent_stop_reads_the_answer_from_the_session_log() {
        // The whole real log (three turns). Each stop's own payload
        // timestamp picks its turn: turn 1 answered "hello", turn 3 only ran
        // a tool.
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("events.jsonl");
        std::fs::write(&log, include_str!("testdata/copilot-1.0.110-events.jsonl")).unwrap();
        let stops: Vec<i64> = REPLAY
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|record| record["event"] == "agent_stop")
            .map(|record| record["input"]["timestamp"].as_i64().unwrap())
            .collect();
        let answer = |timestamp: Option<i64>| {
            let json = serde_json::json!({
                "sessionId": "s", "cwd": "/w", "stopReason": "end_turn",
                "transcriptPath": log.to_str().unwrap(), "timestamp": timestamp,
            });
            match one("agent_stop", &json.to_string()) {
                AgentEvent::TurnStopped {
                    response,
                    idle_confirmed,
                    ..
                } => {
                    assert!(!idle_confirmed);
                    response
                }
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(answer(Some(stops[0])).as_deref(), Some("hello"));
        assert_eq!(answer(Some(stops[1])), None);
        // Without the hook's time the turn cannot be told apart: no answer.
        assert_eq!(answer(None), None);
        // A missing log costs the answer, never the stop.
        let gone =
            r#"{"sessionId":"s","transcriptPath":"/nonexistent/events.jsonl","timestamp":1}"#;
        assert!(matches!(
            one("agent_stop", gone),
            AgentEvent::TurnStopped { response: None, .. }
        ));
    }

    #[test]
    fn a_failed_tool_and_an_error_say_so() {
        let failed = r#"{"sessionId":"s","toolName":"bash","toolResult":{"resultType":"failure"}}"#;
        assert!(matches!(
            one("post_tool_use", failed),
            AgentEvent::ToolCompleted { success: false, .. }
        ));
        assert!(matches!(
            one(
                "post_tool_use_failure",
                r#"{"sessionId":"s","toolName":"bash"}"#
            ),
            AgentEvent::ToolCompleted { success: false, .. }
        ));
        for error in [
            r#""rate limited""#,
            r#"{"message":"rate limited","name":"x"}"#,
        ] {
            let json = format!(r#"{{"sessionId":"s","error":{error}}}"#);
            match one("error_occurred", &json) {
                AgentEvent::NotificationFired { level, message, .. } => {
                    assert_eq!(level, NotificationLevel::Error);
                    assert_eq!(message, "copilot: rate limited");
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn other_notifications_are_not_a_wait() {
        let json = r#"{"sessionId":"s","notification_type":"agent_idle","message":"done"}"#;
        assert!(matches!(
            one("notification", json),
            AgentEvent::NotificationFired {
                level: NotificationLevel::Info,
                ..
            }
        ));
    }

    #[test]
    fn ignored_and_unknown_hooks() {
        assert!(parse("permission_request", "not even json", None)
            .unwrap()
            .is_none());
        assert!(parse("subagent_stop", "{}", None).unwrap().is_none());
        assert!(parse("bogus", "{}", None).is_err());
        assert!(parse("session_start", r#"{"sessionId":""}"#, None)
            .unwrap()
            .is_none());
        assert!(parse("session_start", "{", None).is_err());
    }

    #[test]
    fn every_wired_event_parses() {
        for (_, flag) in EVENTS {
            assert!(
                Event::parse(flag).is_ok_and(|e| e != Event::Ignored),
                "{flag}"
            );
        }
    }
}
