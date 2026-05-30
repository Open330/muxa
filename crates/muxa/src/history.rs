//! Disk-backed prompt history.
//!
//! Records every `PromptSubmitted` event in an append-only NDJSON file plus
//! a bounded in-memory ring per pane. The live registry (`state::Store`)
//! already holds the *latest* prompt as `Agent::last_prompt`, but that
//! field rides along with the agent record — when the reconciler reaps a
//! dead pane, the GC sweeps a long-stopped agent, or a real session
//! supersedes a synthetic placeholder, the prompt goes with it.
//!
//! This module owns the audit-log half of that split. The two layers are
//! deliberately separate:
//!
//! ```text
//! Store::agents (live state)         PromptHistory (audit log)
//! ──────────────────────────         ─────────────────────────
//! - 1 row per pane (canonical)       - many rows per pane
//! - reaped on pane death             - persists across reaping
//! - GC'd 60 min after Stopped        - bounded by config (count + age)
//! - in-memory only                   - in-memory + NDJSON on disk
//! ```
//!
//! ## Why NDJSON, not `SQLite`
//!
//! The access pattern is "append on every prompt, occasionally read the
//! last N for a given pane". At our scale (tens of prompts per minute
//! peak, tens of thousands lifetime) a sorted in-memory map keyed by pane
//! handles every query in microseconds, and the disk file is small enough
//! to load fully on daemon start. NDJSON gives us:
//!
//! * Crash safety: a single-line `write(2)` ≤ `PIPE_BUF` is atomic on
//!   POSIX, so we never half-write a record.
//! * Inspectability: `tail -f`, `jq '.'`, `grep` all just work.
//! * Zero new heavyweight deps — `serde_json` is already in the tree.
//! * Easy migration: bumping `v` lets readers skip incompatible records.
//!
//! Bounded retention is enforced two ways: a per-pane in-memory ring
//! (cap = `max_per_pane`) clips oldest entries on append, and a periodic
//! compaction task rewrites the disk file with current in-memory state
//! (also dropping entries older than `max_age_days`).
//!
//! ## Concurrency model
//!
//! Every `apply(PromptSubmitted)` does two things:
//!   1. Lock the in-memory map (write), append, evict overflow — fast.
//!   2. Send the record over an `mpsc` channel to a dedicated writer task.
//!
//! Disk I/O never sits on the event-handling path. If the writer task is
//! slow (rare — local fsync), the bounded mpsc applies backpressure
//! before backing up the broadcast pipeline.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::fs::OpenOptions;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, RwLock};
use tracing::{debug, warn};

use crate::event::AgentKind;

/// On-disk schema version. Incremented when a future change makes older
/// records unreadable; readers ignore lines with unknown `v`.
///
/// **Unstable wire format** — this value may change between minor releases
/// when the persisted prompt-history record shape evolves. External readers
/// should treat unknown `v` values as "skip" rather than pinning to a
/// specific version.
pub const HISTORY_SCHEMA_VERSION: u32 = 1;

/// One persisted prompt. Fields are deliberately flat for grep-ability
/// and so a future migration can add optional fields without breaking
/// existing readers (serde defaults handle absent fields).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HistoryEntry {
    /// Schema version of the on-disk record. Always written as
    /// [`HISTORY_SCHEMA_VERSION`]; readers skip mismatched lines.
    #[serde(default)]
    pub v: u32,
    pub kind: AgentKind,
    pub session_id: String,
    pub pane: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub prompt: String,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl HistoryEntry {
    pub fn new(
        kind: AgentKind,
        session_id: impl Into<String>,
        pane: impl Into<String>,
        prompt: impl Into<String>,
        at: OffsetDateTime,
        model: Option<String>,
    ) -> Self {
        Self {
            v: HISTORY_SCHEMA_VERSION,
            kind,
            session_id: session_id.into(),
            pane: pane.into(),
            cwd: None,
            prompt: prompt.into(),
            at,
            model,
        }
    }

    pub fn with_cwd(
        kind: AgentKind,
        session_id: impl Into<String>,
        pane: impl Into<String>,
        cwd: Option<String>,
        prompt: impl Into<String>,
        at: OffsetDateTime,
        model: Option<String>,
    ) -> Self {
        Self {
            v: HISTORY_SCHEMA_VERSION,
            kind,
            session_id: session_id.into(),
            pane: pane.into(),
            cwd,
            prompt: prompt.into(),
            at,
            model,
        }
    }
}

