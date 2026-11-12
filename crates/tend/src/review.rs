//! Review items: explicit, durable requests that a specific result be looked
//! at. See `docs/proposals/REVIEW_DEBT.md`.
//!
//! An item is created only by a claim (someone asked for review), never by
//! an observation such as a turn stopping, a process exiting or a branch
//! being merged. It stays open until someone records a disposition. tend
//! records *declared* actors; local IPC does not authenticate a human, so a
//! disposition is never proof that a person understood the result.
//!
//! Items outlive their targets: a closed pane, a vanished session or a
//! deleted transcript does not retire an item.

use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

/// Advertised by daemons that serve the `review_*` requests.
pub const REVIEW_CAPABILITY: &str = "review_items_v1";
/// The daemon records a claim and its disposition together
/// ([`ReviewStore::record`]).
pub const REVIEW_RECORD_CAPABILITY: &str = "review_record_v1";

const SCHEMA_VERSION: i64 = 1;
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS review_items (
    id TEXT PRIMARY KEY,
    target_key TEXT NOT NULL,
    generation INTEGER NOT NULL,
    idempotency_key TEXT UNIQUE,
    requested_at_ns INTEGER NOT NULL,
    disposed_at_ns INTEGER,
    body TEXT NOT NULL,
    UNIQUE (target_key, generation)
);
CREATE INDEX IF NOT EXISTS review_items_open
    ON review_items (disposed_at_ns, requested_at_ns);
CREATE TABLE IF NOT EXISTS review_events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    item_id TEXT NOT NULL,
    at_ns INTEGER NOT NULL,
    event TEXT NOT NULL,
    actor_kind TEXT NOT NULL
);
";

const MAX_SUMMARY_BYTES: usize = 1024;
const MAX_ANCHOR_BYTES: usize = 2048;
const MAX_ID_BYTES: usize = 256;
const MAX_KEY_BYTES: usize = 128;
const MAX_LABEL_BYTES: usize = 128;
const MAX_NOTE_BYTES: usize = 1024;
/// Upper bound on one `review_list` page.
pub const MAX_LIST_LIMIT: usize = 500;

/// What the review is about. A descriptor recorded at claim time, not a live
/// handle: tend never resolves or retires an item through it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReviewTarget {
    /// An agent session, for adopted agents with no Work.
    Session {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        session_id: String,
    },
    /// A managed Work, optionally one pipeline alias and generation.
    Work {
        workspace: String,
        work: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alias: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pipeline_generation: Option<u64>,
    },
    /// A collaboration request's result.
    Request {
        request_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attempt_id: Option<String>,
    },
}

impl ReviewTarget {
    /// Items for the same key share one generation sequence. A JSON tuple,
    /// so no component value can make two targets collide.
    fn key(&self) -> String {
        let parts: (&str, Option<&str>, Option<&str>, Option<&str>) = match self {
            Self::Session {
                provider,
                session_id,
            } => ("session", provider.as_deref(), Some(session_id), None),
            Self::Work {
                workspace,
                work,
                alias,
                ..
            } => ("work", Some(workspace), Some(work), alias.as_deref()),
            Self::Request { request_id, .. } => ("request", Some(request_id), None, None),
        };
        serde_json::to_string(&parts).unwrap_or_default()
    }

    fn validate(&self) -> Result<(), ReviewError> {
        let required = |name: &str, value: &str| bounded(name, value, MAX_ID_BYTES, Text::Line);
        let optional = |name: &str, value: Option<&String>| {
            value.map_or(Ok(()), |v| bounded(name, v, MAX_ID_BYTES, Text::Line))
        };
        match self {
            Self::Session {
                provider,
                session_id,
            } => {
                optional("provider", provider.as_ref())?;
                required("session_id", session_id)
            }
            Self::Work {
                workspace,
                work,
                alias,
                ..
            } => {
                required("workspace", workspace)?;
                required("work", work)?;
                optional("alias", alias.as_ref())
            }
            Self::Request {
                request_id,
                attempt_id,
            } => {
                required("request_id", request_id)?;
                optional("attempt_id", attempt_id.as_ref())
            }
        }
    }
}

/// Who declared a claim or disposition. Declared, not authenticated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    User,
    Agent,
    /// A configured policy acting on a claim. Not accepted from clients
    /// until a policy exists to apply it.
    Policy,
}

