//! The snapshots directory: listing, deleting, naming, and keeping muxad's
//! automatic snapshots from piling up.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use super::{read_snapshot, Snapshot, SnapshotOrigin};

/// One directory under the snapshots root that holds a `snapshot.json`.
#[derive(Debug)]
pub(super) struct Entry {
    /// The directory name, which is how muxad and the app refer to it.
    pub id: String,
    pub dir: PathBuf,
    /// The parsed snapshot, or why it could not be read — a listing still
    /// shows an unreadable one so it can be deleted.
    pub snapshot: Result<Snapshot, String>,
}

pub(super) fn root() -> Result<PathBuf> {
    muxa::paths::default_snapshot_dir().context("no data directory for snapshots")
}

/// Every snapshot under `root`, newest first. The order is the directory
/// names', the same one `muxa restore` uses to pick "the most recent".
pub(super) fn entries(root: &Path) -> Vec<Entry> {
    let Ok(listing) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut entries: Vec<Entry> = listing
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|dir| dir.join("snapshot.json").is_file())
        .filter_map(|dir| {
            let id = dir.file_name()?.to_str()?.to_owned();
            let snapshot = read_snapshot(&dir).map_err(|error| format!("{error:#}"));
            Some(Entry { id, dir, snapshot })
        })
        .collect();
    entries.sort_by(|a, b| b.id.cmp(&a.id));
    entries
}

/// A fresh directory for a snapshot taken at `stamp`. Two snapshots in the
/// same second — an automatic one landing next to a manual one — would
/// otherwise overwrite each other; a `-N` suffix still sorts after the bare
/// stamp, so "newest" stays right.
pub(super) fn unique_dir(root: &Path, stamp: i64) -> PathBuf {
    let first = root.join(stamp.to_string());
    if !first.exists() {
        return first;
    }
    (1..=u32::MAX)
        .map(|n| root.join(format!("{stamp}-{n}")))
        .find(|dir| !dir.exists())
        .expect("an unused suffix")
}

/// Ids name a directory directly under the root and nothing else.
pub(super) fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// Remove one snapshot directory. Refuses anything that is not a snapshot
/// directly under `root`, however the id is spelled.
pub(super) fn delete(root: &Path, id: &str) -> Result<PathBuf> {
    if !valid_id(id) {
        bail!("{id:?} is not a snapshot id");
    }
    let dir = root.join(id);
    if !dir.join("snapshot.json").is_file() {
        bail!("no snapshot {id} under {}", root.display());
    }
    let canonical = dir
        .canonicalize()
        .with_context(|| format!("resolving {}", dir.display()))?;
    let canonical_root = root
        .canonicalize()
        .with_context(|| format!("resolving {}", root.display()))?;
    if canonical.parent() != Some(canonical_root.as_path()) {
        bail!("{} is not directly under {}", dir.display(), root.display());
    }
    std::fs::remove_dir_all(&canonical)
        .with_context(|| format!("removing {}", canonical.display()))?;
    Ok(dir)
}

/// The automatic snapshots beyond the newest `keep`, oldest last. Manual and
/// reload snapshots are someone's deliberate save and are never pruned.
pub(super) fn prunable(entries: &[Entry], keep: usize) -> Vec<&Entry> {
    entries
        .iter()
        .filter(|entry| {
            entry
                .snapshot
                .as_ref()
                .is_ok_and(|snapshot| snapshot.origin == SnapshotOrigin::Auto)
        })
        .skip(keep)
        .collect()
}

