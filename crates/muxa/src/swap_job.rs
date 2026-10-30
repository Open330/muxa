//! Durable account-swap jobs: the accepted plan, every effect's intent and
//! result, and what was observed, in one private `SQLite` file.
//!
//! A swap stops processes and types into panes; a daemon that dies part way
//! must not leave anyone guessing what happened, and must never do it again
//! by itself. So:
//!
//! - the whole plan is written before the first effect;
//! - every effect's intent is committed before it happens, and its result
//!   after ([`Journal`] is the executor's [`EffectObserver`]): an intent with
//!   no result is an effect that may or may not have happened;
//! - one manager owns the file at a time ([`SwapJobStore::acquire`]), and
//!   only an owner that is provably gone is taken over;
//! - on taking over, every job the previous owner left unfinished is marked
//!   for a person to look at ([`SwapJobStore::recover`]). Nothing is
//!   replayed.
//!
//! What is never written: typed text (follow-ups, drafts, relaunch lines —
//! a launch line can carry credentials), environments, command lines and
//! the executor's free-form notes, which can quote a prompt. Text is kept as
//! a digest and a length.
//!
//! Internal for now: nothing outside this module runs jobs yet.

// Wired into the daemon in a follow-up; until then only tests construct it.
#![cfg_attr(not(test), allow(dead_code))]

#[cfg(test)]
mod tests;

use crate::account_swap::effects::{Effect, EffectObserver, ObserverError, Op, Purpose, Transport};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// The schema this build writes and reads. Any other version is refused
/// without being touched.
const SCHEMA_VERSION: i64 = 2;

const SCHEMA: &str = "
CREATE TABLE owner (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    nonce TEXT NOT NULL,
    pid INTEGER NOT NULL,
    process_start TEXT,
    acquired_at TEXT NOT NULL
);
CREATE TABLE jobs (
    job_id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL,
    request_key TEXT NOT NULL UNIQUE,
    request_digest TEXT NOT NULL,
    plan_digest TEXT NOT NULL,
    owner_nonce TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN
        ('accepted', 'running', 'done', 'needs_reconciliation', 'abandoned')),
    cancel_requested INTEGER NOT NULL DEFAULT 0,
    plan_json TEXT NOT NULL,
    revision INTEGER NOT NULL,
    reason TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    retry_of TEXT
);
CREATE TABLE targets (
    job_id TEXT NOT NULL REFERENCES jobs (job_id),
    target_id INTEGER NOT NULL,
    attempt INTEGER NOT NULL,
    phase TEXT NOT NULL,
    disposition TEXT,
    reason TEXT,
    lineage TEXT,
    retry_of TEXT,
    PRIMARY KEY (job_id, target_id, attempt)
);
CREATE UNIQUE INDEX targets_retry_of ON targets (retry_of) WHERE retry_of IS NOT NULL;
CREATE TABLE effects (
    job_id TEXT NOT NULL,
    target_id INTEGER NOT NULL,
    attempt INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    nonce TEXT NOT NULL UNIQUE,
    grp INTEGER NOT NULL,
    purpose TEXT NOT NULL,
    op_json TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('intent', 'accepted', 'refused', 'unconfirmed')),
    intent_at TEXT NOT NULL,
    result_at TEXT,
    PRIMARY KEY (job_id, target_id, attempt, seq),
    FOREIGN KEY (job_id, target_id, attempt) REFERENCES targets (job_id, target_id, attempt)
);
CREATE TABLE observations (
    job_id TEXT NOT NULL,
    target_id INTEGER NOT NULL,
    attempt INTEGER NOT NULL,
    seq INTEGER NOT NULL,
    kind TEXT NOT NULL,
    value_json TEXT NOT NULL,
    observed_at TEXT NOT NULL,
    PRIMARY KEY (job_id, target_id, attempt, seq),
    FOREIGN KEY (job_id, target_id, attempt) REFERENCES targets (job_id, target_id, attempt)
);
";

/// v1 -> v2: a retried target names the attempt it retries, and every
/// target the root target it descends from. Nullable additions only: a v1
/// target is its own root.
const MIGRATE_1_TO_2: &str = "
ALTER TABLE jobs ADD COLUMN retry_of TEXT;
ALTER TABLE targets ADD COLUMN lineage TEXT;
ALTER TABLE targets ADD COLUMN retry_of TEXT;
CREATE UNIQUE INDEX targets_retry_of ON targets (retry_of) WHERE retry_of IS NOT NULL;
";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum JobError {
    #[error("swap job store: {0}")]
    Store(String),
    #[error("swap job store has schema version {found}; this build reads {SCHEMA_VERSION}")]
    UnsupportedSchema { found: i64 },
    #[error("swap jobs are owned by another live manager (pid {pid})")]
    OwnedElsewhere { pid: u32 },
    #[error("this manager no longer owns the swap job store")]
    Fenced,
    #[error("request key {0} was already used for a different swap")]
    KeyReused(String),
    #[error("swap job {0} not found")]
    NotFound(String),
    #[error("swap job {job}: {why}")]
    Invalid { job: String, why: String },
}

impl From<rusqlite::Error> for JobError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.to_string())
    }
}

impl From<std::io::Error> for JobError {
    fn from(error: std::io::Error) -> Self {
        Self::Store(error.to_string())
    }
}

/// Who a manager is: a nonce for this run, and the process behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnerIdentity {
    pub(crate) nonce: String,
    pub(crate) pid: u32,
    /// [`crate::agent_conversation::process_start`] of `pid`, where known.
    pub(crate) process_start: Option<String>,
}

/// Whether a previous owner is still running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Liveness {
    /// Its pid is running and, where start times are known, is the same
    /// process.
    Alive,
    /// Its pid is gone, or now belongs to a different process.
    Gone,
}

/// Proof of ownership, handed back on every write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Lease {
    nonce: String,
}

/// One target as accepted: where it is, what it was verified to be, and
/// what it was assigned. Fixed before the first effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PlannedTarget {
    /// Where this target sits among everything the request reported.
    pub(crate) report_index: usize,
    /// The canonical endpoint (pane on its server).
    pub(crate) endpoint: String,
    pub(crate) host: String,
    pub(crate) socket: Option<String>,
    pub(crate) pane: String,
    pub(crate) provider: String,
    pub(crate) conversation_id: String,
    pub(crate) session_id_source: String,
    pub(crate) conversation_verified: bool,
    pub(crate) process_pid: u32,
    pub(crate) process_start: Option<String>,
    pub(crate) from_account: Option<String>,
    pub(crate) assigned_account: Option<String>,
}

