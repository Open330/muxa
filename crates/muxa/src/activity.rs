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
use tokio::sync::{broadcast, mpsc, oneshot};
use tracing::{debug, warn};

#[cfg(unix)]
const ACTIVITY_FILE_MODE: u32 = 0o600;

pub const ACTIVITY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ActivityEntry {
    StateTransition(StateTransitionEntry),
    SessionForeground(SessionForegroundEntry),
    HumanInteraction(HumanInteractionEntry),
}

impl ActivityEntry {
    pub fn schema_version(&self) -> u32 {
        match self {
            Self::StateTransition(e) => e.v,
            Self::SessionForeground(e) => e.v,
            Self::HumanInteraction(e) => e.v,
        }
    }

    pub fn at(&self) -> OffsetDateTime {
        match self {
            Self::StateTransition(e) => e.at,
            Self::SessionForeground(e) => e.ended_at,
            Self::HumanInteraction(e) => e.ended_at,
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
    pub session_name: Option<String>,
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
    pub session_name: Option<String>,
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
            session_name,
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
            session_name,
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

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum HumanInteractionKind {
    MuxaWatch,
    MuxaPromptInput,
    TmuxAttach,
    /// A tmux client's `client_activity` advanced between polls while its active
    /// pane was *not* in copy mode — a keypress / interaction with the program.
    /// Unlike [`Self::TmuxAttach`] (which spans the whole attach, idle included),
    /// this marks an instant of real hands-on input, so it distinguishes active
    /// work from a forgotten attach. Counts toward both `active` and `work_active`.
    TmuxInput,
    /// Like [`Self::TmuxInput`], but the active pane was in copy/view mode, so the
    /// advance is scrollback navigation (reading agent output) rather than typing.
    /// Counts toward `active` (engaged/attended) but is excluded from `work_active`
    /// so passively watching a long agent run does not read as hands-on work.
    TmuxScroll,
}

impl HumanInteractionKind {
    /// Input ticks ([`Self::TmuxInput`] / [`Self::TmuxScroll`]) are instantaneous
    /// input markers, not presence spans: they feed the `active` estimate via their
    /// own padding window and must never be treated as raw human-presence time.
    pub fn is_input_tick(self) -> bool {
        matches!(self, Self::TmuxInput | Self::TmuxScroll)
    }

    /// Whether this tick is hands-on work (typing) rather than scrollback reading.
    /// Only relevant for input ticks; non-tick kinds return `true` (they are not
    /// scroll) but callers should gate on [`Self::is_input_tick`] first.
    pub fn is_work_input(self) -> bool {
        !matches!(self, Self::TmuxScroll)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HumanInteractionEntry {
    #[serde(default)]
    pub v: u32,
    pub kind: HumanInteractionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub ended_at: OffsetDateTime,
    pub duration_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HumanInteractionInput {
    pub kind: HumanInteractionKind,
    pub pane: Option<String>,
    pub session_id: Option<String>,
    pub session_name: Option<String>,
    pub started_at: OffsetDateTime,
    pub ended_at: OffsetDateTime,
}

impl HumanInteractionEntry {
    pub fn new(input: HumanInteractionInput) -> Self {
        let HumanInteractionInput {
            kind,
            pane,
            session_id,
            session_name,
            started_at,
            ended_at,
        } = input;
        Self {
            v: ACTIVITY_SCHEMA_VERSION,
            kind,
            pane,
            session_id,
            session_name,
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
    /// Load, filter, and rewrite only after earlier appends have reached disk.
    Compact {
        cutoff: OffsetDateTime,
        complete: oneshot::Sender<std::io::Result<usize>>,
    },
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

    /// Queue compaction in the same FIFO as appends. Only this caller waits for
    /// completion; ingest remains bounded and non-blocking via `try_send`.
    pub async fn compact(&self, max_age: time::Duration) -> CompactReport {
        let cutoff = OffsetDateTime::now_utc() - max_age;
        let (complete, completed) = oneshot::channel();
        if let Err(e) = self
            .writer
            .try_send(WriterMsg::Compact { cutoff, complete })
        {
            warn!(error = %e, "activity compaction queue full; skipping rewrite this cycle");
            return CompactReport {
                rewrite_skipped: true,
                ..CompactReport::default()
            };
        }

        let mut report = CompactReport {
            rewrite_dispatched: true,
            ..CompactReport::default()
        };
        match completed.await {
            Ok(Ok(aged_out)) => report.aged_out = aged_out,
            Ok(Err(_)) => report.rewrite_skipped = true,
            Err(e) => {
                warn!(error = %e, "activity writer stopped before compaction completed");
                report.rewrite_skipped = true;
            }
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

pub async fn append_entry(path: &Path, entry: &ActivityEntry) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    let mut file = open_appender(path).await?;
    write_one(&mut file, entry).await?;
    file.flush().await
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
                    WriterMsg::Compact { cutoff, complete } => {
                        drop(appender.take());
                        let result = compact_file(&path, cutoff).await;
                        if let Err(e) = &result {
                            warn!(error = %e, path = %path.display(), "activity compaction failed");
                        }
                        appender = match open_appender(&path).await {
                            Ok(f) => Some(f),
                            Err(e) => {
                                warn!(error = %e, "could not reopen activity file after rewrite");
                                None
                            }
                        };
                        let _ = complete.send(result);
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

async fn compact_file(path: &Path, cutoff: OffsetDateTime) -> std::io::Result<usize> {
    let loaded = load(path).await?;
    let before = loaded.len();
    let retained = loaded
        .into_iter()
        .filter(|entry| entry.at() >= cutoff)
        .collect::<Vec<_>>();
    let aged_out = before - retained.len();
    atomic_rewrite(path, &retained).await?;
    Ok(aged_out)
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
            session_name: Some("main".into()),
            cwd: Some("/repo".into()),
            from: AgentState::Working,
            to: AgentState::Idle,
            state_entered_at: Some(datetime!(2026-05-31 00:00:00 UTC)),
        });

        assert_eq!(entry.duration_secs, 60);
        assert_eq!(entry.session_name.as_deref(), Some("main"));
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

    #[test]
    fn human_interaction_computes_duration() {
        let entry = HumanInteractionEntry::new(HumanInteractionInput {
            kind: HumanInteractionKind::MuxaPromptInput,
            pane: Some("%1".into()),
            session_id: Some("$1".into()),
            session_name: Some("main".into()),
            started_at: datetime!(2026-05-31 00:00:00 UTC),
            ended_at: datetime!(2026-05-31 00:00:42 UTC),
        });

        assert_eq!(entry.duration_secs, 42);
        assert_eq!(entry.kind, HumanInteractionKind::MuxaPromptInput);
        assert_eq!(entry.session_name.as_deref(), Some("main"));
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
                r#"{"type":"human_interaction","v":1,"kind":"muxa_prompt_input","pane":"%1","session_id":"$1","session_name":"main","started_at":"2026-05-31T00:00:00Z","ended_at":"2026-05-31T00:00:03Z","duration_secs":3}"#,
                "\n",
                r#"{"type":"session_foreground","v":999,"session_id":"$2","session_name":"old","started_at":"2026-05-31T00:00:00Z","ended_at":"2026-05-31T00:00:05Z","duration_secs":5}"#,
                "\n",
                "not-json\n",
            ),
        )
        .await
        .unwrap();

        let entries = load(&path).await.unwrap();
        assert_eq!(entries.len(), 2);
    }

    #[tokio::test]
    async fn append_entry_writes_one_ndjson_line() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("activity.ndjson");
        let entry =
            ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
                kind: HumanInteractionKind::MuxaWatch,
                pane: Some("%1".into()),
                session_id: Some("$1".into()),
                session_name: Some("main".into()),
                started_at: datetime!(2026-05-31 00:00:00 UTC),
                ended_at: datetime!(2026-05-31 00:00:05 UTC),
            }));

        append_entry(&path, &entry).await.unwrap();

        let entries = load(&path).await.unwrap();
        assert_eq!(entries, vec![entry]);
    }

    #[tokio::test]
    async fn compaction_runs_after_preceding_queued_appends() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("activity.ndjson");
        let now = OffsetDateTime::now_utc();
        let entry = ActivityEntry::SessionForeground(SessionForegroundEntry::new(
            "$1",
            "main",
            now - time::Duration::seconds(5),
            now,
        ));
        let (writer, rx) = mpsc::channel(4);
        let log = Arc::new(ActivityLog {
            path: path.clone(),
            writer,
        });

        // Hold the writer offline until both commands are queued. A compactor
        // that snapshots disk here sees an empty file and later erases `entry`.
        log.append(entry.clone());
        let compact = tokio::spawn({
            let log = Arc::clone(&log);
            async move { log.compact(time::Duration::days(1)).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while log.writer.capacity() != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("append and compact commands should both be queued");

        let (shutdown, _) = broadcast::channel(1);
        let writer_task = tokio::spawn(run_writer(path.clone(), rx, shutdown.subscribe()));
        let report = compact.await.unwrap();
        assert!(report.rewrite_dispatched);
        assert!(!report.rewrite_skipped);
        assert_eq!(report.aged_out, 0);

        let _ = shutdown.send(());
        writer_task.await.unwrap();
        assert_eq!(load(&path).await.unwrap(), vec![entry]);
    }
}
