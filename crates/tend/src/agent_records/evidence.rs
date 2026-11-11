//! What an agent actually ran in its latest turn, from its own records: the
//! commands it called and what came back, paired by the provider's call id.
//! Shown before the agent's own summary when someone reviews a result, so
//! the reader sees evidence first. Read only on the host whose records
//! these are; a session id is never looked up on another host.

use serde::{Deserialize, Serialize};

use super::{ReadRequest, RecordStore, SearchRequest};

/// At most this many call/result pairs, the latest ones.
pub const MAX_PAIRS: usize = 6;
/// Events read to find them.
const SCAN_EVENTS: usize = 80;
const CALL_CHARS: usize = 600;
const RESULT_CHARS: usize = 1200;

/// One command the agent ran and what came back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidencePair {
    pub tool: Option<String>,
    pub call: String,
    pub call_at: Option<String>,
    /// None when no result was recorded for this call (yet).
    pub result: Option<String>,
    pub result_at: Option<String>,
    /// Only what the provider recorded as an error, never inferred.
    pub is_error: Option<bool>,
    /// Either text was cut to fit.
    pub clipped: bool,
    /// The record events, so a reader can open the full text.
    pub call_event: String,
    pub result_event: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEvidence {
    pub session_id: String,
    /// Oldest first.
    pub pairs: Vec<EvidencePair>,
    /// More calls happened in the turn than are shown.
    pub more: bool,
    /// False when some records could not be read: missing pairs are then
    /// not evidence that nothing ran.
    pub complete: bool,
}

/// The latest calls `session_id` made since `since` (RFC 3339, the time of
/// the prompt being reviewed), paired with their results. A turn needs a
/// start: without one the whole session would pass for this turn's
/// evidence, so an empty `since` is refused.
pub fn turn_evidence(
    store: &RecordStore,
    session_id: &str,
    since: &str,
) -> Result<TurnEvidence, String> {
    if since.trim().is_empty() {
        return Err("no request time to start the turn from".into());
    }
    let request = SearchRequest {
        session_id: Some(session_id.to_string()),
        from: Some(since.to_string()),
        kinds: vec!["tool_call".into(), "tool_result".into()],
        limit: SCAN_EVENTS,
        ..SearchRequest::default()
    };
    let found = store.search(&request)?;
    // Newest first from the store; work oldest first.
    let mut events: Vec<_> = found
        .matches
        .into_iter()
        .map(|hit| hit.event)
        .filter(|event| event.session_id == session_id)
        .collect();
    events.reverse();
    let mut pairs: Vec<EvidencePair> = Vec::new();
    for event in &events {
        if event.kind != "tool_call" {
            continue;
        }
        let (call, call_cut) = clip(&event.text, CALL_CHARS);
        let result = event.call_id.as_ref().and_then(|id| {
            events
                .iter()
                .find(|other| other.kind == "tool_result" && other.call_id.as_ref() == Some(id))
        });
        // Search results carry only the start of each event; read the
        // result itself so its end, where tools print their verdict, is
        // kept.
        let (result_text, result_cut) = result
            .map(|r| {
                full_ends(store, &r.event_id).map_or_else(
                    || (clip(&r.text, RESULT_CHARS).0, true),
                    |(text, whole)| {
                        let (kept, cut) = clip_ends(&text, RESULT_CHARS);
                        (kept, cut || !whole)
                    },
                )
            })
            .map_or((None, false), |(text, cut)| (Some(text), cut));
        pairs.push(EvidencePair {
            tool: event.tool_name.clone(),
            call,
            call_at: event.timestamp.clone(),
            result: result_text,
            result_at: result.and_then(|r| r.timestamp.clone()),
            is_error: result.and_then(|r| r.is_error),
            clipped: call_cut
                || result_cut
                || event.truncated
                || result.is_some_and(|r| r.truncated),
            call_event: event.event_id.clone(),
            result_event: result.map(|r| r.event_id.clone()),
        });
    }
    let more = pairs.len() > MAX_PAIRS || found.has_more;
    if pairs.len() > MAX_PAIRS {
        pairs.drain(..pairs.len() - MAX_PAIRS);
    }
    Ok(TurnEvidence {
        session_id: session_id.to_string(),
        pairs,
        more,
        complete: found.coverage.complete,
    })
}

/// Pages read per event, at most: a result longer than this keeps its
/// start and the last page read, and is marked as not whole.
const MAX_PAGES: usize = 8;

/// An event's full text when it fits in [`MAX_PAGES`] reads, else its first
/// page and the last page read joined by a marked gap; and whether it is
/// whole. None when the event cannot be read.
fn full_ends(store: &RecordStore, event_id: &str) -> Option<(String, bool)> {
    let mut first = String::new();
    let mut last = String::new();
    let mut offset = 0;
    for page in 0..MAX_PAGES {
        let read = store
            .read(&ReadRequest {
                event_id: event_id.to_string(),
                before: 0,
                after: 0,
                include_tool_pair: false,
                text_offset: offset,
                max_chars: 64_000,
            })
            .ok()?;
        let text = read
            .events
            .into_iter()
            .find(|e| e.event_id == event_id)?
            .text;
        if page == 0 {
            first.clone_from(&text);
        } else {
            last = text;
        }
        match read.next_text_offset {
            Some(next) if next > offset => offset = next,
            _ => {
                return Some((
                    if last.is_empty() {
                        first
                    } else {
                        format!("{first}{last}")
                    },
                    true,
                ));
            }
        }
    }
    Some((format!("{first}\n…\n{last}"), false))
}