/// An entry settled while planning — skipped, refused, busy — recorded with
/// the plan so the whole report survives a restart in its order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SettledEntry {
    pub(crate) report_index: usize,
    pub(crate) pane: Option<String>,
    pub(crate) agent_session_id: String,
    pub(crate) disposition: Disposition,
    pub(crate) reason: String,
}

/// The follow-up as stored: never the text itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TextRef {
    pub(crate) sha256: String,
    pub(crate) chars: usize,
}

impl TextRef {
    pub(crate) fn of(text: &str) -> Self {
        Self {
            sha256: hex_sha256(text.as_bytes()),
            chars: text.chars().count(),
        }
    }
}

/// The accepted plan, as written before any effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct AcceptedPlan {
    pub(crate) node_id: String,
    pub(crate) continue_when: String,
    pub(crate) continue_text: Option<TextRef>,
    /// Targets to run, in run order; `target_id` is the index here.
    pub(crate) targets: Vec<PlannedTarget>,
    pub(crate) settled: Vec<SettledEntry>,
}

/// A digest of the request exactly as the caller made it — its typed
/// options, the follow-up wording included — and nothing a live plan would
/// add (pids, assignments, which agents exist now). The same request sent
/// twice digests the same even after the first one changed the world;
/// two requests that differ only in a same-length wording do not.
#[must_use]
pub(crate) fn request_digest(request: &impl Serialize) -> String {
    let canonical = serde_json::to_value(request).unwrap_or_default();
    hex_sha256(canonical.to_string().as_bytes())
}

/// A digest of the plan as accepted, kept for audit; never used to match a
/// repeated request.
fn plan_digest(plan: &AcceptedPlan) -> String {
    hex_sha256(serde_json::to_string(plan).unwrap_or_default().as_bytes())
}

fn hex_sha256(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Where a target is in its attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub(crate) enum Phase {
    Pending,
    Stopping,
    Launching,
    Readiness,
    Verifying,
    FollowThrough,
    Finished,
}

/// How a target's attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub(crate) enum Disposition {
    Swapped,
    Failed,
    /// Deliberately left on its account (it may still have been typed into).
    Kept,
    Skipped,
    /// Interrupted; what happened has to be looked at, not assumed.
    NeedsReconciliation,
}

/// A typed observation the executor made, never parsed from a note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub(crate) enum Observation {
    /// The process and conversation acted on, read just before the stop.
    Binding {
        pid: u32,
        process_start: Option<String>,
        conversation_id: String,
        verified: bool,
    },
    /// The account the relaunched process actually runs on.
    AccountCheck {
        result: AccountCheck,
        /// The account the process was found on, when it was found.
        observed: Option<String>,
        source: String,
    },
    Readiness {
        result: Readiness,
    },
    Phase {
        phase: Phase,
    },
    /// Whether the follow-up and the draft may be typed, decided from the
    /// relaunch's evidence before either is. Not that they were sent: the
    /// effects record that.
    Continuation {
        decision: ContinueDecision,
        withheld: Option<WithheldReason>,
        draft_allowed: bool,
    },
    /// What the transport said of a typed follow-up (its prompt and Enter),
    /// recorded as soon as both settled.
    FollowUpSent {
        kept: bool,
        submission: Submission,
    },
    /// What followed it, once the wait for evidence ended.
    FollowUpEvidence {
        kept: bool,
        evidence: TurnEvidence,
    },
}

/// What the transport said of a follow-up's prompt and its Enter. Not what
/// the agent did: a refused write may still have arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub(crate) enum Submission {
    /// Both were accepted.
    Delivered,
    /// One was refused.
    Refused,
    /// Not both settled (one never happened, or its result is unknown).
    Unconfirmed,
}

/// What followed a follow-up within the wait: evidence, never proof, that
/// it was taken up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub(crate) enum TurnEvidence {
    /// The agent's own prompt hook, for this conversation on this pane.
    PromptHook,
    /// The screen shows activity.
    ScreenActivity,
    /// Neither was seen; not proof that nothing happened.
    NoneSeen,
}

/// Whether a due follow-up may be typed into the relaunched agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub(crate) enum ContinueDecision {
    Allowed,
    /// No follow-up was due.
    NotDue,
    Withheld,
}