/// Tunables for [`PromptHistory`]. The daemon constructs one of these from
/// `[history]` config; tests use sensible in-memory-only defaults.
#[derive(Debug, Clone)]
pub struct HistoryOptions {
    /// Path to the NDJSON file. `None` → in-memory only (no disk I/O).
    pub path: Option<PathBuf>,
    /// Maximum entries kept in memory per pane. Older entries are
    /// evicted from the front of the per-pane deque as new ones land.
    pub max_per_pane: usize,
    /// Maximum age of an entry kept after compaction. Append-time keeps
    /// everything inside `max_per_pane`; the periodic compaction task
    /// applies this filter when rewriting disk + memory.
    pub max_age: time::Duration,
    /// Bounded mpsc capacity for the writer task. Sized for ~30s of peak
    /// traffic (tens/sec); above that, [`PromptHistory::append`] backpressures.
    pub writer_channel_capacity: usize,
}

impl Default for HistoryOptions {
    fn default() -> Self {
        Self {
            path: None,
            max_per_pane: 50,
            max_age: time::Duration::days(30),
            writer_channel_capacity: 1024,
        }
    }
}

#[derive(Debug, Default)]
struct Inner {
    /// Per-pane ring buffer of recent entries. `VecDeque` so `push_back` +
    /// `pop_front` is O(1) — the eviction shape is FIFO oldest-first.
    by_pane: HashMap<String, VecDeque<HistoryEntry>>,
}

/// Append-only prompt log with bounded in-memory caching and periodic
/// disk compaction.
///
/// Constructed via [`PromptHistory::spawn`] when the daemon wants the full
/// disk-backed pipeline, or [`PromptHistory::in_memory_only`] for tests
/// and dry runs. Cheap to clone (`Arc` internally).
pub struct PromptHistory {
    inner: RwLock<Inner>,
    opts: HistoryOptions,
    /// Writer-task sender. `None` iff this instance is in-memory only.
    /// `try_send` so the event-handler path never blocks on disk I/O.
    writer: Option<mpsc::Sender<WriterMsg>>,
}

#[derive(Debug)]
enum WriterMsg {
    Append(HistoryEntry),
    /// Atomically rewrite the file with the supplied snapshot. Sent by
    /// `compact()` from the daemon's compaction task.
    Rewrite(Vec<HistoryEntry>),
}

