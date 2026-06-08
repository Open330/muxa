//! Cumulative tmux session foreground-time tracking.
//!
//! The signal this module tracks is intentionally tmux-native:
//! interactive `tmux list-clients` rows grouped by their `client_session`.
//! That maps to "a human has this session in a foreground tmux client",
//! ignores control-mode automation clients, and survives panes/windows
//! coming and going inside the same session.

use crate::activity::{
    ActivityEntry, ActivityLog, HumanInteractionEntry, HumanInteractionInput, HumanInteractionKind,
    SessionForegroundEntry,
};
use crate::tmux::{self, SessionInfo, TmuxError};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;
use tracing::{debug, warn};

#[cfg(unix)]
const SESSION_ACTIVITY_FILE_MODE: u32 = 0o600;

pub const SESSION_ACTIVITY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionActivity {
    pub session_id: String,
    pub name: String,
    pub attached_clients: u32,
    pub total_attached_secs: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub attached_since: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen_at: OffsetDateTime,
    /// Most recent tmux `client_activity` (last human keypress/scroll) observed
    /// for this session, used to detect *new* input between polls. Persisted so
    /// a daemon restart does not re-emit a stale input tick.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub last_input_at: Option<OffsetDateTime>,
}

impl SessionActivity {
    pub fn is_attached(&self) -> bool {
        self.attached_since.is_some()
    }

    pub fn effective_total_secs(&self, now: OffsetDateTime) -> u64 {
        let active_delta = self.attached_since.map_or(0, |since| {
            u64::try_from((now - since).whole_seconds().max(0)).unwrap_or(u64::MAX)
        });
        self.total_attached_secs.saturating_add(active_delta)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionActivityFile {
    #[serde(default)]
    v: u32,
    #[serde(with = "time::serde::rfc3339")]
    saved_at: OffsetDateTime,
    sessions: Vec<SessionActivity>,
}

pub async fn load(path: &Path) -> Vec<SessionActivity> {
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(
                path = %path.display(),
                "no session activity file on disk; starting empty"
            );
            return Vec::new();
        }
        Err(e) => {
            warn!(error = %e, path = %path.display(), "could not read session activity file");
            return Vec::new();
        }
    };

    let parsed: SessionActivityFile = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                error = %e,
                path = %path.display(),
                "session activity file is corrupt; starting empty"
            );
            return Vec::new();
        }
    };

    if parsed.v != SESSION_ACTIVITY_SCHEMA_VERSION {
        warn!(
            file_version = parsed.v,
            expected = SESSION_ACTIVITY_SCHEMA_VERSION,
            path = %path.display(),
            "session activity file has unknown schema version; starting empty",
        );
        return Vec::new();
    }

    parsed.sessions
}

async fn save(path: &Path, sessions: &[SessionActivity]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    if !parent.as_os_str().is_empty() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let basename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("session-activity.json");
    let tmp = parent.join(format!(".{}.{}.tmp", basename, std::process::id()));
    let payload = SessionActivityFile {
        v: SESSION_ACTIVITY_SCHEMA_VERSION,
        saved_at: OffsetDateTime::now_utc(),
        sessions: sessions.to_vec(),
    };
    let bytes = serde_json::to_vec(&payload)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    {
        let mut opts = OpenOptions::new();
        opts.create(true).truncate(true).write(true);
        #[cfg(unix)]
        opts.mode(SESSION_ACTIVITY_FILE_MODE);
        let mut f = opts.open(&tmp).await?;
        f.write_all(&bytes).await?;
        f.flush().await?;
        f.sync_all().await?;
    }
    tokio::fs::rename(&tmp, path).await?;
    if !parent.as_os_str().is_empty() {
        if let Ok(dir) = OpenOptions::new().read(true).open(parent).await {
            let _ = dir.sync_all().await;
        }
    }
    Ok(())
}

