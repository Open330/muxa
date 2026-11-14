//! A small bounded spool for hook events the daemon did not take.
//!
//! Hook ingest is best-effort on the agent's critical path: the hook never
//! blocks or fails the agent. Before this spool a failed ingest was logged at
//! debug level and lost, so a daemon restart, a wedged daemon, or a
//! CLI/daemon protocol mismatch during a staggered upgrade silently dropped
//! state transitions. Now the hook appends the event here, `tendd` replays
//! what is still meaningful at startup, and `tend doctor` reports both the
//! pending spool and the last drain.
//!
//! Bounds: the live file is rotated to `<file>.1` at [`MAX_SPOOL_BYTES`],
//! keeping one previous generation, so the spool never exceeds about twice
//! that. Heartbeats are not spooled: a stale liveness ping has nothing to
//! replay. Neither are headless Ask runs, whose ingest carries an Ask
//! identity the spool does not reproduce.
//!
//! Every append, rotation and drain holds an exclusive advisory lock on
//! `<file>.lock`, so concurrent hooks never lose a line to a rotation and a
//! drain never unlinks a file a hook is still writing.
//!
//! Replay is deterministic and conservative: an event is replayed only when
//! it is younger than [`MAX_REPLAY_AGE`] and the daemon holds nothing newer
//! for that session. `tendd` replays into its registry before it accepts any
//! connection, so no live hook can interleave with the replay. Everything
//! else is counted as skipped.

use crate::event::AgentEvent;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

pub const SPOOL_FILENAME: &str = "hook-spool.ndjson";
pub const DRAIN_REPORT_FILENAME: &str = "hook-spool-drain.json";
pub const SPOOL_SCHEMA_VERSION: u32 = 1;
/// Live spool size at which it rotates to `<file>.1`.
pub const MAX_SPOOL_BYTES: u64 = 512 * 1024;
/// Older events are not replayed: the agent has moved on since.
pub const MAX_REPLAY_AGE: time::Duration = time::Duration::minutes(15);

/// Why an ingest did not land.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestFailure {
    /// No daemon listening on the socket.
    Unreachable,
    /// The daemon did not answer in time.
    Timeout,
    /// The connection closed before an answer (a restart).
    Disconnected,
    /// The daemon speaks another protocol version: a CLI/daemon skew.
    ProtocolMismatch,
    /// The daemon answered and refused the event.
    Refused,
    Other,
}

impl IngestFailure {
    #[must_use]
    pub fn classify(error: &crate::ipc::RuntimeError) -> Self {
        use crate::ipc::RuntimeError;
        match error {
            RuntimeError::NotConnected(_) => Self::Unreachable,
            RuntimeError::Timeout(_) => Self::Timeout,
            RuntimeError::Disconnected => Self::Disconnected,
            RuntimeError::Daemon(message)
                if message.starts_with("protocol mismatch")
                    || message.starts_with("unsupported protocol") =>
            {
                Self::ProtocolMismatch
            }
            RuntimeError::Daemon(_) => Self::Refused,
            _ => Self::Other,
        }
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Unreachable => "daemon unreachable",
            Self::Timeout => "timed out",
            Self::Disconnected => "connection closed",
            Self::ProtocolMismatch => "protocol mismatch",
            Self::Refused => "refused",
            Self::Other => "other error",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpoolEntry {
    pub v: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub spooled_at: OffsetDateTime,
    pub failure: IngestFailure,
    pub error: String,
    /// The writing CLI's IPC protocol and version, for skew diagnosis.
    pub protocol: u32,
    pub cli_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_id: Option<String>,
    pub event: AgentEvent,
}

impl SpoolEntry {
    #[must_use]
    pub fn new(
        event: &AgentEvent,
        launch_id: Option<String>,
        error: &crate::ipc::RuntimeError,
    ) -> Self {
        Self {
            v: SPOOL_SCHEMA_VERSION,
            spooled_at: OffsetDateTime::now_utc(),
            failure: IngestFailure::classify(error),
            error: error.to_string(),
            protocol: crate::event::PROTOCOL_VERSION,
            cli_version: env!("CARGO_PKG_VERSION").to_string(),
            launch_id,
            event: event.clone(),
        }
    }
}

/// `$XDG_DATA_HOME/tend/hook-spool.ndjson`.
#[must_use]
pub fn default_spool_file() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join(crate::paths::CONFIG_DIRNAME).join(SPOOL_FILENAME))
}

