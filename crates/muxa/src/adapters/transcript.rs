//! Claude Code transcript JSONL parser.
//!
//! Claude Code's `Stop` hook fires at the end of every assistant turn but
//! does not include the response body in the hook payload — it gives only
//! a `transcript_path` pointing at a JSONL file. Each line is a JSON object;
//! assistant messages have shape:
//!
//! ```json
//! {
//!   "type": "assistant",
//!   "message": {
//!     "role": "assistant",
//!     "content": [
//!       {"type": "thinking", ...},
//!       {"type": "text", "text": "..."},
//!       {"type": "tool_use", ...}
//!     ]
//!   }
//! }
//! ```
//!
//! [`last_assistant_text`] returns the concatenated `text` content of the
//! *last* such entry — that's the response just delivered by the turn we
//! were notified about.
//!
//! ## Rate-limit detection
//!
//! Claude Code rewrites the trailing assistant entry as a *synthetic*
//! message when an in-flight 429 lands, and tags the surrounding record
//! with structured fields:
//!
//! ```json
//! {
//!   "type":"assistant",
//!   "message":{"model":"<synthetic>","role":"assistant","content":[
//!     {"type":"text","text":"You've hit your limit · resets 2:40pm (Asia/Seoul)"}
//!   ]},
//!   "error":"rate_limit",
//!   "isApiErrorMessage":true,
//!   "apiErrorStatus":429
//! }
//! ```
//!
//! [`last_turn_outcome`] walks the tail and returns whichever signal landed
//! *last* — a normal response, a rate-limit synthetic, or `None`. The Stop
//! hook in [`crate::adapters::claude`] uses this to decide whether to emit
//! `TurnStopped` or `RateLimited`. Acts as a fallback for environments
//! where the official `StopFailure` hook isn't wired (older Claude Code
//! installs, sub-agent rate-limits surfaced as `tool_result` text).

use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

/// Read at most this many bytes from the tail of the transcript. Long
/// sessions can run to multi-MB; one assistant turn is realistically well
/// under 100 KB even with verbose reasoning, so 256 KB is comfortable
/// headroom while keeping the hook handler's CPU/IO bounded.
const TAIL_BYTES: u64 = 256 * 1024;

/// Extract the last assistant turn's user-visible text from `path`.
///
/// Returns `None` for any failure mode (file missing, malformed lines,
/// no assistant entry in the tail window) — the caller treats this as
/// "we couldn't capture a response" and proceeds. Failure is silent by
/// design: hook handlers run synchronously inside the agent CLI, and
/// noisy errors there bleed into the user's terminal.
pub fn last_assistant_text(path: &Path) -> Option<String> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;

    let reader = BufReader::new(f);
    let mut last_text: Option<String> = None;
    // When we seek into the middle of a long file, the first emitted line
    // is almost certainly a fragment. `serde_json::from_str` rejects it,
    // and `extract_assistant_text` returns None — so the partial line is
    // silently skipped without special-casing.
    for line in reader.lines().map_while(Result::ok) {
        if let Some(text) = extract_assistant_text(&line) {
            last_text = Some(text);
        }
    }
    last_text
}

