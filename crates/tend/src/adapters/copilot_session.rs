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

/// [`last_answer`] for the `agentStop` hook: waits until the log holds
/// Copilot's own `hook.start` record for this stop (written after the
/// answer), so a slow flush does not cost the answer. After
/// [`STOP_FLUSH_WAIT`] it reads whatever is there.
#[must_use]
pub fn answer_at_stop(path: &Path) -> Option<String> {
    let deadline = std::time::Instant::now() + STOP_FLUSH_WAIT;
    loop {
        let records = tail(path)?;
        if stop_recorded(&records) || std::time::Instant::now() >= deadline {
            return answer_in(&records);
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// Does the log already hold the `agentStop` hook of the current turn
/// (after the last prompt)?
fn stop_recorded(records: &[Value]) -> bool {
    for record in records.iter().rev() {
        if is_user_prompt(record) {
            return false;
        }
        if record["type"] == "hook.start" && record["data"]["hookType"] == "agentStop" {
            return true;
        }
    }
    false
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

    #[test]
    fn at_a_stop_the_answer_flushed_while_the_hook_runs_is_read() {
        let prompt = r#"{"type":"user.message","data":{"content":"q"}}"#;
        let (_dir, path) = write(&format!("{prompt}\n"));
        let late = path.clone();
        // Copilot 1.0.110 writes the answer and its own hook record ~200 ms
        // after starting the hook.
        let writer = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let mut file = std::fs::OpenOptions::new().append(true).open(late).unwrap();
            std::io::Write::write_all(
                &mut file,
                concat!(
                    r#"{"type":"assistant.message","data":{"content":"late"}}"#,
                    "\n",
                    r#"{"type":"hook.start","data":{"hookType":"agentStop"}}"#,
                    "\n"
                )
                .as_bytes(),
            )
            .unwrap();
        });
        assert_eq!(answer_at_stop(&path).as_deref(), Some("late"));
        writer.join().unwrap();
    }

    #[test]
    fn at_a_stop_a_log_that_never_flushes_costs_the_answer_after_a_bound() {
        let (_dir, path) = write("{\"type\":\"user.message\",\"data\":{\"content\":\"q\"}}\n");
        let started = std::time::Instant::now();
        assert_eq!(answer_at_stop(&path), None);
        let waited = started.elapsed();
        assert!(
            waited >= STOP_FLUSH_WAIT && waited < STOP_FLUSH_WAIT * 3,
            "{waited:?}"
        );
        // A stop already on disk is read at once.
        let text = concat!(
            r#"{"type":"user.message","data":{"content":"q"}}"#,
            "\n",
            r#"{"type":"assistant.message","data":{"content":"a"}}"#,
            "\n",
            r#"{"type":"hook.start","data":{"hookType":"agentStop"}}"#,
            "\n"
        );
        let (_dir, path) = write(text);
        let started = std::time::Instant::now();
        assert_eq!(answer_at_stop(&path).as_deref(), Some("a"));
        assert!(started.elapsed() < STOP_FLUSH_WAIT);
    }

    #[test]
    fn a_missing_log_is_none() {
        assert_eq!(last_answer(Path::new("/nonexistent/events.jsonl")), None);
    }
}