/// `$XDG_DATA_HOME/tend/hook-spool-drain.json`.
#[must_use]
pub fn default_drain_report_file() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| {
        d.join(crate::paths::CONFIG_DIRNAME)
            .join(DRAIN_REPORT_FILENAME)
    })
}

/// The previous generation of a rotated spool: `<file>.1`.
#[must_use]
pub fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".1");
    PathBuf::from(name)
}

/// Whether an event is worth spooling at all. `headless_ask` is whether
/// the hook runs inside a headless Ask (`TEND_ASK` set).
#[must_use]
pub fn spoolable(event: &AgentEvent, headless_ask: bool) -> bool {
    !headless_ask && !matches!(event, AgentEvent::Heartbeat { .. })
}

fn lock_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".lock");
    PathBuf::from(name)
}

/// How long a hook waits for the spool lock before giving the event up. The
/// hook runs on the agent's critical path; a drain or another hook holds the
/// lock for milliseconds, so contention past this means something is stuck.
pub const APPEND_LOCK_BUDGET: std::time::Duration = std::time::Duration::from_millis(250);

/// Hold the spool's exclusive lock until the returned file is dropped,
/// waiting at most `budget` (`None`: as long as it takes).
fn lock(path: &Path, budget: Option<std::time::Duration>) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(lock_path(path))?;
    let Some(budget) = budget else {
        file.lock()?;
        return Ok(file);
    };
    let deadline = std::time::Instant::now() + budget;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "hook spool is locked",
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
    }
}

/// Append one entry. Best-effort by contract: the caller is a hook on the
/// agent's critical path, so every failure is swallowed into the return
/// value and never surfaced to the agent.
/// Waits at most [`APPEND_LOCK_BUDGET`] for the lock.
pub fn append(path: &Path, entry: &SpoolEntry) -> std::io::Result<()> {
    let _lock = lock(path, Some(APPEND_LOCK_BUDGET))?;
    if std::fs::metadata(path).is_ok_and(|meta| meta.len() >= MAX_SPOOL_BYTES) {
        let previous = rotated_path(path);
        #[cfg(windows)]
        let _ = std::fs::remove_file(&previous);
        std::fs::rename(path, &previous)?;
    }
    let mut line = serde_json::to_vec(entry).map_err(std::io::Error::other)?;
    line.push(b'\n');
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(&line)
}

/// Every readable entry, oldest generation first. Unreadable lines (a torn
/// write, a newer schema) are counted, not fatal.
#[must_use]
pub fn read_all(path: &Path) -> (Vec<SpoolEntry>, usize) {
    let mut entries = Vec::new();
    let mut unreadable = 0;
    for file in [rotated_path(path), path.to_path_buf()] {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            match serde_json::from_str::<SpoolEntry>(line) {
                Ok(entry) if entry.v == SPOOL_SCHEMA_VERSION => entries.push(entry),
                _ => unreadable += 1,
            }
        }
    }
    (entries, unreadable)
}

/// Read and remove both generations under the spool lock, so no hook can
/// append in between and lose its line.
#[must_use]
pub fn take(path: &Path) -> (Vec<SpoolEntry>, usize) {
    let Ok(_lock) = lock(path, None) else {
        // Without the lock a hook could write into a file being removed;
        // leave the spool for the next start.
        return (Vec::new(), 0);
    };
    let taken = read_all(path);
    clear(path);
    taken
}

