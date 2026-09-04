//! `OpenAI` Codex CLI adapter.
//!
//! Codex's hook engine is a verbatim port of Claude Code's (it's literally
//! named `ClaudeHooksEngine` upstream). Wire up in `~/.codex/config.toml`:
//!
//! ```toml
//! [[hooks.SessionStart]]
//!   [[hooks.SessionStart.hooks]]
//!   type = "command"
//!   command = "muxa hook codex --event session_start"
//! [[hooks.UserPromptSubmit]]
//!   [[hooks.UserPromptSubmit.hooks]]
//!   type = "command"
//!   command = "muxa hook codex --event user_prompt_submit"
//! # ... (PreToolUse / PostToolUse / PermissionRequest / Stop)
//! ```

use super::hook::{truncate, AdapterError, HookAdapter};
use crate::event::{AgentEvent, AgentId, AgentKind, NotificationLevel};
use serde::Deserialize;
use time::OffsetDateTime;

pub struct CodexAdapter;

const CONVERSATION_RECAP_HEADER: &str = "Conversation recap";
const MAX_RECAP_BYTES: usize = 2_000;

/// Extract Codex's user-visible `Conversation recap` from a captured pane.
///
/// The compacted rollout item is intentionally opaque, while the TUI renders a
/// short plaintext recap between a box-rule header and the next composer. The
/// screen detector uses this best-effort parser to persist that text in the
/// same `Agent::recap` field Claude fills from its transcript. A strict chrome
/// match prevents ordinary prompts that merely mention "Conversation recap"
/// from being mistaken for a summary.
#[must_use]
pub fn conversation_recap_from_capture(raw: &str) -> Option<String> {
    let prepared = crate::screen::prepare_capture(raw, usize::MAX);
    let lines: Vec<&str> = prepared.lines().collect();
    let header = lines
        .iter()
        .rposition(|line| is_conversation_recap_header(line))?;

    let mut body: Vec<String> = Vec::new();
    let mut started = false;
    for line in &lines[header + 1..] {
        let trimmed = line.trim();
        if !started && trimmed.is_empty() {
            continue;
        }
        if recap_boundary(line) {
            break;
        }
        if trimmed.is_empty() {
            if started && body.last().is_some_and(|line| !line.is_empty()) {
                body.push(String::new());
            }
            continue;
        }

        started = true;
        // Codex indents recap prose by two cells. Remove only that known
        // chrome indent; additional indentation may be meaningful markdown.
        body.push(
            line.strip_prefix("  ")
                .unwrap_or(line)
                .trim_end()
                .to_owned(),
        );
    }

    while body.last().is_some_and(String::is_empty) {
        body.pop();
    }
    let recap = body.join("\n").trim().to_owned();
    (!recap.is_empty()).then(|| truncate(recap, MAX_RECAP_BYTES))
}

fn is_conversation_recap_header(line: &str) -> bool {
    line.trim_matches(|c: char| c.is_whitespace() || matches!(c, '─' | '━'))
        == CONVERSATION_RECAP_HEADER
}

/// TUI rows that can follow the recap but are not part of it. Column-zero
/// markers begin the next turn; status/footer rows retain Codex's two-cell
/// indent and therefore need their own narrow checks.
fn recap_boundary(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }

    let begins_at_column_zero = line.chars().next().is_some_and(|c| !c.is_whitespace());
    if begins_at_column_zero
        && trimmed
            .chars()
            .next()
            .is_some_and(|c| matches!(c, '›' | '>' | '•' | '─' | '━'))
    {
        return true;
    }

    let lower = trimmed.to_ascii_lowercase();
    (lower.contains("background terminal") && lower.contains("running"))
        || (lower.starts_with("gpt-") && trimmed.contains(" · "))
        || lower.ends_with("for shortcuts")
}

#[derive(Debug, Deserialize)]
pub struct Input {
    pub session_id: String,
    pub cwd: Option<String>,
    #[serde(default)]
    pub tool_name: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub last_assistant_message: Option<String>,
}

#[derive(Debug, Clone, Copy)]
pub enum Event {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PostToolUse,
    PermissionRequest,
    Stop,
}

impl HookAdapter for CodexAdapter {
    type Event = Event;
    type Input = Input;
    const KIND: AgentKind = AgentKind::Codex;

    fn parse_event(flag: &str) -> Result<Event, AdapterError> {
        Ok(match flag {
            "session_start" => Event::SessionStart,
            "user_prompt_submit" => Event::UserPromptSubmit,
            "pre_tool_use" => Event::PreToolUse,
            "post_tool_use" => Event::PostToolUse,
            "permission_request" => Event::PermissionRequest,
            "stop" => Event::Stop,
            other => return Err(AdapterError::UnknownEvent(other.into())),
        })
    }

    fn normalize(event: Event, input: Input, pane: Option<String>) -> AgentEvent {
        let id = AgentId {
            kind: AgentKind::Codex,
            session_id: input.session_id,
            surface: None,
            pane,
            tmux_socket: None,
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
                subagent: None,
                at,
            },
            Event::PostToolUse => AgentEvent::ToolCompleted {
                id,
                tool: input.tool_name.unwrap_or_else(|| "unknown".into()),
                success: true,
                at,
            },
            Event::PermissionRequest => AgentEvent::NotificationFired {
                id,
                level: NotificationLevel::NeedsInput,
                message: format!(
                    "codex permission: {}",
                    input.tool_name.unwrap_or_else(|| "tool".into())
                ),
                at,
            },
            Event::Stop => AgentEvent::TurnStopped {
                id,
                response: None,
                recap: None,
                ai_title: None,
                // Real hook: never a synthetic idle observation. A response-less
                // Codex `Stop` can fire while a permission prompt is still on
                // screen, so it must NOT clear a waiting row (see state.rs).
                idle_confirmed: false,
                at,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_visible_conversation_recap_without_tui_chrome() {
        let capture = "- finished prior output\n\
─ Worked for 14m 27s ───\n\
\n\
─ Conversation recap ─────\n\
\n\
  CAL 일감 18건을 각각 검토하는 작업입니다. 검토 기준은 최신 코드와 Linear입니다.\n\
  다음 단계는 각 일감의 상태와 근거를 확인하는 것입니다.\n\
\n\
  1 background terminal running · /ps to view · /stop to close\n\
\n\
› Ask Codex to do anything\n\
\n\
  gpt-5.6-sol xhigh · ~/project\n";

        assert_eq!(
            conversation_recap_from_capture(capture).as_deref(),
            Some(
                "CAL 일감 18건을 각각 검토하는 작업입니다. 검토 기준은 최신 코드와 Linear입니다.\n다음 단계는 각 일감의 상태와 근거를 확인하는 것입니다."
            ),
        );
    }

    #[test]
    fn picks_latest_strict_header_and_strips_ansi() {
        let capture = "─ Conversation recap ──\n\n  old recap\n\n\
› next prompt\n\
\x1b[0;1m─ Conversation recap\x1b[0;2m ───\n\n  latest recap\n";
        assert_eq!(
            conversation_recap_from_capture(capture).as_deref(),
            Some("latest recap"),
        );

        let ordinary_text = "› Can you explain Conversation recap?\n\n• Yes.";
        assert_eq!(conversation_recap_from_capture(ordinary_text), None);
    }

    #[test]
    fn empty_or_chrome_only_recap_is_none() {
        assert_eq!(
            conversation_recap_from_capture(
                "─ Conversation recap ──\n\n› Ask Codex to do anything\n"
            ),
            None,
        );
        assert_eq!(conversation_recap_from_capture("plain output"), None);
    }
}