/// Why a due follow-up was withheld.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, strum::Display)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub(crate) enum WithheldReason {
    /// No relaunched agent process resumes exactly the planned conversation.
    BindingMissing,
    /// More than one does, or one resumes it more than once.
    BindingAmbiguous,
    /// An agent process's arguments could not be read.
    BindingUnreadable,
    /// `verified`: the conversation was taken on the registry's word.
    UnverifiedConversation,
    /// `verified`: the relaunch's account was not confirmed.
    AccountUnconfirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AccountCheck {
    /// Nothing was checked: `aas`-launched agents have no configured
    /// profile to recognise the process by.
    Unchecked,
    Matched,
    Mismatched,
    Unreadable,
    Unmapped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Readiness {
    Ready,
    Blocked,
    Exited,
    Unsettled,
}

/// A job as read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobStatus {
    pub(crate) job_id: String,
    pub(crate) node_id: String,
    pub(crate) revision: u64,
    pub(crate) state: String,
    pub(crate) reason: Option<String>,
    pub(crate) targets: Vec<TargetStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TargetStatus {
    pub(crate) target_id: u32,
    pub(crate) attempt: u32,
    pub(crate) phase: String,
    pub(crate) disposition: Option<String>,
    pub(crate) reason: Option<String>,
    /// An effect whose intent was recorded but whose result was not: it may
    /// or may not have happened.
    pub(crate) uncertain_effects: u32,
    pub(crate) effects: u32,
    /// The follow-up decision: `allowed`, `not_due`, or why it was
    /// withheld. `None` before one was made.
    pub(crate) continuation: Option<String>,
    /// The latest follow-up typed: the transport's word, and the evidence
    /// once its wait ended.
    pub(crate) follow_up: Option<crate::account_swap::FollowUpReport>,
}

/// A follow-up's transport word from its effects alone, for an attempt
/// that stopped before recording it (a crash, or a record that could not
/// be written): its latest group's prompt and Enter. No evidence.
fn follow_up_from_effects(
    connection: &Connection,
    job: &str,
    target: u32,
    attempt: u32,
) -> Result<Option<crate::account_swap::FollowUpReport>, JobError> {
    let mut statement = connection.prepare(
        "SELECT purpose, state FROM effects
         WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3
           AND purpose IN ('follow_up', 'kept_follow_up')
           AND grp = (SELECT MAX(grp) FROM effects
                      WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3
                        AND purpose IN ('follow_up', 'kept_follow_up'))",
    )?;
    let rows: Vec<(String, String)> = statement
        .query_map(params![job, target, attempt], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        .collect::<Result<_, _>>()?;
    let Some((purpose, _)) = rows.first() else {
        return Ok(None);
    };
    let submission = if rows.iter().any(|(_, state)| state == "refused") {
        Submission::Refused
    } else if rows.len() == 2 && rows.iter().all(|(_, state)| state == "accepted") {
        Submission::Delivered
    } else {
        Submission::Unconfirmed
    };
    Ok(Some(crate::account_swap::FollowUpReport {
        kept: purpose == "kept_follow_up",
        submission: submission.to_string(),
        evidence: None,
    }))
}

/// The follow-up from its two journalled observations. An attempt types at
/// most one (a kept session's or the relaunch's), so its evidence is the
/// one of the same kind; none yet means the wait never ended.
fn follow_up_status(
    sent: Option<String>,
    evidence: Option<String>,
) -> Option<crate::account_swap::FollowUpReport> {
    let parse = |json: Option<String>| json.and_then(|json| serde_json::from_str(&json).ok());
    let Some(Observation::FollowUpSent { kept, submission }) = parse(sent) else {
        return None;
    };
    let evidence = match parse(evidence) {
        Some(Observation::FollowUpEvidence {
            kept: evidence_kept,
            evidence,
        }) if evidence_kept == kept => Some(evidence.to_string()),
        _ => None,
    };
    Some(crate::account_swap::FollowUpReport {
        kept,
        submission: submission.to_string(),
        evidence,
    })
}

/// Effect purposes at or past the relaunch: once any was even intended, the
/// target may be running again, so a retry only observes it.
const LAUNCH_PURPOSES: &str = "'relaunch', 'respawn', 'keep_pane_open', 'release_pane', \
    'startup_dialog', 'follow_up', 'restore_draft'";

/// Phases at or past the relaunch.
const LAUNCH_PHASES: &str = "'launching', 'readiness', 'verifying', 'follow_through'";

/// A target of an ended job, as a retry sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetrySource {
    pub(crate) target_id: u32,
    pub(crate) attempt: u32,
    pub(crate) disposition: Option<String>,
    /// The root target it descends from (`job:target`).
    pub(crate) lineage: String,
    pub(crate) uncertain_effects: u32,
    /// Anything at or past the relaunch was intended or observed.
    pub(crate) launched: bool,
    /// The job that already retried this attempt.
    pub(crate) retried_by: Option<String>,
}

/// A retried target's claim on the parent attempt it retries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RetryLink {
    /// Its index in the retry's plan.
    pub(crate) target_index: usize,
    pub(crate) parent_target: u32,
    pub(crate) parent_attempt: u32,
    pub(crate) lineage: String,
}

impl RetryLink {
    fn parent_ref(&self, parent: &str) -> String {
        format!("{parent}:{}:{}", self.parent_target, self.parent_attempt)
    }
}

/// What [`SwapJobStore::create_job`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Created {
    pub(crate) job_id: String,
    /// The same request key and digest were seen before: that job.
    pub(crate) existing: bool,
}

pub(crate) struct SwapJobStore {
    path: PathBuf,
    /// The file opened, held so its identity can be compared with whatever
    /// the path names now.
    file: std::fs::File,
    connection: Mutex<Connection>,
    /// Latched once the path stopped naming that file: an intent committed
    /// to a renamed or replaced database is one a restarted manager would
    /// never see, so nothing is written again, even if the path comes back.
    invalid: AtomicBool,
}

fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

impl SwapJobStore {
    /// `<data dir>/muxa/swap-jobs.sqlite3`.
    #[must_use]
    pub(crate) fn default_path() -> Option<PathBuf> {
        crate::paths::data_dir().map(|dir| dir.join("muxa").join("swap-jobs.sqlite3"))
    }

