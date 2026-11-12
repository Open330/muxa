//! Retention for directories under the data dir that otherwise only grow.
//!
//! Every policy here is documented and configurable (`[storage]`), and a
//! zero setting turns it off. Nothing here is the only copy of user data:
//! `backups/` holds copies of earlier installs taken before replacing them.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Delete all but the newest `keep` entries of `directory`, newest by
/// creation time (modification time where the filesystem records no
/// creation time). Creation time is the stable key: deleting a directory's
/// contents updates its modification time, so with mtime one pruner
/// halfway through removing an old entry made it look newest to another
/// running at the same moment, and that one deleted a backup it should have
/// kept.
///
/// Safety does not rest on that key alone: selection through deletion holds
/// an exclusive lock on `<directory>/.prune.lock` (a pruner that finds it
/// held skips this pass), and each victim is first renamed to a hidden
/// `.removing-<name>` so no other reader can mistake a half-deleted entry
/// for a live backup. A `.removing-*` left by an interrupted pass is
/// finished on the next one. Entries that vanish meanwhile are skipped.
/// Hidden entries (`.name`) are left alone, symlinks are
/// removed as links and never followed. `keep == 0` disables pruning, and
/// so does a `directory` that is itself a symlink or not a directory: the
/// sweep never reaches outside the data directory through a link.
/// Returns the paths removed.
pub fn prune_backups(directory: &Path, keep: usize) -> std::io::Result<Vec<PathBuf>> {
    if keep == 0 {
        return Ok(Vec::new());
    }
    match std::fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            tracing::warn!(path = %directory.display(), "backup retention skipped: not a real directory");
            return Ok(Vec::new());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    }
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(directory.join(PRUNE_LOCK))?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            tracing::debug!(path = %directory.display(), "backup retention skipped: another pruner is running");
            return Ok(Vec::new());
        }
        Err(std::fs::TryLockError::Error(error)) => return Err(error),
    }
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with(QUARANTINE_PREFIX) {
            // An earlier pass chose it and was interrupted.
            remove_entry(&entry.path());
            continue;
        }
        if name.starts_with('.') {
            continue;
        }
        let metadata = match std::fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            // Another pruner removed it first.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        candidates.push((
            age_key(&metadata),
            entry.file_name(),
            entry.path(),
            metadata,
        ));
    }
    if candidates.len() <= keep {
        return Ok(Vec::new());
    }
    // Newest first; the name breaks ties so the order is deterministic.
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut removed = Vec::new();
    for (_, name, path, _) in candidates.into_iter().skip(keep) {
        let mut hidden = std::ffi::OsString::from(QUARANTINE_PREFIX);
        hidden.push(&name);
        let quarantined = directory.join(hidden);
        match std::fs::rename(&path, &quarantined) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "backup retention could not remove an entry");
                continue;
            }
        }
        remove_entry(&quarantined);
        removed.push(path);
    }
    drop(lock);
    Ok(removed)
}

const PRUNE_LOCK: &str = ".prune.lock";
const QUARANTINE_PREFIX: &str = ".removing-";

/// Remove a quarantined entry; a link is removed as a link.
fn remove_entry(path: &Path) {
    let result = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(_) => return,
    };
    if let Err(error) = result {
        if std::fs::symlink_metadata(path).is_ok() {
            tracing::warn!(%error, path = %path.display(), "backup retention could not finish removing an entry");
        }
    }
}

