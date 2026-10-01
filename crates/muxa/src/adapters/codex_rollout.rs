//! `OpenAI` Codex CLI rollout-file lifecycle and rate-limit parser.
//!
//! Codex has no error / rate-limit hook (its hook surface is the same five
//! lifecycle events Claude Code ships: `SessionStart`, `UserPromptSubmit`,
//! `PreToolUse`, `PostToolUse`, `Stop`). When a turn is blocked by a usage
//! cap — often *before* the turn even starts, so no `Stop` fires — nothing
//! on the hook channel tells muxa about it. The signal lives instead on
//! disk: codex appends a JSONL rollout per session under
//!
//! ```text
//! ~/.codex/sessions/YYYY/MM/DD/rollout-<ISO8601>-<session_id>.jsonl
//! ```
//!
//! and stamps a `token_count` event after every model response carrying the
//! current rate-limit windows:
//!
//! ```json
//! {
//!   "timestamp": "2026-06-12T06:26:08.491Z",
//!   "type": "event_msg",
//!   "payload": {
//!     "type": "token_count",
//!     "rate_limits": {
//!       "primary":   {"used_percent": 5.0,  "window_minutes": 300,   "resets_at": 1781262859},
//!       "secondary": {"used_percent": 46.0, "window_minutes": 10080,  "resets_at": 1781745469},
//!       "rate_limit_reached_type": null
//!     }
//!   }
//! }
//! ```
//!
//! Map windows by `window_minutes`: 300 is the 5-hour window and 10080 the
//! 7-day one. Older records use `primary` for 5h and `secondary` for 7d;
//! newer Codex can put the weekly window in `primary` alone. The reconciler
//! polls this via [`session_rate_limits`] and feeds the result through the
//! existing `Heartbeat` (percentages) and `RateLimited` (hard cap) paths.
//!
//! Utilization and credit balances are telemetry, not proof that the active
//! model/plan is blocked. Only an explicit `rate_limit_reached_type` marks a
//! cap; 100% utilization or an empty credit balance alone must not change
//! the agent's lifecycle state.
//!
//! This mirrors the Claude [`transcript`](super::transcript) module: an
//! unofficial on-disk format read best-effort, guarded by golden fixtures.
//! Every function returns `None` on any failure (missing dir, malformed
//! lines, truncated tail) — the caller treats that as "no reading this tick".

use crate::event::RateLimitScope;
use serde::Deserialize;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// Read at most this many bytes from the tail of a rollout file. Rollouts
/// grow unbounded over a long session, but the freshest `rate_limits`
/// record is always near the end — 256 KB is comfortable headroom for the
/// last several events while keeping per-tick IO bounded. Matches the tail
/// budget the Claude transcript parser uses.
const TAIL_BYTES: u64 = 256 * 1024;

/// One rate-limit window parsed from codex's `payload.rate_limits`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Window {
    /// Utilization, 0–100 (codex documents `used_percent`).
    pub used_percent: f32,
    /// Absolute reset time, decoded from the `resets_at` Unix timestamp.
    /// `None` when codex omitted it (the field moves independently of
    /// `used_percent`).
    pub resets_at: Option<OffsetDateTime>,
}

/// The latest `rate_limits` snapshot found in a rollout file.
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimits {
    /// Time of the telemetry record, when supplied by Codex.
    pub at: Option<OffsetDateTime>,
    /// `primary` window — the 5-hour rolling cap.
    pub five_hour: Option<Window>,
    /// `secondary` window — the 7-day weekly cap.
    pub seven_day: Option<Window>,
    /// Explicit blocked signal, mapped using the reported window duration.
    /// Percentages and credit balances never imply a blocked turn.
    pub reached: Option<RateLimitScope>,
}

// ---------------------------------------------------------------------------
// Wire shapes — only the fields we consume, everything else ignored.

#[derive(Deserialize)]
struct Record {
    timestamp: Option<String>,
    payload: Option<Payload>,
}

#[derive(Deserialize)]
struct Payload {
    rate_limits: Option<RawRateLimits>,
}

