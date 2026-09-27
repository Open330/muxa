//! Durable enrolled operators: OIDC accounts that a holder of the dashboard
//! token linked as operators. Only `(issuer, subject)` authorizes; the email
//! is kept for display. Sessions are never written to disk.
use super::{Enrolled, MAX_ENROLLED};
use rusqlite::{params, Connection};
use std::{fs::File, io, path::Path, sync::Mutex, time::Duration};

/// `MXOP`: refuses to adopt a database written by anything else.
const APPLICATION_ID: i64 = 0x4D58_4F50;

pub(super) struct Storage {
    connection: Mutex<Connection>,
    lock: File,
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
        || (application != 0 && (application != APPLICATION_ID || version != 1))
    {
        return Err(io::Error::other("not a supported muxa operator database"));
    }
    Ok(())
}

impl Storage {
    /// Open (creating if needed) the store and load every entry. Holding an
    /// exclusive lease on a sibling lock file keeps a second daemon from
    /// racing this one's view of who is enrolled.
    pub(super) fn open(path: &Path) -> io::Result<(Self, Vec<Enrolled>)> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
        let private_file = crate::dashboard::sharing::private_file;
        let lock = private_file(&path.with_extension("lock"))?;
        lock.try_lock()
            .map_err(|error| io::Error::other(format!("cannot lock operator storage: {error}")))?;
        private_file(path)?;
        let connection = Connection::open(path).map_err(io::Error::other)?;
        validate_database(&connection)?;
        connection
            .busy_timeout(Duration::from_secs(2))
            .map_err(io::Error::other)?;
        connection
            .execute_batch(
                "PRAGMA synchronous=FULL; PRAGMA application_id=0x4D584F50; PRAGMA user_version=1; \
                 CREATE TABLE IF NOT EXISTS operators (id TEXT PRIMARY KEY, issuer TEXT NOT NULL, \
                 subject TEXT NOT NULL, email TEXT, created_at INTEGER NOT NULL, \
                 last_seen_at INTEGER NOT NULL, UNIQUE (issuer, subject));",
            )
            .map_err(io::Error::other)?;
        let entries = {
            let mut statement = connection
                .prepare(
                    "SELECT id, issuer, subject, email, created_at, last_seen_at FROM operators \
                     ORDER BY created_at, id",
                )
                .map_err(io::Error::other)?;
            let rows = statement
                .query_map([], |row| {
                    Ok(Enrolled {
                        id: row.get(0)?,
                        issuer: row.get(1)?,
                        subject: row.get(2)?,
                        email: row.get(3)?,
                        created_at: row.get(4)?,
                        last_seen_at: row.get(5)?,
                    })
                })
                .map_err(io::Error::other)?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(io::Error::other)?
        };
        if entries.len() > MAX_ENROLLED {
            return Err(io::Error::other(
                "operator storage exceeds supported limits",
            ));
        }
        Ok((
            Self {
                connection: Mutex::new(connection),
                lock,
            },
            entries,
        ))
    }

    fn connection(&self) -> io::Result<std::sync::MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| io::Error::other("operator storage unavailable"))
    }

    pub(super) fn insert(&self, entry: &Enrolled) -> io::Result<()> {
        self.connection()?
            .execute(
                "INSERT INTO operators VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    entry.id,
                    entry.issuer,
                    entry.subject,
                    entry.email,
                    entry.created_at,
                    entry.last_seen_at
                ],
            )
            .map(drop)
            .map_err(io::Error::other)
    }

    pub(super) fn touch(&self, id: &str, email: Option<&str>, at: i64) -> io::Result<()> {
        self.connection()?
            .execute(
                "UPDATE operators SET last_seen_at = ?2, email = COALESCE(?3, email) WHERE id = ?1",
                params![id, at, email],
            )
            .map(drop)
            .map_err(io::Error::other)
    }

    pub(super) fn remove(&self, id: &str) -> io::Result<()> {
        self.connection()?
            .execute("DELETE FROM operators WHERE id = ?1", [id])
            .map(drop)
            .map_err(io::Error::other)
    }
}

impl Drop for Storage {
    fn drop(&mut self) {
        // Release the lease explicitly; see the sharing store.
        let _ = self.lock.unlock();
    }
}