pub fn apply_sample<S: BuildHasher>(
    records: &mut HashMap<String, SessionActivity, S>,
    sessions: &[SessionInfo],
    now: OffsetDateTime,
) -> bool {
    apply_sample_report(records, sessions, now).changed
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplySampleReport {
    pub changed: bool,
    pub intervals: Vec<SessionForegroundEntry>,
}

pub fn apply_sample_report<S: BuildHasher>(
    records: &mut HashMap<String, SessionActivity, S>,
    sessions: &[SessionInfo],
    now: OffsetDateTime,
) -> ApplySampleReport {
    let mut changed = false;
    let mut intervals = Vec::new();
    let live_ids: HashSet<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();

    for session in sessions {
        let is_attached = session.attached_clients > 0;
        let record = records
            .entry(session.session_id.clone())
            .or_insert_with(|| SessionActivity {
                session_id: session.session_id.clone(),
                name: session.name.clone(),
                attached_clients: 0,
                total_attached_secs: 0,
                attached_since: None,
                last_seen_at: now,
                last_input_at: None,
            });

        if record.name != session.name {
            record.name.clone_from(&session.name);
            changed = true;
        }
        if record.attached_clients != session.attached_clients {
            record.attached_clients = session.attached_clients;
            changed = true;
        }
        record.last_seen_at = now;

        match (record.attached_since, is_attached) {
            (None, true) => {
                record.attached_since = Some(now);
                changed = true;
            }
            (Some(since), false) => {
                intervals.push(SessionForegroundEntry::new(
                    record.session_id.clone(),
                    record.name.clone(),
                    since,
                    now,
                ));
                add_elapsed(record, since, now);
                record.attached_since = None;
                changed = true;
            }
            (Some(_), true) | (None, false) => {}
        }
    }

    for record in records.values_mut() {
        if live_ids.contains(record.session_id.as_str()) {
            continue;
        }
        if let Some(since) = record.attached_since {
            intervals.push(SessionForegroundEntry::new(
                record.session_id.clone(),
                record.name.clone(),
                since,
                now,
            ));
            add_elapsed(record, since, now);
            record.attached_since = None;
            record.attached_clients = 0;
            record.last_seen_at = now;
            changed = true;
        }
    }

    ApplySampleReport { changed, intervals }
}

fn add_elapsed(record: &mut SessionActivity, since: OffsetDateTime, now: OffsetDateTime) {
    let secs = u64::try_from((now - since).whole_seconds().max(0)).unwrap_or(u64::MAX);
    record.total_attached_secs = record.total_attached_secs.saturating_add(secs);
}

pub struct SessionActivityTracker {
    path: PathBuf,
    interval: Duration,
    activity_log: Option<Arc<ActivityLog>>,
}

impl SessionActivityTracker {
    pub fn new(path: PathBuf, interval: Duration) -> Self {
        Self {
            path,
            interval,
            activity_log: None,
        }
    }

    #[must_use]
    pub fn with_activity_log(mut self, activity_log: Option<Arc<ActivityLog>>) -> Self {
        self.activity_log = activity_log;
        self
    }

    pub async fn run(self, mut shutdown: broadcast::Receiver<()>) {
        let loaded = load(&self.path).await;
        let mut records: HashMap<String, SessionActivity> = loaded
            .into_iter()
            .map(|r| (r.session_id.clone(), r))
            .collect();

        let mut tick = tokio::time::interval(self.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    self.poll_once(&mut records).await;
                }
                _ = shutdown.recv() => {
                    let mut sessions = records.values().cloned().collect::<Vec<_>>();
                    sessions.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.session_id.cmp(&b.session_id)));
                    if let Err(e) = save(&self.path, &sessions).await {
                        warn!(error = %e, path = %self.path.display(), "could not save session activity on shutdown");
                    }
                    debug!("session activity tracker shutting down");
                    break;
                }
            }
        }
    }

    async fn poll_once(&self, records: &mut HashMap<String, SessionActivity>) {
        let sample = tokio::task::spawn_blocking(sample_activity)
            .await
            .unwrap_or_else(|e| Err(format!("join error: {e}")));
        let sample = match sample {
            Ok(sample) => sample,
            Err(e) => {
                debug!(error = %e, "session activity poll skipped");
                return;
            }
        };

        let now = OffsetDateTime::now_utc();
        // Capture which sessions were attached *before* this poll mutates state —
        // input detection needs it to tell a real keypress from a fresh attach.
        let was_attached: HashSet<String> = records
            .iter()
            .filter(|(_, record)| record.attached_since.is_some())
            .map(|(id, _)| id.clone())
            .collect();
        let report = apply_sample_report(records, &sample.sessions, now);
        for interval in report.intervals {
            if let Some(activity_log) = &self.activity_log {
                activity_log.append(ActivityEntry::SessionForeground(interval));
            }
        }

        // Reading detection: when a session's tmux `client_activity` advances
        // while it was already attached, the human pressed a key or scrolled —
        // record a short input tick so `active` can credit reading, not just
        // typing. Idle attaches never advance, and a fresh attach only re-seeds.
        let (inputs, input_changed) =
            collect_input_interactions(records, &sample.last_input_by_id, &was_attached);
        for entry in inputs {
            if let Some(activity_log) = &self.activity_log {
                activity_log.append(ActivityEntry::HumanInteraction(entry));
            }
        }

        if report.changed || input_changed {
            let mut sessions = records.values().cloned().collect::<Vec<_>>();
            sessions.sort_by(|a, b| {
                a.name
                    .cmp(&b.name)
                    .then_with(|| a.session_id.cmp(&b.session_id))
            });
            if let Err(e) = save(&self.path, &sessions).await {
                warn!(
                    error = %e,
                    path = %self.path.display(),
                    "could not save session activity"
                );
            }
        }
    }
}