/// Remove both generations after a drain.
pub fn clear(path: &Path) {
    for file in [rotated_path(path), path.to_path_buf()] {
        let _ = std::fs::remove_file(file);
    }
}

/// What a daemon-start drain did, kept for `tend doctor`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrainReport {
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub drained_at: Option<OffsetDateTime>,
    pub replayed: usize,
    /// Older than the replay window, or superseded by newer daemon state.
    pub skipped: usize,
    /// Replays the daemon refused again (kept for older reports; an
    /// in-process replay is not refused).
    #[serde(default)]
    pub failed: usize,
    pub unreadable: usize,
    /// Entries per failure kind, as the hooks recorded them.
    pub failures: BTreeMap<IngestFailure, usize>,
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub oldest: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339::option", default)]
    pub newest: Option<OffsetDateTime>,
    /// The newest recorded error text, and the CLI that wrote it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_cli_version: Option<String>,
}

impl DrainReport {
    #[must_use]
    pub fn total(&self) -> usize {
        self.failures.values().sum()
    }
}

/// Split spooled entries into those to replay, in order, and a report of
/// the rest. `latest_activity` answers the daemon's newest activity for a
/// session, if it tracks one.
pub fn plan_replay(
    entries: Vec<SpoolEntry>,
    unreadable: usize,
    now: OffsetDateTime,
    latest_activity: impl Fn(&str) -> Option<OffsetDateTime>,
) -> (Vec<SpoolEntry>, DrainReport) {
    let mut report = DrainReport {
        drained_at: Some(now),
        unreadable,
        ..DrainReport::default()
    };
    let mut replay = Vec::new();
    for entry in entries {
        *report.failures.entry(entry.failure).or_default() += 1;
        report.oldest = Some(
            report
                .oldest
                .map_or(entry.spooled_at, |o| o.min(entry.spooled_at)),
        );
        if report
            .newest
            .is_none_or(|newest| entry.spooled_at >= newest)
        {
            report.newest = Some(entry.spooled_at);
            report.last_error = Some(entry.error.clone());
            report.last_cli_version = Some(entry.cli_version.clone());
        }
        let at = entry.event.at();
        let fresh = now - at <= MAX_REPLAY_AGE;
        let superseded =
            latest_activity(&entry.event.id().session_id).is_some_and(|latest| latest >= at);
        if fresh && !superseded {
            replay.push(entry);
        } else {
            report.skipped += 1;
        }
    }
    // Replay in event order, whatever order the hooks appended in.
    replay.sort_by_key(|entry| entry.event.at());
    (replay, report)
}

pub fn write_report(path: &Path, report: &DrainReport) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_vec_pretty(report).map_err(std::io::Error::other)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, json)?;
    std::fs::rename(temporary, path)
}