#[derive(Deserialize)]
struct RawRateLimits {
    primary: Option<RawWindow>,
    secondary: Option<RawWindow>,
    /// `null` normally; a window name (`"primary"` / `"secondary"`) when a
    /// cap was reached.
    #[serde(default)]
    rate_limit_reached_type: Option<String>,
}

#[derive(Deserialize)]
struct RawWindow {
    used_percent: Option<f32>,
    window_minutes: Option<u32>,
    /// Unix epoch *seconds*. Optional even when `used_percent` is present.
    resets_at: Option<i64>,
}

fn window(w: Option<RawWindow>) -> Option<Window> {
    let w = w?;
    Some(Window {
        used_percent: w.used_percent?,
        resets_at: w
            .resets_at
            .and_then(|s| OffsetDateTime::from_unix_timestamp(s).ok()),
    })
}

/// Parse a single rollout line into a [`RateLimits`] snapshot, or `None`
/// when the line isn't a rate-limit-bearing record. Walking callers keep
/// the most recent `Some(...)`.
fn parse_line(line: &str) -> Option<RateLimits> {
    let rec: Record = serde_json::from_str(line.trim()).ok()?;
    let raw = rec.payload?.rate_limits?;

    // Newer Codex can put the weekly window in `primary` with no
    // `secondary`. Prefer the duration; keep positional defaults for older
    // records that omitted it.
    let primary_scope = window_scope(raw.primary.as_ref(), RateLimitScope::FiveHour);
    let secondary_scope = window_scope(raw.secondary.as_ref(), RateLimitScope::SevenDay);
    let primary = window(raw.primary);
    let secondary = window(raw.secondary);
    let mut five_hour = None;
    let mut seven_day = None;
    for (scope, reading) in [(primary_scope, primary), (secondary_scope, secondary)] {
        match scope {
            RateLimitScope::FiveHour if reading.is_some() => five_hour = reading,
            RateLimitScope::SevenDay if reading.is_some() => seven_day = reading,
            _ => {}
        }
    }
    // A snapshot can describe an exhausted quota for a different model or
    // limit while this turn continues on plan quota. Never infer a lifecycle
    // error from percentages or account credit balances.
    let reached = match raw.rate_limit_reached_type.as_deref() {
        Some("primary") => Some(primary_scope),
        Some("secondary") => Some(secondary_scope),
        Some(_) => Some(RateLimitScope::Unknown),
        None => None,
    };

    Some(RateLimits {
        at: rec.timestamp.as_deref().and_then(|timestamp| {
            OffsetDateTime::parse(timestamp, &time::format_description::well_known::Rfc3339).ok()
        }),
        five_hour,
        seven_day,
        reached,
    })
}

fn window_scope(window: Option<&RawWindow>, fallback: RateLimitScope) -> RateLimitScope {
    match window.and_then(|w| w.window_minutes) {
        Some(300) => RateLimitScope::FiveHour,
        Some(10080) => RateLimitScope::SevenDay,
        Some(_) => RateLimitScope::Unknown,
        None => fallback,
    }
}

/// Read the tail of a rollout file and return the most recent `rate_limits`
/// snapshot. `None` for any failure mode (file missing, no rate-limit
/// record in the tail window, malformed lines) — same silent-failure
/// contract as the Claude transcript parser.
pub fn latest_rate_limits(path: &Path) -> Option<RateLimits> {
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(start)).ok()?;

    let reader = BufReader::new(f);
    let mut latest: Option<RateLimits> = None;
    // A mid-file seek almost always lands inside a line; that fragment
    // fails to parse and is silently skipped, same as the transcript reader.
    for line in reader.lines().map_while(Result::ok) {
        if let Some(rl) = parse_line(&line) {
            latest = Some(rl);
        }
    }
    latest
}

/// Latest lifecycle evidence persisted by Codex, independent of quota telemetry.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub at: OffsetDateTime,
    pub event: StatusEvent,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StatusEvent {
    Working,
    Stopped {
        response: Option<String>,
        interrupted: bool,
    },
    Error {
        message: String,
    },
}

