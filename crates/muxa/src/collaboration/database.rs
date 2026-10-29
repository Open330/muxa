//! One initialized connection per store; a changed pathname invalidates it.
use super::{secure_database_files, CollaborationError};
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Mutex,
};

pub(super) struct DurableDatabase {
    pub(super) path: PathBuf,
    // Keep the original file alive so its inode cannot be recycled under us.
    file: std::fs::File,
    connection: Mutex<Connection>,
    invalid: AtomicBool,
}

impl DurableDatabase {
    pub(super) fn initialize(path: &Path) -> Result<Self, CollaborationError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        super::prepare_database_file(path)?;
        let file = std::fs::File::open(path)?;
        // Pin before initialization or loading: startup replacement must not
        // couple a projection read from one file to a connection to another.
        let connection = super::open_database(path)?;
        let database = Self {
            path: path.to_path_buf(),
            file,
            connection: Mutex::new(connection),
            invalid: AtomicBool::new(false),
        };
        database.verify()?;
        secure_database_files(path)?;
        Ok(database)
    }

    pub(super) fn verify(&self) -> Result<(), CollaborationError> {
        if self.invalid.load(Ordering::Acquire) {
            return Err(self.unavailable());
        }
        let same = self.file.metadata().and_then(|original| {
            std::fs::metadata(&self.path).map(|current| same_file(&original, &current))
        });
        if !matches!(same, Ok(true)) {
            // Do not silently recover if the pathname later comes back. The
            // in-memory projection must be reloaded before using another file.
            self.invalid.store(true, Ordering::Release);
            return Err(self.unavailable());
        }
        Ok(())
    }

    fn unavailable(&self) -> CollaborationError {
        CollaborationError::Unavailable(format!(
            "mailbox database disappeared or was replaced: {}",
            self.path.display()
        ))
    }

    pub(super) fn with_connection<T>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T, CollaborationError>,
    ) -> Result<T, CollaborationError> {
        let mut connection = self.connection.lock().map_err(|_| {
            CollaborationError::Unavailable("mailbox database lock poisoned".into())
        })?;
        self.verify()?;
        // A permission failure must precede a commit, otherwise the caller
        // could skip publishing memory even though the database changed.
        secure_database_files(&self.path)?;
        let result = operation(&mut connection);
        // A replacement racing an operation must never be published to memory
        // as a successful durable mutation. External file changes are not an
        // atomic participant in SQLite's transaction protocol.
        self.verify()?;
        result
    }
}

#[cfg(unix)]
fn same_file(original: &std::fs::Metadata, current: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    original.dev() == current.dev() && original.ino() == current.ino()
}

#[cfg(not(unix))]
fn same_file(original: &std::fs::Metadata, current: &std::fs::Metadata) -> bool {
    // Non-Unix platforms must provide a stable creation timestamp. Fail closed
    // rather than assume an unverifiable file is the initialized database.
    matches!((original.created(), current.created()), (Ok(a), Ok(b)) if a == b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disappearing_path_during_an_operation_is_not_reported_as_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mailbox.sqlite3");
        drop(super::super::open_database(&path).unwrap());
        let database = DurableDatabase::initialize(&path).unwrap();
        let result = database.with_connection(|connection| {
            connection.execute(
                "INSERT INTO collaboration_metadata(key,value) VALUES ('probe','1')",
                [],
            )?;
            std::fs::rename(&path, dir.path().join("moved.sqlite3"))?;
            Ok(())
        });
        assert!(matches!(result, Err(CollaborationError::Unavailable(_))));
        assert!(!path.exists());
        assert!(database.with_connection(|_| Ok(())).is_err());
    }
}