/// One tmux poll: live sessions (with attached-client counts) plus, per session
/// id, the most recent non-control `client_activity` epoch (last human input).
struct ActivitySample {
    sessions: Vec<SessionInfo>,
    last_input_by_id: HashMap<String, i64>,
}

/// Width of the synthetic interval emitted for a detected input tick. The tick
/// itself is a point in time; `active` pads it with its own window, so a hair of
/// width here just keeps the interval non-empty.
const INPUT_TICK_SECS: i64 = 1;

fn sample_activity() -> Result<ActivitySample, String> {
    let empty = || ActivitySample {
        sessions: Vec::new(),
        last_input_by_id: HashMap::new(),
    };
    let mut sessions = match tmux::list_sessions() {
        Ok(sessions) => sessions,
        Err(TmuxError::NonZero(msg)) if msg.trim_start().starts_with("no server running") => {
            return Ok(empty());
        }
        Err(e) => return Err(e.to_string()),
    };
    let clients = match tmux::list_clients() {
        Ok(clients) => clients,
        Err(TmuxError::NonZero(msg)) if msg.trim_start().starts_with("no server running") => {
            return Ok(empty());
        }
        Err(e) => return Err(e.to_string()),
    };
    apply_client_counts(&mut sessions, &clients);
    let last_input_by_id = input_epochs_by_session_id(&sessions, &clients);
    Ok(ActivitySample {
        sessions,
        last_input_by_id,
    })
}

/// Fold each session's most recent interactive `client_activity` epoch and key
/// it by the stable session id. tmux reports activity per *client* (keyed by
/// session name), so we take the max across a session's clients and remap to id.
/// Control-mode (automation) clients and unreported activity (`<= 0`) are
/// ignored. Pure (no tmux call) so it can be unit-tested directly.
fn input_epochs_by_session_id(
    sessions: &[SessionInfo],
    clients: &[tmux::ClientInfo],
) -> HashMap<String, i64> {
    let mut by_name: HashMap<&str, i64> = HashMap::new();
    for client in clients {
        if client.control_mode || client.last_activity <= 0 {
            continue;
        }
        let slot = by_name.entry(client.session.as_str()).or_default();
        *slot = (*slot).max(client.last_activity);
    }
    let mut by_id = HashMap::new();
    for session in sessions {
        if let Some(&epoch) = by_name.get(session.name.as_str()) {
            by_id.insert(session.session_id.clone(), epoch);
        }
    }
    by_id
}