/// Read actual event timestamps so unchanged files cannot manufacture recent
/// activity or continually reorder the watch list. Partial JSON is skipped.
pub fn latest_status(path: &Path) -> Option<Status> {
    let mut file = File::open(path).ok()?;
    let start = file.metadata().ok()?.len().saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut latest = None;
    let mut response = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(record) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let payload = &record["payload"];
        let event = match (record["type"].as_str(), payload["type"].as_str()) {
            (Some("event_msg"), Some("task_started" | "user_message")) => {
                response = None;
                StatusEvent::Working
            }
            (Some("event_msg"), Some("task_complete")) => {
                if let Some(error) = payload.get("error").filter(|error| !error.is_null()) {
                    StatusEvent::Error {
                        message: super::hook::truncate(
                            error["message"]
                                .as_str()
                                .unwrap_or("Codex turn failed")
                                .to_owned(),
                            4_000,
                        ),
                    }
                } else {
                    let final_response = payload["last_agent_message"]
                        .as_str()
                        .filter(|text| !text.trim().is_empty())
                        .map(|text| super::hook::truncate(text.to_owned(), 4_000))
                        .or_else(|| response.clone());
                    StatusEvent::Stopped {
                        response: final_response,
                        interrupted: false,
                    }
                }
            }
            (Some("event_msg"), Some("turn_aborted")) => {
                response = None;
                StatusEvent::Stopped {
                    response: None,
                    interrupted: true,
                }
            }
            (Some("response_item"), Some("message")) if payload["role"] == "user" => {
                response = None;
                StatusEvent::Working
            }
            (Some("response_item"), Some("message")) if payload["role"] == "assistant" => {
                if matches!(payload["phase"].as_str(), Some("final" | "final_answer")) {
                    let text = payload["content"].as_array().map(|parts| {
                        parts
                            .iter()
                            .filter(|part| part["type"] == "output_text")
                            .filter_map(|part| part["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n")
                    });
                    response = text
                        .filter(|text| !text.trim().is_empty())
                        .map(|text| super::hook::truncate(text, 4_000));
                    StatusEvent::Stopped {
                        response: response.clone(),
                        interrupted: false,
                    }
                } else {
                    StatusEvent::Working
                }
            }
            (
                Some("response_item"),
                Some(
                    "reasoning"
                    | "function_call"
                    | "function_call_output"
                    | "custom_tool_call"
                    | "custom_tool_call_output",
                ),
            ) => StatusEvent::Working,
            _ => continue,
        };
        let Some(at) = record["timestamp"].as_str().and_then(|value| {
            OffsetDateTime::parse(value, &time::format_description::well_known::Rfc3339).ok()
        }) else {
            continue;
        };
        if latest
            .as_ref()
            .is_none_or(|previous: &Status| at >= previous.at)
        {
            latest = Some(Status { at, event });
        }
    }
    latest
}

