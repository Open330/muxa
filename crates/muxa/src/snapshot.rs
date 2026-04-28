//! Disk-backed snapshot of the live agent registry.
//!
//! The [`crate::state::Store`] is event-driven and entirely in-memory: every
//! hook landing mutates the `agents` map, and a daemon restart drops the
//! lot. Without persistence the operator pays a real cost — `muxa watch`
//! comes back blank, then slowly re-fills only as live agents happen to
//! emit a hook (which can take minutes for an idle Claude session).
//! Discovery synthesizes `synthetic-%X` placeholders to plaster over that
//! gap, but they carry no real `session_id`, no `last_prompt`, no
//! `last_response`, no model/cost metadata.
//!
//! This module closes that gap by mirroring the registry to one JSON file
//! (`$XDG_DATA_HOME/muxa/state.json` by default). A daemon restart hydrates
//! the [`Store`](crate::state::Store) from the file before discovery and
//! the IPC server come up; the reconciler reaps anything whose pane has
//! since died.
//!
//! ## Why JSON, not NDJSON
//!
//! [`crate::history`] uses NDJSON because the access pattern is
//! append-on-prompt-read-most-recent-N. The state snapshot is a single
//! always-current view: tens of agent rows, rewritten in full on each
//! save. NDJSON would force replay-and-compaction on every load for
//! exactly zero benefit at this scale. A full JSON rewrite via temp-file
//! plus rename is ~15 KB of disk traffic per debounce window, atomically
//! observable, and trivially editable by a human in `jq`.
//!
//! ## Trigger model — event-driven, debounced
//!
//! Every [`Store`](crate::state::Store) mutator (`apply`, `gc`,
//! `reconcile`) calls `dirty.notify_one()` after dropping its write lock.
//! [`Snapshotter::run`] waits on that `Notify`, then sleeps
//! `debounce` ms before snapshotting and writing. This shape gives us:
//!
//! * **Zero idle I/O.** No tick fires when nothing has changed.
//! * **Coalescing.** A burst (`PromptSubmitted` → `ToolStarted` →
//!   `ToolCompleted` → `TurnStopped` within ms) collapses into one disk
//!   write because `Notify::notify_one()` saturates to one pending wakeup.
//! * **Off the hot path.** Mutators only do an atomic notify; serialization
//!   and fsync run on the snapshotter task.
//!
//! ## Failure tolerance
//!
//! Loading: missing file → empty initial set. Corrupt file or schema-version
//! mismatch → log warn, start empty. Disk write failure → log warn, keep
//! running. The snapshot is best-effort recovery, not a source of truth —
//! losing it just degrades a restart back to the `synthetic-%X` baseline.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::{broadcast, Notify};
use tracing::{debug, info, warn};

/// File mode applied to the snapshot file (and its in-flight tempfile).
/// Snapshots include user prompts, responses, and `cwd`s — sensitive
/// enough that the file should not be world-readable. Mirrors the
/// posture used for the IPC socket via `harden_permissions`.
#[cfg(unix)]
const STATE_FILE_MODE: u32 = 0o600;

use crate::state::{Agent, SharedStore};

/// On-disk schema version. Bump when an incompatible field shape lands;
/// readers ignore files with an unknown `v` and start from empty rather
/// than crashing the daemon on a malformed payload.
pub const STATE_SCHEMA_VERSION: u32 = 1;

/// Wire format for the on-disk snapshot. Flat JSON so `jq` works without
/// ceremony and a future migration can add optional fields without
/// breaking older readers (serde defaults handle absent fields).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StateFile {
    /// Schema version of this payload. Always written as
    /// [`STATE_SCHEMA_VERSION`]; loader skips files with a mismatch.
    #[serde(default)]
    v: u32,
    /// When the snapshot was written, RFC 3339. Informational — the loader
    /// doesn't gate on it, but it's handy in incident postmortems.
    #[serde(with = "time::serde::rfc3339")]
    saved_at: OffsetDateTime,
    /// Full registry contents. Order is not stable across writes (the
    /// store is a `HashMap`); consumers that care must sort.
    agents: Vec<Agent>,
}

