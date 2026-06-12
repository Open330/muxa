//! `OpenAI` Codex CLI rollout-file rate-limit parser.
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
//! `primary` is the 5-hour rolling window, `secondary` the 7-day one — the
//! same two scopes Claude Code's statusline exposes, so they map straight
//! onto muxa's `rate_limit_5h_*` / `rate_limit_7d_*` fields. The reconciler
//! polls this via [`session_rate_limits`] and feeds the result through the
//! existing `Heartbeat` (percentages) and `RateLimited` (hard cap) paths.
//!
//! Codex also has a **credit-based plan**: once the rolling windows are
//! spent, `primary`/`secondary` go null and a `credits` object takes over
//! (`{"has_credits":false,"unlimited":false,"balance":"0"}` = out of
//! credits). We treat that as a cap too — see [`RateLimits::reached`].
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
    /// `primary` window — the 5-hour rolling cap.
    pub five_hour: Option<Window>,
    /// `secondary` window — the 7-day weekly cap.
    pub seven_day: Option<Window>,
    /// Set when codex is *blocked right now*, mapped to the scope that
    /// tripped. Three signals feed it (see [`parse_line`]):
    /// 1. `rate_limit_reached_type` is non-null (explicit, but codex rarely
    ///    sets it), 2. a window reads `used_percent >= 100`, or 3. the
    ///    account is on the credit model (`primary`/`secondary` both null)
    ///    and its `credits` are exhausted. `None` when none apply.
    pub reached: Option<RateLimitScope>,
}

// ---------------------------------------------------------------------------
// Wire shapes — only the fields we consume, everything else ignored.

#[derive(Deserialize)]
struct Record {
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
    /// Present (and `primary`/`secondary` null) on the credit-based plan —
    /// codex switches an account here once its rolling windows are spent.
    #[serde(default)]
    credits: Option<RawCredits>,
    /// `null` normally; a window name (`"primary"` / `"secondary"`) when a
    /// cap was reached.
    #[serde(default)]
    rate_limit_reached_type: Option<String>,
}

#[derive(Deserialize)]
struct RawWindow {
    used_percent: Option<f32>,
    /// Unix epoch *seconds*. Optional even when `used_percent` is present.
    resets_at: Option<i64>,
}

#[derive(Deserialize)]
struct RawCredits {
    /// Whether the account currently has credits available. `false` with
    /// `unlimited == false` is codex's "out of credits" signal.
    #[serde(default)]
    has_credits: Option<bool>,
    #[serde(default)]
    unlimited: Option<bool>,
}

/// A window counts as capped at 100% utilization (`used_percent` is 0–100).
const SATURATED_PCT: f32 = 100.0;

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

    let five_hour = window(raw.primary);
    let seven_day = window(raw.secondary);

    // An explicit `rate_limit_reached_type` is the most authoritative signal,
    // but codex sets it rarely. Fall back to window saturation, then — for
    // credit-plan accounts, where the windows are absent — to credit
    // exhaustion. The "both windows null" guard matters: an account on the
    // window plan can carry a `credits` object with `has_credits: false`
    // while still having window quota, and that is NOT a cap.
    let reached = match raw.rate_limit_reached_type.as_deref() {
        Some("primary") => Some(RateLimitScope::FiveHour),
        Some("secondary") => Some(RateLimitScope::SevenDay),
        Some(_) => Some(RateLimitScope::Unknown),
        None if five_hour.is_some_and(|w| w.used_percent >= SATURATED_PCT) => {
            Some(RateLimitScope::FiveHour)
        }
        None if seven_day.is_some_and(|w| w.used_percent >= SATURATED_PCT) => {
            Some(RateLimitScope::SevenDay)
        }
        None if five_hour.is_none()
            && seven_day.is_none()
            && raw.credits.is_some_and(credits_exhausted) =>
        {
            Some(RateLimitScope::Unknown)
        }
        None => None,
    };

    Some(RateLimits {
        five_hour,
        seven_day,
        reached,
    })
}

/// True when a `credits` object says the account is out of credits: it isn't
/// an unlimited plan and `has_credits` is explicitly `false`.
fn credits_exhausted(c: RawCredits) -> bool {
    c.unlimited != Some(true) && c.has_credits == Some(false)
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
    fn window_saturation_marks_reached_without_reached_type() {
        // 100% on the 5h window with rate_limit_reached_type still null —
        // codex rarely sets the explicit field, so saturation must suffice.
        let five = parse_line(&rate_limit_line(100.0, 46.0, "null")).unwrap();
        assert_eq!(five.reached, Some(RateLimitScope::FiveHour));
        let seven = parse_line(&rate_limit_line(80.0, 100.0, "null")).unwrap();
        assert_eq!(seven.reached, Some(RateLimitScope::SevenDay));
        // 99% is not yet capped.
        let under = parse_line(&rate_limit_line(99.0, 99.0, "null")).unwrap();
        assert!(under.reached.is_none());
    }

    /// Credit-plan line: windows null, `credits.has_credits:false` → capped.
    /// This is the real shape seen on a credit-exhausted `pro` account.
    fn credit_line(has_credits: bool, unlimited: bool) -> String {
        format!(
            r#"{{"timestamp":"2026-06-12T06:26:08.491Z","type":"event_msg","payload":{{"type":"token_count","info":{{}},"rate_limits":{{"limit_id":"premium","limit_name":null,"primary":null,"secondary":null,"credits":{{"has_credits":{has_credits},"unlimited":{unlimited},"balance":"0"}},"individual_limit":null,"plan_type":"pro","rate_limit_reached_type":null}}}}}}"#
        )
    }

    #[test]
    fn credit_exhaustion_marks_reached() {
        let exhausted = parse_line(&credit_line(false, false)).unwrap();
        assert!(exhausted.five_hour.is_none() && exhausted.seven_day.is_none());
        assert_eq!(exhausted.reached, Some(RateLimitScope::Unknown));

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
        // The window-plan account can carry credits.has_credits:false while a
        // window still has quota (real: ~849 such records on disk). The
        // "both windows null" guard must keep this from reading as a cap.
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
