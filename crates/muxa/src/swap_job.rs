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
use std::sync::Mutex;

/// The schema this build writes and reads. Any other version is refused
/// without being touched.
const SCHEMA_VERSION: i64 = 1;

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
    fingerprint TEXT NOT NULL,
    owner_nonce TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN
        ('accepted', 'running', 'done', 'needs_reconciliation', 'abandoned')),
    cancel_requested INTEGER NOT NULL DEFAULT 0,
    plan_json TEXT NOT NULL,
    revision INTEGER NOT NULL,
    reason TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
CREATE TABLE targets (
    job_id TEXT NOT NULL REFERENCES jobs (job_id),
    target_id INTEGER NOT NULL,
    attempt INTEGER NOT NULL,
    phase TEXT NOT NULL,
    disposition TEXT,
    reason TEXT,
    PRIMARY KEY (job_id, target_id, attempt)
);
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
    pub(crate) assigned_account: String,
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
    pub(crate) targets: Vec<PlannedTarget>,
}

/// A digest of the full request, follow-up text included, so two requests
/// that differ only in a same-length wording never dedupe as one.
#[must_use]
pub(crate) fn fingerprint(plan: &AcceptedPlan, continue_text: Option<&str>) -> String {
    let canonical = serde_json::json!({
        "plan": plan,
        "continue_text": continue_text,
    });
    hex_sha256(canonical.to_string().as_bytes())
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
        source: String,
    },
    Readiness {
        result: Readiness,
    },
    Phase {
        phase: Phase,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AccountCheck {
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
}

/// What [`SwapJobStore::create_job`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Created {
    pub(crate) job_id: String,
    /// The same request key and fingerprint were seen before: that job.
    pub(crate) existing: bool,
}

pub(crate) struct SwapJobStore {
    path: PathBuf,
    connection: Mutex<Connection>,
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
            SCHEMA_VERSION => {
                connection.pragma_update(None, "synchronous", "FULL")?;
            }
            found => return Err(JobError::UnsupportedSchema { found }),
        }
        secure_files(path)?;
        Ok(Self {
            path: path.to_path_buf(),
            connection: Mutex::new(connection),
        })
    }

    fn with<T>(
        &self,
        op: impl FnOnce(&mut Connection) -> Result<T, JobError>,
    ) -> Result<T, JobError> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| JobError::Store("connection lock poisoned".into()))?;
        let result = op(&mut connection);
        secure_files(&self.path)?;
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

    /// Accept a plan, written whole before any effect. The same request key
    /// with the same fingerprint returns the job it made before; with a
    /// different one, it is refused.
    pub(crate) fn create_job(
        &self,
        lease: &Lease,
        request_key: &str,
        fingerprint: &str,
        plan: &AcceptedPlan,
    ) -> Result<Created, JobError> {
        self.with(|connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            fence(&tx, lease)?;
            let seen: Option<(String, String)> = tx
                .query_row(
                    "SELECT job_id, fingerprint FROM jobs WHERE request_key = ?1",
                    [request_key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((job_id, seen)) = seen {
                return if seen == fingerprint {
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
                "INSERT INTO jobs (job_id, node_id, request_key, fingerprint, owner_nonce, state,
                                   plan_json, revision, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, 'accepted', ?6, 1, ?7, ?7)",
                params![job_id, plan.node_id, request_key, fingerprint, lease.nonce, plan_json, at],
            )?;
            for index in 0..plan.targets.len() {
                tx.execute(
                    "INSERT INTO targets (job_id, target_id, attempt, phase) VALUES (?1, ?2, 1, 'pending')",
                    params![job_id, index],
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
            let (state, reason): (String, Option<String>) = connection
                .query_row(
                    "SELECT state, reason FROM jobs WHERE job_id = ?1",
                    [job],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .ok_or_else(|| JobError::NotFound(job.to_owned()))?;
            let mut statement = connection.prepare(
                "SELECT t.target_id, t.attempt, t.phase, t.disposition, t.reason,
                        (SELECT COUNT(*) FROM effects e WHERE e.job_id = t.job_id
                            AND e.target_id = t.target_id AND e.attempt = t.attempt
                            AND e.state = 'intent'),
                        (SELECT COUNT(*) FROM effects e WHERE e.job_id = t.job_id
                            AND e.target_id = t.target_id AND e.attempt = t.attempt)
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
                    })
                })?
                .collect::<Result<_, _>>()?;
            Ok(JobStatus {
                job_id: job.to_owned(),
                state,
                reason,
                targets,
            })
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
    pending: Option<u32>,
}

impl EffectObserver for Journal<'_> {
    fn before(&mut self, effect: &Effect) -> Result<(), ObserverError> {
        if self.pending.is_some() {
            return Err(ObserverError(
                "an earlier effect has no recorded result".into(),
            ));
        }
        let seq = self
            .store
            .with(|connection| {
                let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                fence(&tx, self.lease)?;
                owned_running(&tx, self.lease, &self.job)?;
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
                        uuid::Uuid::new_v4().to_string(),
                        effect.group,
                        purpose_name(effect.purpose),
                        op_json(&effect.op),
                        now()
                    ],
                )?;
                tx.commit()?;
                Ok(seq)
            })
            .map_err(|error| ObserverError(error.to_string()))?;
        self.pending = Some(seq);
        Ok(())
    }

    fn after(&mut self, _effect: &Effect, transport: Transport) -> Result<(), ObserverError> {
        let Some(seq) = self.pending.take() else {
            return Err(ObserverError(
                "a result arrived with no recorded intent".into(),
            ));
        };
        let state = match transport {
            Transport::Accepted => "accepted",
            Transport::Refused => "refused",
            Transport::Unconfirmed => "unconfirmed",
        };
        self.store
            .with(|connection| {
                let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                fence(&tx, self.lease)?;
                let changed = tx.execute(
                    "UPDATE effects SET state = ?5, result_at = ?6
                     WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3 AND seq = ?4
                       AND state = 'intent'",
                    params![self.job, self.target, self.attempt, seq, state, now()],
                )?;
                if changed != 1 {
                    return Err(JobError::Invalid {
                        job: self.job.clone(),
                        why: format!("effect {seq} has no open intent"),
                    });
                }
                tx.commit()?;
                Ok(())
            })
            .map_err(|error| ObserverError(error.to_string()))
    }
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