/// Read a final assistant response from the current turn only. Commentary,
/// tool output, and a previous turn's answer must not turn a response-less
/// Stop (for example at a permission prompt) into a successful completion.
pub fn latest_final_response(path: &Path) -> Option<String> {
    let mut file = File::open(path).ok()?;
    let start = file.metadata().ok()?.len().saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut latest = None;
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let payload = &value["payload"];
        match (value["type"].as_str(), payload["type"].as_str()) {
            (Some("event_msg"), Some("task_started" | "user_message" | "turn_aborted")) => {
                latest = None;
            }
            (Some("response_item"), Some("message")) if payload["role"] == "user" => latest = None,
            (Some("response_item"), Some("message"))
                if payload["role"] == "assistant"
                    && matches!(payload["phase"].as_str(), Some("final" | "final_answer")) =>
            {
                let text = payload["content"].as_array().map(|parts| {
                    parts
                        .iter()
                        .filter(|part| part["type"] == "output_text")
                        .filter_map(|part| part["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                });
                latest = text.filter(|text| !text.trim().is_empty());
            }
            (Some("event_msg"), Some("task_complete")) => {
                if payload.get("error").is_some_and(|error| !error.is_null()) {
                    latest = None;
                    continue;
                }
                if let Some(text) = payload["last_agent_message"]
                    .as_str()
                    .filter(|text| !text.trim().is_empty())
                {
                    latest = Some(text.to_owned());
                }
            }
            _ => {}
        }
    }
    latest
}

/// Locate the rollout JSONL for `session_id` by scanning the
/// date-partitioned `sessions_root` around `now`.
///
/// Codex names each file `rollout-<ISO8601>-<session_id>.jsonl` and files it
/// under `YYYY/MM/DD/` by the *start* date in the user's **local** timezone
/// (the ISO stamp in the filename is local — a session opened at 06:24 UTC
/// in KST lands under a `…T15-24-…` name). `now` here is UTC, so the local
/// rollout date can be one day *ahead* of (or behind) the UTC date. We
/// therefore scan `now + 1 day` through `now - lookback_days`: a local
/// offset is at most ±14h, so the local date is always within ±1 of the UTC
/// date, and the forward day closes the gap for east-of-UTC zones.
///
/// We match on the `session_id` suffix and return the first hit, newest day
/// first. Bounded so the scan is a handful of `read_dir`s, not a recursive
/// walk of the whole history back to the first session.
pub fn locate_rollout(
    sessions_root: &Path,
    session_id: &str,
    now: OffsetDateTime,
    lookback_days: u16,
) -> Option<PathBuf> {
    let suffix = format!("-{session_id}.jsonl");
    // Candidate dates, newest first: tomorrow (UTC), today, then back
    // `lookback_days`. `next_day`/`previous_day` only return `None` at the
    // ends of the representable calendar, which we'll never hit in practice.
    let mut dates = Vec::with_capacity(usize::from(lookback_days) + 2);
    if let Some(tomorrow) = now.date().next_day() {
        dates.push(tomorrow);
    }
    let mut date = now.date();
    dates.push(date);
    for _ in 0..lookback_days {
        let Some(prev) = date.previous_day() else {
            break;
        };
        dates.push(prev);
        date = prev;
    }

    // Resumed sessions retain their original rollout date. Codex uses UUIDv7
    // session IDs, whose timestamp lets us find old live sessions without a
    // recursive history scan or expanding every poll's directory budget.
    if let Some(created) = uuid::Uuid::parse_str(session_id)
        .ok()
        .filter(|id| id.get_version_num() == 7)
        .and_then(|id| id.get_timestamp())
        .and_then(|timestamp| i64::try_from(timestamp.to_unix().0).ok())
        .and_then(|seconds| OffsetDateTime::from_unix_timestamp(seconds).ok())
    {
        for candidate in [
            created.date().next_day(),
            Some(created.date()),
            created.date().previous_day(),
        ]
        .into_iter()
        .flatten()
        {
            if !dates.contains(&candidate) {
                dates.push(candidate);
            }
        }
    }

    for date in dates {
        let dir = sessions_root
            .join(format!("{:04}", date.year()))
            .join(format!("{:02}", u8::from(date.month())))
            .join(format!("{:02}", date.day()));
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with("rollout-") && name.ends_with(suffix.as_str()) {
                    return Some(entry.path());
                }
            }
        }
    }
    None
}

/// The default codex sessions tree, `~/.codex/sessions`. `None` when the
/// home directory can't be resolved. The daemon passes this to the
/// reconciler; tests inject a temp dir instead.
pub fn default_sessions_root() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex").join("sessions"))
}

