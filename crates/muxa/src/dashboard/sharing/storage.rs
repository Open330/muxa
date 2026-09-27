//! Security decisions are committed before acknowledging them. Session cookies,
//! terminal output and prompt bodies are deliberately never written to disk.
use super::{Delivery, Grant, Permission, Registry, Sharing, SharingConfig, Target};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::File,
    io,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[cfg(test)]
type WriteGate = Arc<(std::sync::Barrier, std::sync::Barrier)>;

pub(super) struct Storage {
    connection: Mutex<Connection>,
    lock: File,
    #[cfg(test)]
    write_gate: Mutex<Option<WriteGate>>,
}

#[derive(Serialize, Deserialize)]
struct Record {
    id: String,
    target: Target,
    #[serde(default)]
    window: bool,
    #[serde(default)]
    peers: Vec<Target>,
    email: String,
    subject: Option<String>,
    permission: Permission,
    expires_at: i64,
    revoked: bool,
    #[serde(default)]
    deliveries: HashMap<String, Delivery>,
}
impl Record {
    fn from_grant(g: &Grant) -> Self {
        Self {
            id: g.id.clone(),
            target: g.target.clone(),
            window: g.window,
            peers: g.peers.clone(),
            email: g.email.clone(),
            subject: g.subject.clone(),
            permission: g.permission,
            expires_at: g.expires_at,
            revoked: g.revoked,
            deliveries: g.deliveries.clone(),
        }
    }
    fn into_grant(self) -> Grant {
        let remaining = self
            .expires_at
            .saturating_sub(time::OffsetDateTime::now_utc().unix_timestamp())
            .clamp(0, 86400);
        Grant {
            id: self.id,
            target: self.target,
            window: self.window,
            peers: self.peers,
            email: self.email,
            subject: self.subject,
            permission: self.permission,
            expires_at: self.expires_at,
            expires: Instant::now() + Duration::from_secs(remaining.unsigned_abs()),
            revoked: self.revoked,
            last_prompt: None,
            dirty: false,
            capture: None,
            deliveries: self.deliveries,
        }
    }
}

fn private_file(path: &Path) -> io::Result<File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(io::Error::other("sharing storage must not be a symlink"));
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

fn validate_database(connection: &Connection) -> io::Result<()> {
    let application: i64 = connection
        .query_row("PRAGMA application_id", [], |row| row.get(0))
        .map_err(io::Error::other)?;
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .map_err(io::Error::other)?;
    let tables: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table'",
            [],
            |row| row.get(0),
        )
        .map_err(io::Error::other)?;
    if (application == 0 && tables != 0)
        || (application != 0 && (application != 0x4D58_5348 || version != 1))
    {
        return Err(io::Error::other("not a supported muxa sharing database"));
    }
    Ok(())
}

impl Storage {
    fn open(path: &Path, config: &SharingConfig) -> io::Result<(Arc<Self>, Registry)> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        let lock = private_file(&path.with_extension("lock"))?;
        lock.try_lock()
            .map_err(|error| io::Error::other(format!("cannot lock sharing storage: {error}")))?;
        private_file(path)?;
        let mut connection = Connection::open(path).map_err(io::Error::other)?;
        validate_database(&connection)?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(io::Error::other)?;
        connection.execute_batch("PRAGMA synchronous=FULL; PRAGMA application_id=0x4D585348; PRAGMA user_version=1; CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL); CREATE TABLE IF NOT EXISTS grants (id TEXT PRIMARY KEY, expires_at INTEGER NOT NULL, record TEXT NOT NULL);").map_err(io::Error::other)?;
        let identity =
            serde_json::to_string(&(1, config.origin(), &config.issuer_url, &config.client_id))
                .map_err(io::Error::other)?;
        let transaction = connection.transaction().map_err(io::Error::other)?;
        let stored: Option<String> = transaction
            .query_row(
                "SELECT value FROM settings WHERE key='identity'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(io::Error::other)?;
        if stored.as_ref().is_some_and(|value| value != &identity) {
            return Err(io::Error::other("sharing identity/configuration changed; use a separate storage_path or restore the original configuration"));
        }
        transaction
            .execute(
                "INSERT OR IGNORE INTO settings VALUES ('identity', ?1)",
                [&identity],
            )
            .map_err(io::Error::other)?;
        transaction
            .execute(
                "DELETE FROM grants WHERE expires_at <= ?1",
                [time::OffsetDateTime::now_utc().unix_timestamp()],
            )
            .map_err(io::Error::other)?;
        transaction.commit().map_err(io::Error::other)?;
        let mut registry = Registry::default();
        {
            let mut statement = connection
                .prepare("SELECT record FROM grants")
                .map_err(io::Error::other)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(io::Error::other)?;
            for row in rows {
                let record: Record = serde_json::from_str(&row.map_err(io::Error::other)?)
                    .map_err(io::Error::other)?;
                if registry.grants.len() >= super::MAX_GRANTS
                    || record.deliveries.len() > 1024
                    || record.peers.len() > 15
                {
                    return Err(io::Error::other("sharing storage exceeds supported limits"));
                }
                let grant = record.into_grant();
                registry
                    .grants
                    .insert(grant.id.clone(), Arc::new(tokio::sync::Mutex::new(grant)));
            }
        }
        Ok((
            Arc::new(Self {
                connection: Mutex::new(connection),
                lock,
                #[cfg(test)]
                write_gate: Mutex::new(None),
            }),
            registry,
        ))
    }

