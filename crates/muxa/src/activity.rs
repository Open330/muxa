//! Append-only activity ledger.
//!
//! `state.json` answers "what is true now?" and `prompts.ndjson` answers
//! "what did the user ask?". This module owns the duration side: closed
//! intervals for agent state transitions and tmux session foreground time.
//! The file is NDJSON so it stays grep-able and cheap to append.

use crate::event::{AgentKind, AgentState};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use time::OffsetDateTime;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, warn};

#[cfg(unix)]
const ACTIVITY_FILE_MODE: u32 = 0o600;

pub const ACTIVITY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActivityEntry {
    StateTransition(StateTransitionEntry),
    SessionForeground(SessionForegroundEntry),
}

impl ActivityEntry {
    pub fn schema_version(&self) -> u32 {
        match self {
            Self::StateTransition(e) => e.v,
            Self::SessionForeground(e) => e.v,
        }
    }

    pub fn at(&self) -> OffsetDateTime {
        match self {
            Self::StateTransition(e) => e.at,
            Self::SessionForeground(e) => e.ended_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StateTransitionEntry {
    #[serde(default)]
    pub v: u32,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub kind: AgentKind,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub from: AgentState,
    pub to: AgentState,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub state_entered_at: Option<OffsetDateTime>,
    pub duration_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateTransitionInput {
    pub at: OffsetDateTime,
    pub kind: AgentKind,
    pub session_id: String,
    pub pane: Option<String>,
    pub cwd: Option<String>,
    pub from: AgentState,
    pub to: AgentState,
    pub state_entered_at: Option<OffsetDateTime>,
}

impl StateTransitionEntry {
    pub fn new(input: StateTransitionInput) -> Self {
        let StateTransitionInput {
            at,
            kind,
            session_id,
            pane,
            cwd,
            from,
            to,
            state_entered_at,
        } = input;
        let duration_secs = state_entered_at.map_or(0, |since| duration_secs(since, at));
        Self {
            v: ACTIVITY_SCHEMA_VERSION,
            at,
            kind,
            session_id,
            pane,
            cwd,
            from,
            to,
            state_entered_at,
            duration_secs,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionForegroundEntry {
    #[serde(default)]
    pub v: u32,
    pub session_id: String,
    pub session_name: String,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub ended_at: OffsetDateTime,
    pub duration_secs: u64,
}

impl SessionForegroundEntry {
    pub fn new(
        session_id: impl Into<String>,
        session_name: impl Into<String>,
        started_at: OffsetDateTime,
        ended_at: OffsetDateTime,
    ) -> Self {
        Self {
            v: ACTIVITY_SCHEMA_VERSION,
            session_id: session_id.into(),
            session_name: session_name.into(),
            started_at,
            ended_at,
            duration_secs: duration_secs(started_at, ended_at),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ActivityOptions {
    pub path: PathBuf,
    pub writer_channel_capacity: usize,
}

impl ActivityOptions {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            writer_channel_capacity: 1024,
        }
    }
}

#[derive(Debug)]
pub struct ActivityLog {
    path: PathBuf,
    writer: mpsc::Sender<WriterMsg>,
}

#[derive(Debug)]
enum WriterMsg {
    Append(ActivityEntry),
    Rewrite(Vec<ActivityEntry>),
}

impl ActivityLog {
    pub async fn spawn(
        opts: ActivityOptions,
        shutdown: broadcast::Receiver<()>,
    ) -> std::io::Result<(Arc<Self>, tokio::task::JoinHandle<()>)> {
        if let Some(parent) = opts.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let (tx, rx) = mpsc::channel(opts.writer_channel_capacity);
        let writer_path = opts.path.clone();
        let handle = tokio::spawn(run_writer(writer_path, rx, shutdown));
        Ok((
            Arc::new(Self {
                path: opts.path,
                writer: tx,
            }),
            handle,
        ))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, entry: ActivityEntry) {
        if let Err(e) = self.writer.try_send(WriterMsg::Append(entry)) {
            warn!(error = %e, "activity writer queue full; entry not persisted");
        }
    }

    pub async fn compact(&self, max_age: time::Duration) -> CompactReport {
        let cutoff = OffsetDateTime::now_utc() - max_age;
        let loaded = match load(&self.path).await {
            Ok(entries) => entries,
            Err(e) => {
                warn!(error = %e, path = %self.path.display(), "activity compaction load failed");
                return CompactReport {
                    rewrite_skipped: true,
                    ..CompactReport::default()
                };
            }
        };
        let before = loaded.len();
        let retained = loaded
            .into_iter()
            .filter(|entry| entry.at() >= cutoff)
            .collect::<Vec<_>>();
        let aged_out = before - retained.len();
        let mut report = CompactReport {
            aged_out,
            ..CompactReport::default()
        };
        if let Err(e) = self.writer.try_send(WriterMsg::Rewrite(retained)) {
            warn!(error = %e, "activity compaction queue full; skipping rewrite this cycle");
            report.rewrite_skipped = true;
        } else {
            report.rewrite_dispatched = true;
        }
        report
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CompactReport {
    pub aged_out: usize,
    pub rewrite_dispatched: bool,
    pub rewrite_skipped: bool,
}

pub async fn load(path: &Path) -> std::io::Result<Vec<ActivityEntry>> {
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut reader = BufReader::new(file);
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut skipped = 0usize;
    loop {
        buf.clear();
        let n = reader.read_line(&mut buf).await?;
        if n == 0 {
            break;
        }
        let trimmed = buf.trim_end_matches('\n').trim_end_matches('\r');
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<ActivityEntry>(trimmed) {
            Ok(entry) if entry.schema_version() == ACTIVITY_SCHEMA_VERSION => out.push(entry),
            Ok(_) | Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        debug!(skipped, path = %path.display(), "skipped unreadable activity lines");
    }
    Ok(out)
}

async fn run_writer(
    path: PathBuf,
    mut rx: mpsc::Receiver<WriterMsg>,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut appender = match open_appender(&path).await {
        Ok(f) => Some(f),
        Err(e) => {
            warn!(error = %e, path = %path.display(), "activity writer disabled (open failed)");
            None
        }
    };

    loop {
        tokio::select! {
            biased;
            msg = rx.recv() => {
                let Some(msg) = msg else { break };
                match msg {
                    WriterMsg::Append(entry) => {
                        if let Some(file) = appender.as_mut() {
                            if let Err(e) = write_one(file, &entry).await {
                                warn!(error = %e, "activity append failed");
                            }
                        }
                    }
                    WriterMsg::Rewrite(entries) => {
                        drop(appender.take());
                        if let Err(e) = atomic_rewrite(&path, &entries).await {
                            warn!(error = %e, "activity compaction rewrite failed");
                        }
                        appender = match open_appender(&path).await {
                            Ok(f) => Some(f),
                            Err(e) => {
                                warn!(error = %e, "could not reopen activity file after rewrite");
                                None
                            }
                        };
                    }
                }
            }
            _ = shutdown.recv() => {
                debug!("activity writer shutting down");
                break;
            }
        }
    }
}

async fn open_appender(path: &Path) -> std::io::Result<tokio::fs::File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    opts.mode(ACTIVITY_FILE_MODE);
    opts.open(path).await
}

async fn write_one(file: &mut tokio::fs::File, entry: &ActivityEntry) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    file.write_all(&bytes).await
}

async fn atomic_rewrite(path: &Path, entries: &[ActivityEntry]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("activity.ndjson")
    ));
    {
        let mut opts = OpenOptions::new();
        opts.create(true).truncate(true).write(true);
        #[cfg(unix)]
        opts.mode(ACTIVITY_FILE_MODE);
        let mut f = opts.open(&tmp).await?;
        for entry in entries {
            write_one(&mut f, entry).await?;
        }
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

fn duration_secs(started_at: OffsetDateTime, ended_at: OffsetDateTime) -> u64 {
    u64::try_from((ended_at - started_at).whole_seconds().max(0)).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentKind, AgentState};
    use time::macros::datetime;

    #[test]
    fn state_transition_computes_duration() {
        let entry = StateTransitionEntry::new(StateTransitionInput {
            at: datetime!(2026-05-31 00:01:00 UTC),
            kind: AgentKind::Codex,
            session_id: "s".into(),
            pane: Some("%1".into()),
            cwd: Some("/repo".into()),
            from: AgentState::Working,
            to: AgentState::Idle,
            state_entered_at: Some(datetime!(2026-05-31 00:00:00 UTC)),
        });

        assert_eq!(entry.duration_secs, 60);
    }

    #[test]
    fn session_foreground_computes_duration() {
        let entry = SessionForegroundEntry::new(
            "$1",
            "main",
            datetime!(2026-05-31 00:00:00 UTC),
            datetime!(2026-05-31 00:00:05 UTC),
        );

        assert_eq!(entry.duration_secs, 5);
    }

    #[tokio::test]
    async fn disk_roundtrip_skips_unknown_schema() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("activity.ndjson");
        tokio::fs::write(
            &path,
            concat!(
                r#"{"type":"session_foreground","v":1,"session_id":"$1","session_name":"main","started_at":"2026-05-31T00:00:00Z","ended_at":"2026-05-31T00:00:05Z","duration_secs":5}"#,
                "\n",
                r#"{"type":"session_foreground","v":999,"session_id":"$2","session_name":"old","started_at":"2026-05-31T00:00:00Z","ended_at":"2026-05-31T00:00:05Z","duration_secs":5}"#,
                "\n",
                "not-json\n",
            ),
        )
        .await
        .unwrap();

        let entries = load(&path).await.unwrap();
        assert_eq!(entries.len(), 1);
    }
}
