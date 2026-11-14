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
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// How much of the log's end is read. A turn's answer and the prompt
/// before it are near the end; a tool result larger than this between them
/// costs the answer, never the stop.
const TAIL_BYTES: u64 = 512 * 1024;

/// The log's last complete lines, oldest first.
pub(crate) fn tail(path: &Path) -> Option<Vec<Value>> {
    tail_bytes(path, TAIL_BYTES)
}

fn tail_bytes(path: &Path, max: u64) -> Option<Vec<Value>> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(max);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.take(max).read_to_end(&mut bytes).ok()?;
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

/// How much of the log's end [`poll`] reads. An `abort` is the last thing
/// Copilot writes for a cancelled turn, so it sits in the last few lines.
const SETTLE_TAIL_BYTES: u64 = 64 * 1024;

/// `$COPILOT_HOME` (or `~/.copilot`), when it exists: where the session
/// logs live.
#[must_use]
pub fn default_home() -> Option<PathBuf> {
    std::env::var_os("COPILOT_HOME")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| dirs::home_dir().map(|home| home.join(".copilot")))
        .filter(|path| path.is_dir())
}

/// How the log's current turn ended without a hook.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TurnEnd {
    /// `abort`: Esc on a permission prompt (or mid-turn).
    Cancelled { at: OffsetDateTime },
    /// `session.error` with `errorType: "query"`: Copilot gave up on the
    /// turn, e.g. after its model-call retries ran out (1.0.110 fires a
    /// recoverable `errorOccurred` per retry and nothing at the end).
    Failed { at: OffsetDateTime, message: String },
}

/// A `session.error` that ends the turn. Only `errorType: "query"` (what a
/// failed turn logged on 1.0.110) counts: an SDK extension's
/// `session.log(…, {level: "error"})` writes `session.error` with
/// `errorType: "notification"` in the middle of a working turn.
fn is_turn_failure(record: &Value) -> bool {
    record["type"] == "session.error" && record["data"]["errorType"] == "query"
}

/// The current turn's hookless ending: the latest of the turn's start
/// (`user.message`, `assistant.turn_start`) and its end (`abort`,
/// `session.error`), when it is an end.
///
/// `assistant.turn_end` alone settles nothing: Copilot writes one after
/// every model call, mid-turn too.
fn turn_end(records: &[Value]) -> Option<TurnEnd> {
    let record = records.iter().rev().find(|record| {
        is_user_prompt(record)
            || record["type"] == "assistant.turn_start"
            || record["type"] == "abort"
            || is_turn_failure(record)
    })?;
    let at = || {
        OffsetDateTime::parse(
            record["timestamp"].as_str()?,
            &time::format_description::well_known::Rfc3339,
        )
        .ok()
    };
    match record["type"].as_str()? {
        "abort" => Some(TurnEnd::Cancelled { at: at()? }),
        "session.error" if is_turn_failure(record) => Some(TurnEnd::Failed {
            at: at()?,
            message: record["data"]["message"]
                .as_str()
                .unwrap_or("session error")
                .to_string(),
        }),
        _ => None,
    }
}

/// The states a missing hook can leave a Copilot row in, and so the only
/// ones [`poll`] reads and settles.
const SETTLED_STATES: &[crate::event::AgentState] = &[
    crate::event::AgentState::Working,
    crate::event::AgentState::WaitingInput,
    crate::event::AgentState::WaitingChoice,
];