/// When an entry was made: its creation time, else its modification time.
fn age_key(metadata: &std::fs::Metadata) -> SystemTime {
    metadata
        .created()
        .or_else(|_| metadata.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn touch(path: &Path, age: Duration) {
        let at = SystemTime::now() - age;
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_modified(at).unwrap();
    }

    /// Create entries oldest first, far enough apart for any filesystem's
    /// creation-time resolution.
    fn create_in_order(dir: &Path, names: &[&str]) {
        for name in names {
            let entry = dir.join(name);
            if name.contains('.') {
                std::fs::write(&entry, b"x").unwrap();
            } else {
                std::fs::create_dir(&entry).unwrap();
                std::fs::write(entry.join("payload"), b"x").unwrap();
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    fn names(paths: &[PathBuf]) -> Vec<String> {
        let mut names: Vec<_> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn keeps_the_newest_entries_and_skips_hidden_ones() {
        let dir = tempfile::tempdir().unwrap();
        create_in_order(dir.path(), &[".keep", "loose.bak", "old", "mid", "new"]);
        let removed = prune_backups(dir.path(), 2).unwrap();
        assert_eq!(names(&removed), ["loose.bak", "old"]);
        assert!(dir.path().join("mid").exists());
        assert!(dir.path().join("new").exists());
        assert!(dir.path().join(".keep").exists());
    }

    #[test]
    fn a_recent_modification_does_not_make_an_old_backup_new() {
        let dir = tempfile::tempdir().unwrap();
        create_in_order(dir.path(), &["first", "second", "third"]);
        // What a concurrent pruner's partial `remove_dir_all` does to it.
        std::fs::remove_file(dir.path().join("first").join("payload")).unwrap();
        touch(&dir.path().join("second").join("payload"), Duration::ZERO);
        let removed = prune_backups(dir.path(), 2).unwrap();
        // Only meaningful where the filesystem records creation time.
        if std::fs::metadata(dir.path())
            .and_then(|m| m.created())
            .is_ok()
        {
            assert_eq!(names(&removed), ["first"]);
        } else {
            assert_eq!(removed.len(), 1);
        }
    }

    #[test]
    fn concurrent_pruners_keep_exactly_the_newest_entries() {
        let dir = tempfile::tempdir().unwrap();
        let entries: Vec<String> = (0..12).map(|i| format!("backup-{i:02}")).collect();
        let refs: Vec<&str> = entries.iter().map(String::as_str).collect();
        create_in_order(dir.path(), &refs);
        let root = std::sync::Arc::new(dir.path().to_path_buf());
        let threads: Vec<_> = (0..5)
            .map(|_| {
                let root = root.clone();
                std::thread::spawn(move || prune_backups(&root, 5).unwrap())
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let mut left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| !name.starts_with('.'))
            .collect();
        left.sort();
        assert_eq!(left, refs[7..]);
    }

    #[test]
    fn zero_keep_and_missing_directory_do_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), b"x").unwrap();
        assert!(prune_backups(dir.path(), 0).unwrap().is_empty());
        assert!(dir.path().join("a").exists());
        assert!(prune_backups(&dir.path().join("missing"), 1)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_held_prune_lock_skips_the_pass() {
        let dir = tempfile::tempdir().unwrap();
        create_in_order(dir.path(), &["a", "b", "c"]);
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.path().join(PRUNE_LOCK))
            .unwrap();
        lock.lock().unwrap();
        assert!(prune_backups(dir.path(), 1).unwrap().is_empty());
        drop(lock);
        assert_eq!(names(&prune_backups(dir.path(), 1).unwrap()), ["a", "b"]);
    }

    #[test]
    fn a_half_deleted_entry_is_hidden_and_finished_by_the_next_pass() {
        let dir = tempfile::tempdir().unwrap();
        create_in_order(dir.path(), &["mid", "new"]);
        // An interrupted pass left its quarantined victim, half-emptied and
        // with a fresh mtime: it is neither counted nor kept.
        let leftover = dir.path().join(format!("{QUARANTINE_PREFIX}old"));
        std::fs::create_dir(&leftover).unwrap();
        std::fs::write(leftover.join("rest"), b"x").unwrap();
        assert!(prune_backups(dir.path(), 2).unwrap().is_empty());
        assert!(!leftover.exists());
        assert!(dir.path().join("mid").exists() && dir.path().join("new").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_removed_without_following_them() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("precious"), b"x").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        std::fs::write(dir.path().join("newer"), b"x").unwrap();
        let removed = prune_backups(dir.path(), 1).unwrap();
        assert_eq!(removed.len(), 1);
        assert!(outside.path().join("precious").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_backups_root_is_never_swept() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        for index in 0..4 {
            std::fs::write(outside.path().join(format!("user-{index}")), b"x").unwrap();
        }
        let root = dir.path().join("backups");
        std::os::unix::fs::symlink(outside.path(), &root).unwrap();
        assert!(prune_backups(&root, 1).unwrap().is_empty());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 4);
        // A plain file at the root is skipped too.
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        assert!(prune_backups(&file, 1).unwrap().is_empty());
    }
}