/// Load a snapshot from disk. Missing file → empty vec; corrupt file or
/// schema mismatch → warn and return empty rather than fail-fast, so a
/// daemon with a bad state.json on disk can still start (it'll just lose
/// the previous run's state, same as if the file had been deleted).
pub async fn load(path: &Path) -> Vec<Agent> {
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %path.display(), "no state snapshot on disk; starting empty");
            return Vec::new();
        }
        Err(e) => {
            warn!(error = %e, path = %path.display(), "could not read state snapshot");
            return Vec::new();
        }
    };

    let parsed: StateFile = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            warn!(error = %e, path = %path.display(), "state snapshot is corrupt; starting empty");
            return Vec::new();
        }
    };

    if parsed.v != STATE_SCHEMA_VERSION {
        warn!(
            file_version = parsed.v,
            expected = STATE_SCHEMA_VERSION,
            path = %path.display(),
            "state snapshot has unknown schema version; starting empty",
        );
        return Vec::new();
    }

    info!(
        agents = parsed.agents.len(),
        path = %path.display(),
        saved_at = %parsed.saved_at,
        "rehydrated agent registry from snapshot",
    );
    parsed.agents
}

/// Atomically replace the snapshot file. Writes to a per-process
/// tempfile, fsyncs the body, renames over the destination, then fsyncs
/// the parent directory so the rename itself is durable across power
/// loss. Crash mid-write leaves the previous good snapshot untouched.
///
/// Tempfile name embeds the PID so two `muxad` instances racing on the
/// same path (misconfigured deploy, overlapping restart) can't stomp
/// each other mid-write — defense-in-depth on top of the daemon's own
/// single-writer-task serialization.
async fn save(path: &Path, agents: &[Agent]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    if !parent.as_os_str().is_empty() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let basename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("state.json");
    let tmp = parent.join(format!(".{}.{}.tmp", basename, std::process::id()));

    let payload = StateFile {
        v: STATE_SCHEMA_VERSION,
        saved_at: OffsetDateTime::now_utc(),
        agents: agents.to_vec(),
    };
    let bytes = serde_json::to_vec(&payload)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    {
        let mut opts = OpenOptions::new();
        opts.create(true).truncate(true).write(true);
        // 0600 on the tempfile so an `ls` between create and rename can't
        // expose contents to other local users; the rename preserves the
        // tempfile's mode, so the destination inherits it automatically.
        // `tokio::fs::OpenOptions::mode` is gated to Unix platforms.
        #[cfg(unix)]
        opts.mode(STATE_FILE_MODE);
        let mut f = opts.open(&tmp).await?;
        f.write_all(&bytes).await?;
        // fsync the body before the rename — we want the file's contents
        // durable before any crash window opens between rename and the
        // next write. The cost is a single fsync per debounce window
        // (default 200ms), which is fine.
        f.flush().await?;
        f.sync_all().await?;
    }
    tokio::fs::rename(&tmp, path).await?;

    // Durability of the rename itself depends on the parent directory's
    // metadata being on disk. Without this, on most journaled
    // filesystems (ext4, xfs) a power loss between rename and the next
    // sync can leave `state.json` pointing at the *old* inode despite
    // an apparently-successful save. Cheap (one extra fsync per debounce
    // window) and closes the durability gap the body fsync alone leaves.
    if !parent.as_os_str().is_empty() {
        if let Ok(dir) = OpenOptions::new().read(true).open(parent).await {
            let _ = dir.sync_all().await;
        }
    }

    Ok(())
}

/// Configuration knobs the daemon hands to [`Snapshotter`]. Mirrors the
/// `[state]` config section but lives here so the runtime layer doesn't
/// pull in `crate::config` for this one struct.
#[derive(Debug, Clone)]
pub struct SnapshotterOptions {
    pub path: PathBuf,
    /// Coalescing window applied between a `dirty` wakeup and the actual
    /// disk write.
    pub debounce: Duration,
}

/// Event-driven snapshot writer. Wakes on [`Store`](crate::state::Store)
/// `dirty` notifications, debounces, and writes the registry to disk.
///
/// Generic over no traits — the only inputs it needs are a store handle
/// and the dirty signal. Tests can drive it directly by constructing one
/// and calling `notify_one()` on the same `Notify` they hand in.
pub struct Snapshotter {
    store: SharedStore,
    dirty: Arc<Notify>,
    opts: SnapshotterOptions,
}

impl Snapshotter {
    pub fn new(store: SharedStore, dirty: Arc<Notify>, opts: SnapshotterOptions) -> Self {
        Self { store, dirty, opts }
    }