/// Compare each session's freshly sampled `client_activity` against the last one
/// we recorded; a strictly newer value means human input arrived since the last
/// poll, so emit a `TmuxInput` tick.
///
/// `was_attached` holds the session ids that were already attached *before* this
/// poll. We only emit when the session is in that set, because tmux initializes
/// `#{client_activity}` to the *attach* time of a new client — so a fresh attach
/// (a reattach after detaching, or an extra client) advances the epoch with no
/// keypress. For those we just re-seed the baseline silently and start counting
/// real input from the next poll. The very first observation (`None`) likewise
/// only seeds. Backwards epochs (server restart / clock skew) fall through the
/// `Some(_)` arm and are ignored until the high-water mark is exceeded again.
///
/// Returns the ticks to append and whether any record changed (to drive persist).
fn collect_input_interactions<S: BuildHasher>(
    records: &mut HashMap<String, SessionActivity, S>,
    last_input_by_id: &HashMap<String, i64>,
    was_attached: &HashSet<String>,
) -> (Vec<HumanInteractionEntry>, bool) {
    let mut entries = Vec::new();
    let mut changed = false;
    for (session_id, &epoch) in last_input_by_id {
        let Ok(at) = OffsetDateTime::from_unix_timestamp(epoch) else {
            continue;
        };
        let Some(record) = records.get_mut(session_id) else {
            continue;
        };
        match record.last_input_at {
            Some(prev) if at > prev => {
                if was_attached.contains(session_id) {
                    entries.push(HumanInteractionEntry::new(HumanInteractionInput {
                        kind: HumanInteractionKind::TmuxInput,
                        pane: None,
                        session_id: Some(record.session_id.clone()),
                        session_name: Some(record.name.clone()),
                        started_at: at - time::Duration::seconds(INPUT_TICK_SECS),
                        ended_at: at,
                    }));
                }
                // Advance the baseline whether or not we emitted, so a reattach's
                // attach-time bump is absorbed rather than counted next poll.
                record.last_input_at = Some(at);
                changed = true;
            }
            None => {
                record.last_input_at = Some(at);
                changed = true;
            }
            Some(_) => {}
        }
    }
    (entries, changed)
}