    pub(super) fn save(&self, grant: &Grant) -> io::Result<()> {
        #[cfg(test)]
        {
            let gate = self.write_gate.lock().unwrap().take();
            if let Some(gate) = gate {
                gate.0.wait();
                gate.1.wait();
            }
        }

        let record = serde_json::to_string(&Record::from_grant(grant)).map_err(io::Error::other)?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| io::Error::other("sharing storage unavailable"))?;
        let transaction = connection.transaction().map_err(io::Error::other)?;
        transaction
            .execute(
                "DELETE FROM grants WHERE expires_at <= ?1",
                [time::OffsetDateTime::now_utc().unix_timestamp()],
            )
            .map_err(io::Error::other)?;
        transaction.execute("INSERT INTO grants VALUES (?1, ?2, ?3) ON CONFLICT(id) DO UPDATE SET expires_at=excluded.expires_at, record=excluded.record", params![grant.id, grant.expires_at, record]).map_err(io::Error::other)?;
        transaction.commit().map_err(io::Error::other)
    }
}

impl Sharing {
    pub(in crate::dashboard) async fn open(config: Option<SharingConfig>) -> io::Result<Arc<Self>> {
        let Some(config) = config else {
            return Ok(Self::new(None));
        };
        let path = config
            .storage_path
            .clone()
            .or_else(|| dirs::data_dir().map(|d| d.join("muxa/dashboard-sharing/shares.sqlite3")))
            .ok_or_else(|| {
                io::Error::other("sharing storage_path is required without an XDG data directory")
            })?;
        let config_for_open = config.clone();
        let (storage, registry) =
            tokio::task::spawn_blocking(move || Storage::open(&path, &config_for_open))
                .await
                .map_err(io::Error::other)??;
        let mut sharing = Self::new(Some(config));
        let inner = Arc::get_mut(&mut sharing).expect("new sharing state");
        inner.storage = Some(storage);
        inner.registry = tokio::sync::Mutex::new(registry);
        Ok(sharing)
    }

    pub(super) async fn persist(
        &self,
        mut grant: tokio::sync::OwnedMutexGuard<Grant>,
    ) -> Result<tokio::sync::OwnedMutexGuard<Grant>, super::routes::Failure> {
        let Some(storage) = self.storage.clone() else {
            grant.dirty = false;
            return Ok(grant);
        };
        tokio::task::spawn_blocking(move || {
            storage.save(&grant).map_err(|error| {
                tracing::error!(%error, "sharing storage write failed");
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    "sharing storage unavailable; change was not confirmed",
                )
            })?;
            grant.dirty = false;
            drop(storage);
            Ok(grant)
        })
        .await
        .map_err(|_| {
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "sharing storage unavailable",
            )
        })?
    }
}

#[cfg(test)]
impl Storage {
    pub(super) fn test_block_next_write(&self) -> WriteGate {
        let gate = Arc::new((std::sync::Barrier::new(2), std::sync::Barrier::new(2)));
        *self.write_gate.lock().unwrap() = Some(gate.clone());
        gate
    }
    pub(super) fn test_read_only(&self, read_only: bool) {
        self.connection
            .lock()
            .unwrap()
            .pragma_update(None, "query_only", read_only)
            .unwrap();
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        // Explicitly release the lease: a concurrent fork may temporarily
        // inherit this descriptor before its close-on-exec takes effect.
        let _ = self.lock.unlock();
    }
}
