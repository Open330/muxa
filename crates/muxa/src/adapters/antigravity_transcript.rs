//! Antigravity CLI (`agy`) transcript JSONL parser.
//!
//! agy's hook payloads carry neither the user's prompt nor the model's
//! reply — they give a `transcriptPath` instead. Each line of that file is
//! one conversation *step*:
//!
//! ```json
//! {"step_index":9,"source":"USER_EXPLICIT","type":"USER_INPUT","status":"DONE",
//!  "content":"<USER_REQUEST>\nNow run: echo hi\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\n…\n</ADDITIONAL_METADATA>"}
//! {"step_index":11,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE",
//!  "tool_calls":[{"name":"run_command","args":{…}}]}
//! {"step_index":13,"source":"MODEL","type":"PLANNER_RESPONSE","status":"DONE",
//!  "content":"```bash\n$ echo hi\nhi\n```"}
//! ```
//!
//! [`last_user_request`] feeds the `PreInvocation` hook's `PromptSubmitted`
//! and [`last_assistant_text`] feeds the `Stop` hook's `TurnStopped` — the
//! same division of labour [`super::transcript`] performs for Claude Code.
//!
//! Both read only the tail of the file and fail silently: a hook handler runs
//! synchronously inside the agent's loop, so a parse error must never become
//! terminal noise (or a stall) for the user.

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

/// Read at most this many bytes from the tail of the transcript.
///
/// Mirrors the Claude transcript reader's window: agy appends a
/// step per tool call and long sessions run to multi-MB, but the records we
/// want are always within the last turn or two. The `USER_INPUT` we resolve
/// on `PreInvocation` was appended moments earlier, and the trailing
/// `PLANNER_RESPONSE` we resolve on `Stop` is the very last record.
const TAIL_BYTES: u64 = 256 * 1024;

/// One transcript line. Every field is optional so a record shape we don't
/// recognize (or a newer agy build's extra keys) is skipped rather than
/// aborting the scan.
#[derive(Debug, serde::Deserialize)]
struct Step {
    #[serde(default)]
    r#type: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    content: Option<String>,
}

/// The prompt text of the most recent *explicit user* turn.
///
/// agy wraps the prompt in `<USER_REQUEST>` and appends an
/// `<ADDITIONAL_METADATA>` block holding the local time; both are stripped so
/// the watch UI shows what the operator actually typed. Returns `None` when
/// the file is unreadable, holds no user step in the tail window, or the
/// prompt is empty after unwrapping.
///
/// `USER_EXPLICIT` is required: agy also writes `SYSTEM_MESSAGE` steps and
/// system-sourced `USER_INPUT` (continuation nudges, injected reminders), and
/// surfacing those as "the user's prompt" would be a lie.
pub fn last_user_request(path: &Path) -> Option<String> {
    last_matching(path, |s| {
        if s.r#type.as_deref() != Some("USER_INPUT") || s.source.as_deref() != Some("USER_EXPLICIT")
        {
            return None;
        }
        let text = unwrap_user_request(s.content.as_deref()?);
        (!text.is_empty()).then_some(text)
    })
}

/// The user-visible text of the most recent model response.
///
/// Skips `PLANNER_RESPONSE` records that carry only `tool_calls` — those are
/// the model *acting*, not answering — and the `GENERIC` records holding tool
/// output. Returns `None` when nothing in the tail window qualifies.
pub fn last_assistant_text(path: &Path) -> Option<String> {
    last_matching(path, |s| {
        if s.r#type.as_deref() != Some("PLANNER_RESPONSE") || s.source.as_deref() != Some("MODEL") {
            return None;
        }
        let text = s.content.as_deref()?.trim();
        (!text.is_empty()).then(|| text.to_string())
    })
}

/// Scan the tail window and return `pick`'s value for the LAST line it
/// accepted. Shared by both public readers so the seek/skip-fragment/parse
/// handling lives in exactly one place.
fn last_matching(path: &Path, pick: impl Fn(&Step) -> Option<String>) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;

    let mut found: Option<String> = None;
    // Seeking into the middle of the file almost always lands mid-line. That
    // first fragment fails to parse as JSON and is skipped here without a
    // special case, exactly as in the Claude transcript reader.
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(step) = serde_json::from_str::<Step>(line) else {
            continue;
        };
        if let Some(text) = pick(&step) {
            found = Some(text);
        }
    }
    found
}

/// Strip agy's `<USER_REQUEST>` wrapper and the `<ADDITIONAL_METADATA>` block
/// that follows it.
///
/// Tolerant by design: a payload with no wrapper (a shape change, or a step
/// written by a different agy surface) falls back to the trimmed content
/// rather than yielding nothing.
fn unwrap_user_request(content: &str) -> String {
    const OPEN: &str = "<USER_REQUEST>";
    const CLOSE: &str = "</USER_REQUEST>";

    if let Some(rest) = content.find(OPEN).map(|i| &content[i + OPEN.len()..]) {
        if let Some(end) = rest.find(CLOSE) {
            return rest[..end].trim().to_string();
        }
        // Opened but never closed — take what's there minus any trailing
        // metadata block rather than dropping the prompt entirely.
        return strip_metadata(rest).trim().to_string();
    }
    strip_metadata(content).trim().to_string()
}