impl PromptHistory {
    /// In-memory-only history. No disk I/O, no writer task. Used by tests
    /// and as the default when `[history] enabled = false`.
    pub fn in_memory_only(opts: HistoryOptions) -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(Inner::default()),
            opts: HistoryOptions { path: None, ..opts },
            writer: None,
        })
    }

    /// Initialize a disk-backed history: load existing records into
    /// memory, then spawn a writer task that owns the file.
    ///
    /// Returns the [`PromptHistory`] plus a `JoinHandle` for the writer
    /// so the daemon can await graceful shutdown. The writer exits when
    /// `shutdown` fires *after* draining any pending appends.
    pub async fn spawn(
        opts: HistoryOptions,
        shutdown: broadcast::Receiver<()>,
    ) -> std::io::Result<(Arc<Self>, tokio::task::JoinHandle<()>)> {
        let path = opts
            .path
            .clone()
            .expect("PromptHistory::spawn called without a path");

        // Make sure the parent directory exists. NDJSON is permissive about
        // missing files (we'll create on first write), but the dir must be
        // creatable up-front or every append will fail.
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let mut inner = Inner::default();
        let initial = load_from_disk(&path).await?;
        for entry in initial {
            push_bounded(&mut inner, entry, opts.max_per_pane);
        }

        let (tx, rx) = mpsc::channel(opts.writer_channel_capacity);
        let writer_path = path.clone();
        let handle = tokio::spawn(run_writer(writer_path, rx, shutdown));

        Ok((
            Arc::new(Self {
                inner: RwLock::new(inner),
                opts,
                writer: Some(tx),
            }),
            handle,
        ))
    }

    pub fn options(&self) -> &HistoryOptions {
        &self.opts
    }

    /// Record one prompt. Updates the in-memory ring buffer immediately and
    /// fires a fire-and-forget disk write on the writer task.
    ///
    /// The disk send uses `try_send`, so a backed-up writer (rare) drops
    /// the disk record but **keeps the in-memory copy** — better to lose a
    /// rare disk write than to stall the broadcast pipeline behind it.
    /// The drop is logged for diagnosis.
    pub async fn append(&self, entry: HistoryEntry) {
        {
            let mut inner = self.inner.write().await;
            push_bounded(&mut inner, entry.clone(), self.opts.max_per_pane);
        }
        if let Some(tx) = &self.writer {
            if let Err(e) = tx.try_send(WriterMsg::Append(entry)) {
                warn!(error = %e, "history writer queue full; entry not persisted");
            }
        }
    }

    /// Most-recent-first prompts for a pane. `limit = 0` → all available.
    pub async fn recent_for_pane(&self, pane: &str, limit: usize) -> Vec<HistoryEntry> {
        let inner = self.inner.read().await;
        let Some(deque) = inner.by_pane.get(pane) else {
            return Vec::new();
        };
        let iter = deque.iter().rev().cloned();
        if limit == 0 {
            iter.collect()
        } else {
            iter.take(limit).collect()
        }
    }

    /// Most-recent-first prompts across all panes. `limit = 0` → all.
    pub async fn recent_all(&self, limit: usize) -> Vec<HistoryEntry> {
        let inner = self.inner.read().await;
        let mut all: Vec<HistoryEntry> = inner
            .by_pane
            .values()
            .flat_map(|d| d.iter().cloned())
            .collect();
        all.sort_by_key(|e| std::cmp::Reverse(e.at));
        if limit > 0 {
            all.truncate(limit);
        }
        all
    }

    /// Drop entries older than `opts.max_age` from memory and rewrite the
    /// disk file from the surviving in-memory state. Safe to call on a
    /// timer; idempotent. Returns the number of entries dropped.
    pub async fn compact(&self) -> CompactReport {
        let cutoff = OffsetDateTime::now_utc() - self.opts.max_age;
        let mut report = CompactReport::default();

        let snapshot: Vec<HistoryEntry> = {
            let mut inner = self.inner.write().await;
            for deque in inner.by_pane.values_mut() {
                let before = deque.len();
                deque.retain(|e| e.at >= cutoff);
                report.aged_out += before - deque.len();
            }
            inner.by_pane.retain(|_, d| !d.is_empty());
            inner
                .by_pane
                .values()
                .flat_map(|d| d.iter().cloned())
                .collect()
        };

        if let Some(tx) = &self.writer {
            // Bounded send so a stuck writer can't block compaction.
            // Compaction misfires are recoverable on the next tick.
            if let Err(e) = tx.try_send(WriterMsg::Rewrite(snapshot)) {
                warn!(error = %e, "history compaction queue full; skipping rewrite this cycle");
                report.rewrite_skipped = true;
            } else {
                report.rewrite_dispatched = true;
            }
        }
        report
    }

    /// Total entries across all panes. Mostly for tests and diagnostics.
    pub async fn len(&self) -> usize {
        self.inner
            .read()
            .await
            .by_pane
            .values()
            .map(VecDeque::len)
            .sum()
    }

    /// True iff there are no entries at all.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

/// Outcome of one [`PromptHistory::compact`] call.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CompactReport {
    /// Entries dropped because they were older than `max_age`.
    pub aged_out: usize,
    /// `true` if a rewrite was successfully queued for the writer task.
    pub rewrite_dispatched: bool,
    /// `true` if the rewrite was skipped because the writer queue was
    /// full. Recoverable — next tick will try again.
    pub rewrite_skipped: bool,
}

fn push_bounded(inner: &mut Inner, entry: HistoryEntry, max_per_pane: usize) {
    let pane = entry.pane.clone();
    let deque = inner.by_pane.entry(pane).or_default();
    deque.push_back(entry);
    while deque.len() > max_per_pane {
        deque.pop_front();
    }
}

