//! Copilot CLI's own session log, `$COPILOT_HOME/session-state/<id>/events.jsonl`.
//!
//! One JSON object per line, `{"type", "timestamp", "id", "data"}`, appended
//! as the session runs (Copilot CLI 1.0.110). The types tend reads:
//!
//! - `user.message` — `data.content`; Copilot also writes a `source:
//!   "memory"` one before each prompt, which is not the user's.
//! - `assistant.message` — `data.content`, empty while the model only calls
//!   tools.
//! - `assistant.turn_end`, `abort` (`reason: "user_initiated"`) — what Esc
//!   leaves behind when no hook fires.
//!
//! Only the tail is read: the log holds every tool result of the session
//! and grows without bound.

use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// How much of the log's end is read. A turn's answer and the prompt
/// before it are near the end; a tool result larger than this between them
/// costs the answer, never the stop.
const TAIL_BYTES: u64 = 512 * 1024;

/// The log's last complete lines, oldest first.
pub(crate) fn tail(path: &Path) -> Option<Vec<Value>> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(TAIL_BYTES).read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines = text.lines();
    if start > 0 {
        // The first line was cut by the seek.
        lines.next();
    }
    Some(
        lines
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect(),
    )
}

/// Is this `user.message` something the user sent (not Copilot's own
/// memory preamble)?
fn is_user_prompt(record: &Value) -> bool {
    record["type"] == "user.message" && record["data"]["source"].as_str().is_none()
}

/// The text of the last turn's answer: the last non-empty
/// `assistant.message` after the last prompt the user sent, or `None` when
/// the turn ended without one.
#[must_use]
pub fn last_answer(path: &Path) -> Option<String> {
    answer_in(&tail(path)?)
}

/// How long `agentStop` waits for Copilot to flush the turn to its log.
/// On 1.0.110 the log trails the hook by up to ~200 ms: when the hook
/// starts, the answer may not be on disk yet, and Copilot flushes it while
/// the hook runs.
const STOP_FLUSH_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