/// Parse one transcript line. Returns `Some(text)` only when the line is
/// a complete assistant entry with at least one non-empty text segment.
fn extract_assistant_text(line: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    if v.get("type")?.as_str()? != "assistant" {
        return None;
    }
    let content = v.get("message")?.get("content")?.as_array()?;
    let mut out = String::new();
    for c in content {
        if c.get("type").and_then(serde_json::Value::as_str) != Some("text") {
            continue;
        }
        if let Some(text) = c.get("text").and_then(serde_json::Value::as_str) {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(text);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Outcome of the last assistant turn captured in the transcript tail.
/// Either a normal response, a rate-limit hit, or nothing recognized.
#[derive(Debug, PartialEq, Eq)]
pub enum TurnOutcome {
    /// Normal assistant response text.
    Response(String),
    /// Synthetic rate-limit message ("You've hit your limit · resets …").
    RateLimited(String),
}

/// Walk the transcript tail and return whichever signal landed *last*.
///
/// Two-tracker scan rather than a "find last rate-limit" pass: when a
/// session is rate-limited and the user later types "continue" + gets
/// served a real turn, the rate-limit entry is still in the tail but
/// the agent is no longer blocked. We must report the more recent
/// outcome of the two — anything else would leave the watch row glowing
/// red after the user is back to normal work.
///
/// Returns `None` for any failure mode (file missing, malformed lines,
/// no recognizable entry in the window) — same silent-failure contract
/// as [`last_assistant_text`].
pub fn last_turn_outcome(path: &Path) -> Option<TurnOutcome> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;

    let reader = BufReader::new(f);
    let mut latest: Option<TurnOutcome> = None;
    for line in reader.lines().map_while(Result::ok) {
        if let Some(outcome) = classify_transcript_line(&line) {
            latest = Some(outcome);
        }
    }
    latest
}

/// Classify a single transcript line into a turn outcome. Walking
/// callers track the most recent `Some(...)` return.
fn classify_transcript_line(line: &str) -> Option<TurnOutcome> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;

    // Form 1: an assistant entry tagged with `error:"rate_limit"` and/or
    // `apiErrorStatus:429`. This is the canonical shape Claude Code
    // writes when the in-flight request 429s.
    let is_rate_limited = v.get("error").and_then(serde_json::Value::as_str) == Some("rate_limit")
        || v.get("apiErrorStatus").and_then(serde_json::Value::as_i64) == Some(429)
        || v.get("isApiErrorMessage")
            .and_then(serde_json::Value::as_bool)
            == Some(true);

    if v.get("type")?.as_str()? == "assistant" {
        let text = extract_assistant_text(line)?;
        return Some(if is_rate_limited {
            TurnOutcome::RateLimited(text)
        } else {
            TurnOutcome::Response(text)
        });
    }

    // Form 2: a `tool_result` whose text contains "You've hit your
    // limit". Subagent rate-limits propagate to the parent transcript
    // as user-role tool_result entries without the structured fields.
    if v.get("type")?.as_str()? == "user" {
        let content = v.get("message")?.get("content")?.as_array()?;
        for c in content {
            if c.get("type").and_then(serde_json::Value::as_str) != Some("tool_result") {
                continue;
            }
            // tool_result `content` can be a string or an array of
            // {type:"text", text:"..."} chunks; cover both.
            let text = match c.get("content") {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(serde_json::Value::Array(arr)) => arr
                    .iter()
                    .filter_map(|item| item.get("text").and_then(serde_json::Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => continue,
            };
            if text.contains("You've hit your limit") || text.contains("Claude usage limit reached")
            {
                return Some(TurnOutcome::RateLimited(text));
            }
        }
    }
    None
}

/// Session-level summary signals captured from the transcript tail.
///
/// Claude Code writes two summary-ish records that a dashboard can show as
/// "what is this agent actually doing":
///
/// ```json
/// {"type":"system","subtype":"away_summary","content":"… (disable recaps in /config)"}
/// {"type":"ai-title","aiTitle":"맥북 상태 및 로컬 문제점 파악","sessionId":"…"}
/// ```
///
/// Neither is exposed through a hook payload or the statusline JSON — the
/// transcript is the only stable read path, which is why this lives here.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    /// The user-facing "recap" (`※ recap: …`). Rich, but *sparse*: Claude
    /// Code only generates one when the user comes back after being away,
    /// so a long unbroken working stretch pushes the last one out of the
    /// tail window. That's deliberate here — a recap old enough to fall
    /// outside the window is stale, and callers are expected to fall back
    /// to the fresher [`Self::ai_title`].
    pub recap: Option<String>,
    /// Claude Code's rolling short session title — the same string it puts
    /// in the tmux pane title. Rewritten far more often than a recap
    /// (hundreds of records per session vs. a handful), so it's the
    /// practical steady-state summary.
    pub ai_title: Option<String>,
}

impl SessionSummary {
    /// True when neither signal was found — lets callers skip attaching an
    /// all-`None` summary to an event.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.recap.is_none() && self.ai_title.is_none()
    }
}

/// UI chrome Claude Code appends to every recap. Carrying it into a
/// dashboard column would waste width and read as noise.
const RECAP_SUFFIX: &str = "(disable recaps in /config)";

/// Scan the transcript tail once and return the most recent recap and
/// session title found there.
///
/// Same silent-failure contract as [`last_assistant_text`]: any IO or parse
/// problem yields an empty [`SessionSummary`] rather than an error, because
/// this runs inside the agent's hook process.
pub fn last_session_summary(path: &Path) -> SessionSummary {
    let mut out = SessionSummary::default();
    let Ok(mut f) = File::open(path) else {
        return out;
    };
    let Ok(meta) = f.metadata() else {
        return out;
    };
    let start = meta.len().saturating_sub(TAIL_BYTES);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return out;
    }
    for line in BufReader::new(f).lines().map_while(Result::ok) {
        match extract_summary(&line) {
            Some(Summary::Recap(t)) => out.recap = Some(t),
            Some(Summary::AiTitle(t)) => out.ai_title = Some(t),
            None => {}
        }
    }
    out
}