#[must_use]
pub fn read_report(path: &Path) -> Option<DrainReport> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentId, AgentKind};

    fn started(session: &str, at: OffsetDateTime) -> AgentEvent {
        AgentEvent::Started {
            id: AgentId {
                kind: AgentKind::ClaudeCode,
                session_id: session.into(),
                surface: None,
                pane: Some("%1".into()),
                tmux_socket: None,
                cwd: None,
            },
            at,
        }
    }

    fn entry(session: &str, at: OffsetDateTime, failure: IngestFailure) -> SpoolEntry {
        SpoolEntry {
            v: SPOOL_SCHEMA_VERSION,
            spooled_at: at,
            failure,
            error: format!("{failure:?}"),
            protocol: 1,
            cli_version: "0.0.0".into(),
            launch_id: None,
            event: started(session, at),
        }
    }

    #[test]
    fn appends_rotates_and_reads_both_generations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SPOOL_FILENAME);
        let now = OffsetDateTime::now_utc();
        append(&path, &entry("a", now, IngestFailure::Unreachable)).unwrap();
        // Force a rotation on the next append.
        // `write`, not `append`: Windows resizes only through a handle with
        // write (not just append) access.
        let big = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        big.set_len(MAX_SPOOL_BYTES).unwrap();
        append(&path, &entry("b", now, IngestFailure::Timeout)).unwrap();
        assert!(rotated_path(&path).exists());
        let (entries, unreadable) = read_all(&path);
        // The padded generation keeps its first line; the padding is not JSON.
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].event.id().session_id, "b");
        assert!(unreadable >= 1);
        clear(&path);
        assert!(!path.exists() && !rotated_path(&path).exists());
    }

    #[test]
    fn concurrent_appenders_across_rotations_lose_no_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = std::sync::Arc::new(dir.path().join(SPOOL_FILENAME));
        let now = OffsetDateTime::now_utc();
        // Large entries so the run crosses the rotation threshold.
        let mut big = entry("x", now, IngestFailure::Unreachable);
        big.error = "e".repeat(16 * 1024);
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                let big = big.clone();
                std::thread::spawn(move || {
                    for _ in 0..4 {
                        append(&path, &big).unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        // 32 × ~16 KiB stays under two generations, so nothing was dropped.
        let (entries, unreadable) = take(&path);
        assert_eq!((entries.len(), unreadable), (32, 0));
        assert!(!path.exists() && !rotated_path(&path).exists());
    }

    #[test]
    fn a_held_lock_bounds_the_hook_append_instead_of_stalling_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(SPOOL_FILENAME);
        let held = lock(&path, None).unwrap();
        let started = std::time::Instant::now();
        let error = append(
            &path,
            &entry("a", OffsetDateTime::now_utc(), IngestFailure::Timeout),
        )
        .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(started.elapsed() < APPEND_LOCK_BUDGET * 4);
        drop(held);
        append(
            &path,
            &entry("a", OffsetDateTime::now_utc(), IngestFailure::Timeout),
        )
        .unwrap();
        assert_eq!(read_all(&path).0.len(), 1);
    }

    #[test]
    fn replays_only_fresh_events_the_daemon_has_not_superseded() {
        let now = OffsetDateTime::now_utc();
        let entries = vec![
            entry(
                "fresh",
                now - time::Duration::minutes(1),
                IngestFailure::Unreachable,
            ),
            entry(
                "stale",
                now - time::Duration::minutes(30),
                IngestFailure::Unreachable,
            ),
            entry(
                "known",
                now - time::Duration::minutes(2),
                IngestFailure::ProtocolMismatch,
            ),
            entry(
                "earlier",
                now - time::Duration::minutes(3),
                IngestFailure::Disconnected,
            ),
        ];
        let latest = |session: &str| (session == "known").then_some(now);
        let (replay, report) = plan_replay(entries, 0, now, latest);
        let sessions: Vec<_> = replay
            .iter()
            .map(|e| e.event.id().session_id.as_str())
            .collect();
        assert_eq!(
            sessions,
            ["earlier", "fresh"],
            "fresh, unsuperseded, in event order"
        );
        assert_eq!(report.skipped, 2);
        assert_eq!(report.total(), 4);
        assert_eq!(report.failures[&IngestFailure::ProtocolMismatch], 1);
        assert_eq!(report.last_error.as_deref(), Some("Unreachable"));
    }

    #[test]
    fn classifies_protocol_skew_apart_from_other_refusals() {
        use crate::ipc::RuntimeError;
        assert_eq!(
            IngestFailure::classify(&RuntimeError::Daemon(
                "protocol mismatch: server=3 client=4".into()
            )),
            IngestFailure::ProtocolMismatch
        );
        assert_eq!(
            IngestFailure::classify(&RuntimeError::Daemon("no".into())),
            IngestFailure::Refused
        );
        assert_eq!(
            IngestFailure::classify(&RuntimeError::NotConnected("/x".into())),
            IngestFailure::Unreachable
        );
    }
}