    /// Open (creating if absent) and validate the store. Opening never
    /// recovers anything: that needs ownership first.
    pub(crate) fn open(path: &Path) -> Result<Self, JobError> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            create_private_dir(parent)?;
        }
        prepare_file(path)?;
        let file = std::fs::File::open(path)?;
        let mut connection = Connection::open(path)?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                connection.pragma_update(None, "journal_mode", "WAL")?;
                connection.pragma_update(None, "synchronous", "FULL")?;
                let tx = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
                tx.execute_batch(SCHEMA)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                tx.commit()?;
            }
            1 => {
                connection.pragma_update(None, "synchronous", "FULL")?;
                // Exclusive: no other connection reads a half-migrated file;
                // the version moves last, in the same transaction.
                let tx = connection.transaction_with_behavior(TransactionBehavior::Exclusive)?;
                tx.execute_batch(MIGRATE_1_TO_2)?;
                tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                tx.commit()?;
            }
            SCHEMA_VERSION => {
                connection.pragma_update(None, "synchronous", "FULL")?;
            }
            found => return Err(JobError::UnsupportedSchema { found }),
        }
        secure_files(path)?;
        let store = Self {
            path: path.to_path_buf(),
            file,
            connection: Mutex::new(connection),
            invalid: AtomicBool::new(false),
        };
        store.verify()?;
        Ok(store)
    }

    /// The path still names the file this store opened.
    fn verify(&self) -> Result<(), JobError> {
        if self.invalid.load(Ordering::Acquire) {
            return Err(self.replaced());
        }
        let same = self.file.metadata().and_then(|original| {
            std::fs::metadata(&self.path).map(|current| same_file(&original, &current))
        });
        if matches!(same, Ok(true)) {
            Ok(())
        } else {
            self.invalid.store(true, Ordering::Release);
            Err(self.replaced())
        }
    }

    fn replaced(&self) -> JobError {
        JobError::Store(format!(
            "the swap job database disappeared or was replaced: {}",
            self.path.display()
        ))
    }

    fn with<T>(
        &self,
        op: impl FnOnce(&mut Connection) -> Result<T, JobError>,
    ) -> Result<T, JobError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| JobError::Store("connection lock poisoned".into()))?;
        self.verify()?;
        secure_files(&self.path)?;
        let result = op(&mut connection);
        // A rename or replacement racing the operation means what it wrote
        // may not be where the next manager looks: never report it done.
        self.verify()?;
        result
    }

    /// Become the store's one manager. A previous owner is taken over only
    /// when `liveness` says it is gone.
    pub(crate) fn acquire(
        &self,
        me: &OwnerIdentity,
        liveness: &dyn Fn(u32, Option<&str>) -> Liveness,
    ) -> Result<Lease, JobError> {
        self.with(|connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let current: Option<(String, u32, Option<String>)> = tx
                .query_row("SELECT nonce, pid, process_start FROM owner WHERE id = 1", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .optional()?;
            if let Some((nonce, pid, start)) = current {
                if nonce != me.nonce && liveness(pid, start.as_deref()) == Liveness::Alive {
                    return Err(JobError::OwnedElsewhere { pid });
                }
            }
            tx.execute(
                "INSERT INTO owner (id, nonce, pid, process_start, acquired_at)
                 VALUES (1, ?1, ?2, ?3, ?4)
                 ON CONFLICT (id) DO UPDATE SET nonce = ?1, pid = ?2, process_start = ?3, acquired_at = ?4",
                params![me.nonce, me.pid, me.process_start, now()],
            )?;
            tx.commit()?;
            Ok(Lease {
                nonce: me.nonce.clone(),
            })
        })
    }

    /// Mark every job another owner left unfinished. A job that never did
    /// anything is abandoned; one that did is left for a person, with each
    /// unfinished target saying how far it got. Nothing is replayed.
    pub(crate) fn recover(&self, lease: &Lease) -> Result<Vec<String>, JobError> {
        self.with(|connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            fence(&tx, lease)?;
            let jobs: Vec<String> = {
                let mut statement = tx.prepare(
                    "SELECT job_id FROM jobs
                     WHERE state IN ('accepted', 'running') AND owner_nonce != ?1
                     ORDER BY created_at",
                )?;
                let rows = statement.query_map([&lease.nonce], |row| row.get(0))?;
                rows.collect::<Result<_, _>>()?
            };
            for job in &jobs {
                let effects: u32 = tx.query_row(
                    "SELECT COUNT(*) FROM effects WHERE job_id = ?1",
                    [job],
                    |row| row.get(0),
                )?;
                if effects == 0 {
                    tx.execute(
                        "UPDATE targets SET disposition = 'skipped', reason = 'not_started'
                         WHERE job_id = ?1 AND disposition IS NULL",
                        [job],
                    )?;
                    set_state(
                        &tx,
                        job,
                        "abandoned",
                        Some("its manager stopped before any effect"),
                    )?;
                    continue;
                }
                let unfinished: Vec<(u32, u32)> = {
                    let mut statement = tx.prepare(
                        "SELECT target_id, attempt FROM targets
                         WHERE job_id = ?1 AND disposition IS NULL",
                    )?;
                    let rows = statement.query_map([job], |row| Ok((row.get(0)?, row.get(1)?)))?;
                    rows.collect::<Result<_, _>>()?
                };
                for (target, attempt) in unfinished {
                    let reason = interrupted_reason(&tx, job, target, attempt)?;
                    tx.execute(
                        "UPDATE targets SET disposition = 'needs_reconciliation', reason = ?4
                         WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3",
                        params![job, target, attempt, reason],
                    )?;
                }
                set_state(
                    &tx,
                    job,
                    "needs_reconciliation",
                    Some("its manager stopped part way"),
                )?;
            }
            tx.commit()?;
            Ok(jobs)
        })
    }

    /// Whether `request_key` was used before: `Some(job)` for the same
    /// request, an error for a different one, `None` if unseen. Checked
    /// before any planning, so a repeated request is answered with the job
    /// it made rather than planned again against a world it changed.
    pub(crate) fn lookup(
        &self,
        request_key: &str,
        request_digest: &str,
    ) -> Result<Option<String>, JobError> {
        self.with(|connection| {
            let seen: Option<(String, String)> = connection
                .query_row(
                    "SELECT job_id, request_digest FROM jobs WHERE request_key = ?1",
                    [request_key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            match seen {
                None => Ok(None),
                Some((job, seen)) if seen == request_digest => Ok(Some(job)),
                Some(_) => Err(JobError::KeyReused(request_key.to_owned())),
            }
        })
    }

    /// [`SwapJobStore::status`] of the job a request key made.
    pub(crate) fn status_by_key(&self, request_key: &str) -> Result<JobStatus, JobError> {
        let job: String = self.with(|connection| {
            connection
                .query_row(
                    "SELECT job_id FROM jobs WHERE request_key = ?1",
                    [request_key],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| JobError::NotFound(request_key.to_owned()))
        })?;
        self.status(&job)
    }

    /// Accept a plan, written whole before any effect. The same request key
    /// with the same request digest returns the job it made before; with a
    /// different one, it is refused.
    pub(crate) fn create_job(
        &self,
        lease: &Lease,
        request_key: &str,
        request_digest: &str,
        plan: &AcceptedPlan,
    ) -> Result<Created, JobError> {
        self.with(|connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            fence(&tx, lease)?;
            let seen: Option<(String, String)> = tx
                .query_row(
                    "SELECT job_id, request_digest FROM jobs WHERE request_key = ?1",
                    [request_key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((job_id, seen)) = seen {
                return if seen == request_digest {
                    Ok(Created {
                        job_id,
                        existing: true,
                    })
                } else {
                    Err(JobError::KeyReused(request_key.to_owned()))
                };
            }
            let job_id = format!("swap-{}", uuid::Uuid::new_v4());
            let at = now();
            let plan_json =
                serde_json::to_string(plan).map_err(|error| JobError::Store(error.to_string()))?;
            tx.execute(
                "INSERT INTO jobs (job_id, node_id, request_key, request_digest, plan_digest,
                                   owner_nonce, state, plan_json, revision, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'accepted', ?7, 1, ?8, ?8)",
                params![
                    job_id,
                    plan.node_id,
                    request_key,
                    request_digest,
                    plan_digest(plan),
                    lease.nonce,
                    plan_json,
                    at
                ],
            )?;
            for index in 0..plan.targets.len() {
                tx.execute(
                    "INSERT INTO targets (job_id, target_id, attempt, phase, lineage)
                     VALUES (?1, ?2, 1, 'pending', ?3)",
                    params![job_id, index, format!("{job_id}:{index}")],
                )?;
            }
            tx.commit()?;
            Ok(Created {
                job_id,
                existing: false,
            })
        })
    }

    /// Each target of an ended `job`, at its latest attempt, as a retry
    /// would see it: how it ended, whether anything past the stop was ever
    /// attempted, and which later job already retried it.
    pub(crate) fn retry_sources(&self, job: &str) -> Result<Vec<RetrySource>, JobError> {
        self.with(|connection| {
            let state: String = connection
                .query_row("SELECT state FROM jobs WHERE job_id = ?1", [job], |row| {
                    row.get(0)
                })
                .optional()?
                .ok_or_else(|| JobError::NotFound(job.to_owned()))?;
            if matches!(state.as_str(), "accepted" | "running") {
                return Err(JobError::Invalid {
                    job: job.to_owned(),
                    why: "still running; retry it once it has ended".into(),
                });
            }
            let mut statement = connection.prepare(&format!(
                "SELECT t.target_id, t.attempt, t.disposition,
                        COALESCE(t.lineage, t.job_id || ':' || t.target_id),
                        (SELECT COUNT(*) FROM effects e WHERE e.job_id = t.job_id
                            AND e.target_id = t.target_id AND e.attempt = t.attempt
                            AND e.state = 'intent'),
                        (SELECT COUNT(*) FROM effects e WHERE e.job_id = t.job_id
                            AND e.target_id = t.target_id AND e.attempt = t.attempt
                            AND e.purpose IN ({LAUNCH_PURPOSES}))
                        + (SELECT COUNT(*) FROM observations o WHERE o.job_id = t.job_id
                            AND o.target_id = t.target_id AND o.attempt = t.attempt
                            AND (o.kind IN ('readiness', 'account_check', 'continuation')
                                 OR (o.kind = 'phase'
                                     AND json_extract(o.value_json, '$.phase') IN ({LAUNCH_PHASES})))),
                        (SELECT c.job_id FROM targets c
                            WHERE c.retry_of = t.job_id || ':' || t.target_id || ':' || t.attempt)
                 FROM targets t
                 WHERE t.job_id = ?1 AND t.attempt =
                     (SELECT MAX(l.attempt) FROM targets l
                      WHERE l.job_id = t.job_id AND l.target_id = t.target_id)
                 ORDER BY t.target_id"
            ))?;
            let sources = statement
                .query_map([job], |row| {
                    Ok(RetrySource {
                        target_id: row.get(0)?,
                        attempt: row.get(1)?,
                        disposition: row.get(2)?,
                        lineage: row.get(3)?,
                        uncertain_effects: row.get(4)?,
                        launched: row.get::<_, u32>(5)? > 0,
                        retried_by: row.get(6)?,
                    })
                })?
                .collect::<Result<_, _>>()?;
            Ok(sources)
        })
    }

    /// Accept a retry of `parent`: like [`Self::create_job`], and each
    /// target in `links` claims the parent attempt it retries in the same
    /// transaction. An attempt another retry already claimed refuses the
    /// whole retry, as does one that is no longer its target's latest.
    pub(crate) fn create_retry_job(
        &self,
        lease: &Lease,
        request_key: &str,
        request_digest: &str,
        plan: &AcceptedPlan,
        parent: &str,
        links: &[RetryLink],
    ) -> Result<Created, JobError> {
        self.with(|connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            fence(&tx, lease)?;
            let seen: Option<(String, String)> = tx
                .query_row(
                    "SELECT job_id, request_digest FROM jobs WHERE request_key = ?1",
                    [request_key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((job_id, seen)) = seen {
                return if seen == request_digest {
                    Ok(Created {
                        job_id,
                        existing: true,
                    })
                } else {
                    Err(JobError::KeyReused(request_key.to_owned()))
                };
            }
            for link in links {
                let latest: Option<u32> = tx.query_row(
                    "SELECT MAX(attempt) FROM targets WHERE job_id = ?1 AND target_id = ?2",
                    params![parent, link.parent_target],
                    |row| row.get(0),
                )?;
                let claimed: Option<String> = tx
                    .query_row(
                        "SELECT job_id FROM targets WHERE retry_of = ?1",
                        [link.parent_ref(parent)],
                        |row| row.get(0),
                    )
                    .optional()?;
                if latest != Some(link.parent_attempt) || claimed.is_some() {
                    return Err(JobError::Invalid {
                        job: parent.to_owned(),
                        why: format!(
                            "target {} was retried meanwhile{}",
                            link.parent_target,
                            claimed.map_or_else(String::new, |job| format!(" (by {job})"))
                        ),
                    });
                }
            }
            let job_id = format!("swap-{}", uuid::Uuid::new_v4());
            let at = now();
            let plan_json =
                serde_json::to_string(plan).map_err(|error| JobError::Store(error.to_string()))?;
            tx.execute(
                "INSERT INTO jobs (job_id, node_id, request_key, request_digest, plan_digest,
                                   owner_nonce, state, plan_json, revision, created_at, updated_at,
                                   retry_of)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'accepted', ?7, 1, ?8, ?8, ?9)",
                params![
                    job_id,
                    plan.node_id,
                    request_key,
                    request_digest,
                    plan_digest(plan),
                    lease.nonce,
                    plan_json,
                    at,
                    parent
                ],
            )?;
            for index in 0..plan.targets.len() {
                let link = links.iter().find(|link| link.target_index == index);
                tx.execute(
                    "INSERT INTO targets (job_id, target_id, attempt, phase, lineage, retry_of)
                     VALUES (?1, ?2, 1, 'pending', ?3, ?4)",
                    params![
                        job_id,
                        index,
                        link.map_or_else(
                            || format!("{job_id}:{index}"),
                            |link| link.lineage.clone()
                        ),
                        link.map(|link| link.parent_ref(parent))
                    ],
                )?;
            }
            tx.commit()?;
            Ok(Created {
                job_id,
                existing: false,
            })
        })
    }

    /// The accepted plan, as written.
    pub(crate) fn plan(&self, job: &str) -> Result<AcceptedPlan, JobError> {
        self.with(|connection| {
            let json: String = connection
                .query_row(
                    "SELECT plan_json FROM jobs WHERE job_id = ?1",
                    [job],
                    |row| row.get(0),
                )
                .optional()?
                .ok_or_else(|| JobError::NotFound(job.to_owned()))?;
            serde_json::from_str(&json).map_err(|error| JobError::Store(error.to_string()))
        })
    }

    pub(crate) fn start(&self, lease: &Lease, job: &str) -> Result<(), JobError> {
        self.transition(lease, job, "accepted", "running")
    }

    /// Record how a target's attempt ended.
    pub(crate) fn finish_target(
        &self,
        lease: &Lease,
        job: &str,
        target: u32,
        attempt: u32,
        disposition: Disposition,
    ) -> Result<(), JobError> {
        self.with(|connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            fence(&tx, lease)?;
            owned_running(&tx, lease, job)?;
            if disposition != Disposition::NeedsReconciliation
                && open_intents(&tx, job, target, attempt)? > 0
            {
                return Err(JobError::Invalid {
                    job: job.to_owned(),
                    why: format!(
                        "target {target} has an effect with no recorded result; \
                         it can only end as needs_reconciliation"
                    ),
                });
            }
            let changed = tx.execute(
                "UPDATE targets SET disposition = ?4, phase = 'finished'
                 WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3 AND disposition IS NULL",
                params![job, target, attempt, disposition.to_string()],
            )?;
            if changed != 1 {
                return Err(JobError::Invalid {
                    job: job.to_owned(),
                    why: format!("target {target} attempt {attempt} is not open"),
                });
            }
            bump(&tx, job)?;
            tx.commit()?;
            Ok(())
        })
    }

    /// The job ended: every target's outcome is already recorded. Says
    /// nothing about whether each one succeeded.
    pub(crate) fn finish(&self, lease: &Lease, job: &str) -> Result<(), JobError> {
        self.with(|connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            fence(&tx, lease)?;
            owned_running(&tx, lease, job)?;
            let open: u32 = tx.query_row(
                "SELECT COUNT(*) FROM targets WHERE job_id = ?1 AND disposition IS NULL",
                [job],
                |row| row.get(0),
            )?;
            if open > 0 {
                return Err(JobError::Invalid {
                    job: job.to_owned(),
                    why: format!("{open} target(s) have no recorded outcome yet"),
                });
            }
            set_state(&tx, job, "done", None)?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Record a typed observation for a target's attempt; a phase also
    /// moves the target.
    pub(crate) fn observe(
        &self,
        lease: &Lease,
        job: &str,
        target: u32,
        attempt: u32,
        observation: &Observation,
    ) -> Result<(), JobError> {
        self.with(|connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            fence(&tx, lease)?;
            owned_running(&tx, lease, job)?;
            open_attempt(&tx, job, target, attempt)?;
            let seq: u32 = tx.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM observations
                 WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3",
                params![job, target, attempt],
                |row| row.get(0),
            )?;
            let value =
                serde_json::to_value(observation).map_err(|error| JobError::Store(error.to_string()))?;
            let kind = value["kind"].as_str().unwrap_or_default().to_owned();
            tx.execute(
                "INSERT INTO observations (job_id, target_id, attempt, seq, kind, value_json, observed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![job, target, attempt, seq, kind, value.to_string(), now()],
            )?;
            if let Observation::Phase { phase } = observation {
                tx.execute(
                    "UPDATE targets SET phase = ?4 WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3",
                    params![job, target, attempt, phase.to_string()],
                )?;
            }
            bump(&tx, job)?;
            tx.commit()?;
            Ok(())
        })
    }

    /// A job and its targets, read back.
    pub(crate) fn status(&self, job: &str) -> Result<JobStatus, JobError> {
        self.with(|connection| {
            let (state, reason, node_id, revision): (String, Option<String>, String, u64) =
                connection
                    .query_row(
                        "SELECT state, reason, node_id, revision FROM jobs WHERE job_id = ?1",
                        [job],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()?
                    .ok_or_else(|| JobError::NotFound(job.to_owned()))?;
            let mut statement = connection.prepare(
                "SELECT t.target_id, t.attempt, t.phase, t.disposition, t.reason,
                        (SELECT COUNT(*) FROM effects e WHERE e.job_id = t.job_id
                            AND e.target_id = t.target_id AND e.attempt = t.attempt
                            AND e.state = 'intent'),
                        (SELECT COUNT(*) FROM effects e WHERE e.job_id = t.job_id
                            AND e.target_id = t.target_id AND e.attempt = t.attempt),
                        (SELECT o.value_json FROM observations o WHERE o.job_id = t.job_id
                            AND o.target_id = t.target_id AND o.attempt = t.attempt
                            AND o.kind = 'continuation' ORDER BY o.seq DESC LIMIT 1),
                        (SELECT o.value_json FROM observations o WHERE o.job_id = t.job_id
                            AND o.target_id = t.target_id AND o.attempt = t.attempt
                            AND o.kind = 'follow_up_sent' ORDER BY o.seq DESC LIMIT 1),
                        (SELECT o.value_json FROM observations o WHERE o.job_id = t.job_id
                            AND o.target_id = t.target_id AND o.attempt = t.attempt
                            AND o.kind = 'follow_up_evidence' ORDER BY o.seq DESC LIMIT 1)
                 FROM targets t WHERE t.job_id = ?1 ORDER BY t.target_id, t.attempt",
            )?;
            let targets = statement
                .query_map([job], |row| {
                    Ok(TargetStatus {
                        target_id: row.get(0)?,
                        attempt: row.get(1)?,
                        phase: row.get(2)?,
                        disposition: row.get(3)?,
                        reason: row.get(4)?,
                        uncertain_effects: row.get(5)?,
                        effects: row.get(6)?,
                        continuation: row
                            .get::<_, Option<String>>(7)?
                            .and_then(|json| serde_json::from_str::<Observation>(&json).ok())
                            .and_then(|observation| match observation {
                                Observation::Continuation {
                                    decision, withheld, ..
                                } => Some(withheld.map_or_else(
                                    || decision.to_string(),
                                    |reason| reason.to_string(),
                                )),
                                _ => None,
                            }),
                        follow_up: follow_up_status(row.get(8)?, row.get(9)?),
                    })
                })?
                .collect::<Result<Vec<TargetStatus>, _>>()?;
            let mut targets = targets;
            for target in &mut targets {
                if target.follow_up.is_none() {
                    target.follow_up =
                        follow_up_from_effects(connection, job, target.target_id, target.attempt)?;
                }
            }
            Ok(JobStatus {
                job_id: job.to_owned(),
                node_id,
                revision,
                state,
                reason,
                targets,
            })
        })
    }

    /// Hold the connection, so every other use of the store waits.
    #[cfg(test)]
    pub(crate) fn hold(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection.lock().unwrap()
    }

    /// This manager still owns a store whose file is still its own.
    pub(crate) fn check(&self, lease: &Lease) -> Result<(), JobError> {
        self.with(|connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Deferred)?;
            fence(&tx, lease)?;
            Ok(())
        })
    }

    /// The executor's observer for one target's attempt.
    pub(crate) fn journal<'a>(
        &'a self,
        lease: &'a Lease,
        job: &str,
        target: u32,
        attempt: u32,
    ) -> Journal<'a> {
        Journal {
            store: self,
            lease,
            job: job.to_owned(),
            target,
            attempt,
            pending: None,
            failed: false,
            effects: 0,
        }
    }

    fn transition(&self, lease: &Lease, job: &str, from: &str, to: &str) -> Result<(), JobError> {
        self.with(|connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            fence(&tx, lease)?;
            let changed = tx.execute(
                "UPDATE jobs SET state = ?3, revision = revision + 1, updated_at = ?4
                 WHERE job_id = ?1 AND state = ?2 AND owner_nonce = ?5",
                params![job, from, to, now(), lease.nonce],
            )?;
            if changed != 1 {
                return Err(JobError::Invalid {
                    job: job.to_owned(),
                    why: format!("not {from} under this manager"),
                });
            }
            tx.commit()?;
            Ok(())
        })
    }
}