/// Which summary signal a transcript line carried, if any.
enum Summary {
    Recap(String),
    AiTitle(String),
}

/// Classify one transcript line as a recap or a session title.
fn extract_summary(line: &str) -> Option<Summary> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
    match v.get("type")?.as_str()? {
        "system"
            if v.get("subtype").and_then(serde_json::Value::as_str) == Some("away_summary") =>
        {
            let content = v.get("content")?.as_str()?.trim();
            let cleaned = content.strip_suffix(RECAP_SUFFIX).unwrap_or(content).trim();
            (!cleaned.is_empty()).then(|| Summary::Recap(cleaned.to_string()))
        }
        "ai-title" => {
            let title = v.get("aiTitle")?.as_str()?.trim();
            (!title.is_empty()).then(|| Summary::AiTitle(title.to_string()))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write(lines: &[&str]) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        f
    }

    #[test]
    fn returns_none_for_missing_file() {
        let path = Path::new("/tmp/definitely-not-a-real-transcript-99999.jsonl");
        assert!(last_assistant_text(path).is_none());
    }

    #[test]
    fn returns_none_for_empty_file() {
        let f = NamedTempFile::new().unwrap();
        assert!(last_assistant_text(f.path()).is_none());
    }

    #[test]
    fn returns_none_when_no_assistant_entries() {
        let f = write(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"hi"}]}}"#,
            r#"{"type":"queue-operation","operation":"remove"}"#,
        ]);
        assert!(last_assistant_text(f.path()).is_none());
    }

    #[test]
    fn picks_text_from_last_assistant_entry() {
        let f = write(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"first"}]}}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"reply"}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"second"}]}}"#,
        ]);
        assert_eq!(last_assistant_text(f.path()).as_deref(), Some("second"));
    }

    #[test]
    fn concatenates_multiple_text_segments_with_newlines() {
        let f = write(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"a"},{"type":"thinking","thinking":"…"},{"type":"text","text":"b"}]}}"#,
        ]);
        assert_eq!(last_assistant_text(f.path()).as_deref(), Some("a\nb"));
    }

    #[test]
    fn skips_assistant_entry_with_only_thinking_or_tool_use() {
        let f = write(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"…"},{"type":"tool_use","id":"t","name":"Bash","input":{}}]}}"#,
        ]);
        // No `text` segment — treated as "no response" rather than empty string.
        assert!(last_assistant_text(f.path()).is_none());
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let f = write(&[
            "not json at all",
            r#"{"type":"assistant"}"#, // missing message.content
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"good"}]}}"#,
            r"{", // truncated
        ]);
        assert_eq!(last_assistant_text(f.path()).as_deref(), Some("good"));
    }

    #[test]
    fn rate_limit_synthetic_assistant_classified_as_rate_limited() {
        let f = write(&[
            r#"{"type":"assistant","message":{"model":"<synthetic>","role":"assistant","content":[{"type":"text","text":"You've hit your limit · resets 2:40pm (Asia/Seoul)"}]},"error":"rate_limit","isApiErrorMessage":true,"apiErrorStatus":429}"#,
        ]);
        match last_turn_outcome(f.path()) {
            Some(TurnOutcome::RateLimited(text)) => {
                assert!(text.contains("You've hit your limit"));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    /// User resumed after a rate-limit and the next turn served normally.
    /// The most recent outcome wins — anything else leaves the watch row
    /// glowing red after the user is back to normal work.
    #[test]
    fn normal_turn_after_rate_limit_classified_as_response() {
        let f = write(&[
            r#"{"type":"assistant","message":{"model":"<synthetic>","role":"assistant","content":[{"type":"text","text":"You've hit your limit · resets 2:40pm"}]},"error":"rate_limit","isApiErrorMessage":true,"apiErrorStatus":429}"#,
            r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"continue"}]}}"#,
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"sure, picking up where we left off"}]}}"#,
        ]);
        match last_turn_outcome(f.path()) {
            Some(TurnOutcome::Response(text)) => {
                assert_eq!(text, "sure, picking up where we left off");
            }
            other => panic!("expected Response, got {other:?}"),
        }
    }

    /// Sub-agent rate-limit surfaces in the parent transcript as a
    /// `tool_result` body without the structured `error` field.
    #[test]
    fn tool_result_with_rate_limit_text_classified_as_rate_limited() {
        let f = write(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"You've hit your limit · resets 6pm (Asia/Seoul)"}]}}"#,
        ]);
        match last_turn_outcome(f.path()) {
            Some(TurnOutcome::RateLimited(text)) => {
                assert!(text.contains("You've hit your limit"));
            }
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    /// Older Claude Code wording — keep the regex permissive enough to
    /// catch both phrasings.
    #[test]
    fn legacy_usage_limit_phrasing_classified_as_rate_limited() {
        let f = write(&[
            r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"Claude usage limit reached. Your limit will reset at 2pm (America/New_York)"}]}}"#,
        ]);
        assert!(matches!(
            last_turn_outcome(f.path()),
            Some(TurnOutcome::RateLimited(_))
        ));
    }

    #[test]
    fn tail_seek_drops_partial_first_line_without_data_loss() {
        // Build a file larger than TAIL_BYTES so the seek skips the
        // earliest entries entirely. Earlier assistant entries fall
        // outside the window and must not be returned.
        let mut f = NamedTempFile::new().unwrap();
        // First entry is way back at byte 0 — outside any reasonable tail.
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"too-old"}}]}}}}"#
        ).unwrap();
        // Pad with junk lines to push the next real entry past TAIL_BYTES.
        let pad = "x".repeat(1024);
        for _ in 0..(usize::try_from(TAIL_BYTES).unwrap() / 1024 + 4) {
            writeln!(f, "{pad}").unwrap();
        }
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"role":"assistant","content":[{{"type":"text","text":"recent"}}]}}}}"#
        ).unwrap();
        assert_eq!(last_assistant_text(f.path()).as_deref(), Some("recent"));
    }

    // --- session summary (recap / ai-title) -----------------------------

    #[test]
    fn summary_is_empty_for_missing_file_and_plain_transcript() {
        assert!(last_session_summary(Path::new("/definitely/not/here")).is_empty());
        let f = write(&[
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi"}]}}"#,
        ]);
        assert!(last_session_summary(f.path()).is_empty());
    }

    #[test]
    fn picks_latest_recap_and_title_and_strips_recap_chrome() {
        let f = write(&[
            r#"{"type":"ai-title","aiTitle":"old title","sessionId":"s"}"#,
            r#"{"type":"system","subtype":"away_summary","content":"first recap (disable recaps in /config)"}"#,
            r#"{"type":"system","subtype":"turn_duration","content":"not a recap"}"#,
            r#"{"type":"ai-title","aiTitle":"맥북 상태 및 로컬 문제점 파악","sessionId":"s"}"#,
            r#"{"type":"system","subtype":"away_summary","content":"latest recap (disable recaps in /config)"}"#,
        ]);
        let s = last_session_summary(f.path());
        assert_eq!(s.recap.as_deref(), Some("latest recap"));
        assert_eq!(s.ai_title.as_deref(), Some("맥북 상태 및 로컬 문제점 파악"));
        assert!(!s.is_empty());
    }

    #[test]
    fn summary_tolerates_missing_chrome_blank_values_and_junk() {
        let f = write(&[
            "not json at all",
            r#"{"type":"ai-title","aiTitle":"   ","sessionId":"s"}"#,
            r#"{"type":"system","subtype":"away_summary","content":"   "}"#,
            // A recap without the trailing chrome must survive verbatim.
            r#"{"type":"system","subtype":"away_summary","content":"bare recap"}"#,
        ]);
        let s = last_session_summary(f.path());
        assert_eq!(s.recap.as_deref(), Some("bare recap"));
        assert_eq!(s.ai_title, None, "blank titles must not be captured");
    }
}