    /// Run the writer loop until `shutdown` fires.
    ///
    /// Loop shape:
    /// 1. Wait for either a dirty signal or an immediate shutdown.
    /// 2. On dirty: race a `sleep(debounce)` against shutdown so the
    ///    final transitions before SIGTERM are durable even if they
    ///    landed mid-debounce.
    /// 3. Flush — `store.snapshot()` + `save(path, …)` (atomic rename).
    ///
    /// Multiple notifies during the debounce window collapse into one
    /// wakeup thanks to `Notify`'s saturating-to-1 semantics, so a
    /// tool-heavy turn that fires four events in 1 ms produces exactly
    /// one disk write.
    pub async fn run(self, mut shutdown: broadcast::Receiver<()>) {
        loop {
            tokio::select! {
                () = self.dirty.notified() => {
                    // Coalesce a burst of adjacent signals before
                    // serializing. The inner select makes the debounce
                    // shutdown-aware: if SIGTERM lands during the wait we
                    // skip the rest of the window, flush, and exit — so
                    // events that arrived just before shutdown are not
                    // lost. The flush after the select runs in both arms.
                    let early_shutdown = tokio::select! {
                        () = tokio::time::sleep(self.opts.debounce) => false,
                        _ = shutdown.recv() => true,
                    };
                    self.flush().await;
                    if early_shutdown {
                        debug!("snapshotter shutting down");
                        return;
                    }
                }
                _ = shutdown.recv() => {
                    // Shutdown with no pending dirty — still flush so the
                    // file on disk matches the registry the daemon is
                    // about to drop. Cheap when nothing changed (rewrites
                    // the same bytes) and worth it for the consistency
                    // guarantee on a clean restart.
                    self.flush().await;
                    debug!("snapshotter shutting down");
                    return;
                }
            }
        }
    }

    /// Snapshot the registry and write it through `save`. Errors are
    /// logged but never propagated — a write failure shouldn't take down
    /// the daemon, and the next dirty signal will retry naturally.
    async fn flush(&self) {
        let agents = self.store.snapshot().await;
        if let Err(e) = save(&self.opts.path, &agents).await {
            warn!(
                error = %e,
                path = %self.opts.path.display(),
                "state snapshot write failed",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, AgentId, AgentKind};
    use crate::state::Store;
    use tempfile::TempDir;
    use time::macros::datetime;

    fn started_event(sid: &str, pane: &str, at: OffsetDateTime) -> AgentEvent {
        AgentEvent::Started {
            id: AgentId {
                kind: AgentKind::ClaudeCode,
                session_id: sid.into(),
                pane: Some(pane.into()),
                cwd: None,
            },
            at,
        }
    }

    /// `load` of a missing file returns empty without error — first-run
    /// daemons must boot cleanly even if XDG dir is empty.
    #[tokio::test]
    async fn load_missing_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nope.json");
        assert!(load(&path).await.is_empty());
    }

    /// `load` of a corrupt file logs a warning but still returns empty
    /// instead of panicking — a bad state.json must not wedge the daemon.
    #[tokio::test]
    async fn load_corrupt_file_returns_empty() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.json");
        tokio::fs::write(&path, b"{not valid json").await.unwrap();
        assert!(load(&path).await.is_empty());
    }