/// [`EffectObserver`] writing one target attempt's effects: the intent is
/// committed before the effect happens, the result after. An intent that
/// cannot be committed stops the run before the effect; a result that cannot
/// be committed leaves the intent standing, which is read as "may or may not
/// have happened".
pub(crate) struct Journal<'a> {
    store: &'a SwapJobStore,
    lease: &'a Lease,
    job: String,
    target: u32,
    attempt: u32,
    /// The intent written by the last `before`, awaiting its `after`.
    pending: Option<Pending>,
    /// A write to the store failed: the job cannot go on.
    failed: bool,
    /// Intents written through this journal.
    effects: u32,
}

impl Journal<'_> {
    /// An effect was started whose result is not recorded.
    pub(crate) fn uncertain(&self) -> bool {
        self.pending.is_some()
    }

    /// The store refused a write at some point.
    pub(crate) fn failed(&self) -> bool {
        self.failed
    }

    /// How many effects were started through this journal.
    pub(crate) fn effects(&self) -> u32 {
        self.effects
    }
}

/// An intent awaiting its result: which row, and which effect it was for.
struct Pending {
    seq: u32,
    nonce: String,
    effect: Effect,
}

impl EffectObserver for Journal<'_> {
    fn observe(&mut self, observation: &Observation) -> Result<(), ObserverError> {
        self.store
            .observe(
                self.lease,
                &self.job,
                self.target,
                self.attempt,
                observation,
            )
            .map_err(|error| {
                self.failed = true;
                ObserverError(error.to_string())
            })
    }

    fn before(&mut self, effect: &Effect) -> Result<(), ObserverError> {
        if self.pending.is_some() {
            return Err(ObserverError(
                "an earlier effect has no recorded result".into(),
            ));
        }
        let nonce = uuid::Uuid::new_v4().to_string();
        let written = self.store.with(|connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            fence(&tx, self.lease)?;
            owned_running(&tx, self.lease, &self.job)?;
            open_attempt(&tx, &self.job, self.target, self.attempt)?;
            // Held in the file, not in this adapter: another journal for
            // the same attempt, or one made after a restart, sees it too.
            if open_intents(&tx, &self.job, self.target, self.attempt)? > 0 {
                return Err(JobError::Invalid {
                    job: self.job.clone(),
                    why: format!(
                        "target {} has an effect with no recorded result",
                        self.target
                    ),
                });
            }
            let seq: u32 = tx.query_row(
                "SELECT COALESCE(MAX(seq), 0) + 1 FROM effects
                     WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3",
                params![self.job, self.target, self.attempt],
                |row| row.get(0),
            )?;
            tx.execute(
                "INSERT INTO effects (job_id, target_id, attempt, seq, nonce, grp, purpose,
                                          op_json, state, intent_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'intent', ?9)",
                params![
                    self.job,
                    self.target,
                    self.attempt,
                    seq,
                    nonce,
                    effect.group,
                    purpose_name(effect.purpose),
                    op_json(&effect.op),
                    now()
                ],
            )?;
            tx.commit()?;
            Ok(seq)
        });
        let seq = written.map_err(|error| {
            self.failed = true;
            ObserverError(error.to_string())
        })?;
        self.effects += 1;
        self.pending = Some(Pending {
            seq,
            nonce,
            effect: effect.clone(),
        });
        Ok(())
    }

    /// Settle the pending intent, and only for the effect it was written
    /// for. A mismatched or failed settlement keeps it pending, so nothing
    /// further can be started on this attempt.
    fn after(&mut self, effect: &Effect, transport: Transport) -> Result<(), ObserverError> {
        let Some(pending) = &self.pending else {
            return Err(ObserverError(
                "a result arrived with no recorded intent".into(),
            ));
        };
        if pending.effect != *effect {
            return Err(ObserverError(format!(
                "a result for {:?} arrived while {:?} is pending",
                effect.purpose, pending.effect.purpose
            )));
        }
        let state = match transport {
            Transport::Accepted => "accepted",
            Transport::Refused => "refused",
            Transport::Unconfirmed => "unconfirmed",
        };
        let (seq, nonce) = (pending.seq, pending.nonce.clone());
        let settled = self.store.with(|connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            fence(&tx, self.lease)?;
            let changed = tx.execute(
                "UPDATE effects SET state = ?6, result_at = ?7
                     WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3 AND seq = ?4
                       AND nonce = ?5 AND state = 'intent'",
                params![
                    self.job,
                    self.target,
                    self.attempt,
                    seq,
                    nonce,
                    state,
                    now()
                ],
            )?;
            if changed != 1 {
                return Err(JobError::Invalid {
                    job: self.job.clone(),
                    why: format!("effect {seq} has no open intent"),
                });
            }
            tx.commit()?;
            Ok(())
        });
        settled.map_err(|error| {
            self.failed = true;
            ObserverError(error.to_string())
        })?;
        self.pending = None;
        Ok(())
    }
}

