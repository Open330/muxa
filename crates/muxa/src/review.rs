//! Review items: explicit, durable requests that a specific result be looked
//! at. See `docs/proposals/REVIEW_DEBT.md`.
//!
//! An item is created only by a claim (someone asked for review), never by
//! an observation such as a turn stopping, a process exiting or a branch
//! being merged. It stays open until someone records a disposition. muxa
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
/// handle: muxa never resolves or retires an item through it.
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
    /// Items for the same key share one generation sequence.
    fn key(&self) -> String {
        match self {
            Self::Session {
                provider,
                session_id,
            } => format!(
                "session\0{}\0{session_id}",
                provider.as_deref().unwrap_or("")
            ),
            Self::Work {
                workspace,
                work,
                alias,
                ..
            } => format!(
                "work\0{workspace}\0{work}\0{}",
                alias.as_deref().unwrap_or("")
            ),
            Self::Request { request_id, .. } => format!("request\0{request_id}"),
        }
    }

    fn validate(&self) -> Result<(), ReviewError> {
        let required = |name: &str, value: &str| bounded(name, value, MAX_ID_BYTES, true);
        let optional = |name: &str, value: Option<&String>| {
            value.map_or(Ok(()), |v| bounded(name, v, MAX_ID_BYTES, true))
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
            bounded("actor label", label, MAX_LABEL_BYTES, true)?;
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
}

impl NewReview {
    fn validate(&self) -> Result<(), ReviewError> {
        self.target.validate()?;
        bounded("summary", &self.summary, MAX_SUMMARY_BYTES, true)?;
        if let Some(anchor) = &self.anchor {
            bounded("anchor", anchor, MAX_ANCHOR_BYTES, true)?;
        }
        if let Some(key) = &self.idempotency_key {
            bounded("idempotency_key", key, MAX_KEY_BYTES, true)?;
        }
        self.requested_by.validate_client()
    }
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

fn bounded(name: &str, value: &str, max: usize, nonblank: bool) -> Result<(), ReviewError> {
    if nonblank && value.trim().is_empty() {
        return Err(ReviewError::Invalid(format!("{name} must not be blank")));
    }
    if value.len() > max {
        return Err(ReviewError::Invalid(format!("{name} exceeds {max} bytes")));
    }
    Ok(())
}

fn nanos(at: OffsetDateTime) -> i64 {
    i64::try_from(at.unix_timestamp_nanos()).unwrap_or(i64::MAX)
}

/// Durable review queue owned by muxad.
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
    /// `<data dir>/muxa/reviews.sqlite3`.
    #[must_use]
    pub fn default_path() -> Option<PathBuf> {
        crate::paths::data_dir().map(|dir| dir.join("muxa").join("reviews.sqlite3"))
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
        if std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(ReviewError::Storage(
                "review store must not be a symlink".into(),
            ));
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path)?;
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
    /// when the file cannot be opened, so review failures never stop muxad.
    #[must_use]
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            inner: Err(reason.into()),
        }
    }

    fn initialize(mut connection: Connection, path: Option<&Path>) -> Result<Self, ReviewError> {
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version > SCHEMA_VERSION {
            return Err(ReviewError::Storage(format!(
                "review store schema {version} is newer than this muxa ({SCHEMA_VERSION})"
            )));
        }
        let tx = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
        tx.execute_batch(SCHEMA)?;
        tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tx.commit()?;
        #[cfg(unix)]
        if let Some(path) = path {
            use std::os::unix::fs::PermissionsExt;
            for suffix in ["", "-wal", "-shm"] {
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

    /// Newest first. `open_only` hides disposed items.
    pub async fn list(
        &self,
        open_only: bool,
        limit: usize,
    ) -> Result<Vec<ReviewItem>, ReviewError> {
        let limit = i64::try_from(limit.clamp(1, MAX_LIST_LIMIT)).unwrap_or(1);
        self.with(move |connection| {
            let sql = if open_only {
                "SELECT body FROM review_items WHERE disposed_at_ns IS NULL
                 ORDER BY requested_at_ns DESC, rowid DESC LIMIT ?1"
            } else {
                "SELECT body FROM review_items ORDER BY requested_at_ns DESC, rowid DESC LIMIT ?1"
            };
            let mut statement = connection.prepare(sql)?;
            let rows = statement.query_map(params![limit], |row| row.get::<_, String>(0))?;
            rows.map(|body| Ok(serde_json::from_str(&body?)?)).collect()
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
            bounded("note", note, MAX_NOTE_BYTES, true)?;
        }
        self.with(move |connection| {
            dispose(connection, &id, kind, by, note, OffsetDateTime::now_utc())
        })
        .await
    }
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
        }
    }

    fn user() -> ReviewActor {
        ReviewActor {
            kind: ActorKind::User,
            label: None,
        }
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
        let path = dir.path().join("muxa").join("reviews.sqlite3");
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
            .list(true, 10)
            .await
            .unwrap()
            .into_iter()
            .map(|i| i.id)
            .collect();
        assert_eq!(open, vec![open_id.clone()]);
        let all = store.list(false, 10).await.unwrap();
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
}
