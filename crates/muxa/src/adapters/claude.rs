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
//!   "StopFailure":      [{ "hooks": [{ "type":"command", "command":"muxa hook claude --event stop_failure"       }]}],
//!   "SessionEnd":       [{ "hooks": [{ "type":"command", "command":"muxa hook claude --event session_end"        }]}]
//! }
//! ```

use super::hook::{truncate, AdapterError, HookAdapter};
use super::transcript;
use crate::event::{
    AgentEvent, AgentId, AgentKind, NotificationLevel, RateLimitScope, RateLimitSource,
};
use serde::Deserialize;
use std::path::PathBuf;
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
    /// Path to the JSONL session transcript. Provided by Claude Code on
    /// every hook event; we use it on `Stop` to extract the assistant's
    /// last response since the hook payload itself doesn't carry one.
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,
    /// `StopFailure` hook only: which API error class triggered the
    /// failure. Documented values include `rate_limit`,
    /// `authentication_failed`, `billing_error`, `invalid_request`,
    /// `server_error`, `max_output_tokens`, `unknown`.
    #[serde(default)]
    pub error: Option<String>,
    /// `StopFailure` hook only: free-form details from the upstream
    /// API response (e.g., `"429 Too Many Requests"`).
    #[serde(default)]
    pub error_details: Option<String>,
    /// `StopFailure` hook only: assistant text rendered in-TUI just
    /// before the failure (e.g., `"You've hit your limit · resets …"`).
    /// Echoes content the transcript also captures, but is cheaper to
    /// read here and avoids racing the JSONL flush.
    #[serde(default)]
    pub last_assistant_message: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum Event {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    Notification,
    Stop,
    StopFailure,
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
            "stop_failure" => Event::StopFailure,
            "session_end" => Event::SessionEnd,
            other => return Err(AdapterError::UnknownEvent(other.into())),
        })
    }

    fn normalize(event: Event, input: Input, pane: Option<String>) -> AgentEvent {
        let id = AgentId {
            kind: AgentKind::ClaudeCode,
            session_id: input.session_id,
            surface: None,
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
            Event::PreToolUse => pre_tool_event(id, input.tool_name, at),
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
            Event::Stop => {
                // Walk the transcript tail. If the last recognizable
                // entry is a rate-limit synthetic, treat the Stop hook
                // as a rate-limit signal (fallback for installs that
                // don't have the StopFailure hook wired). Otherwise
                // emit a normal `TurnStopped` with the assistant text.
                match input
                    .transcript_path
                    .as_deref()
                    .and_then(transcript::last_turn_outcome)
                {
                    Some(transcript::TurnOutcome::RateLimited(text)) => AgentEvent::RateLimited {
                        id,
                        scope: RateLimitScope::Unknown,
                        source: RateLimitSource::Transcript,
                        // No reset timestamp: parsing "resets 2:40pm
                        // (Asia/Seoul)" reliably is fragile (locale,
                        // 12-hour ambiguity, whitespace). The watch UI
                        // renders the message verbatim until a richer
                        // signal lands.
                        resets_at: None,
                        message: Some(truncate(text, 4_000)),
                        at,
                    },
                    Some(transcript::TurnOutcome::Response(text)) => AgentEvent::TurnStopped {
                        id,
                        response: Some(truncate(text, 4_000)),
                        at,
                    },
                    None => AgentEvent::TurnStopped {
                        id,
                        response: None,
                        at,
                    },
                }
            }
            Event::StopFailure => {
                let error_kind = input.error.as_deref().unwrap_or("unknown");
                if error_kind == "rate_limit" {
                    AgentEvent::RateLimited {
                        id,
                        // The hook payload doesn't tell us which window
                        // tripped — that's only on the statusline / per-
                        // request response headers.
                        scope: RateLimitScope::Unknown,
                        source: RateLimitSource::StopFailure,
                        // Same reason — no reset timestamp from this
                        // signal alone. The store keeps any prior
                        // `rate_limited_until` it learned from a richer
                        // source.
                        resets_at: None,
                        message: input
                            .last_assistant_message
                            .or(input.error_details)
                            .map(|m| truncate(m, 4_000)),
                        at,
                    }
                } else {
                    // Non-rate-limit StopFailures (auth, billing, server
                    // error, …) flip the row to Error so the user can't
                    // miss the dead session. Surface the human-readable
                    // message when one is provided.
                    let message = input
                        .last_assistant_message
                        .or(input.error_details)
                        .unwrap_or_else(|| format!("StopFailure: {error_kind}"));
                    AgentEvent::NotificationFired {
                        id,
                        level: NotificationLevel::Error,
                        message: truncate(message, 4_000),
                        at,
                    }
                }
            }
            Event::SessionEnd => AgentEvent::SessionEnded { id, at },
        }
    }
}