fn apply_client_counts(sessions: &mut [SessionInfo], clients: &[tmux::ClientInfo]) {
    let mut counts: HashMap<&str, u32> = HashMap::new();
    for client in clients {
        if client.control_mode {
            continue;
        }
        *counts.entry(client.session.as_str()).or_default() += 1;
    }
    for session in sessions {
        session.attached_clients = counts.get(session.name.as_str()).copied().unwrap_or(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn session(id: &str, name: &str, attached_clients: u32) -> SessionInfo {
        SessionInfo {
            session_id: id.into(),
            name: name.into(),
            attached_clients,
        }
    }

    #[test]
    fn apply_sample_accumulates_attached_edges() {
        let mut records = HashMap::new();
        let t0 = datetime!(2026-05-29 00:00:00 UTC);
        let t1 = datetime!(2026-05-29 00:00:10 UTC);
        let t2 = datetime!(2026-05-29 00:00:15 UTC);

        assert!(apply_sample(&mut records, &[session("$1", "main", 1)], t0));
        let record = records.get("$1").unwrap();
        assert_eq!(record.attached_since, Some(t0));
        assert_eq!(record.effective_total_secs(t1), 10);

        assert!(apply_sample(&mut records, &[session("$1", "main", 0)], t2));
        let record = records.get("$1").unwrap();
        assert_eq!(record.attached_since, None);
        assert_eq!(record.total_attached_secs, 15);
    }

    #[test]
    fn apply_sample_report_emits_detach_interval() {
        let mut records = HashMap::new();
        let t0 = datetime!(2026-05-29 00:00:00 UTC);
        let t1 = datetime!(2026-05-29 00:00:15 UTC);

        apply_sample_report(&mut records, &[session("$1", "main", 1)], t0);
        let report = apply_sample_report(&mut records, &[session("$1", "main", 0)], t1);

        assert_eq!(report.intervals.len(), 1);
        assert_eq!(report.intervals[0].session_id, "$1");
        assert_eq!(report.intervals[0].session_name, "main");
        assert_eq!(report.intervals[0].duration_secs, 15);
    }

    #[test]
    fn disappearing_attached_session_closes_interval() {
        let mut records = HashMap::new();
        let t0 = datetime!(2026-05-29 00:00:00 UTC);
        let t1 = datetime!(2026-05-29 00:00:20 UTC);

        apply_sample_report(&mut records, &[session("$1", "main", 1)], t0);
        let report = apply_sample_report(&mut records, &[], t1);

        assert_eq!(report.intervals.len(), 1);
        assert_eq!(report.intervals[0].session_id, "$1");
        assert_eq!(report.intervals[0].duration_secs, 20);
        let record = records.get("$1").unwrap();
        assert_eq!(record.attached_since, None);
        assert_eq!(record.total_attached_secs, 20);
        assert_eq!(record.attached_clients, 0);
    }

    #[test]
    fn client_counts_drive_user_attached_sessions() {
        let mut sessions = vec![session("$1", "main", 99), session("$2", "work", 99)];
        let clients = vec![
            tmux::ClientInfo {
                session: "main".into(),
                control_mode: false,
                last_activity: 0,
            },
            tmux::ClientInfo {
                session: "main".into(),
                control_mode: true,
                last_activity: 0,
            },
        ];

        apply_client_counts(&mut sessions, &clients);

        assert_eq!(sessions[0].attached_clients, 1);
        assert_eq!(sessions[1].attached_clients, 0);
    }

    #[test]
    fn tmux_input_tick_emitted_only_when_activity_advances() {
        let now = datetime!(2026-05-30 12:00:00 UTC);
        let mut records = HashMap::new();
        records.insert(
            "$1".to_string(),
            SessionActivity {
                session_id: "$1".into(),
                name: "main".into(),
                attached_clients: 1,
                total_attached_secs: 0,
                attached_since: Some(now),
                last_seen_at: now,
                last_input_at: None,
            },
        );
        let attached: HashSet<String> = ["$1".to_string()].into_iter().collect();
        let t1 = datetime!(2026-05-30 11:59:00 UTC).unix_timestamp();
        let by_id: HashMap<String, i64> = [("$1".to_string(), t1)].into_iter().collect();

        // First observation seeds the baseline without emitting a tick.
        let (entries, changed) = collect_input_interactions(&mut records, &by_id, &attached);
        assert!(entries.is_empty());
        assert!(changed);

        // Same epoch again: no new input, nothing emitted.
        let (entries, _) = collect_input_interactions(&mut records, &by_id, &attached);
        assert!(entries.is_empty());

        // A newer epoch while already attached means the human typed/scrolled.
        let t2 = datetime!(2026-05-30 11:59:30 UTC).unix_timestamp();
        let by_id2: HashMap<String, i64> = [("$1".to_string(), t2)].into_iter().collect();
        let (entries, changed) = collect_input_interactions(&mut records, &by_id2, &attached);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, HumanInteractionKind::TmuxInput);
        assert_eq!(entries[0].session_id.as_deref(), Some("$1"));
        assert!(changed);
    }

    #[test]
    fn fresh_attach_reseeds_without_emitting_input() {
        // A reattach bumps tmux `client_activity` to the attach time with no
        // keypress. Because the session was NOT attached before this poll, the
        // advance must re-seed the baseline silently — not fabricate input.
        let now = datetime!(2026-05-30 12:00:00 UTC);
        let mut records = HashMap::new();
        records.insert(
            "$1".to_string(),
            SessionActivity {
                session_id: "$1".into(),
                name: "main".into(),
                attached_clients: 1,
                total_attached_secs: 0,
                attached_since: Some(now),
                last_seen_at: now,
                last_input_at: Some(datetime!(2026-05-30 10:00:00 UTC)),
            },
        );
        let reattach = datetime!(2026-05-30 11:59:00 UTC).unix_timestamp();
        let by_id: HashMap<String, i64> = [("$1".to_string(), reattach)].into_iter().collect();
        let not_attached: HashSet<String> = HashSet::new();

        let (entries, changed) = collect_input_interactions(&mut records, &by_id, &not_attached);
        assert!(entries.is_empty(), "reattach must not emit a phantom tick");
        assert!(
            changed,
            "baseline still advances so the next poll measures input"
        );
        assert_eq!(
            records["$1"].last_input_at,
            Some(datetime!(2026-05-30 11:59:00 UTC))
        );
    }

    #[test]
    fn input_epochs_take_max_ignoring_control_clients() {
        let sessions = vec![session("$1", "main", 9), session("$2", "work", 9)];
        let clients = vec![
            tmux::ClientInfo {
                session: "main".into(),
                control_mode: false,
                last_activity: 100,
            },
            tmux::ClientInfo {
                session: "main".into(),
                control_mode: false,
                last_activity: 250,
            },
            tmux::ClientInfo {
                session: "main".into(),
                control_mode: true,
                last_activity: 999,
            },
            tmux::ClientInfo {
                session: "work".into(),
                control_mode: false,
                last_activity: 0,
            },
        ];

        let by_id = input_epochs_by_session_id(&sessions, &clients);

        // Max of the two interactive clients on "main"; the control client and
        // its higher epoch are ignored. "work" has only a 0 epoch, so it's absent.
        assert_eq!(by_id.get("$1"), Some(&250));
        assert_eq!(by_id.get("$2"), None);
    }
}