/// Settle Copilot rows a missing hook left working or waiting: for each,
/// read its session log and, when the turn there was cancelled or failed,
/// record that at the log's own time. The Store's precedence drops it when
/// a newer hook (the next prompt) has already moved the row.
pub async fn poll(store: &crate::state::SharedStore, home: &Path) {
    use crate::event::{AgentEvent, AgentId, AgentKind};
    let targets: Vec<AgentId> = store
        .snapshot()
        .await
        .into_iter()
        .filter(|agent| {
            agent.kind == AgentKind::Copilot
                && SETTLED_STATES.contains(&agent.state)
                // A session id names a directory; refuse anything that
                // could leave `session-state/`.
                && !agent.session_id.is_empty()
                && !agent.session_id.contains(['/', '\\'])
                && !agent.session_id.starts_with('.')
        })
        .map(|agent| AgentId {
            kind: AgentKind::Copilot,
            session_id: agent.session_id,
            surface: agent.surface,
            pane: agent.pane,
            tmux_socket: agent.tmux_socket,
            cwd: agent.cwd,
        })
        .collect();
    if targets.is_empty() {
        return;
    }
    let root = home.join("session-state");
    let settled = tokio::task::spawn_blocking(move || {
        targets
            .into_iter()
            .filter_map(|id| {
                let log = root.join(&id.session_id).join("events.jsonl");
                let end = turn_end(&tail_bytes(&log, SETTLE_TAIL_BYTES)?)?;
                Some((id, end))
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let now = OffsetDateTime::now_utc();
    for (id, end) in settled {
        let event = match end {
            TurnEnd::Cancelled { at } => AgentEvent::TurnStopped {
                id,
                response: None,
                recap: None,
                ai_title: None,
                // Positive evidence the wait is over: Copilot recorded the
                // permission prompt cancelled (`permission.completed`,
                // `abort`). Without it the Store keeps a response-less stop
                // on a waiting row waiting, as it must for Codex's early
                // `Stop`.
                idle_confirmed: true,
                at,
            },
            TurnEnd::Failed { at, message } => AgentEvent::NotificationFired {
                id,
                level: crate::event::NotificationLevel::Error,
                message: super::hook::truncate(format!("copilot: {message}"), 1_000),
                at,
            },
        };
        if event.at() > now {
            continue;
        }
        // Only a row that still exists and still needs settling: the user
        // may have forgotten it while the log was read.
        store
            .apply_observed_if_in(
                &event,
                crate::provenance::StateSource::ProviderRecord,
                SETTLED_STATES,
            )
            .await;
    }
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

    /// The real log cut after its first `n` lines.
    fn real_until(pattern: &str) -> Vec<Value> {
        let mut records = Vec::new();
        for line in REAL.lines() {
            records.push(serde_json::from_str(line).unwrap());
            if line.contains(pattern) {
                break;
            }
        }
        records
    }

    #[test]
    fn only_an_abort_after_the_turn_started_is_a_cancel() {
        // Turn 2 of the real session, cut at Esc's `abort`.
        let Some(TurnEnd::Cancelled { at }) = turn_end(&real_until("\"abort\"")) else {
            panic!("no cancel");
        };
        assert_eq!(at.to_string(), "2026-11-14 14:54:30.051 +00:00:00");
        // The whole log: turn 3 started after the abort and finished.
        let all: Vec<Value> = REAL
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(turn_end(&all), None);
        // Turn 1 ends with `assistant.turn_end`, which is no cancel.
        assert_eq!(turn_end(&real_until("agentStop")), None);
    }

    #[test]
    fn a_turn_whose_model_call_retries_ran_out_failed() {
        // Recorded on 1.0.110 with the model connection cut mid-answer.
        let records: Vec<Value> = include_str!("testdata/copilot-1.0.110-model-error-events.jsonl")
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let Some(TurnEnd::Failed { at, message }) = turn_end(&records) else {
            panic!("no failure");
        };
        assert_eq!(at.to_string(), "2026-11-14 16:55:26.487 +00:00:00");
        assert!(
            message.starts_with("Execution failed: Failed to get response"),
            "{message}"
        );
        // A diagnostic an extension logs is not a failure, during a turn or
        // after a real end.
        let diagnostic = serde_json::json!({"type": "session.error", "timestamp": "2026-11-14T16:55:30Z",
            "data": {"errorType": "notification", "message": "extension diagnostic"}});
        let mut after = records.clone();
        after.push(diagnostic.clone());
        assert!(matches!(turn_end(&after), Some(TurnEnd::Failed { .. })));
        let mut working = records[..3].to_vec();
        working.push(diagnostic.clone());
        working.push(serde_json::json!({"type": "tool.execution_start", "timestamp": "2026-11-14T16:55:31Z", "data": {}}));
        assert_eq!(turn_end(&working), None);
        let mut cancelled = real_until("\"abort\"");
        cancelled.push(diagnostic);
        assert!(matches!(
            turn_end(&cancelled),
            Some(TurnEnd::Cancelled { .. })
        ));
        // A later prompt starts a new turn: the old failure settles nothing.
        let mut later = records.clone();
        later.push(serde_json::json!({"type": "user.message", "timestamp": "2026-11-14T16:56:00Z", "data": {"content": "again"}}));
        assert_eq!(turn_end(&later), None);
    }

    mod poll {
        use super::super::*;
        use crate::event::{AgentEvent, AgentId, AgentKind, AgentState, NotificationLevel};
        use crate::state::Store;
        use time::Duration;

        fn id(session: &str, pane: &str) -> AgentId {
            AgentId {
                kind: AgentKind::Copilot,
                session_id: session.into(),
                surface: None,
                pane: Some(pane.into()),
                tmux_socket: None,
                cwd: Some("/w".into()),
            }
        }

        /// `seconds` from now. The Store dates a hook's watermark when it
        /// is ingested, so test times sit around now, as real hooks do.
        fn at(seconds: i64) -> OffsetDateTime {
            OffsetDateTime::now_utc() + Duration::seconds(seconds)
        }

        fn rfc3339(at: OffsetDateTime) -> String {
            at.format(&time::format_description::well_known::Rfc3339)
                .unwrap()
        }

        async fn hook(store: &crate::state::SharedStore, events: Vec<AgentEvent>) {
            for ev in events {
                store.apply_with_ask(&ev, None).await;
            }
        }

        fn write_log(home: &Path, session: &str, lines: &[String]) {
            let dir = home.join("session-state").join(session);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("events.jsonl"), lines.join("\n") + "\n").unwrap();
        }

        fn prompt_line(at: OffsetDateTime) -> String {
            format!(
                r#"{{"type":"user.message","timestamp":"{}","data":{{"content":"touch b.txt"}}}}"#,
                rfc3339(at)
            )
        }

        fn abort_line(at: OffsetDateTime) -> String {
            format!(
                r#"{{"type":"abort","timestamp":"{}","data":{{"reason":"user_initiated"}}}}"#,
                rfc3339(at)
            )
        }

        /// A Copilot row waiting on a permission prompt, as its hooks
        /// leave it.
        async fn waiting_row(store: &crate::state::SharedStore, session: &str, pane: &str) {
            let id = id(session, pane);
            hook(
                store,
                vec![
                    AgentEvent::Started {
                        id: id.clone(),
                        at: at(-3),
                    },
                    AgentEvent::PromptSubmitted {
                        id: id.clone(),
                        prompt: "touch b.txt".into(),
                        at: at(-2),
                    },
                    AgentEvent::NotificationFired {
                        id,
                        level: NotificationLevel::NeedsInput,
                        message: "copilot permission: Run command: touch b.txt".into(),
                        at: at(-1),
                    },
                ],
            )
            .await;
            assert_eq!(
                store.by_session(session).await.unwrap().state,
                AgentState::WaitingInput
            );
        }

        #[tokio::test]
        async fn esc_on_a_permission_prompt_settles_the_row_at_the_cancel() {
            let home = tempfile::tempdir().unwrap();
            let store = Store::shared();
            waiting_row(&store, "s1", "%1").await;
            // Esc: Copilot writes `abort` and fires no hook.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let cancel = OffsetDateTime::now_utc();
            write_log(
                home.path(),
                "s1",
                &[prompt_line(at(-2)), abort_line(cancel)],
            );
            poll(&store, home.path()).await;
            let row = store.by_session("s1").await.unwrap();
            assert_eq!(row.state, AgentState::Idle);
            assert_eq!(row.last_response, None);
            assert!((row.state_entered_at - cancel).abs() < Duration::milliseconds(1));
            // Settled rows are not read again.
            poll(&store, home.path()).await;
            assert_eq!(
                store.by_session("s1").await.unwrap().state,
                AgentState::Idle
            );
        }

        #[tokio::test]
        async fn a_turn_copilot_gave_up_on_fails_the_row() {
            let home = tempfile::tempdir().unwrap();
            let store = Store::shared();
            let id = id("f1", "%1");
            hook(
                &store,
                vec![
                    AgentEvent::Started {
                        id: id.clone(),
                        at: at(-3),
                    },
                    AgentEvent::PromptSubmitted {
                        id,
                        prompt: "count".into(),
                        at: at(-2),
                    },
                ],
            )
            .await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let failed = OffsetDateTime::now_utc();
            write_log(
                home.path(),
                "f1",
                &[
                    prompt_line(at(-2)),
                    format!(
                        r#"{{"type":"session.error","timestamp":"{}","data":{{"errorType":"query","message":"Execution failed: retried 5 times"}}}}"#,
                        rfc3339(failed)
                    ),
                ],
            );
            poll(&store, home.path()).await;
            let row = store.by_session("f1").await.unwrap();
            assert_eq!(row.state, AgentState::Error);
            assert_eq!(
                row.last_notification.as_deref(),
                Some("copilot: Execution failed: retried 5 times")
            );
        }

        #[tokio::test]
        async fn a_newer_hook_wins_over_an_older_cancel() {
            let home = tempfile::tempdir().unwrap();
            let store = Store::shared();
            waiting_row(&store, "s2", "%1").await;
            let cancel = OffsetDateTime::now_utc();
            write_log(
                home.path(),
                "s2",
                &[prompt_line(at(-2)), abort_line(cancel)],
            );
            // The next prompt is hooked before Copilot flushes it to its log.
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            hook(
                &store,
                vec![AgentEvent::PromptSubmitted {
                    id: id("s2", "%1"),
                    prompt: "again".into(),
                    at: at(0),
                }],
            )
            .await;
            poll(&store, home.path()).await;
            assert_eq!(
                store.by_session("s2").await.unwrap().state,
                AgentState::Working
            );
        }

        /// From the #538 review: the row is forgotten while its log is read.
        #[test]
        fn a_row_forgotten_during_the_read_is_not_recreated() {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .max_blocking_threads(1)
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let home = tempfile::tempdir().unwrap();
                let store = Store::shared();
                waiting_row(&store, "gone", "%1").await;
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                write_log(
                    home.path(),
                    "gone",
                    &[prompt_line(at(-2)), abort_line(OffsetDateTime::now_utc())],
                );
                // Hold the only blocking thread so the poll's read waits.
                let (release, held) = std::sync::mpsc::channel::<()>();
                let blocker = tokio::task::spawn_blocking(move || held.recv().ok());
                let polling = {
                    let store = store.clone();
                    let home = home.path().to_path_buf();
                    tokio::spawn(async move { poll(&store, &home).await })
                };
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                assert!(store.forget_agent("gone").await);
                release.send(()).unwrap();
                blocker.await.unwrap();
                polling.await.unwrap();
                assert!(store.by_session("gone").await.is_none());
            });
        }

        #[tokio::test]
        async fn a_row_that_moved_on_during_the_read_is_left_alone() {
            let store = Store::shared();
            waiting_row(&store, "s4", "%1").await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let stop = AgentEvent::TurnStopped {
                id: id("s4", "%1"),
                response: None,
                recap: None,
                ai_title: None,
                idle_confirmed: true,
                at: OffsetDateTime::now_utc(),
            };
            store
                .apply_observed_if_in(
                    &stop,
                    crate::provenance::StateSource::ProviderRecord,
                    &[AgentState::Working],
                )
                .await;
            assert_eq!(
                store.by_session("s4").await.unwrap().state,
                AgentState::WaitingInput
            );
        }

        #[tokio::test]
        async fn a_session_id_that_leaves_the_log_tree_is_never_read() {
            let home = tempfile::tempdir().unwrap();
            let store = Store::shared();
            let evil = "../s3";
            waiting_row(&store, evil, "%1").await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            // An abort log where `session-state/../s3` would point.
            let dir = home.path().join("s3");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("events.jsonl"),
                abort_line(OffsetDateTime::now_utc()) + "\n",
            )
            .unwrap();
            poll(&store, home.path()).await;
            assert_eq!(
                store.by_session(evil).await.unwrap().state,
                AgentState::WaitingInput
            );
        }

        #[tokio::test]
        async fn other_kinds_and_a_missing_log_are_left_alone() {
            let home = tempfile::tempdir().unwrap();
            let store = Store::shared();
            let mut codex = id("c1", "%1");
            codex.kind = AgentKind::Codex;
            hook(
                &store,
                vec![
                    AgentEvent::Started {
                        id: codex.clone(),
                        at: at(-2),
                    },
                    AgentEvent::PromptSubmitted {
                        id: codex,
                        prompt: "x".into(),
                        at: at(-1),
                    },
                ],
            )
            .await;
            waiting_row(&store, "nolog", "%2").await;
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            write_log(home.path(), "c1", &[abort_line(OffsetDateTime::now_utc())]);
            poll(&store, home.path()).await;
            assert_eq!(
                store.by_session("c1").await.unwrap().state,
                AgentState::Working
            );
            assert_eq!(
                store.by_session("nolog").await.unwrap().state,
                AgentState::WaitingInput
            );
        }
    }

    #[test]
    fn a_missing_log_is_none() {
        assert_eq!(last_answer(Path::new("/nonexistent/events.jsonl")), None);
    }
}