/// A long result keeps its start and its end, where tools print their
/// verdict (a failing test, an exit status), and marks the gap.
fn clip_ends(text: &str, max: usize) -> (String, bool) {
    let trimmed = text.trim();
    let count = trimmed.chars().count();
    if count <= max {
        return (trimmed.to_string(), false);
    }
    let head = max / 4;
    let tail = max - head;
    let start: String = trimmed.chars().take(head).collect();
    let end: String = trimmed.chars().skip(count - tail).collect();
    (format!("{start}\n…\n{end}"), true)
}

fn clip(text: &str, max: usize) -> (String, bool) {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        (trimmed.to_string(), false)
    } else {
        (trimmed.chars().take(max).collect::<String>() + "…", true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &std::path::Path, lines: &[serde_json::Value]) {
        let body: Vec<String> = lines.iter().map(ToString::to_string).collect();
        std::fs::write(dir.join("s1.jsonl"), body.join("\n") + "\n").unwrap();
    }

    #[test]
    fn calls_pair_with_their_results_and_stay_within_the_turn() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("proj");
        std::fs::create_dir_all(&project).unwrap();
        let line = |ts: &str, content: serde_json::Value, role: &str| {
            serde_json::json!({
                "type": role, "sessionId": "s1", "cwd": "/w", "timestamp": ts,
                "message": {"role": role, "content": content}
            })
        };
        write(
            &project,
            &[
                line(
                    "2026-11-11T00:00:00Z",
                    serde_json::json!([{"type":"tool_use","id":"old","name":"Bash","input":{"command":"ls"}}]),
                    "assistant",
                ),
                line(
                    "2026-11-11T00:00:01Z",
                    serde_json::json!([{"type":"tool_result","tool_use_id":"old","content":"a"}]),
                    "user",
                ),
                line(
                    "2026-11-11T01:00:00Z",
                    serde_json::json!([{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]),
                    "assistant",
                ),
                line(
                    "2026-11-11T01:00:05Z",
                    serde_json::json!([{"type":"tool_result","tool_use_id":"t1","content":"test result: ok","is_error":false}]),
                    "user",
                ),
                line(
                    "2026-11-11T01:00:06Z",
                    serde_json::json!([{"type":"tool_use","id":"t2","name":"Bash","input":{"command":"git push"}}]),
                    "assistant",
                ),
                line(
                    "2026-11-11T01:00:07Z",
                    serde_json::json!([{"type":"tool_use","id":"t3","name":"Bash","input":{"command":"cargo test --all"}}]),
                    "assistant",
                ),
                line(
                    "2026-11-11T01:00:09Z",
                    serde_json::json!([{"type":"tool_result","tool_use_id":"t3","content":
                        format!("{}FAILED: 1 test failed", "running a long test suite\n".repeat(120))}]),
                    "user",
                ),
            ],
        );
        let cache = tempfile::tempdir().unwrap();
        let store = RecordStore::new(
            cache.path().join("index.sqlite3"),
            vec![root.path().to_path_buf()],
        );
        let evidence = turn_evidence(&store, "s1", "2026-11-11T00:30:00Z").unwrap();
        assert_eq!(evidence.pairs.len(), 3, "{evidence:?}");
        assert!(evidence.pairs[0].call.contains("cargo test"));
        assert_eq!(evidence.pairs[0].result.as_deref(), Some("test result: ok"));
        assert!(evidence.pairs[1].call.contains("git push"));
        // A long result keeps the verdict printed at its end.
        assert!(
            evidence.pairs[2]
                .result
                .as_deref()
                .unwrap()
                .ends_with("FAILED: 1 test failed"),
            "{:?}",
            evidence.pairs[2]
        );
        assert!(evidence.pairs[2].clipped);
        assert_eq!(evidence.pairs[1].result, None, "no result recorded yet");
        assert!(!evidence.more);
        // Another session's id finds nothing; without a start, nothing is
        // read, so an earlier task's commands cannot pass for this turn's.
        assert!(turn_evidence(&store, "s2", "2026-11-11T00:30:00Z")
            .unwrap()
            .pairs
            .is_empty());
        assert!(turn_evidence(&store, "s1", "").is_err());
    }

    #[test]
    fn long_text_is_clipped_and_says_so() {
        let (text, cut) = clip(&"x".repeat(CALL_CHARS + 10), CALL_CHARS);
        assert!(cut);
        assert_eq!(text.chars().count(), CALL_CHARS + 1);
        assert_eq!(clip("  short  ", 10), ("short".into(), false));
        // A long result keeps its verdict at the end.
        let output = format!("{}\nFAILED: 1 test failed", "line\n".repeat(400));
        let (kept, cut) = clip_ends(&output, RESULT_CHARS);
        assert!(cut);
        assert!(kept.ends_with("FAILED: 1 test failed"));
        assert!(kept.contains("\n…\n"));
    }
}