/// The target's attempt has not ended: no new effect or observation is
/// taken for one that has.
fn open_attempt(
    tx: &rusqlite::Transaction,
    job: &str,
    target: u32,
    attempt: u32,
) -> Result<(), JobError> {
    let disposition: Option<Option<String>> = tx
        .query_row(
            "SELECT disposition FROM targets WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3",
            params![job, target, attempt],
            |row| row.get(0),
        )
        .optional()?;
    match disposition {
        Some(None) => Ok(()),
        Some(Some(ended)) => Err(JobError::Invalid {
            job: job.to_owned(),
            why: format!("target {target} attempt {attempt} already ended ({ended})"),
        }),
        None => Err(JobError::Invalid {
            job: job.to_owned(),
            why: format!("target {target} attempt {attempt} does not exist"),
        }),
    }
}

/// Effects of this attempt whose intent has no result.
fn open_intents(
    tx: &rusqlite::Transaction,
    job: &str,
    target: u32,
    attempt: u32,
) -> Result<u32, JobError> {
    Ok(tx.query_row(
        "SELECT COUNT(*) FROM effects WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3
           AND state = 'intent'",
        params![job, target, attempt],
        |row| row.get(0),
    )?)
}

#[cfg(unix)]
fn same_file(original: &std::fs::Metadata, current: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    original.dev() == current.dev() && original.ino() == current.ino()
}