/// Convenience: locate the rollout for `session_id` and return its latest
/// rate-limit snapshot in one call. `None` when the file can't be found or
/// carries no rate-limit record.
pub fn session_rate_limits(
    sessions_root: &Path,
    session_id: &str,
    now: OffsetDateTime,
    lookback_days: u16,
) -> Option<RateLimits> {
    let path = locate_rollout(sessions_root, session_id, now, lookback_days)?;
    latest_rate_limits(&path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::{tempdir, TempDir};
    use time::macros::datetime;

    #[test]
    fn lifecycle_tracks_reasoning_completion_errors_and_new_turns() {
        let dir = tempdir().unwrap();
        let mut lines = Vec::new();
        let cases = [
            (
                "event_msg",
                serde_json::json!({"type":"task_started"}),
                StatusEvent::Working,
            ),
            (
                "response_item",
                serde_json::json!({"type":"reasoning"}),
                StatusEvent::Working,
            ),
            (
                "response_item",
                serde_json::json!({"type":"custom_tool_call_output"}),
                StatusEvent::Working,
            ),
            (
                "event_msg",
                serde_json::json!({"type":"task_complete","last_agent_message":"Done"}),
                StatusEvent::Stopped {
                    response: Some("Done".into()),
                    interrupted: false,
                },
            ),
            (
                "event_msg",
                serde_json::json!({"type":"task_started"}),
                StatusEvent::Working,
            ),
            (
                "event_msg",
                serde_json::json!({"type":"task_complete"}),
                StatusEvent::Stopped {
                    response: None,
                    interrupted: false,
                },
            ),
            (
                "event_msg",
                serde_json::json!({"type":"task_complete","error":{"message":"Model request failed"}}),
                StatusEvent::Error {
                    message: "Model request failed".into(),
                },
            ),
            (
                "event_msg",
                serde_json::json!({"type":"turn_aborted"}),
                StatusEvent::Stopped {
                    response: None,
                    interrupted: true,
                },
            ),
        ];
        for (index, (kind, payload, expected)) in cases.into_iter().enumerate() {
            let at = datetime!(2026-10-01 05:00:00 UTC)
                + time::Duration::seconds(i64::try_from(index).unwrap());
            lines.push(serde_json::json!({"timestamp":at.format(&time::format_description::well_known::Rfc3339).unwrap(),"type":kind,"payload":payload}).to_string());
            let path = write_rollout(dir.path(), "lifecycle", &lines);
            assert_eq!(
                latest_status(&path),
                Some(Status {
                    at,
                    event: expected
                })
            );
        }
        let path = write_rollout(dir.path(), "lifecycle", &lines);
        let before = latest_status(&path);
        writeln!(
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap(),
            "{{broken tail"
        )
        .unwrap();
        assert_eq!(latest_status(&path), before);
    }

    #[test]
    fn locate_resumed_uuid_session_outside_recent_date_window() {
        let root = tempdir().unwrap();
        let sid = "01a0ddac-d547-7ef1-a41a-f6b5fa5c2ded";
        let day = root.path().join("2026/09/26");
        let path = write_rollout(&day, sid, &[]);
        assert_eq!(
            locate_rollout(root.path(), sid, datetime!(2026-10-01 05:00:00 UTC), 1),
            Some(path)
        );
    }

    #[test]
    fn final_response_ignores_commentary_and_previous_turns() {
        let dir = tempdir().unwrap();
        let final_line = serde_json::json!({"type":"response_item", "payload":{
            "type":"message", "role":"assistant", "phase":"final_answer",
            "content":[{"type":"output_text","text":"Reviews complete"}]
        }})
        .to_string();
        let commentary = serde_json::json!({"type":"response_item", "payload":{
            "type":"message", "role":"assistant", "phase":"commentary",
            "content":[{"type":"output_text","text":"Investigating"}]
        }})
        .to_string();
        let path = write_rollout(
            dir.path(),
            "s",
            &[
                "invalid fragment".into(),
                final_line.clone(),
                commentary.clone(),
            ],
        );
        assert_eq!(
            latest_final_response(&path).as_deref(),
            Some("Reviews complete")
        );
        for boundary in [
            serde_json::json!({"type":"event_msg", "payload":{"type":"task_started"}}),
            serde_json::json!({"type":"event_msg", "payload":{"type":"turn_aborted"}}),
            serde_json::json!({"type":"event_msg", "payload":{"type":"task_complete","error":{"message":"Capacity"}}}),
            serde_json::json!({"type":"event_msg", "payload":{"type":"user_message"}}),
            serde_json::json!({"type":"response_item", "payload":{"type":"message", "role":"user"}}),
        ] {
            let path = write_rollout(
                dir.path(),
                "s",
                &[final_line.clone(), boundary.to_string(), commentary.clone()],
            );
            assert_eq!(latest_final_response(&path), None);
        }
        assert_eq!(latest_final_response(&dir.path().join("missing")), None);
    }

    /// A real-shape `token_count` rollout line with the given window
    /// percentages and `rate_limit_reached_type`.
    fn rate_limit_line(primary_pct: f32, secondary_pct: f32, reached: &str) -> String {
        format!(
            r#"{{"timestamp":"2026-06-12T06:26:08.491Z","type":"event_msg","payload":{{"type":"token_count","info":{{}},"rate_limits":{{"limit_id":"codex","limit_name":null,"primary":{{"used_percent":{primary_pct},"window_minutes":300,"resets_at":1781262859}},"secondary":{{"used_percent":{secondary_pct},"window_minutes":10080,"resets_at":1781745469}},"credits":null,"individual_limit":null,"plan_type":"pro","rate_limit_reached_type":{reached}}}}}}}"#
        )
    }

    fn write_rollout(dir: &Path, session_id: &str, lines: &[String]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("rollout-2026-06-12T15-24-44-{session_id}.jsonl"));
        let mut f = File::create(&path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
        path
    }

    #[test]
    fn parses_windows_and_no_reached() {
        let line = rate_limit_line(5.0, 46.0, "null");
        let rl = parse_line(&line).expect("parsed");
        assert!((rl.five_hour.unwrap().used_percent - 5.0).abs() < f32::EPSILON);
        assert!((rl.seven_day.unwrap().used_percent - 46.0).abs() < f32::EPSILON);
        assert_eq!(
            rl.five_hour.unwrap().resets_at.unwrap().unix_timestamp(),
            1_781_262_859
        );
        assert!(rl.reached.is_none());
    }

    #[test]
    fn maps_reached_type_to_scope() {
        let primary = parse_line(&rate_limit_line(100.0, 46.0, r#""primary""#)).unwrap();
        assert_eq!(primary.reached, Some(RateLimitScope::FiveHour));
        let secondary = parse_line(&rate_limit_line(20.0, 100.0, r#""secondary""#)).unwrap();
        assert_eq!(secondary.reached, Some(RateLimitScope::SevenDay));
        let weird = parse_line(&rate_limit_line(20.0, 30.0, r#""something_new""#)).unwrap();
        assert_eq!(weird.reached, Some(RateLimitScope::Unknown));
    }

    #[test]
    fn window_saturation_does_not_infer_a_blocked_turn() {
        // 100% is telemetry; only an explicit blocked signal changes state.
        let five = parse_line(&rate_limit_line(100.0, 46.0, "null")).unwrap();
        assert_eq!(five.reached, None);
        let seven = parse_line(&rate_limit_line(80.0, 100.0, "null")).unwrap();
        assert_eq!(seven.reached, None);
        // 99% is not yet capped.
        let under = parse_line(&rate_limit_line(99.0, 99.0, "null")).unwrap();
        assert!(under.reached.is_none());
    }

    #[test]
    fn weekly_primary_uses_duration_without_inferring_a_block() {
        // Codex 0.159.2: a weekly primary window with spendable credits.
        let mut record = serde_json::json!({"type":"event_msg", "payload":{
            "type":"token_count", "rate_limits":{
                "primary":{"used_percent":100.0,"window_minutes":10080,"resets_at":1_791_269_103},
                "secondary":null,
                "credits":{"has_credits":true,"unlimited":false,"balance":"60000"},
                "rate_limit_reached_type":null
            }
        }});
        let reading = parse_line(&record.to_string()).unwrap();
        assert!(reading.five_hour.is_none());
        assert!((reading.seven_day.unwrap().used_percent - 100.0).abs() < f32::EPSILON);
        assert_eq!(reading.reached, None);

        let limits = &mut record["payload"]["rate_limits"];
        limits["credits"]["has_credits"] = false.into();
        assert_eq!(parse_line(&record.to_string()).unwrap().reached, None);
        record["payload"]["rate_limits"]["credits"]["unlimited"] = true.into();
        assert_eq!(parse_line(&record.to_string()).unwrap().reached, None);
        record["payload"]["rate_limits"]["rate_limit_reached_type"] = "primary".into();
        assert_eq!(
            parse_line(&record.to_string()).unwrap().reached,
            Some(RateLimitScope::SevenDay),
            "only explicit blocks change lifecycle state"
        );
    }

    #[test]
    fn window_duration_overrides_position_without_guessing_unknown_scopes() {
        for (minutes, expected) in [
            (300, RateLimitScope::FiveHour),
            (10080, RateLimitScope::SevenDay),
            (60, RateLimitScope::Unknown),
        ] {
            let record = serde_json::json!({"payload":{"rate_limits":{
                "primary":null,
                "secondary":{"used_percent":100.0,"window_minutes":minutes},
                "rate_limit_reached_type":"secondary"
            }}});
            assert_eq!(
                parse_line(&record.to_string()).unwrap().reached,
                Some(expected)
            );
        }
        let legacy =
            parse_line(r#"{"payload":{"rate_limits":{"primary":{"used_percent":100.0}}}}"#)
                .unwrap();
        assert!((legacy.five_hour.unwrap().used_percent - 100.0).abs() < f32::EPSILON);
        assert_eq!(legacy.reached, None);
    }

    /// Credit telemetry is independent of the active plan/model quota.
    fn credit_line(has_credits: bool, unlimited: bool) -> String {
        format!(
            r#"{{"timestamp":"2026-06-12T06:26:08.491Z","type":"event_msg","payload":{{"type":"token_count","info":{{}},"rate_limits":{{"limit_id":"premium","limit_name":null,"primary":null,"secondary":null,"credits":{{"has_credits":{has_credits},"unlimited":{unlimited},"balance":"0"}},"individual_limit":null,"plan_type":"pro","rate_limit_reached_type":null}}}}}}"#
        )
    }

    #[test]
    fn credit_exhaustion_does_not_infer_a_blocked_turn() {
        let exhausted = parse_line(&credit_line(false, false)).unwrap();
        assert!(exhausted.five_hour.is_none() && exhausted.seven_day.is_none());
        assert_eq!(exhausted.reached, None);

        // Has credits, or unlimited → not capped.
        assert!(parse_line(&credit_line(true, false))
            .unwrap()
            .reached
            .is_none());
        assert!(parse_line(&credit_line(false, true))
            .unwrap()
            .reached
            .is_none());
    }

    #[test]
    fn has_credits_false_with_live_window_is_not_capped() {
        // Credit availability cannot establish whether plan quota is blocked.
        let line = r#"{"type":"event_msg","payload":{"type":"token_count","rate_limits":{"primary":{"used_percent":40.0,"window_minutes":300,"resets_at":1781262859},"secondary":{"used_percent":50.0,"window_minutes":10080,"resets_at":1781745469},"credits":{"has_credits":false,"unlimited":false,"balance":"0"},"rate_limit_reached_type":null}}}"#;
        let rl = parse_line(line).unwrap();
        assert!(rl.reached.is_none());
    }

    #[test]
    fn non_rate_limit_lines_are_skipped() {
        // session_meta and task_started records have no payload.rate_limits.
        assert!(parse_line(r#"{"type":"session_meta","payload":{"id":"x"}}"#).is_none());
        assert!(parse_line(
            r#"{"type":"event_msg","payload":{"type":"task_started","turn_id":"t"}}"#
        )
        .is_none());
        assert!(parse_line("not json").is_none());
    }

    #[test]
    fn latest_rate_limits_picks_last_record() {
        let dir = tempdir().unwrap();
        let path = write_rollout(
            dir.path(),
            "sess",
            &[
                rate_limit_line(5.0, 46.0, "null"),
                rate_limit_line(6.0, 46.0, "null"),
                rate_limit_line(7.0, 47.0, "null"),
            ],
        );
        let rl = latest_rate_limits(&path).unwrap();
        assert!((rl.five_hour.unwrap().used_percent - 7.0).abs() < f32::EPSILON);
        assert!((rl.seven_day.unwrap().used_percent - 47.0).abs() < f32::EPSILON);
    }

    #[test]
    fn latest_rate_limits_missing_file_is_none() {
        assert!(latest_rate_limits(Path::new("/tmp/no-such-rollout-zzz.jsonl")).is_none());
    }

    /// Build a `sessions_root/YYYY/MM/DD` tree and confirm locate matches on
    /// the session-id suffix.
    fn dated_root(now: OffsetDateTime) -> (TempDir, PathBuf) {
        let root = tempdir().unwrap();
        let day = root
            .path()
            .join(format!("{:04}", now.year()))
            .join(format!("{:02}", u8::from(now.month())))
            .join(format!("{:02}", now.day()));
        (root, day)
    }

    #[test]
    fn locate_finds_rollout_for_today() {
        let now = datetime!(2026-06-12 15:30:00 UTC);
        let (root, day) = dated_root(now);
        write_rollout(&day, "019eba81-uuid", &[rate_limit_line(5.0, 46.0, "null")]);
        // A decoy from another session must not match.
        write_rollout(&day, "other-uuid", &[rate_limit_line(9.0, 9.0, "null")]);

        let found = locate_rollout(root.path(), "019eba81-uuid", now, 1).expect("located");
        assert!(found
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("019eba81-uuid.jsonl"));
    }

    #[test]
    fn locate_searches_back_to_yesterday() {
        let now = datetime!(2026-06-12 15:30:00 UTC);
        let yesterday = now.date().previous_day().unwrap();
        let root = tempdir().unwrap();
        let day = root
            .path()
            .join(format!("{:04}", yesterday.year()))
            .join(format!("{:02}", u8::from(yesterday.month())))
            .join(format!("{:02}", yesterday.day()));
        write_rollout(&day, "yday-uuid", &[rate_limit_line(5.0, 46.0, "null")]);

        // lookback of 0 (today only) misses it; lookback of 1 finds it.
        assert!(locate_rollout(root.path(), "yday-uuid", now, 0).is_none());
        assert!(locate_rollout(root.path(), "yday-uuid", now, 1).is_some());
    }

    /// East-of-UTC timezones (e.g. KST, UTC+9) can have a local rollout date
    /// one day ahead of the UTC date the poll computes. The scan must look
    /// one day forward to find such a file even with `lookback_days = 0`.
    #[test]
    fn locate_finds_rollout_dated_one_day_ahead_of_utc() {
        let now = datetime!(2026-06-12 23:30:00 UTC);
        let tomorrow = now.date().next_day().unwrap();
        let root = tempdir().unwrap();
        let day = root
            .path()
            .join(format!("{:04}", tomorrow.year()))
            .join(format!("{:02}", u8::from(tomorrow.month())))
            .join(format!("{:02}", tomorrow.day()));
        write_rollout(&day, "ahead-uuid", &[rate_limit_line(5.0, 46.0, "null")]);

        assert!(locate_rollout(root.path(), "ahead-uuid", now, 0).is_some());
    }

    #[test]
    fn session_rate_limits_end_to_end() {
        let now = datetime!(2026-06-12 15:30:00 UTC);
        let (root, day) = dated_root(now);
        write_rollout(
            &day,
            "live-uuid",
            &[
                rate_limit_line(50.0, 46.0, "null"),
                rate_limit_line(100.0, 46.0, r#""primary""#),
            ],
        );
        let rl = session_rate_limits(root.path(), "live-uuid", now, 1).unwrap();
        assert!((rl.five_hour.unwrap().used_percent - 100.0).abs() < f32::EPSILON);
        assert_eq!(rl.reached, Some(RateLimitScope::FiveHour));
    }

    #[test]
    fn session_rate_limits_unknown_session_is_none() {
        let now = datetime!(2026-06-12 15:30:00 UTC);
        let (root, _day) = dated_root(now);
        assert!(session_rate_limits(root.path(), "ghost", now, 1).is_none());
    }
}
