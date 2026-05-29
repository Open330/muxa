//! Cumulative tmux session foreground-time tracking.
//!
//! The signal this module tracks is intentionally tmux-native:
//! interactive `tmux list-clients` rows grouped by their `client_session`.
//! That maps to "a human has this session in a foreground tmux client",
//! ignores control-mode automation clients, and survives panes/windows
//! coming and going inside the same session.

use crate::tmux::{self, SessionInfo, TmuxError};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};
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
    let mut changed = false;
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
            add_elapsed(record, since, now);
            record.attached_since = None;
            record.attached_clients = 0;
            record.last_seen_at = now;
            changed = true;
        }
    }

    changed
}

fn add_elapsed(record: &mut SessionActivity, since: OffsetDateTime, now: OffsetDateTime) {
    let secs = u64::try_from((now - since).whole_seconds().max(0)).unwrap_or(u64::MAX);
    record.total_attached_secs = record.total_attached_secs.saturating_add(secs);
}

pub struct SessionActivityTracker {
    path: PathBuf,
    interval: Duration,
}

impl SessionActivityTracker {
    pub fn new(path: PathBuf, interval: Duration) -> Self {
        Self { path, interval }
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
        let sample = tokio::task::spawn_blocking(list_sessions_for_activity)
            .await
            .unwrap_or_else(|e| Err(format!("join error: {e}")));
        let sessions = match sample {
            Ok(sessions) => sessions,
            Err(e) => {
                debug!(error = %e, "session activity poll skipped");
                return;
            }
        };

        let changed = apply_sample(records, &sessions, OffsetDateTime::now_utc());
        if changed {
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

fn list_sessions_for_activity() -> Result<Vec<SessionInfo>, String> {
    let mut sessions = match tmux::list_sessions() {
        Ok(sessions) => sessions,
        Err(TmuxError::NonZero(msg)) if msg.trim_start().starts_with("no server running") => {
            return Ok(Vec::new());
        }
        Err(e) => return Err(e.to_string()),
    };
    let clients = match tmux::list_clients() {
        Ok(clients) => clients,
        Err(TmuxError::NonZero(msg)) if msg.trim_start().starts_with("no server running") => {
            return Ok(Vec::new());
        }
        Err(e) => return Err(e.to_string()),
    };
    apply_client_counts(&mut sessions, &clients);
    Ok(sessions)
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
    fn disappearing_attached_session_closes_interval() {
        let mut records = HashMap::new();
        let t0 = datetime!(2026-05-29 00:00:00 UTC);
        let t1 = datetime!(2026-05-29 00:00:20 UTC);

        apply_sample(&mut records, &[session("$1", "main", 1)], t0);
        assert!(apply_sample(&mut records, &[], t1));
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
            },
            tmux::ClientInfo {
                session: "main".into(),
                control_mode: true,
            },
        ];

        apply_client_counts(&mut sessions, &clients);

        assert_eq!(sessions[0].attached_clients, 1);
        assert_eq!(sessions[1].attached_clients, 0);
    }
}