impl ActorKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Policy => "policy",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewActor {
    pub kind: ActorKind,
    /// Free-form display label (an agent name, a pane). Not an identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

impl ReviewActor {
    fn validate_client(&self) -> Result<(), ReviewError> {
        if self.kind == ActorKind::Policy {
            return Err(ReviewError::Invalid(
                "actor kind `policy` is reserved for configured policies".into(),
            ));
        }
        if let Some(label) = &self.label {
            bounded("actor label", label, MAX_LABEL_BYTES, Text::Line)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DispositionKind {
    /// Looked at and accepted by the declared actor.
    Accepted,
    /// Sent back for more work. Not a successful review.
    Returned,
    /// No longer needs review (asked by mistake, replaced elsewhere).
    Withdrawn,
}

impl DispositionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Returned => "returned",
            Self::Withdrawn => "withdrawn",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewDisposition {
    pub kind: DispositionKind,
    pub by: ReviewActor,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewItem {
    pub id: String,
    pub target: ReviewTarget,
    /// 1-based, per target. A retry with the same idempotency key returns
    /// the existing item; every new claim takes the next generation.
    pub generation: u64,
    /// What to inspect, in the claimer's words.
    pub summary: String,
    /// Optional caller-supplied pointer to the evidence (a commit, a log
    /// path, a transcript locator). Recorded, never opened.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(with = "time::serde::rfc3339")]
    pub requested_at: OffsetDateTime,
    pub requested_by: ReviewActor,
    /// What the claimer wants decided, the options, their recommendation
    /// and the evidence (`review_decision_v1`). Declared, never verified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<crate::decision::Decision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<ReviewDisposition>,
}

impl ReviewItem {
    #[must_use]
    pub fn is_open(&self) -> bool {
        self.disposition.is_none()
    }
}

/// A claim that a result needs review.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewReview {
    pub target: ReviewTarget,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anchor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub requested_by: ReviewActor,
    /// Needs a daemon advertising
    /// [`crate::decision::REVIEW_DECISION_CAPABILITY`]; older ones refuse
    /// the unknown field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<crate::decision::Decision>,
}

impl NewReview {
    fn validate(&self) -> Result<(), ReviewError> {
        self.target.validate()?;
        bounded("summary", &self.summary, MAX_SUMMARY_BYTES, Text::Prose)?;
        if let Some(anchor) = &self.anchor {
            bounded("anchor", anchor, MAX_ANCHOR_BYTES, Text::Line)?;
        }
        if let Some(key) = &self.idempotency_key {
            bounded("idempotency_key", key, MAX_KEY_BYTES, Text::Line)?;
        }
        if let Some(decision) = &self.decision {
            decision.validate().map_err(ReviewError::Invalid)?;
        }
        self.requested_by.validate_client()
    }
}

/// Which page of items to list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewQuery {
    pub open_only: bool,
    /// Oldest first instead of newest first.
    pub oldest_first: bool,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewPage {
    pub items: Vec<ReviewItem>,
    pub has_more: bool,
}

/// Bounded aggregate for status surfaces.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSummary {
    pub open: u64,
    #[serde(
        default,
        with = "time::serde::rfc3339::option",
        skip_serializing_if = "Option::is_none"
    )]
    pub oldest_requested_at: Option<OffsetDateTime>,
}

#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    #[error("invalid review request: {0}")]
    Invalid(String),
    #[error("no review item {0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("review store unavailable: {0}")]
    Unavailable(String),
    #[error("review store: {0}")]
    Storage(String),
}

impl From<rusqlite::Error> for ReviewError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

impl From<serde_json::Error> for ReviewError {
    fn from(error: serde_json::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

impl From<std::io::Error> for ReviewError {
    fn from(error: std::io::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

use crate::decision::Text;

fn bounded(name: &str, value: &str, max: usize, text: Text) -> Result<(), ReviewError> {
    crate::decision::check(name, value, max, text).map_err(ReviewError::Invalid)
}

fn nanos(at: OffsetDateTime) -> i64 {
    i64::try_from(at.unix_timestamp_nanos()).unwrap_or(i64::MAX)
}

/// Durable review queue owned by tendd.
#[derive(Clone)]
pub struct ReviewStore {
    inner: Result<Arc<Mutex<Connection>>, String>,
}

impl std::fmt::Debug for ReviewStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReviewStore")
            .field("available", &self.inner.is_ok())
            .finish()
    }
}

impl ReviewStore {
    /// `<data dir>/tend/reviews.sqlite3`.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        crate::paths::data_dir().map(|dir| dir.join("tend").join("reviews.sqlite3"))
    }

    /// Open (creating if absent) the store at `path`.
    pub fn open(path: &Path) -> Result<Self, ReviewError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        prepare_file(path)?;
        let connection = Connection::open(path)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        Self::initialize(connection, Some(path))
    }

    /// A store that keeps nothing on disk, for tests and embedders.
    #[must_use]
    pub fn in_memory() -> Self {
        let connection = Connection::open_in_memory()
            .map_err(ReviewError::from)
            .and_then(|c| Self::initialize(c, None));
        connection.unwrap_or_else(|error| Self::unavailable(error.to_string()))
    }