/// Whether two snapshots describe the same workspace: the same windows,
/// layouts, panes, directories, commands and agent conversations. When it was
/// taken, why, and which window happened to be focused do not count —
/// switching windows is not a change worth another snapshot.
pub(super) fn same_topology(a: &Snapshot, b: &Snapshot) -> bool {
    let windows = |snapshot: &Snapshot| {
        snapshot
            .windows
            .iter()
            .map(|window| {
                (
                    window.session.clone(),
                    window.index.clone(),
                    window.name.clone(),
                    window.layout.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    a.host == b.host && a.socket == b.socket && windows(a) == windows(b) && a.panes == b.panes
}

/// The newest readable snapshot of the same server as `snapshot`.
pub(super) fn newest_of_server<'a>(entries: &'a [Entry], snapshot: &Snapshot) -> Option<&'a Entry> {
    entries.iter().find(|entry| {
        entry
            .snapshot
            .as_ref()
            .is_ok_and(|other| other.host == snapshot.host && other.socket == snapshot.socket)
    })
}

#[cfg(test)]
mod tests {
    use super::super::{write_snapshot, AgentShape, PaneShape, WindowShape};
    use super::*;

    fn snapshot(origin: SnapshotOrigin) -> Snapshot {
        Snapshot {
            version: 1,
            taken_at: "2026-09-23T05:05:12Z".into(),
            origin,
            host: "tmux".into(),
            socket: "default".into(),
            windows: vec![WindowShape {
                session: "work".into(),
                index: "0".into(),
                name: "main".into(),
                active: true,
                layout: "c3a1,80x24,0,0,5".into(),
            }],
            panes: vec![PaneShape {
                session: "work".into(),
                window_index: "0".into(),
                pane_index: "0".into(),
                path: "/tmp".into(),
                command: Some("claude".into()),
                argv: None,
                replayable: true,
                agent: Some(AgentShape {
                    kind: "claude_code".into(),
                    session_id: "abc".into(),
                }),
            }],
        }
    }

    #[test]
    fn topology_ignores_time_origin_and_focus() {
        let a = snapshot(SnapshotOrigin::Manual);
        let mut b = snapshot(SnapshotOrigin::Auto);
        b.taken_at = "2026-09-23T06:00:00Z".into();
        b.windows[0].active = false;
        assert!(same_topology(&a, &b));

        let mut moved = a.clone();
        moved.panes[0].path = "/elsewhere".into();
        assert!(!same_topology(&a, &moved));
        let mut resized = a.clone();
        resized.windows[0].layout = "d4b2,120x40,0,0,5".into();
        assert!(!same_topology(&a, &resized));
        let mut new_conversation = a.clone();
        new_conversation.panes[0].agent.as_mut().unwrap().session_id = "def".into();
        assert!(!same_topology(&a, &new_conversation));
    }

    #[test]
    fn listing_is_newest_first_and_keeps_unreadable_snapshots() {
        let root = tempfile::tempdir().unwrap();
        for (id, origin) in [
            ("1790000000", SnapshotOrigin::Auto),
            ("1790000900", SnapshotOrigin::Manual),
            ("1790000900-1", SnapshotOrigin::Auto),
        ] {
            write_snapshot(&snapshot(origin), &root.path().join(id)).unwrap();
        }
        let broken = root.path().join("1790000500");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(broken.join("snapshot.json"), "{not json").unwrap();
        std::fs::create_dir_all(root.path().join("not-a-snapshot")).unwrap();

        let entries = entries(root.path());
        let ids: Vec<_> = entries.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(
            ids,
            ["1790000900-1", "1790000900", "1790000500", "1790000000"]
        );
        assert!(entries[2].snapshot.is_err());
    }

    #[test]
    fn only_automatic_snapshots_beyond_the_limit_are_pruned() {
        let root = tempfile::tempdir().unwrap();
        for (id, origin) in [
            ("1000", SnapshotOrigin::Auto),
            ("1001", SnapshotOrigin::Manual),
            ("1002", SnapshotOrigin::Auto),
            ("1003", SnapshotOrigin::Reload),
            ("1004", SnapshotOrigin::Auto),
        ] {
            write_snapshot(&snapshot(origin), &root.path().join(id)).unwrap();
        }
        let entries = entries(root.path());
        let pruned: Vec<_> = prunable(&entries, 2)
            .into_iter()
            .map(|entry| entry.id.as_str())
            .collect();
        assert_eq!(pruned, ["1000"]);
        assert!(prunable(&entries, 10).is_empty());
    }

    #[test]
    fn a_second_snapshot_in_the_same_second_gets_its_own_directory() {
        let root = tempfile::tempdir().unwrap();
        let first = unique_dir(root.path(), 1_790_000_000);
        assert_eq!(first, root.path().join("1790000000"));
        std::fs::create_dir_all(&first).unwrap();
        assert_eq!(
            unique_dir(root.path(), 1_790_000_000),
            root.path().join("1790000000-1")
        );
    }

    #[test]
    fn delete_refuses_anything_but_a_snapshot_under_the_root() {
        let root = tempfile::tempdir().unwrap();
        write_snapshot(&snapshot(SnapshotOrigin::Manual), &root.path().join("1000")).unwrap();
        for id in ["", "..", "../etc", ".hidden", "a/b", "missing"] {
            assert!(delete(root.path(), id).is_err(), "{id:?} must be refused");
        }
        delete(root.path(), "1000").unwrap();
        assert!(!root.path().join("1000").exists());
    }

    #[test]
    fn an_old_snapshot_reads_as_manual() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("1000");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("snapshot.json"),
            r#"{"version":1,"taken_at":"t","host":"tmux","socket":"default","windows":[],"panes":[]}"#,
        )
        .unwrap();
        let snapshot = read_snapshot(&dir).unwrap();
        assert_eq!(snapshot.origin, SnapshotOrigin::Manual);
    }
}