/// [`last_answer`] for the `agentStop` hook whose payload `timestamp` is
/// `stop_ms` (Unix milliseconds). Copilot writes its own `hook.start
/// agentStop` record with that exact timestamp after the answer, and on
/// 1.0.110 flushes it up to ~200 ms into the hook. This waits (at most
/// [`STOP_FLUSH_WAIT`]) for that record and reads the answer from what
/// precedes it, so a log that still ends at an earlier turn can never
/// supply that turn's answer. `None` when the stop is not on disk in time.
#[must_use]
pub fn answer_at_stop(path: &Path, stop_ms: i64) -> Option<String> {
    let deadline = std::time::Instant::now() + STOP_FLUSH_WAIT;
    loop {
        let records = tail(path)?;
        // The first stop at or after this hook's time is this hook's.
        if let Some(stop) = records
            .iter()
            .position(|record| is_stop_at(record, stop_ms))
        {
            return answer_in(&records[..stop]);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Is `record` Copilot's `hook.start` for the `agentStop` hook fired at
/// `stop_ms` (or later)?
fn is_stop_at(record: &Value, stop_ms: i64) -> bool {
    record["type"] == "hook.start"
        && record["data"]["hookType"] == "agentStop"
        && record_ms(record).is_some_and(|at| at >= stop_ms)
}

/// A record's `timestamp` (RFC 3339) in Unix milliseconds.
fn record_ms(record: &Value) -> Option<i64> {
    let at = time::OffsetDateTime::parse(
        record["timestamp"].as_str()?,
        &time::format_description::well_known::Rfc3339,
    )
    .ok()?;
    i64::try_from(at.unix_timestamp_nanos() / 1_000_000).ok()
}

fn answer_in(records: &[Value]) -> Option<String> {
    for record in records.iter().rev() {
        if is_user_prompt(record) {
            return None;
        }
        if record["type"] == "assistant.message" {
            if let Some(text) = record["data"]["content"]
                .as_str()
                .map(str::trim)
                .filter(|text| !text.is_empty())
            {
                return Some(text.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = include_str!("testdata/copilot-1.0.110-events.jsonl");

    fn write(text: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, text).unwrap();
        (dir, path)
    }

    #[test]
    fn the_answer_is_the_last_text_after_the_last_prompt() {
        // The real log ends with a second prompt that was cancelled: no answer.
        let (_dir, path) = write(REAL);
        assert_eq!(last_answer(&path), None);
        // Cut after the first turn, its answer is "hello".
        let first_turn: String = REAL
            .lines()
            .take_while(|line| !line.contains("touch b.txt"))
            .fold(String::new(), |text, line| text + line + "\n");
        let (_dir, path) = write(&first_turn);
        assert_eq!(last_answer(&path).as_deref(), Some("hello"));
    }

    #[test]
    fn copilots_memory_preamble_is_not_a_prompt() {
        let text = [
            r#"{"type":"user.message","data":{"content":"q"}}"#,
            r#"{"type":"assistant.message","data":{"content":"a"}}"#,
            r#"{"type":"user.message","data":{"content":"","source":"memory"}}"#,
        ]
        .join("\n");
        let (_dir, path) = write(&text);
        assert_eq!(last_answer(&path).as_deref(), Some("a"));
    }

    #[test]
    fn a_long_log_is_read_from_its_end() {
        let filler = format!(
            "{{\"type\":\"tool.execution_complete\",\"data\":{{\"result\":\"{}\"}}}}\n",
            "x".repeat(4096)
        );
        let mut text = String::from("{\"type\":\"user.message\",\"data\":{\"content\":\"q\"}}\n");
        text.push_str(&filler.repeat(200));
        text.push_str("{\"type\":\"assistant.message\",\"data\":{\"content\":\"done\"}}\n");
        let (_dir, path) = write(&text);
        assert_eq!(last_answer(&path).as_deref(), Some("done"));
        assert!(tail(&path).unwrap().len() < 200);
    }

    fn line(kind: &str, at_ms: i64, data: &str) -> String {
        let at = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(at_ms) * 1_000_000)
            .unwrap()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        format!("{{\"type\":\"{kind}\",\"timestamp\":\"{at}\",\"data\":{data}}}\n")
    }

    /// A whole turn: its prompt, answer and Copilot's record of the stop.
    fn turn(prompt_ms: i64, answer: &str, stop_ms: i64) -> String {
        line("user.message", prompt_ms, r#"{"content":"q"}"#)
            + &line(
                "assistant.message",
                prompt_ms + 10,
                &format!(r#"{{"content":"{answer}"}}"#),
            )
            + &line("hook.start", stop_ms, r#"{"hookType":"agentStop"}"#)
    }

    fn append_later(path: &Path, text: String) -> std::thread::JoinHandle<()> {
        let path = path.to_path_buf();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
            std::io::Write::write_all(&mut file, text.as_bytes()).unwrap();
        })
    }

    #[test]
    fn at_a_stop_the_turn_flushed_while_the_hook_runs_is_read() {
        // Only the prompt is on disk when the hook starts.
        let (_dir, path) = write(&line("user.message", 1_000, r#"{"content":"q"}"#));
        let writer = append_later(
            &path,
            line("assistant.message", 1_100, r#"{"content":"late"}"#)
                + &line("hook.start", 2_000, r#"{"hookType":"agentStop"}"#),
        );
        assert_eq!(answer_at_stop(&path, 2_000).as_deref(), Some("late"));
        writer.join().unwrap();
    }

    /// From the #536 review: the log still ends at a complete earlier turn
    /// and the whole current turn arrives later.
    #[test]
    fn at_a_stop_an_earlier_turns_answer_is_never_read() {
        let (_dir, path) = write(&turn(1_000, "OLD ANSWER", 2_000));
        let writer = append_later(&path, turn(5_000, "NEW ANSWER", 6_000));
        assert_eq!(answer_at_stop(&path, 6_000).as_deref(), Some("NEW ANSWER"));
        writer.join().unwrap();
        // The current stop never reaches the disk: no answer, after the bound.
        let (_dir, path) = write(&turn(1_000, "OLD ANSWER", 2_000));
        let started = std::time::Instant::now();
        assert_eq!(answer_at_stop(&path, 6_000), None);
        let waited = started.elapsed();
        assert!(
            waited >= STOP_FLUSH_WAIT && waited < STOP_FLUSH_WAIT * 3,
            "{waited:?}"
        );
    }

    #[test]
    fn at_a_stop_already_on_disk_the_answer_is_read_at_once() {
        // A later turn written after it does not change which answer is read.
        let (_dir, path) = write(&(turn(1_000, "first", 2_000) + &turn(5_000, "second", 6_000)));
        let started = std::time::Instant::now();
        assert_eq!(answer_at_stop(&path, 2_000).as_deref(), Some("first"));
        assert_eq!(answer_at_stop(&path, 6_000).as_deref(), Some("second"));
        assert!(started.elapsed() < STOP_FLUSH_WAIT);
        // A turn whose answer is only tool calls has none.
        let (_dir, path) = write(&turn(1_000, "", 2_000));
        assert_eq!(answer_at_stop(&path, 2_000), None);
    }

    #[test]
    fn a_missing_log_is_none() {
        assert_eq!(last_answer(Path::new("/nonexistent/events.jsonl")), None);
    }
}
