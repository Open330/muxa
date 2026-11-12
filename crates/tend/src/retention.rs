//! Retention for directories under the data dir that otherwise only grow.
//!
//! Every policy here is documented and configurable (`[storage]`), and a
//! zero setting turns it off. Nothing here is the only copy of user data:
//! `backups/` holds copies of earlier installs taken before replacing them.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Delete all but the newest `keep` entries of `directory`, newest by
/// modification time. Hidden entries (`.name`) are left alone, symlinks are
/// removed as links and never followed. `keep == 0` disables pruning.
/// Returns the paths removed.
pub fn prune_backups(directory: &Path, keep: usize) -> std::io::Result<Vec<PathBuf>> {
    if keep == 0 {
        return Ok(Vec::new());
    }
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        candidates.push((modified, entry.file_name(), entry.path(), metadata));
    }
    if candidates.len() <= keep {
        return Ok(Vec::new());
    }
    // Newest first; the name breaks ties so the order is deterministic.
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut removed = Vec::new();
    for (_, _, path, metadata) in candidates.into_iter().skip(keep) {
        let result = if metadata.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => removed.push(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "backup retention could not remove an entry");
            }
        }
    }
    Ok(removed)
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

    // Setting a directory's mtime through `File` needs Unix semantics.
    #[cfg(unix)]
    #[test]
    fn keeps_the_newest_entries_and_skips_hidden_ones() {
        let dir = tempfile::tempdir().unwrap();
        for (index, name) in ["old", "mid", "new"].iter().enumerate() {
            let entry = dir.path().join(name);
            std::fs::create_dir(&entry).unwrap();
            let file = entry.join("payload");
            std::fs::write(&file, b"x").unwrap();
            let age = Duration::from_secs(((3 - index) * 3600) as u64);
            filetime_dir(&entry, age);
        }
        std::fs::write(dir.path().join("loose.bak"), b"x").unwrap();
        touch(
            &dir.path().join("loose.bak"),
            Duration::from_secs(10 * 3600),
        );
        std::fs::write(dir.path().join(".keep"), b"x").unwrap();
        touch(&dir.path().join(".keep"), Duration::from_secs(99 * 3600));

        let removed = prune_backups(dir.path(), 2).unwrap();
        let mut names: Vec<_> = removed
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["loose.bak", "old"]);
        assert!(dir.path().join("mid").exists());
        assert!(dir.path().join("new").exists());
        assert!(dir.path().join(".keep").exists());
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
    fn filetime_dir(path: &Path, age: Duration) {
        let at = SystemTime::now() - age;
        let dir = std::fs::File::open(path).unwrap();
        dir.set_modified(at).unwrap();
    }
}