    /// A store that refuses every request with `reason`. The daemon uses it
    /// when the file cannot be opened, so review failures never stop tendd.
    #[must_use]
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            inner: Err(reason.into()),
        }
    }

    fn initialize(mut connection: Connection, path: Option<&Path>) -> Result<Self, ReviewError> {
        // Read the version inside the exclusive transaction: a newer tend
        // committing its schema while this opener waits must be seen, not
        // overwritten back to this version.
        let tx = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(ReviewError::Storage(format!(
                "review store schema {version} is newer than this tend ({SCHEMA_VERSION})"
            )));
        }
        tx.execute_batch(SCHEMA)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        if let Some(path) = path {
            secure_sidecars(path)?;
        }
        Ok(Self {
            inner: Ok(Arc::new(Mutex::new(connection))),
        })
    }

    async fn with<T: Send + 'static>(
        &self,
        work: impl FnOnce(&mut Connection) -> Result<T, ReviewError> + Send + 'static,
    ) -> Result<T, ReviewError> {
        let connection = self.inner.clone().map_err(ReviewError::Unavailable)?;
        tokio::task::spawn_blocking(move || {
            let mut guard = connection
                .lock()
                .map_err(|_| ReviewError::Storage("review store lock poisoned".into()))?;
            work(&mut guard)
        })
        .await
        .map_err(|error| ReviewError::Storage(error.to_string()))?
    }

    /// Record a claim. A retry with the same idempotency key and target
    /// returns the original item; the same key for another target conflicts.
    pub async fn create(&self, new: NewReview) -> Result<ReviewItem, ReviewError> {
        new.validate()?;
        self.with(move |connection| create(connection, new, OffsetDateTime::now_utc()))
            .await
    }

    pub async fn get(&self, id: String) -> Result<ReviewItem, ReviewError> {
        self.with(move |connection| load(connection, &id)?.ok_or(ReviewError::NotFound(id)))
            .await
    }

    /// One bounded page. `open_only` hides disposed items; `has_more` says
    /// whether another page follows at `offset + items.len()`.
    pub async fn list(&self, query: ReviewQuery) -> Result<ReviewPage, ReviewError> {
        let limit = query.limit.clamp(1, MAX_LIST_LIMIT);
        self.with(move |connection| {
            let filter = if query.open_only {
                "WHERE disposed_at_ns IS NULL"
            } else {
                ""
            };
            let order = if query.oldest_first { "ASC" } else { "DESC" };
            let sql = format!(
                "SELECT body FROM review_items {filter}
                 ORDER BY requested_at_ns {order}, rowid {order} LIMIT ?1 OFFSET ?2"
            );
            let mut statement = connection.prepare(&sql)?;
            let fetch = i64::try_from(limit + 1).unwrap_or(i64::MAX);
            let offset = i64::try_from(query.offset).unwrap_or(i64::MAX);
            let rows =
                statement.query_map(params![fetch, offset], |row| row.get::<_, String>(0))?;
            let mut items = rows
                .map(|body| Ok(serde_json::from_str(&body?)?))
                .collect::<Result<Vec<ReviewItem>, ReviewError>>()?;
            let has_more = items.len() > limit;
            items.truncate(limit);
            Ok(ReviewPage { items, has_more })
        })
        .await
    }

    /// Decided items, most recently decided first: the review half of
    /// "what did I decide". `has_more` as in [`Self::list`].
    pub async fn decided(&self, limit: usize) -> Result<ReviewPage, ReviewError> {
        let limit = limit.clamp(1, MAX_LIST_LIMIT);
        self.with(move |connection| {
            let mut statement = connection.prepare(
                "SELECT body FROM review_items WHERE disposed_at_ns IS NOT NULL
                 ORDER BY disposed_at_ns DESC, rowid DESC LIMIT ?1",
            )?;
            let fetch = i64::try_from(limit + 1).unwrap_or(i64::MAX);
            let rows = statement.query_map(params![fetch], |row| row.get::<_, String>(0))?;
            let mut items = rows
                .map(|body| Ok(serde_json::from_str(&body?)?))
                .collect::<Result<Vec<ReviewItem>, ReviewError>>()?;
            let has_more = items.len() > limit;
            items.truncate(limit);
            Ok(ReviewPage { items, has_more })
        })
        .await
    }

    pub async fn summary(&self) -> Result<ReviewSummary, ReviewError> {
        self.with(|connection| {
            let (open, oldest): (i64, Option<i64>) = connection.query_row(
                "SELECT COUNT(*), MIN(requested_at_ns) FROM review_items
                 WHERE disposed_at_ns IS NULL",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            Ok(ReviewSummary {
                open: u64::try_from(open).unwrap_or(0),
                oldest_requested_at: oldest
                    .and_then(|ns| OffsetDateTime::from_unix_timestamp_nanos(i128::from(ns)).ok()),
            })
        })
        .await
    }

    /// Record a claim already decided, in one transaction: a person who
    /// says "I reviewed this" both asks and answers. Needs an idempotency
    /// key: a retry returns the same item, an open item under that key is
    /// decided, and a different disposition on a decided one conflicts.
    /// Both events are logged.
    pub async fn record(
        &self,
        new: NewReview,
        kind: DispositionKind,
        note: Option<String>,
    ) -> Result<ReviewItem, ReviewError> {
        new.validate()?;
        if new.idempotency_key.is_none() {
            return Err(ReviewError::Invalid(
                "recording a decided review needs an idempotency key".into(),
            ));
        }
        if let Some(note) = &note {
            bounded("note", note, MAX_NOTE_BYTES, Text::Prose)?;
        }
        self.with(move |connection| record(connection, new, kind, note, OffsetDateTime::now_utc()))
            .await
    }

    /// Record a disposition. Repeating the same disposition is idempotent;
    /// a different one on a disposed item conflicts.
    pub async fn dispose(
        &self,
        id: String,
        kind: DispositionKind,
        by: ReviewActor,
        note: Option<String>,
    ) -> Result<ReviewItem, ReviewError> {
        by.validate_client()?;
        if let Some(note) = &note {
            bounded("note", note, MAX_NOTE_BYTES, Text::Prose)?;
        }
        self.with(move |connection| {
            dispose(connection, &id, kind, by, note, OffsetDateTime::now_utc())
        })
        .await
    }
}