#[cfg(not(unix))]
fn same_file(original: &std::fs::Metadata, current: &std::fs::Metadata) -> bool {
    // Without a stable creation time there is no identity to compare: fail
    // closed rather than assume the path still names the opened file.
    matches!((original.created(), current.created()), (Ok(a), Ok(b)) if a == b)
}

fn purpose_name(purpose: Purpose) -> String {
    serde_json::to_value(purpose)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn op_json(op: &Op) -> String {
    serde_json::to_string(op).unwrap_or_default()
}

/// Refuse a write from anyone but the current owner.
fn fence(tx: &rusqlite::Transaction, lease: &Lease) -> Result<(), JobError> {
    let owner: Option<String> = tx
        .query_row("SELECT nonce FROM owner WHERE id = 1", [], |row| row.get(0))
        .optional()?;
    if owner.as_deref() == Some(lease.nonce.as_str()) {
        Ok(())
    } else {
        Err(JobError::Fenced)
    }
}

/// The job is running and was started by this manager.
fn owned_running(tx: &rusqlite::Transaction, lease: &Lease, job: &str) -> Result<(), JobError> {
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT state, owner_nonce FROM jobs WHERE job_id = ?1",
            [job],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match row {
        None => Err(JobError::NotFound(job.to_owned())),
        Some((state, owner)) if state == "running" && owner == lease.nonce => Ok(()),
        Some((state, _)) => Err(JobError::Invalid {
            job: job.to_owned(),
            why: format!("is {state}, not running under this manager"),
        }),
    }
}

fn set_state(
    tx: &rusqlite::Transaction,
    job: &str,
    state: &str,
    reason: Option<&str>,
) -> Result<(), JobError> {
    tx.execute(
        "UPDATE jobs SET state = ?2, reason = ?3, revision = revision + 1, updated_at = ?4
         WHERE job_id = ?1",
        params![job, state, reason, now()],
    )?;
    Ok(())
}

fn bump(tx: &rusqlite::Transaction, job: &str) -> Result<(), JobError> {
    tx.execute(
        "UPDATE jobs SET revision = revision + 1, updated_at = ?2 WHERE job_id = ?1",
        params![job, now()],
    )?;
    Ok(())
}

/// How far an interrupted target got, from what was written, never from a
/// note: an effect with no result first, then the last recorded effect.
fn interrupted_reason(
    tx: &rusqlite::Transaction,
    job: &str,
    target: u32,
    attempt: u32,
) -> Result<String, JobError> {
    let uncertain: Option<String> = tx
        .query_row(
            "SELECT purpose FROM effects WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3
               AND state = 'intent' ORDER BY seq DESC LIMIT 1",
            params![job, target, attempt],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(purpose) = uncertain {
        return Ok(format!("uncertain:{purpose}"));
    }
    let last: Option<String> = tx
        .query_row(
            "SELECT purpose FROM effects WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3
             ORDER BY seq DESC LIMIT 1",
            params![job, target, attempt],
            |row| row.get(0),
        )
        .optional()?;
    Ok(last.map_or_else(
        || "not_started".to_owned(),
        |purpose| format!("after:{purpose}"),
    ))
}

/// A directory only its owner can enter.
fn create_private_dir(path: &Path) -> Result<(), JobError> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Create the database file owner-only before `SQLite` opens it, so there is
/// no first-open window with the umask's permissions.
fn prepare_file(path: &Path) -> Result<(), JobError> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?;
    secure_files(path)
}

/// Owner-only permissions on the database and its WAL and shared-memory
/// files.
fn secure_files(path: &Path) -> Result<(), JobError> {
    #[cfg(unix)]
    {
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
    Ok(())
}