/// Read every parseable NDJSON line from `path` into a Vec. Missing file
/// returns Ok(empty); unparseable / wrong-schema lines are logged and
/// skipped so a single bad line doesn't poison startup.
async fn load_from_disk(path: &Path) -> std::io::Result<Vec<HistoryEntry>> {
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
        match serde_json::from_str::<HistoryEntry>(trimmed) {
            Ok(entry) if entry.v == HISTORY_SCHEMA_VERSION => out.push(entry),
            // Either the line was not parseable JSON (e.g. truncated by a
            // crash mid-write) or the schema version didn't match. Both
            // are skipped — a single bad line must not poison startup.
            Ok(_) | Err(_) => skipped += 1,
        }
    }
    if skipped > 0 {
        debug!(skipped, path = %path.display(), "skipped unreadable history lines");
    }
    Ok(out)
}

/// Writer task body. Owns the file handle in append mode for `Append`
/// messages; `Rewrite` swaps the file via tempfile + rename.
async fn run_writer(
    path: PathBuf,
    mut rx: mpsc::Receiver<WriterMsg>,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut appender = match open_appender(&path).await {
        Ok(f) => Some(f),
        Err(e) => {
            warn!(error = %e, path = %path.display(), "history writer disabled (open failed)");
            None
        }
    };

    loop {
        tokio::select! {
            biased; // drain pending writes before shutdown
            msg = rx.recv() => {
                let Some(msg) = msg else { break };
                match msg {
                    WriterMsg::Append(entry) => {
                        if let Some(file) = appender.as_mut() {
                            if let Err(e) = write_one(file, &entry).await {
                                warn!(error = %e, "history append failed");
                            }
                        }
                    }
                    WriterMsg::Rewrite(snapshot) => {
                        // Drop the appender across the rename so we don't
                        // hold a stale fd to the deleted inode.
                        drop(appender.take());
                        if let Err(e) = atomic_rewrite(&path, &snapshot).await {
                            warn!(error = %e, "history compaction rewrite failed");
                        }
                        appender = match open_appender(&path).await {
                            Ok(f) => Some(f),
                            Err(e) => {
                                warn!(error = %e, "could not reopen history file after rewrite");
                                None
                            }
                        };
                    }
                }
            }
            _ = shutdown.recv() => {
                debug!("history writer shutting down");
                break;
            }
        }
    }
}

/// File mode applied to `prompts.ndjson` — and the tempfile that
/// compaction renames over it. The audit log records full prompt text,
/// which is sensitive enough that the file should not be world-readable.
/// Mirrors `state.json`'s posture and the IPC socket's chmod 0600.
#[cfg(unix)]
const HISTORY_FILE_MODE: u32 = 0o600;

async fn open_appender(path: &Path) -> std::io::Result<tokio::fs::File> {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    opts.mode(HISTORY_FILE_MODE);
    opts.open(path).await
}

async fn write_one(file: &mut tokio::fs::File, entry: &HistoryEntry) -> std::io::Result<()> {
    let mut bytes = serde_json::to_vec(entry)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    bytes.push(b'\n');
    file.write_all(&bytes).await?;
    // No fsync on each write: at our cadence the cost would dominate. The
    // OS write-back happens within seconds; a hard crash inside that
    // window may lose 1-2 entries, which is acceptable for an audit log.
    Ok(())
}

