//! Restart handling. Only a pending attempt with a complete, empty effect
//! journal may be deferred. Every resume claims a fresh attempt atomically.
use super::{
    fence, interrupted_reason, params, set_state, AcceptedPlan, JobError, Lease, SwapJobStore,
    TransactionBehavior,
};

const LATEST: &str = "t.attempt = (SELECT MAX(l.attempt) FROM targets l
    WHERE l.job_id = t.job_id AND l.target_id = t.target_id)";

pub(super) fn aggregate(tx: &rusqlite::Transaction<'_>, job: &str) -> Result<(), JobError> {
    let (open, uncertain, deferred): (u32, u32, u32) = tx.query_row(
        &format!(
            "SELECT COALESCE(SUM(disposition IS NULL), 0),
            COALESCE(SUM(disposition = 'needs_reconciliation'), 0),
            COALESCE(SUM(disposition = 'deferred'), 0)
            FROM targets t WHERE job_id = ?1 AND {LATEST}"
        ),
        [job],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    // Active work remains watchable, but never lose its sibling's warning.
    let state = if open > 0 {
        "running"
    } else if uncertain > 0 {
        "needs_reconciliation"
    } else if deferred > 0 {
        "deferred"
    } else {
        "done"
    };
    let reason = if uncertain > 0 {
        Some("its manager stopped part way")
    } else if deferred > 0 {
        Some("untouched targets are deferred")
    } else {
        None
    };
    set_state(tx, job, state, reason)
}

pub(super) fn defer_one(
    tx: &rusqlite::Transaction<'_>,
    job: &str,
    target: u32,
    attempt: u32,
    reason: &str,
) -> Result<(), JobError> {
    let cancelled: bool = tx.query_row(
        "SELECT cancel_requested FROM jobs WHERE job_id = ?1",
        [job],
        |row| row.get(0),
    )?;
    let changed = tx.execute(
        "UPDATE targets SET disposition = ?4, reason = ?5
        WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3
          AND phase = 'pending' AND disposition IS NULL
          AND attempt = (SELECT MAX(l.attempt) FROM targets l
              WHERE l.job_id = ?1 AND l.target_id = ?2)
          AND NOT EXISTS (SELECT 1 FROM effects e WHERE e.job_id = ?1
              AND e.target_id = ?2 AND e.attempt = ?3)
          AND NOT EXISTS (SELECT 1 FROM observations o WHERE o.job_id = ?1
              AND o.target_id = ?2 AND o.attempt = ?3)
          AND EXISTS (SELECT 1 FROM jobs j WHERE j.job_id = ?1 AND j.evidence_pruned_at IS NULL)",
        params![
            job,
            target,
            attempt,
            if cancelled { "skipped" } else { "deferred" },
            if cancelled { "cancelled" } else { reason }
        ],
    )?;
    if changed != 1 {
        return Err(JobError::Invalid {
            job: job.into(),
            why: "attempt is not provably untouched".into(),
        });
    }
    Ok(())
}

impl SwapJobStore {
    /// Fence starts and settle pending siblings even if a worker is inside
    /// an effect. The same IMMEDIATE transaction orders this against start/cancel.
    pub(crate) fn begin_drain(&self, lease: &Lease) -> Result<(), JobError> {
        self.with(|connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            fence(&tx, lease)?;
            tx.execute(
                "UPDATE owner SET draining = 1 WHERE nonce = ?1",
                [&lease.nonce],
            )?;
            let pending = {
                let mut stmt = tx.prepare(&format!(
                    "SELECT t.job_id, t.target_id, t.attempt
                    FROM targets t JOIN jobs j ON j.job_id = t.job_id
                    WHERE j.owner_nonce = ?1 AND t.disposition IS NULL AND t.phase = 'pending'
                    AND {LATEST} AND j.evidence_pruned_at IS NULL
                    AND NOT EXISTS (SELECT 1 FROM effects e WHERE e.job_id = t.job_id
                        AND e.target_id = t.target_id AND e.attempt = t.attempt)
                    AND NOT EXISTS (SELECT 1 FROM observations o WHERE o.job_id = t.job_id
                        AND o.target_id = t.target_id AND o.attempt = t.attempt)"
                ))?;
                let rows = stmt.query_map([&lease.nonce], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, u32>(1)?,
                        r.get::<_, u32>(2)?,
                    ))
                })?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            for (job, target, attempt) in pending {
                defer_one(&tx, &job, target, attempt, "daemon_shutdown")?;
                aggregate(&tx, &job)?;
            }
            tx.commit()?;
            Ok(())
        })
    }

    pub(super) fn recover_targets(&self, lease: &Lease) -> Result<Vec<String>, JobError> {
        self.with(|connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            fence(&tx, lease)?;
            let jobs = {
                let mut stmt = tx.prepare("SELECT job_id FROM jobs WHERE state IN ('accepted', 'running')
                    AND owner_nonce != ?1 ORDER BY created_at")?;
                let rows = stmt.query_map([&lease.nonce], |r| r.get::<_, String>(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            };
            for job in &jobs {
                let (plan_json, pruned): (String, Option<String>) = tx.query_row(
                    "SELECT plan_json, evidence_pruned_at FROM jobs WHERE job_id = ?1", [job],
                    |r| Ok((r.get(0)?, r.get(1)?)))?;
                let plan: AcceptedPlan = serde_json::from_str(&plan_json).map_err(|e| JobError::Store(e.to_string()))?;
                let pending = {
                    let mut stmt = tx.prepare(&format!("SELECT t.target_id, t.attempt, t.phase,
                        (SELECT COUNT(*) FROM effects e WHERE e.job_id = t.job_id AND e.target_id = t.target_id AND e.attempt = t.attempt),
                        (SELECT COUNT(*) FROM observations o WHERE o.job_id = t.job_id AND o.target_id = t.target_id AND o.attempt = t.attempt)
                        FROM targets t WHERE job_id = ?1 AND disposition IS NULL AND {LATEST}"))?;
                    let rows = stmt.query_map([job], |r| Ok((r.get::<_, u32>(0)?, r.get::<_, u32>(1)?, r.get::<_, String>(2)?, r.get::<_, u32>(3)?, r.get::<_, u32>(4)?)))?;
                    rows.collect::<Result<Vec<_>, _>>()?
                };
                for (target, attempt, phase, effects, observations) in pending {
                    if phase == "pending" && effects == 0 && observations == 0 && pruned.is_none() {
                        defer_one(&tx, job, target, attempt, "manager_crash")?;
                        // Legacy/automation jobs have no reconstructable authority.
                        let ineligible = match plan.resume.as_ref() {
                            None => Some("resume_policy_unavailable"),
                            Some(p) if !p.public => Some("automation_not_resumed"),
                            Some(_) => None,
                        };
                        if let Some(reason) = ineligible {
                            tx.execute("UPDATE targets SET disposition = 'skipped', phase = 'finished', reason = ?4
                                WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3 AND disposition = 'deferred'",
                                params![job, target, attempt, reason])?;
                        }
                    } else {
                        let reason = interrupted_reason(&tx, job, target, attempt)?;
                        tx.execute("UPDATE targets SET disposition = 'needs_reconciliation', reason = ?4
                            WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3 AND disposition IS NULL",
                            params![job, target, attempt, reason])?;
                    }
                }
                aggregate(&tx, job)?;
                // Preserve the old no-policy, no-effect abandoned verdict.
                if plan.resume.is_none() {
                    let effects: u32 = tx.query_row("SELECT COUNT(*) FROM effects WHERE job_id = ?1", [job], |r| r.get(0))?;
                    let started: u32 = tx.query_row("SELECT COUNT(*) FROM targets WHERE job_id = ?1 AND disposition = 'needs_reconciliation'", [job], |r| r.get(0))?;
                    if effects == 0 && started == 0 {
                        set_state(&tx, job, "abandoned", Some("its manager stopped before any effect"))?;
                    }
                }
            }
            tx.commit()?;
            Ok(jobs)
        })
    }

    pub(crate) fn deferred_jobs(&self) -> Result<Vec<String>, JobError> {
        self.with(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT DISTINCT j.job_id FROM jobs j JOIN targets t ON t.job_id = j.job_id
                WHERE t.disposition = 'deferred' AND {LATEST} AND j.evidence_pruned_at IS NULL
                ORDER BY j.created_at"
            ))?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
    }

    /// Claim exactly this deferred attempt. A manual retry uses the same
    /// unique `retry_of` key; either wins, never both. None means it changed.
    pub(crate) fn claim_resume(
        &self,
        lease: &Lease,
        job: &str,
        target: u32,
        attempt: u32,
        withheld: bool,
    ) -> Result<Option<u32>, JobError> {
        self.with(|c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            fence(&tx, lease)?;
            let available: bool = tx.query_row(&format!("SELECT EXISTS(SELECT 1 FROM targets t JOIN jobs j ON j.job_id = t.job_id
                WHERE t.job_id = ?1 AND t.target_id = ?2 AND t.attempt = ?3 AND {LATEST}
                AND t.disposition = 'deferred' AND t.phase = 'pending' AND j.cancel_requested = 0
                AND j.evidence_pruned_at IS NULL AND (SELECT draining FROM owner WHERE id = 1) = 0
                AND NOT EXISTS (SELECT 1 FROM targets r WHERE r.retry_of = ?4)
                AND NOT EXISTS (SELECT 1 FROM effects e WHERE e.job_id = t.job_id AND e.target_id = t.target_id AND e.attempt = t.attempt)
                AND NOT EXISTS (SELECT 1 FROM observations o WHERE o.job_id = t.job_id AND o.target_id = t.target_id AND o.attempt = t.attempt))"),
                params![job, target, attempt, format!("{job}:{target}:{attempt}")], |r| r.get(0))?;
            if !available {
                // A manual retry can win the same unique parent claim.
                // Close its source instead of promising an automatic resume.
                tx.execute("UPDATE targets SET disposition = 'skipped', reason = 'retried_elsewhere', phase = 'finished'
                    WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3 AND disposition = 'deferred'
                    AND EXISTS (SELECT 1 FROM targets r WHERE r.retry_of = ?4)
                    AND attempt = (SELECT MAX(l.attempt) FROM targets l WHERE l.job_id = ?1 AND l.target_id = ?2)",
                    params![job, target, attempt, format!("{job}:{target}:{attempt}")])?;
                aggregate(&tx, job)?;
                tx.commit()?;
                return Ok(None);
            }
            let (lineage, reason): (String, Option<String>) = tx.query_row(
                "SELECT COALESCE(lineage, job_id || ':' || target_id), reason FROM targets WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3",
                params![job, target, attempt], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if reason.as_deref() == Some("manager_crash") {
                let count: u32 = tx.query_row("SELECT COALESCE((SELECT crashes FROM resume_counts WHERE lineage = ?1), 0)", [&lineage], |r| r.get(0))?;
                if count >= 3 {
                    tx.execute("UPDATE targets SET disposition = 'needs_reconciliation', reason = 'crash_resume_limit'
                        WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3", params![job, target, attempt])?;
                    aggregate(&tx, job)?;
                    tx.commit()?;
                    return Ok(None);
                }
                tx.execute("INSERT INTO resume_counts (lineage, crashes) VALUES (?1, 1)
                    ON CONFLICT(lineage) DO UPDATE SET crashes = crashes + 1", [&lineage])?;
            }
            let next = attempt.checked_add(1).ok_or_else(|| JobError::Store("attempt overflow".into()))?;
            tx.execute("INSERT INTO targets (job_id, target_id, attempt, phase, lineage, retry_of, reason)
                VALUES (?1, ?2, ?3, 'pending', ?4, ?5, ?6)",
                params![job, target, next, lineage, format!("{job}:{target}:{attempt}"),
                    withheld.then_some("follow_up_text_unavailable")])?;
            tx.execute("UPDATE jobs SET owner_nonce = ?2 WHERE job_id = ?1", params![job, lease.nonce])?;
            aggregate(&tx, job)?;
            tx.commit()?;
            Ok(Some(next))
        })
    }

    pub(crate) fn defer_attempt(
        &self,
        lease: &Lease,
        job: &str,
        target: u32,
        attempt: u32,
        reason: &str,
    ) -> Result<(), JobError> {
        self.with(|c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            fence(&tx, lease)?;
            // A concurrent drain/cancel already settled it: leave that decision.
            let open: bool = tx.query_row(
                "SELECT disposition IS NULL FROM targets
                WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3",
                params![job, target, attempt],
                |r| r.get(0),
            )?;
            if open {
                defer_one(&tx, job, target, attempt, reason)?;
            }
            aggregate(&tx, job)?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Resolve a deferral without any effect (expiry, changed binding, or
    /// missing resume authority). Claiming and cancellation race safely.
    pub(crate) fn settle_deferred(
        &self,
        lease: &Lease,
        job: &str,
        target: u32,
        attempt: u32,
        reason: &str,
    ) -> Result<(), JobError> {
        self.with(|c| {
            let tx = c.transaction_with_behavior(TransactionBehavior::Immediate)?;
            fence(&tx, lease)?;
            tx.execute(
                "UPDATE targets SET disposition = 'skipped', phase = 'finished', reason = ?4
                WHERE job_id = ?1 AND target_id = ?2 AND attempt = ?3 AND disposition = 'deferred'",
                params![job, target, attempt, reason],
            )?;
            aggregate(&tx, job)?;
            tx.commit()?;
            Ok(())
        })
    }
}