/// Create the database owner-only, or tighten an existing one, through the
/// opened handle before `SQLite` touches it. Refuses a symlink, and a path
/// that changed between the check and the open. Not an atomic no-follow open:
/// the 0700 parent directory is what keeps other users out.
fn prepare_file(path: &Path) -> Result<(), ReviewError> {
    let refuse =
        || ReviewError::Storage("review store must be a regular file, not a symlink".into());
    if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Err(refuse());
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let opened = file.metadata()?;
        let named = std::fs::symlink_metadata(path)?;
        if !opened.is_file() || opened.ino() != named.ino() || opened.dev() != named.dev() {
            return Err(refuse());
        }
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    #[cfg(not(unix))]
    drop(file);
    secure_sidecars(path)
}

/// Owner-only permissions on existing WAL and shared-memory files.
fn secure_sidecars(path: &Path) -> Result<(), ReviewError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for suffix in ["-wal", "-shm"] {
            let mut candidate = path.as_os_str().to_os_string();
            candidate.push(suffix);
            let candidate = PathBuf::from(candidate);
            if candidate.exists() {
                std::fs::set_permissions(candidate, std::fs::Permissions::from_mode(0o600))?;
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn load(connection: &Connection, id: &str) -> Result<Option<ReviewItem>, ReviewError> {
    connection
        .query_row(
            "SELECT body FROM review_items WHERE id = ?1",
            params![id],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(|body| serde_json::from_str(&body).map_err(ReviewError::from))
        .transpose()
}

fn create(
    connection: &mut Connection,
    new: NewReview,
    now: OffsetDateTime,
) -> Result<ReviewItem, ReviewError> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(key) = &new.idempotency_key {
        let existing: Option<String> = tx
            .query_row(
                "SELECT body FROM review_items WHERE idempotency_key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(body) = existing {
            let item: ReviewItem = serde_json::from_str(&body)?;
            if item.target != new.target {
                return Err(ReviewError::Conflict(format!(
                    "idempotency key {key:?} already names review {} for another target",
                    item.id
                )));
            }
            return Ok(item);
        }
    }
    let target_key = new.target.key();
    let previous: i64 = tx.query_row(
        "SELECT COALESCE(MAX(generation), 0) FROM review_items WHERE target_key = ?1",
        params![target_key],
        |row| row.get(0),
    )?;
    let item = ReviewItem {
        id: format!("rv_{}", uuid::Uuid::new_v4().simple()),
        target: new.target,
        generation: u64::try_from(previous).unwrap_or(0) + 1,
        summary: new.summary,
        anchor: new.anchor,
        idempotency_key: new.idempotency_key,
        requested_at: now,
        requested_by: new.requested_by,
        decision: new
            .decision
            .and_then(crate::decision::Decision::into_option),
        disposition: None,
    };
    tx.execute(
        "INSERT INTO review_items
           (id, target_key, generation, idempotency_key, requested_at_ns, body)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            item.id,
            target_key,
            i64::try_from(item.generation).unwrap_or(i64::MAX),
            item.idempotency_key,
            nanos(now),
            serde_json::to_string(&item)?,
        ],
    )?;
    tx.execute(
        "INSERT INTO review_events (item_id, at_ns, event, actor_kind) VALUES (?1, ?2, 'requested', ?3)",
        params![item.id, nanos(now), item.requested_by.kind.as_str()],
    )?;
    tx.commit()?;
    Ok(item)
}

fn record(
    connection: &mut Connection,
    new: NewReview,
    kind: DispositionKind,
    note: Option<String>,
    now: OffsetDateTime,
) -> Result<ReviewItem, ReviewError> {
    let by = new.requested_by.clone();
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let key = new.idempotency_key.clone().unwrap_or_default();
    let existing: Option<String> = tx
        .query_row(
            "SELECT body FROM review_items WHERE idempotency_key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()?;
    let mut item = if let Some(body) = existing {
        let item: ReviewItem = serde_json::from_str(&body)?;
        if item.target != new.target {
            return Err(ReviewError::Conflict(format!(
                "idempotency key {key:?} already names review {} for another target",
                item.id
            )));
        }
        if let Some(existing) = &item.disposition {
            if existing.kind == kind {
                return Ok(item);
            }
            return Err(ReviewError::Conflict(format!(
                "review {} is already {}",
                item.id,
                existing.kind.as_str()
            )));
        }
        item
    } else {
        let target_key = new.target.key();
        let previous: i64 = tx.query_row(
            "SELECT COALESCE(MAX(generation), 0) FROM review_items WHERE target_key = ?1",
            params![target_key],
            |row| row.get(0),
        )?;
        let item = ReviewItem {
            id: format!("rv_{}", uuid::Uuid::new_v4().simple()),
            target: new.target,
            generation: u64::try_from(previous).unwrap_or(0) + 1,
            summary: new.summary,
            anchor: new.anchor,
            idempotency_key: new.idempotency_key,
            requested_at: now,
            requested_by: new.requested_by,
            decision: new
                .decision
                .and_then(crate::decision::Decision::into_option),
            disposition: None,
        };
        tx.execute(
            "INSERT INTO review_items
                   (id, target_key, generation, idempotency_key, requested_at_ns, body)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                item.id,
                target_key,
                i64::try_from(item.generation).unwrap_or(i64::MAX),
                item.idempotency_key,
                nanos(now),
                serde_json::to_string(&item)?,
            ],
        )?;
        tx.execute(
                "INSERT INTO review_events (item_id, at_ns, event, actor_kind) VALUES (?1, ?2, 'requested', ?3)",
                params![item.id, nanos(now), item.requested_by.kind.as_str()],
            )?;
        item
    };
    let actor_kind = by.kind;
    item.disposition = Some(ReviewDisposition {
        kind,
        by,
        at: now,
        note,
    });
    tx.execute(
        "UPDATE review_items SET disposed_at_ns = ?2, body = ?3 WHERE id = ?1",
        params![item.id, nanos(now), serde_json::to_string(&item)?],
    )?;
    tx.execute(
        "INSERT INTO review_events (item_id, at_ns, event, actor_kind) VALUES (?1, ?2, ?3, ?4)",
        params![item.id, nanos(now), kind.as_str(), actor_kind.as_str()],
    )?;
    tx.commit()?;
    Ok(item)
}

fn dispose(
    connection: &mut Connection,
    id: &str,
    kind: DispositionKind,
    by: ReviewActor,
    note: Option<String>,
    now: OffsetDateTime,
) -> Result<ReviewItem, ReviewError> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let mut item = load(&tx, id)?.ok_or_else(|| ReviewError::NotFound(id.to_owned()))?;
    if let Some(existing) = &item.disposition {
        if existing.kind == kind {
            return Ok(item);
        }
        return Err(ReviewError::Conflict(format!(
            "review {id} is already {}",
            existing.kind.as_str()
        )));
    }
    let actor_kind = by.kind;
    item.disposition = Some(ReviewDisposition {
        kind,
        by,
        at: now,
        note,
    });
    tx.execute(
        "UPDATE review_items SET disposed_at_ns = ?2, body = ?3 WHERE id = ?1",
        params![id, nanos(now), serde_json::to_string(&item)?],
    )?;
    tx.execute(
        "INSERT INTO review_events (item_id, at_ns, event, actor_kind) VALUES (?1, ?2, ?3, ?4)",
        params![id, nanos(now), kind.as_str(), actor_kind.as_str()],
    )?;
    tx.commit()?;
    Ok(item)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str) -> ReviewTarget {
        ReviewTarget::Session {
            provider: Some("claude".into()),
            session_id: id.into(),
        }
    }

    fn claim(target: ReviewTarget, key: Option<&str>) -> NewReview {
        NewReview {
            target,
            summary: "check the migration".into(),
            anchor: None,
            idempotency_key: key.map(Into::into),
            requested_by: ReviewActor {
                kind: ActorKind::Agent,
                label: Some("impl".into()),
            },
            decision: None,
        }
    }

    fn page(open_only: bool, limit: usize, offset: usize) -> ReviewQuery {
        ReviewQuery {
            open_only,
            oldest_first: false,
            limit,
            offset,
        }
    }

    fn event_count(path: &Path) -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM review_events", [], |r| r.get(0))
            .unwrap()
    }

    fn user() -> ReviewActor {
        ReviewActor {
            kind: ActorKind::User,
            label: None,
        }
    }

    #[tokio::test]
    async fn a_decided_review_is_recorded_once_with_both_events() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reviews.sqlite3");
        let store = ReviewStore::open(&path).unwrap();
        let mut mine = claim(session("s1"), Some("home:abc"));
        mine.requested_by = user();
        let item = store
            .record(mine.clone(), DispositionKind::Accepted, None)
            .await
            .unwrap();
        assert_eq!(
            item.disposition.as_ref().unwrap().kind,
            DispositionKind::Accepted
        );
        assert!(!item.is_open());
        assert_eq!(event_count(&path), 2, "requested and accepted");
        // A retry (the reply was lost) returns the same item, logs nothing.
        let again = store
            .record(mine.clone(), DispositionKind::Accepted, None)
            .await
            .unwrap();
        assert_eq!(again.id, item.id);
        assert_eq!(event_count(&path), 2);
        // A different decision conflicts; another target under the key
        // conflicts; no key is refused.
        assert!(matches!(
            store
                .record(mine.clone(), DispositionKind::Withdrawn, None)
                .await,
            Err(ReviewError::Conflict(_))
        ));
        let mut elsewhere = mine.clone();
        elsewhere.target = session("s2");
        assert!(matches!(
            store
                .record(elsewhere, DispositionKind::Accepted, None)
                .await,
            Err(ReviewError::Conflict(_))
        ));
        let mut keyless = mine.clone();
        keyless.idempotency_key = None;
        assert!(matches!(
            store.record(keyless, DispositionKind::Accepted, None).await,
            Err(ReviewError::Invalid(_))
        ));
        // An open claim under the key is decided, not duplicated.
        let open = store
            .create(claim(session("s3"), Some("k3")))
            .await
            .unwrap();
        let mut decide = claim(session("s3"), Some("k3"));
        decide.requested_by = user();
        let decided = store
            .record(
                decide,
                DispositionKind::Returned,
                Some("tests missing".into()),
            )
            .await
            .unwrap();
        assert_eq!(decided.id, open.id);
        assert_eq!(decided.disposition.unwrap().kind, DispositionKind::Returned);
        assert_eq!(store.summary().await.unwrap().open, 0);
    }

    #[tokio::test]
    async fn retry_with_same_key_returns_the_original_item() {
        let store = ReviewStore::in_memory();
        let first = store.create(claim(session("s1"), Some("k"))).await.unwrap();
        let retry = store.create(claim(session("s1"), Some("k"))).await.unwrap();
        assert_eq!(first, retry);
        assert_eq!(store.summary().await.unwrap().open, 1);
    }

    #[tokio::test]
    async fn same_key_for_another_target_conflicts() {
        let store = ReviewStore::in_memory();
        store.create(claim(session("s1"), Some("k"))).await.unwrap();
        let err = store
            .create(claim(session("s2"), Some("k")))
            .await
            .unwrap_err();
        assert!(matches!(err, ReviewError::Conflict(_)), "{err}");
    }

    #[tokio::test]
    async fn new_claim_after_acceptance_opens_the_next_generation() {
        let store = ReviewStore::in_memory();
        let first = store.create(claim(session("s1"), None)).await.unwrap();
        assert_eq!(first.generation, 1);
        store
            .dispose(first.id.clone(), DispositionKind::Accepted, user(), None)
            .await
            .unwrap();
        // The accepted item stays accepted; a new claim is a new item.
        let second = store.create(claim(session("s1"), None)).await.unwrap();
        assert_eq!(second.generation, 2);
        assert_ne!(second.id, first.id);
        let first = store.get(first.id).await.unwrap();
        assert_eq!(
            first.disposition.map(|d| d.kind),
            Some(DispositionKind::Accepted)
        );
        let summary = store.summary().await.unwrap();
        assert_eq!(summary.open, 1);
        assert_eq!(summary.oldest_requested_at, Some(second.requested_at));
    }

    #[tokio::test]
    async fn generations_are_per_target() {
        let store = ReviewStore::in_memory();
        store.create(claim(session("s1"), None)).await.unwrap();
        let other = store.create(claim(session("s2"), None)).await.unwrap();
        assert_eq!(other.generation, 1);
    }

    #[tokio::test]
    async fn repeated_disposition_is_idempotent_and_a_different_one_conflicts() {
        let store = ReviewStore::in_memory();
        let item = store.create(claim(session("s1"), None)).await.unwrap();
        let returned = store
            .dispose(
                item.id.clone(),
                DispositionKind::Returned,
                user(),
                Some("tests missing".into()),
            )
            .await
            .unwrap();
        let again = store
            .dispose(item.id.clone(), DispositionKind::Returned, user(), None)
            .await
            .unwrap();
        assert_eq!(returned, again);
        let err = store
            .dispose(item.id, DispositionKind::Accepted, user(), None)
            .await
            .unwrap_err();
        assert!(matches!(err, ReviewError::Conflict(_)), "{err}");
    }

    #[tokio::test]
    async fn clients_cannot_declare_policy_actors() {
        let store = ReviewStore::in_memory();
        let mut new = claim(session("s1"), None);
        new.requested_by.kind = ActorKind::Policy;
        assert!(matches!(
            store.create(new).await.unwrap_err(),
            ReviewError::Invalid(_)
        ));
    }

    #[tokio::test]
    async fn blank_and_oversized_fields_are_rejected() {
        let store = ReviewStore::in_memory();
        let mut new = claim(session("s1"), None);
        new.summary = "  ".into();
        assert!(store.create(new).await.is_err());
        let mut new = claim(session("s1"), None);
        new.summary = "x".repeat(MAX_SUMMARY_BYTES + 1);
        assert!(store.create(new).await.is_err());
        assert!(store.create(claim(session(""), None)).await.is_err());
    }

    #[tokio::test]
    async fn items_survive_reopen_and_list_filters_disposed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tend").join("reviews.sqlite3");
        let (open_id, done_id) = {
            let store = ReviewStore::open(&path).unwrap();
            let open = store.create(claim(session("s1"), None)).await.unwrap();
            let done = store.create(claim(session("s2"), None)).await.unwrap();
            store
                .dispose(done.id.clone(), DispositionKind::Withdrawn, user(), None)
                .await
                .unwrap();
            (open.id, done.id)
        };
        let store = ReviewStore::open(&path).unwrap();
        let open: Vec<_> = store
            .list(page(true, 10, 0))
            .await
            .unwrap()
            .items
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(open, vec![open_id.clone()]);
        let all = store.list(page(false, 10, 0)).await.unwrap().items;
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|i| i.id == done_id && !i.is_open()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[tokio::test]
    async fn unavailable_store_refuses_without_panicking() {
        let store = ReviewStore::unavailable("disk full");
        assert!(matches!(
            store.summary().await.unwrap_err(),
            ReviewError::Unavailable(_)
        ));
        assert!(store.create(claim(session("s1"), None)).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn write_failure_leaves_no_partial_item() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reviews.sqlite3");
        let store = ReviewStore::open(&path).unwrap();
        store.create(claim(session("s1"), None)).await.unwrap();
        // A trigger that aborts the event insert makes the second write fail
        // after the item row was written inside the same transaction.
        store
            .with(|c| {
                c.execute_batch(
                    "CREATE TRIGGER fail_events BEFORE INSERT ON review_events
                     BEGIN SELECT RAISE(ABORT, 'injected'); END;",
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(store.create(claim(session("s2"), None)).await.is_err());
        assert_eq!(store.summary().await.unwrap().open, 1);
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reviews.sqlite3");
        {
            let c = Connection::open(&path).unwrap();
            c.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        assert!(ReviewStore::open(&path).is_err());
    }

    #[test]
    fn a_newer_schema_committed_while_opening_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reviews.sqlite3");
        drop(ReviewStore::open(&path).unwrap());
        let newer = Connection::open(&path).unwrap();
        newer.execute_batch("BEGIN IMMEDIATE").unwrap();
        newer
            .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
            .unwrap();
        let opener = {
            let path = path.clone();
            std::thread::spawn(move || ReviewStore::open(&path).map(|_| ()))
        };
        std::thread::sleep(std::time::Duration::from_millis(200));
        newer.execute_batch("COMMIT").unwrap();
        assert!(opener.join().unwrap().is_err());
        let version: i64 = newer
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION + 1);
    }

    #[tokio::test]
    async fn control_characters_are_refused_and_prose_keeps_newlines() {
        let store = ReviewStore::in_memory();
        let nul = ReviewTarget::Work {
            workspace: "a\0b".into(),
            work: "c".into(),
            alias: None,
            pipeline_generation: None,
        };
        assert!(matches!(
            store.create(claim(nul, None)).await.unwrap_err(),
            ReviewError::Invalid(_)
        ));
        let mut escape = claim(session("s1"), None);
        escape.summary = "ok\u{1b}[2Jgone".into();
        assert!(store.create(escape).await.is_err());
        let mut label = claim(session("s1"), None);
        label.requested_by.label = Some("a\nb".into());
        assert!(store.create(label).await.is_err());
        let mut prose = claim(session("s1"), None);
        prose.summary = "line one\n\tline two".into();
        assert!(store.create(prose).await.is_ok());
        let item = store.create(claim(session("s2"), None)).await.unwrap();
        assert!(store
            .dispose(
                item.id,
                DispositionKind::Returned,
                user(),
                Some("\u{7}".into())
            )
            .await
            .is_err());
    }

    #[test]
    fn target_keys_cannot_collide_across_components() {
        let a = ReviewTarget::Work {
            workspace: "a".into(),
            work: "b/c".into(),
            alias: None,
            pipeline_generation: None,
        };
        let b = ReviewTarget::Work {
            workspace: "a/b".into(),
            work: "c".into(),
            alias: None,
            pipeline_generation: None,
        };
        assert_ne!(a.key(), b.key());
        let session = ReviewTarget::Session {
            provider: None,
            session_id: "x".into(),
        };
        let request = ReviewTarget::Request {
            request_id: "x".into(),
            attempt_id: None,
        };
        assert_ne!(session.key(), request.key());
    }

    #[tokio::test]
    async fn the_oldest_item_stays_reachable_past_one_page() {
        let store = ReviewStore::in_memory();
        let first = store.create(claim(session("s0"), None)).await.unwrap();
        for i in 1..=MAX_LIST_LIMIT {
            store
                .create(claim(session(&format!("s{i}")), None))
                .await
                .unwrap();
        }
        let newest = store.list(page(true, usize::MAX, 0)).await.unwrap();
        assert_eq!(newest.items.len(), MAX_LIST_LIMIT);
        assert!(newest.has_more);
        assert!(newest.items.iter().all(|i| i.id != first.id));
        let rest = store
            .list(page(true, MAX_LIST_LIMIT, MAX_LIST_LIMIT))
            .await
            .unwrap();
        assert_eq!(
            rest.items.iter().map(|i| &i.id).collect::<Vec<_>>(),
            [&first.id]
        );
        assert!(!rest.has_more);
        let mut oldest = page(true, 1, 0);
        oldest.oldest_first = true;
        let oldest = store.list(oldest).await.unwrap();
        assert_eq!(oldest.items[0].id, first.id);
        assert!(oldest.has_more);
    }

    #[tokio::test]
    async fn retries_write_no_extra_events_and_never_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reviews.sqlite3");
        let store = ReviewStore::open(&path).unwrap();
        let item = store.create(claim(session("s1"), Some("k"))).await.unwrap();
        store.create(claim(session("s1"), Some("k"))).await.unwrap();
        assert_eq!(event_count(&path), 1);
        store
            .dispose(item.id.clone(), DispositionKind::Accepted, user(), None)
            .await
            .unwrap();
        store
            .dispose(item.id.clone(), DispositionKind::Accepted, user(), None)
            .await
            .unwrap();
        assert_eq!(event_count(&path), 2);
        // A keyed retry after disposal returns the disposed item; it does
        // not reopen it or open another generation.
        let retry = store.create(claim(session("s1"), Some("k"))).await.unwrap();
        assert_eq!(retry.id, item.id);
        assert!(!retry.is_open());
        assert_eq!(store.summary().await.unwrap().open, 0);
        assert_eq!(event_count(&path), 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn disposition_write_failure_leaves_the_item_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reviews.sqlite3");
        let store = ReviewStore::open(&path).unwrap();
        let item = store.create(claim(session("s1"), None)).await.unwrap();
        store
            .with(|c| {
                c.execute_batch(
                    "CREATE TRIGGER fail_dispositions BEFORE INSERT ON review_events
                     WHEN NEW.event <> 'requested'
                     BEGIN SELECT RAISE(ABORT, 'injected'); END;",
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(store
            .dispose(item.id.clone(), DispositionKind::Accepted, user(), None)
            .await
            .is_err());
        assert!(store.get(item.id).await.unwrap().is_open());
        assert_eq!(store.summary().await.unwrap().open, 1);
        assert_eq!(event_count(&path), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_creates_from_separate_connections_stay_consistent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reviews.sqlite3");
        let stores: Vec<_> = (0..4).map(|_| ReviewStore::open(&path).unwrap()).collect();
        let keyed = futures_join(stores.iter().map(|store| {
            let store = store.clone();
            async move { store.create(claim(session("s1"), Some("k"))).await }
        }))
        .await;
        let ids: std::collections::HashSet<_> = keyed.into_iter().map(|r| r.unwrap().id).collect();
        assert_eq!(ids.len(), 1);
        let unkeyed = futures_join(stores.iter().map(|store| {
            let store = store.clone();
            async move { store.create(claim(session("s2"), None)).await }
        }))
        .await;
        let mut generations: Vec<_> = unkeyed.into_iter().map(|r| r.unwrap().generation).collect();
        generations.sort_unstable();
        assert_eq!(generations, [1, 2, 3, 4]);
        assert_eq!(event_count(&path), 5);
    }

    async fn futures_join<F: std::future::Future + Send + 'static>(
        futures: impl Iterator<Item = F>,
    ) -> Vec<F::Output>
    where
        F::Output: Send + 'static,
    {
        let handles: Vec<_> = futures.map(tokio::spawn).collect();
        let mut out = Vec::new();
        for handle in handles {
            out.push(handle.await.unwrap());
        }
        out
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn existing_permissive_files_are_tightened_and_symlinks_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reviews.sqlite3");
        drop(ReviewStore::open(&path).unwrap());
        for suffix in ["", "-wal", "-shm"] {
            let file = PathBuf::from(format!("{}{suffix}", path.display()));
            if file.exists() {
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
            }
        }
        let store = ReviewStore::open(&path).unwrap();
        store.create(claim(session("s1"), None)).await.unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let file = PathBuf::from(format!("{}{suffix}", path.display()));
            if file.exists() {
                let mode = std::fs::metadata(&file).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o600, "{}", file.display());
            }
        }
        let link = dir.path().join("link.sqlite3");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(ReviewStore::open(&link).is_err());
    }

    #[tokio::test]
    async fn unavailable_store_refuses_every_operation() {
        let store = ReviewStore::unavailable("disk full");
        let unavailable = |e: ReviewError| matches!(e, ReviewError::Unavailable(_));
        assert!(unavailable(
            store.create(claim(session("s1"), None)).await.unwrap_err()
        ));
        assert!(unavailable(store.get("rv_x".into()).await.unwrap_err()));
        assert!(unavailable(
            store.list(page(true, 10, 0)).await.unwrap_err()
        ));
        assert!(unavailable(store.summary().await.unwrap_err()));
        assert!(unavailable(
            store
                .dispose("rv_x".into(), DispositionKind::Accepted, user(), None)
                .await
                .unwrap_err()
        ));
    }
}