    /// `load` of a file with an unknown schema version returns empty and
    /// warns — older daemons reading newer files (or vice versa) cleanly
    /// downgrade to the synthetic-placeholder baseline rather than blow up.
    #[tokio::test]
    async fn load_skips_unknown_schema_version() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.json");
        let payload = serde_json::json!({
            "v": 9999,
            "saved_at": "2026-04-28T00:00:00Z",
            "agents": [],
        });
        tokio::fs::write(&path, payload.to_string()).await.unwrap();
        assert!(load(&path).await.is_empty());
    }

    /// Round-trip: write a registry to disk, read it back, get the same
    /// agents. Covers the happy path of restart-and-rehydrate.
    #[tokio::test]
    async fn save_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.json");

        let store = Store::shared();
        store
            .apply(&started_event("a", "%1", datetime!(2026-04-28 00:00 UTC)))
            .await;
        store
            .apply(&started_event("b", "%2", datetime!(2026-04-28 00:00 UTC)))
            .await;

        let snap = store.snapshot().await;
        save(&path, &snap).await.unwrap();

        let loaded = load(&path).await;
        assert_eq!(loaded.len(), 2);
        let mut sids: Vec<_> = loaded.iter().map(|a| a.session_id.clone()).collect();
        sids.sort();
        assert_eq!(sids, vec!["a".to_string(), "b".to_string()]);
    }

    /// `state.json` carries user prompts, responses, and cwds — it must
    /// not be world-readable. Locks down the chmod-after-create
    /// invariant on the file we wrote on this Unix host.
    #[cfg(unix)]
    #[tokio::test]
    async fn save_writes_file_with_0600_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.json");
        save(&path, &[]).await.unwrap();
        let mode = tokio::fs::metadata(&path)
            .await
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600, "expected 0600, got {:o}", mode & 0o777);
    }

    /// Atomic-rename guarantees: an in-flight write that crashes between
    /// tempfile creation and rename leaves the previous good file untouched.
    /// We can't really crash a process here, so this asserts the weaker
    /// (but observable) property: after a successful save the tempfile is
    /// gone and only the destination remains.
    #[tokio::test]
    async fn save_does_not_leak_tempfile() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.json");
        save(&path, &[]).await.unwrap();
        let mut entries = tokio::fs::read_dir(dir.path()).await.unwrap();
        let mut names = Vec::new();
        while let Some(e) = entries.next_entry().await.unwrap() {
            names.push(e.file_name().to_string_lossy().into_owned());
        }
        assert_eq!(names, vec!["state.json".to_string()]);
    }

    /// End-to-end: run a Snapshotter, fire several `apply` calls in a
    /// burst, observe that the dirty signal coalesces them into a single
    /// disk write containing every event's effect.
    ///
    /// Verifies coalescing structurally rather than by counting writes:
    /// the file's `saved_at` is set on each write, and a single timestamp
    /// is shared by all three events that landed inside one debounce
    /// window. If the writer woke up three times we'd see the third
    /// timestamp only — but we'd never see all three agents at the same
    /// `saved_at` instant unless the wakeups were collapsed.
    #[tokio::test]
    async fn snapshotter_coalesces_burst_into_single_write() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.json");
        let store = Store::shared();
        let dirty = store.dirty();

        let (tx, rx) = broadcast::channel::<()>(1);
        let snapshotter = Snapshotter::new(
            store.clone(),
            dirty,
            SnapshotterOptions {
                path: path.clone(),
                // Long enough that all three apply() calls land before
                // the writer wakes — that's the burst we want to coalesce.
                debounce: Duration::from_millis(80),
            },
        );
        let task = tokio::spawn(snapshotter.run(rx));

        let t = datetime!(2026-04-28 00:00 UTC);
        store.apply(&started_event("a", "%1", t)).await;
        store.apply(&started_event("b", "%2", t)).await;
        store.apply(&started_event("c", "%3", t)).await;

        // Wait long enough for the debounce + write to complete, but not
        // so long that a hypothetical second wakeup could fire (no fresh
        // notify_one was emitted after the first wakeup, so there
        // shouldn't be one — but if there were, we'd see a fresh
        // saved_at on a second pass).
        tokio::time::sleep(Duration::from_millis(250)).await;

        // Read the raw file so we can inspect saved_at, not just agents.
        let bytes = tokio::fs::read(&path).await.unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let agents = parsed["agents"].as_array().unwrap();
        assert_eq!(
            agents.len(),
            3,
            "coalesced write must contain every event's effect",
        );
        assert!(
            parsed["saved_at"].is_string(),
            "saved_at must be present as RFC 3339 string",
        );

        let _ = tx.send(());
        let _ = tokio::time::timeout(Duration::from_millis(500), task).await;
    }

    /// Final-flush invariant: a shutdown that arrives between debounce
    /// windows still produces a snapshot containing the post-shutdown
    /// registry — without it, the very last transitions before SIGTERM
    /// would be lost on every clean restart.
    #[tokio::test]
    async fn snapshotter_final_flush_on_shutdown() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("state.json");
        let store = Store::shared();
        let dirty = store.dirty();

        let (tx, rx) = broadcast::channel::<()>(1);
        let snapshotter = Snapshotter::new(
            store.clone(),
            dirty,
            SnapshotterOptions {
                path: path.clone(),
                // Unrealistically long debounce so the only way the file
                // appears is via the shutdown final-flush branch.
                debounce: Duration::from_secs(60),
            },
        );
        let task = tokio::spawn(snapshotter.run(rx));

        // Apply, then shut down before the debounce window can fire.
        store
            .apply(&started_event("a", "%1", datetime!(2026-04-28 00:00 UTC)))
            .await;
        // Give the snapshotter a moment to enter its sleep.
        tokio::time::sleep(Duration::from_millis(10)).await;
        let _ = tx.send(());
        let _ = tokio::time::timeout(Duration::from_millis(500), task).await;

        let loaded = load(&path).await;
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].session_id, "a");
    }

    /// `Store::hydrate` slurps a snapshot back into a fresh store — the
    /// daemon-startup path. Covers the registry side of a restart.
    #[tokio::test]
    async fn hydrate_repopulates_store() {
        let store_a = Store::shared();
        store_a
            .apply(&started_event("a", "%1", datetime!(2026-04-28 00:00 UTC)))
            .await;
        let snap = store_a.snapshot().await;

        let store_b = Store::shared();
        assert!(store_b.snapshot().await.is_empty());
        store_b.hydrate(snap).await;
        let after = store_b.snapshot().await;
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].session_id, "a");
    }
}