/// Map a `PreToolUse` hook to the right `AgentEvent`. Most tools
/// emit `ToolStarted` (state → Working); a small closed-set of
/// user-blocking tools route through `NotificationFired { NeedsChoice }`
/// so the row reads `WaitingChoice` while the menu is up — both
/// `AskUserQuestion` and `ExitPlanMode` present numbered/menu UIs,
/// distinct from a free-text `Notification` prompt. The matching
/// `PostToolUse` → `ToolCompleted` recovers the row back to Working
/// via `state::mutate_for_event`.
fn pre_tool_event(id: AgentId, tool_name: Option<String>, at: OffsetDateTime) -> AgentEvent {
    let tool_name = tool_name.unwrap_or_else(|| "unknown".into());
    if is_user_blocking_tool(&tool_name) {
        AgentEvent::NotificationFired {
            id,
            level: NotificationLevel::NeedsChoice,
            message: format!("waiting on {tool_name}"),
            at,
        }
    } else {
        AgentEvent::ToolStarted {
            id,
            tool: tool_name,
            at,
        }
    }
}

/// Tools whose `PreToolUse` semantically means "block on the user
/// for input" rather than "agent is doing work". Routing them
/// through `NotificationFired { NeedsInput }` flips the row to
/// `WaitingInput` while the menu is up, so the operator's mental
/// model ("yellow = needs me") matches reality.
///
/// Add new entries here when Claude Code (or upstreams that share
/// this hook surface) ship more user-blocking tools. The list is
/// closed-set on purpose: an unknown tool default-routes to the
/// "agent is working" path so we don't accidentally over-flip.
fn is_user_blocking_tool(name: &str) -> bool {
    matches!(name, "AskUserQuestion" | "ExitPlanMode")
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
    /// Pro/Max usage windows. Documented schema (CC 2.1.80+):
    /// `rate_limits.{five_hour,seven_day}.{used_percentage,resets_at}`.
    /// Always optional — API-key sessions don't populate it.
    #[serde(default)]
    pub rate_limits: Option<StatusRateLimits>,
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

/// `rate_limits` object emitted by Claude Code's statusline JSON.
#[derive(Debug, Deserialize, Default)]
pub struct StatusRateLimits {
    #[serde(default)]
    pub five_hour: Option<StatusRateLimitWindow>,
    #[serde(default)]
    pub seven_day: Option<StatusRateLimitWindow>,
}

#[derive(Debug, Deserialize, Default)]
pub struct StatusRateLimitWindow {
    /// 0..100 — Claude Code documents this as a percentage.
    #[serde(default)]
    pub used_percentage: Option<f32>,
    /// Unix epoch *seconds*. Optional even when `used_percentage` is
    /// known — the documented schema lets the two fields move
    /// independently.
    #[serde(default)]
    pub resets_at: Option<i64>,
}

/// Parse the Claude Code status-line JSON from stdin and build a Heartbeat.
pub fn parse_statusline<R: std::io::Read>(r: &mut R) -> Result<StatusLineInput, AdapterError> {
    let mut buf = String::new();
    r.read_to_string(&mut buf)?;
    Ok(serde_json::from_str(&buf)?)
}

pub fn statusline_heartbeat(input: StatusLineInput, pane: Option<String>) -> AgentEvent {
    fn extract(window: Option<&StatusRateLimitWindow>) -> (Option<f32>, Option<OffsetDateTime>) {
        let pct = window.and_then(|w| w.used_percentage);
        let reset = window
            .and_then(|w| w.resets_at)
            .and_then(|s| OffsetDateTime::from_unix_timestamp(s).ok());
        (pct, reset)
    }
    let (five_hour_pct, five_hour_reset) = extract(
        input
            .rate_limits
            .as_ref()
            .and_then(|rl| rl.five_hour.as_ref()),
    );
    let (seven_day_pct, seven_day_reset) = extract(
        input
            .rate_limits
            .as_ref()
            .and_then(|rl| rl.seven_day.as_ref()),
    );
    AgentEvent::Heartbeat {
        id: AgentId {
            kind: AgentKind::ClaudeCode,
            session_id: input.session_id,
            surface: None,
            pane,
            cwd: input.cwd,
        },
        model: input.model.and_then(|m| m.display_name),
        context_used_pct: input.context_window.and_then(|c| c.used_percentage),
        cost_usd: input.cost.and_then(|c| c.total_cost_usd),
        rate_limit_5h_pct: five_hour_pct,
        rate_limit_5h_resets_at: five_hour_reset,
        rate_limit_7d_pct: seven_day_pct,
        rate_limit_7d_resets_at: seven_day_reset,
        at: OffsetDateTime::now_utc(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn pretool_input(tool_name: &str) -> Input {
        Input {
            session_id: "s".into(),
            cwd: None,
            tool_name: Some(tool_name.into()),
            notification_type: None,
            message: None,
            prompt: None,
            transcript_path: None,
            error: None,
            error_details: None,
            last_assistant_message: None,
        }
    }

    #[test]
    fn pre_tool_use_for_ask_user_question_emits_needs_choice_notification() {
        // The numbered-menu case: AskUserQuestion presents a menu, so
        // the row should land in WaitingChoice (via NeedsChoice), not
        // free-text WaitingInput.
        let ev =
            ClaudeAdapter::normalize(Event::PreToolUse, pretool_input("AskUserQuestion"), None);
        match ev {
            AgentEvent::NotificationFired { level, .. } => {
                assert!(matches!(level, NotificationLevel::NeedsChoice));
            }
            other => panic!("expected NotificationFired, got {other:?}"),
        }
    }

    #[test]
    fn pre_tool_use_for_exit_plan_mode_also_blocks() {
        let ev = ClaudeAdapter::normalize(Event::PreToolUse, pretool_input("ExitPlanMode"), None);
        assert!(matches!(
            ev,
            AgentEvent::NotificationFired {
                level: NotificationLevel::NeedsChoice,
                ..
            }
        ));
    }

    #[test]
    fn pre_tool_use_for_regular_tool_emits_tool_started() {
        // Sanity: non-blocking tools must still go through the
        // ToolStarted path — we don't want every pre-tool hook to
        // read as WaitingInput.
        let ev = ClaudeAdapter::normalize(Event::PreToolUse, pretool_input("Bash"), None);
        match ev {
            AgentEvent::ToolStarted { tool, .. } => assert_eq!(tool, "Bash"),
            other => panic!("expected ToolStarted, got {other:?}"),
        }
    }

    fn stop_input(transcript_path: Option<PathBuf>) -> Input {
        Input {
            session_id: "s".into(),
            cwd: None,
            tool_name: None,
            notification_type: None,
            message: None,
            prompt: None,
            transcript_path,
            error: None,
            error_details: None,
            last_assistant_message: None,
        }
    }

    fn stop_failure_input(error: &str, last_assistant_message: Option<&str>) -> Input {
        Input {
            session_id: "s".into(),
            cwd: None,
            tool_name: None,
            notification_type: None,
            message: None,
            prompt: None,
            transcript_path: None,
            error: Some(error.into()),
            error_details: Some(format!("{error} details")),
            last_assistant_message: last_assistant_message.map(Into::into),
        }
    }

    #[test]
    fn stop_failure_rate_limit_emits_rate_limited_event() {
        let ev = ClaudeAdapter::normalize(
            Event::StopFailure,
            stop_failure_input(
                "rate_limit",
                Some("You've hit your limit · resets 9:30pm (Asia/Seoul)"),
            ),
            None,
        );
        match ev {
            AgentEvent::RateLimited {
                scope,
                source,
                resets_at,
                message,
                ..
            } => {
                assert_eq!(scope, RateLimitScope::Unknown);
                assert_eq!(source, RateLimitSource::StopFailure);
                // StopFailure carries no reset timestamp by design.
                assert!(resets_at.is_none());
                assert_eq!(
                    message.as_deref(),
                    Some("You've hit your limit · resets 9:30pm (Asia/Seoul)")
                );
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn stop_failure_non_rate_limit_emits_error_notification() {
        let ev = ClaudeAdapter::normalize(
            Event::StopFailure,
            stop_failure_input("server_error", Some("upstream 502")),
            None,
        );
        match ev {
            AgentEvent::NotificationFired { level, message, .. } => {
                assert_eq!(level, NotificationLevel::Error);
                assert_eq!(message, "upstream 502");
            }
            other => panic!("expected NotificationFired, got {other:?}"),
        }
    }

    #[test]
    fn stop_failure_unknown_error_falls_back_to_kind_label() {
        // No `last_assistant_message` and no `error_details` — the
        // adapter should still surface a meaningful notification.
        let mut input = stop_failure_input("billing_error", None);
        input.error_details = None;
        let ev = ClaudeAdapter::normalize(Event::StopFailure, input, None);
        match ev {
            AgentEvent::NotificationFired { level, message, .. } => {
                assert_eq!(level, NotificationLevel::Error);
                assert!(
                    message.contains("billing_error"),
                    "fallback message should name the error kind, got {message:?}",
                );
            }
            other => panic!("expected NotificationFired, got {other:?}"),
        }
    }

    #[test]
    fn statusline_passes_through_rate_limits() {
        let json = r#"{
            "session_id": "s1",
            "rate_limits": {
                "five_hour":  { "used_percentage": 84.0, "resets_at": 1745948400 },
                "seven_day":  { "used_percentage": 31.5, "resets_at": 1746466800 }
            }
        }"#;
        let mut cur = std::io::Cursor::new(json);
        let parsed = parse_statusline(&mut cur).unwrap();
        let ev = statusline_heartbeat(parsed, None);
        match ev {
            AgentEvent::Heartbeat {
                rate_limit_5h_pct,
                rate_limit_5h_resets_at,
                rate_limit_7d_pct,
                rate_limit_7d_resets_at,
                ..
            } => {
                assert!((rate_limit_5h_pct.unwrap() - 84.0).abs() < f32::EPSILON);
                assert!((rate_limit_7d_pct.unwrap() - 31.5).abs() < f32::EPSILON);
                assert_eq!(
                    rate_limit_5h_resets_at.unwrap().unix_timestamp(),
                    1_745_948_400
                );
                assert_eq!(
                    rate_limit_7d_resets_at.unwrap().unix_timestamp(),
                    1_746_466_800
                );
            }
            _ => panic!("expected Heartbeat"),
        }
    }

    #[test]
    fn statusline_without_rate_limits_emits_none() {
        let json = r#"{ "session_id": "s1" }"#;
        let mut cur = std::io::Cursor::new(json);
        let parsed = parse_statusline(&mut cur).unwrap();
        let ev = statusline_heartbeat(parsed, None);
        match ev {
            AgentEvent::Heartbeat {
                rate_limit_5h_pct,
                rate_limit_5h_resets_at,
                rate_limit_7d_pct,
                rate_limit_7d_resets_at,
                ..
            } => {
                assert!(rate_limit_5h_pct.is_none());
                assert!(rate_limit_5h_resets_at.is_none());
                assert!(rate_limit_7d_pct.is_none());
                assert!(rate_limit_7d_resets_at.is_none());
            }
            _ => panic!("expected Heartbeat"),
        }
    }

    #[test]
    fn statusline_partial_rate_limits_each_field_independent() {
        // 5h has only the percentage, 7d has only the timestamp — both
        // legal per the documented schema.
        let json = r#"{
            "session_id": "s1",
            "rate_limits": {
                "five_hour":  { "used_percentage": 12.0 },
                "seven_day":  { "resets_at": 1746466800 }
            }
        }"#;
        let mut cur = std::io::Cursor::new(json);
        let parsed = parse_statusline(&mut cur).unwrap();
        let ev = statusline_heartbeat(parsed, None);
        match ev {
            AgentEvent::Heartbeat {
                rate_limit_5h_pct,
                rate_limit_5h_resets_at,
                rate_limit_7d_pct,
                rate_limit_7d_resets_at,
                ..
            } => {
                assert!(rate_limit_5h_pct.is_some());
                assert!(rate_limit_5h_resets_at.is_none());
                assert!(rate_limit_7d_pct.is_none());
                assert!(rate_limit_7d_resets_at.is_some());
            }
            _ => panic!("expected Heartbeat"),
        }
    }

    #[test]
    fn stop_without_transcript_path_emits_none_response() {
        let ev = ClaudeAdapter::normalize(Event::Stop, stop_input(None), None);
        match ev {
            AgentEvent::TurnStopped { response, .. } => assert!(response.is_none()),
            _ => panic!("expected TurnStopped"),
        }
    }

    #[test]
    fn stop_with_transcript_path_pulls_last_assistant_text() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"hello from the model"}}]}}}}"#
        )
        .unwrap();
        let ev = ClaudeAdapter::normalize(Event::Stop, stop_input(Some(f.path().into())), None);
        match ev {
            AgentEvent::TurnStopped { response, .. } => {
                assert_eq!(response.as_deref(), Some("hello from the model"));
            }
            _ => panic!("expected TurnStopped"),
        }
    }

    #[test]
    fn stop_with_long_response_truncates_to_4kb() {
        let mut f = NamedTempFile::new().unwrap();
        let huge = "x".repeat(10_000);
        // Embed the long string in a JSON-safe way.
        let line = format!(
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"{huge}"}}]}}}}"#
        );
        writeln!(f, "{line}").unwrap();
        let ev = ClaudeAdapter::normalize(Event::Stop, stop_input(Some(f.path().into())), None);
        match ev {
            AgentEvent::TurnStopped { response, .. } => {
                let text = response.expect("response present");
                // truncate() bound is 4_000 bytes + a single ellipsis (3
                // bytes in UTF-8) — anything larger means the truncation
                // hook isn't running.
                assert!(text.len() <= 4_000 + 3, "got {} bytes", text.len());
                assert!(text.ends_with('…'));
            }
            _ => panic!("expected TurnStopped"),
        }
    }

    #[test]
    fn stop_with_transcript_rate_limit_emits_rate_limited_event() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"model":"<synthetic>","role":"assistant","content":[{{"type":"text","text":"You've hit your limit · resets 2:40pm (Asia/Seoul)"}}]}},"error":"rate_limit","isApiErrorMessage":true,"apiErrorStatus":429}}"#
        )
        .unwrap();
        let ev = ClaudeAdapter::normalize(Event::Stop, stop_input(Some(f.path().into())), None);
        match ev {
            AgentEvent::RateLimited {
                source, message, ..
            } => {
                assert_eq!(source, RateLimitSource::Transcript);
                assert!(message.unwrap().contains("You've hit your limit"));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn stop_with_unreadable_path_emits_none_response() {
        let path = PathBuf::from("/tmp/no-such-transcript-zzz.jsonl");
        let ev = ClaudeAdapter::normalize(Event::Stop, stop_input(Some(path)), None);
        match ev {
            AgentEvent::TurnStopped { response, .. } => assert!(response.is_none()),
            _ => panic!("expected TurnStopped"),
        }
    }
}