/// Drop everything from `<ADDITIONAL_METADATA>` onward.
fn strip_metadata(s: &str) -> &str {
    match s.find("<ADDITIONAL_METADATA>") {
        Some(i) => &s[..i],
        None => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_transcript(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        f.flush().unwrap();
        f
    }

    #[test]
    fn reads_last_explicit_user_request_unwrapped() {
        let f = write_transcript(&[
            r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","content":"<USER_REQUEST>\nfirst\n</USER_REQUEST>"}"#,
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","content":"ok"}"#,
            r#"{"step_index":2,"source":"USER_EXPLICIT","type":"USER_INPUT","content":"<USER_REQUEST>\nNow run: echo hi\n</USER_REQUEST>\n<ADDITIONAL_METADATA>\nThe current local time is: 2026-08-22T00:59:08+09:00.\n</ADDITIONAL_METADATA>"}"#,
        ]);
        assert_eq!(
            last_user_request(f.path()).as_deref(),
            Some("Now run: echo hi"),
        );
    }

    /// agy writes system-authored steps into the same file. Reporting one as
    /// "the user's prompt" would put text the operator never typed on the
    /// watch row, so both the type and the source must match.
    #[test]
    fn ignores_system_sourced_steps() {
        let f = write_transcript(&[
            r#"{"step_index":0,"source":"USER_EXPLICIT","type":"USER_INPUT","content":"<USER_REQUEST>\nreal prompt\n</USER_REQUEST>"}"#,
            r#"{"step_index":1,"source":"SYSTEM","type":"SYSTEM_MESSAGE","content":"a nudge"}"#,
            r#"{"step_index":2,"source":"SYSTEM","type":"USER_INPUT","content":"<USER_REQUEST>\ninjected\n</USER_REQUEST>"}"#,
        ]);
        assert_eq!(last_user_request(f.path()).as_deref(), Some("real prompt"));
    }

    /// A prompt with no wrapper still comes back — agy's shape is not a
    /// contract muxa can enforce, and dropping the prompt is worse than
    /// showing it verbatim.
    #[test]
    fn falls_back_when_wrapper_absent() {
        let f = write_transcript(&[
            r#"{"source":"USER_EXPLICIT","type":"USER_INPUT","content":"bare prompt\n<ADDITIONAL_METADATA>\ntime\n</ADDITIONAL_METADATA>"}"#,
        ]);
        assert_eq!(last_user_request(f.path()).as_deref(), Some("bare prompt"));
    }

    #[test]
    fn reads_last_assistant_text_skipping_tool_call_steps() {
        let f = write_transcript(&[
            r#"{"step_index":0,"source":"MODEL","type":"PLANNER_RESPONSE","content":"earlier answer"}"#,
            r#"{"step_index":1,"source":"MODEL","type":"PLANNER_RESPONSE","tool_calls":[{"name":"run_command"}]}"#,
            r#"{"step_index":2,"source":"MODEL","type":"GENERIC","content":"The command exited with code 0."}"#,
            r#"{"step_index":3,"source":"MODEL","type":"PLANNER_RESPONSE","content":"final answer"}"#,
        ]);
        assert_eq!(
            last_assistant_text(f.path()).as_deref(),
            Some("final answer"),
        );
    }

    /// A tool-call-only step has no `content`; it must not shadow the real
    /// response that came before it.
    #[test]
    fn tool_call_step_does_not_blank_the_response() {
        let f = write_transcript(&[
            r#"{"source":"MODEL","type":"PLANNER_RESPONSE","content":"the answer"}"#,
            r#"{"source":"MODEL","type":"PLANNER_RESPONSE","tool_calls":[{"name":"view_file"}]}"#,
        ]);
        assert_eq!(last_assistant_text(f.path()).as_deref(), Some("the answer"));
    }

    /// Truncated/garbage lines are skipped, not fatal — the tail seek lands
    /// mid-line on any real transcript.
    #[test]
    fn skips_unparseable_lines() {
        let f = write_transcript(&[
            r#"nal_response"}]}"#,
            r#"{"source":"MODEL","type":"PLANNER_RESPONSE","content":"survived"}"#,
        ]);
        assert_eq!(last_assistant_text(f.path()).as_deref(), Some("survived"));
    }

    #[test]
    fn missing_file_is_none() {
        let p = Path::new("/nonexistent/muxa/agy-transcript.jsonl");
        assert_eq!(last_user_request(p), None);
        assert_eq!(last_assistant_text(p), None);
    }

    #[test]
    fn empty_transcript_is_none() {
        let f = write_transcript(&[]);
        assert_eq!(last_user_request(f.path()), None);
        assert_eq!(last_assistant_text(f.path()), None);
    }
}