async fn atomic_rewrite(path: &Path, snapshot: &[HistoryEntry]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    let tmp = parent.join(format!(
        ".{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("prompts.ndjson")
    ));
    {
        let mut opts = OpenOptions::new();
        opts.create(true).truncate(true).write(true);
        #[cfg(unix)]
        opts.mode(HISTORY_FILE_MODE);
        let mut f = opts.open(&tmp).await?;
        for entry in snapshot {
            write_one(&mut f, entry).await?;
        }
        // fsync the body before the rename — durability across compaction
        // matters more than per-append latency.
        f.flush().await?;
        f.sync_all().await?;
    }
    tokio::fs::rename(&tmp, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn entry(pane: &str, prompt: &str, at: OffsetDateTime) -> HistoryEntry {
        HistoryEntry::new(AgentKind::ClaudeCode, "s", pane, prompt, at, None)
    }

    #[tokio::test]
    async fn in_memory_append_and_recent() {
        let h = PromptHistory::in_memory_only(HistoryOptions::default());
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        let t1 = datetime!(2026-04-24 12:01:00 UTC);
        h.append(entry("%1", "first", t0)).await;
        h.append(entry("%1", "second", t1)).await;

        let got = h.recent_for_pane("%1", 0).await;
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].prompt, "second", "newest first");
        assert_eq!(got[1].prompt, "first");
    }

    #[tokio::test]
    async fn ring_buffer_caps_at_max_per_pane() {
        let opts = HistoryOptions {
            max_per_pane: 3,
            ..Default::default()
        };
        let h = PromptHistory::in_memory_only(opts);
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        for i in 0..5 {
            h.append(entry(
                "%1",
                &format!("p{i}"),
                t0 + time::Duration::seconds(i),
            ))
            .await;
        }
        let got = h.recent_for_pane("%1", 0).await;
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].prompt, "p4", "newest preserved");
        assert_eq!(got[2].prompt, "p2", "oldest within cap preserved");
    }

    #[tokio::test]
    async fn recent_for_unknown_pane_is_empty() {
        let h = PromptHistory::in_memory_only(HistoryOptions::default());
        assert!(h.recent_for_pane("%nope", 5).await.is_empty());
    }

    #[tokio::test]
    async fn recent_all_orders_across_panes() {
        let h = PromptHistory::in_memory_only(HistoryOptions::default());
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        h.append(entry("%a", "A1", t0)).await;
        h.append(entry("%b", "B1", t0 + time::Duration::seconds(1)))
            .await;
        h.append(entry("%a", "A2", t0 + time::Duration::seconds(2)))
            .await;

        let all = h.recent_all(0).await;
        assert_eq!(
            all.iter().map(|e| e.prompt.as_str()).collect::<Vec<_>>(),
            vec!["A2", "B1", "A1"]
        );
    }

    #[test]
    fn history_entry_defaults_missing_cwd() {
        let raw = r#"{
            "v": 1,
            "kind": "claude_code",
            "session_id": "s",
            "pane": "%1",
            "prompt": "hello",
            "at": "2026-04-24T12:00:00Z"
        }"#;

        let got: HistoryEntry = serde_json::from_str(raw).unwrap();
        assert_eq!(got.cwd, None);
    }

    #[test]
    fn history_entry_with_cwd_round_trips() {
        let entry = HistoryEntry::with_cwd(
            AgentKind::ClaudeCode,
            "s",
            "%1",
            Some("/home/june/muxa".into()),
            "hello",
            datetime!(2026-04-24 12:00:00 UTC),
            None,
        );

        let encoded = serde_json::to_string(&entry).unwrap();
        let got: HistoryEntry = serde_json::from_str(&encoded).unwrap();
        assert_eq!(got.cwd.as_deref(), Some("/home/june/muxa"));
    }

    #[tokio::test]
    async fn compact_drops_aged_entries() {
        let opts = HistoryOptions {
            max_age: time::Duration::seconds(60),
            ..Default::default()
        };
        let h = PromptHistory::in_memory_only(opts);
        let now = OffsetDateTime::now_utc();
        h.append(entry("%1", "fresh", now)).await;
        h.append(entry("%1", "stale", now - time::Duration::hours(1)))
            .await;
        assert_eq!(h.len().await, 2);

        let report = h.compact().await;
        assert_eq!(report.aged_out, 1);
        assert_eq!(h.len().await, 1);
        assert_eq!(h.recent_for_pane("%1", 0).await[0].prompt, "fresh");
    }

    #[tokio::test]
    async fn disk_roundtrip_persists_across_reopen() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("prompts.ndjson");
        let opts = HistoryOptions {
            path: Some(path.clone()),
            ..Default::default()
        };
        let (tx, _) = broadcast::channel::<()>(1);

        let (h, writer) = PromptHistory::spawn(opts.clone(), tx.subscribe())
            .await
            .unwrap();
        let t0 = datetime!(2026-04-24 12:00:00 UTC);
        h.append(entry("%1", "persisted", t0)).await;

        // Quiesce: shut down the writer so the appended record is durable
        // before we reopen the file.
        let _ = tx.send(());
        let _ = writer.await;

        // Re-open: a fresh PromptHistory must hydrate from disk.
        let (tx2, _) = broadcast::channel::<()>(1);
        let (h2, writer2) = PromptHistory::spawn(opts, tx2.subscribe()).await.unwrap();
        let got = h2.recent_for_pane("%1", 0).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].prompt, "persisted");
        let _ = tx2.send(());
        let _ = writer2.await;
    }

    /// Audit log carries full prompt text — must not be world-readable.
    /// Locks down the chmod-after-create invariant on the file the writer
    /// task creates on first append.
    #[cfg(unix)]
    #[tokio::test]
    async fn append_writes_file_with_0600_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("prompts.ndjson");
        let opts = HistoryOptions {
            path: Some(path.clone()),
            ..Default::default()
        };
        let (tx, _) = broadcast::channel::<()>(1);
        let (h, writer) = PromptHistory::spawn(opts, tx.subscribe()).await.unwrap();
        h.append(entry("%1", "p", datetime!(2026-04-28 0:00 UTC)))
            .await;
        // Quiesce so the writer has actually flushed the line to disk.
        let _ = tx.send(());
        let _ = writer.await;

        let mode = tokio::fs::metadata(&path)
            .await
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "expected 0600, got {:o}", mode & 0o777);
    }

    /// A truncated/garbage line at the end of the file (e.g. crash mid-write)
    /// must not poison startup. Good lines before it should still load.
    #[tokio::test]
    async fn disk_load_skips_corrupted_lines() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("prompts.ndjson");
        let entry = HistoryEntry::new(
            AgentKind::ClaudeCode,
            "s",
            "%1",
            "good",
            datetime!(2026-04-24 12:00:00 UTC),
            None,
        );
        let mut bytes = serde_json::to_vec(&entry).unwrap();
        bytes.push(b'\n');
        bytes.extend_from_slice(b"{not even json\n"); // corrupted line
        bytes.extend_from_slice(b"{\"v\": 9999, \"kind\": \"claude_code\"}\n"); // future schema
        tokio::fs::write(&path, bytes).await.unwrap();

        let (tx, _) = broadcast::channel::<()>(1);
        let opts = HistoryOptions {
            path: Some(path),
            ..Default::default()
        };
        let (h, writer) = PromptHistory::spawn(opts, tx.subscribe()).await.unwrap();
        let got = h.recent_for_pane("%1", 0).await;
        assert_eq!(
            got.len(),
            1,
            "good line survives, corrupt+future-schema skipped"
        );
        assert_eq!(got[0].prompt, "good");
        let _ = tx.send(());
        let _ = writer.await;
    }

    /// Compaction must rewrite the disk file so aged-out entries are
    /// physically gone, not just hidden in memory.
    #[tokio::test]
    async fn compaction_rewrites_disk_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("prompts.ndjson");
        let opts = HistoryOptions {
            path: Some(path.clone()),
            max_age: time::Duration::seconds(60),
            ..Default::default()
        };
        let (tx, _) = broadcast::channel::<()>(1);
        let (h, writer) = PromptHistory::spawn(opts.clone(), tx.subscribe())
            .await
            .unwrap();
        let now = OffsetDateTime::now_utc();
        h.append(entry("%1", "fresh", now)).await;
        h.append(entry("%1", "stale", now - time::Duration::hours(1)))
            .await;

        // Wait briefly for the writer to flush both appends.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let report = h.compact().await;
        assert_eq!(report.aged_out, 1);
        assert!(report.rewrite_dispatched);

        // Wait for the rewrite to land. We poll the disk size rather than
        // sleeping a fixed window so the test stays robust on slow CI.
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            let bytes = tokio::fs::read(&path).await.unwrap();
            if !bytes.is_empty() && !std::str::from_utf8(&bytes).unwrap().contains("stale") {
                break;
            }
        }
        let on_disk = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(on_disk.contains("fresh"));
        assert!(
            !on_disk.contains("stale"),
            "stale entry must be physically dropped"
        );

        let _ = tx.send(());
        let _ = writer.await;
    }
}
