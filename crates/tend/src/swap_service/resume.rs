//! Admission of fresh attempts for untouched targets after restart.
use super::{
    refusal, AccountChoice, Arc, AttemptRun, ContinueWhen, Engine, EngineStep, Ordering, Origin,
    Service, SwapRequest, SwapScope, TextRef,
};

impl<E: Engine> Service<E> {
    /// Retry unavailable snapshots/guards without claiming another attempt.
    /// Weak ownership lets the daemon/service shut down normally.
    pub(super) fn start_resumer(this: &Arc<Self>) -> Result<(), String> {
        let weak = Arc::downgrade(this);
        std::thread::Builder::new()
            .name("swap-resume".into())
            .spawn(move || loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
                let Some(service) = weak.upgrade() else {
                    break;
                };
                if service.draining.load(Ordering::SeqCst) {
                    break;
                }
                if let Err(error) = Self::resume_pending(&service) {
                    if !service.draining.load(Ordering::SeqCst) {
                        service.set_fault(format!("could not resume deferred swap jobs: {error}"));
                    }
                    break;
                }
            })
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    #[allow(clippy::too_many_lines)] // admission, policy reconstruction, and fenced handover in order
    pub(super) fn resume_pending(this: &Arc<Self>) -> Result<(), String> {
        let _gate = this
            .submit
            .lock()
            .map_err(|_| "swap submission lock poisoned")?;
        this.active()?;
        for job in this.store.deferred_jobs().map_err(|e| e.to_string())? {
            // Exactly one worker per job, including mixed jobs.
            if this.receiver(&job).is_some() {
                continue;
            }
            let plan = this.store.plan(&job).map_err(|e| e.to_string())?;
            let status = this.store.status(&job).map_err(|e| e.to_string())?;
            let deferred: Vec<_> = status
                .targets
                .iter()
                .filter(|t| t.disposition.as_deref() == Some("deferred"))
                .collect();
            let skip = |target: &crate::swap_job::TargetStatus, reason: &str| {
                this.store
                    .settle_deferred(&this.lease, &job, target.target_id, target.attempt, reason)
                    .map_err(|e| e.to_string())
            };
            let Some(policy) = &plan.resume else {
                for target in deferred {
                    skip(target, "resume_policy_unavailable")?;
                }
                continue;
            };
            if !policy.public || status.cancel_requested {
                for target in deferred {
                    skip(
                        target,
                        if status.cancel_requested {
                            "cancelled"
                        } else {
                            "automation_not_resumed"
                        },
                    )?;
                }
                continue;
            }
            // Missing live data/guard is temporary; never settle on absence.
            let Ok(snapshot) = this.provider.snapshot() else {
                continue;
            };
            if snapshot.node_id != plan.node_id {
                for target in deferred {
                    skip(target, "resume_node_changed")?;
                }
                continue;
            }
            let withheld = plan.continue_text.as_ref().is_some_and(|text| {
                !policy.configured_text || &TextRef::of(&snapshot.config.continue_text) != text
            });
            let mut runs = Vec::new();
            let mut taken = Vec::new();
            let mut resolved = None;
            for source in deferred {
                let Some(planned) = usize::try_from(source.target_id)
                    .ok()
                    .and_then(|id| plan.targets.get(id))
                else {
                    return Err(format!("job {job} has an invalid target id"));
                };
                if policy.when_idle
                    && !policy.force
                    && time::OffsetDateTime::now_utc().unix_timestamp() >= policy.idle_deadline
                {
                    skip(source, "idle_timeout")?;
                    continue;
                }
                let Some(account) = &planned.assigned_account else {
                    skip(source, "no_assigned_account")?;
                    continue;
                };
                if !this.reserve(&planned.endpoint, &job) {
                    continue;
                }
                let Ok(guard) = this
                    .provider
                    .acquire_guard(&planned.pane, planned.socket.as_deref())
                else {
                    this.release(&planned.endpoint);
                    continue;
                };
                let mut request = SwapRequest::new(
                    SwapScope::Targets(vec![planned.conversation_id.clone()]),
                    &snapshot.config,
                );
                request.to = AccountChoice::Named(account.clone());
                request.when_idle = policy.when_idle;
                request.force = policy.force;
                request.restore_draft = policy.restore_draft;
                request.bare = policy.bare;
                request.continue_evidence = policy.continue_evidence;
                request.continue_when = if withheld {
                    ContinueWhen::Never
                } else {
                    policy.continue_when
                };
                request.continue_text = if withheld || plan.continue_text.is_none() {
                    None
                } else {
                    Some(snapshot.config.continue_text.clone())
                };
                request.allow_direct_respawn = false;
                let (request, step, changed) =
                    this.engine.prepare_resume(&request, planned, &snapshot);
                let EngineStep::Run(target) = step else {
                    this.release(&planned.endpoint);
                    if changed {
                        skip(source, "changed_while_deferred")?;
                    }
                    continue;
                };
                if refusal(&this.engine.identity(&target), Origin::Public).is_some() {
                    this.release(&planned.endpoint);
                    skip(source, "resume_target_unsupported")?;
                    continue;
                }
                // All potentially unavailable preparation precedes the claim.
                let attempt = match this.store.claim_resume(
                    &this.lease,
                    &job,
                    source.target_id,
                    source.attempt,
                    withheld,
                ) {
                    Ok(Some(attempt)) => attempt,
                    Ok(None) => {
                        this.release(&planned.endpoint);
                        continue;
                    }
                    Err(error) => {
                        this.release(&planned.endpoint);
                        return Err(error.to_string());
                    }
                };
                // Preparation was protected; execution reacquires exclusion and
                // revalidates the process. Never queue held input guards behind
                // another target that could take a long time to execute.
                drop(guard);
                taken.push(planned.endpoint.clone());
                runs.push(AttemptRun {
                    target_id: source.target_id,
                    attempt,
                    report_index: planned.report_index,
                    identity: this.engine.identity(&target),
                    target,
                });
                resolved.get_or_insert(request);
            }
            if let Some(request) = resolved {
                this.done
                    .lock()
                    .map_err(|_| "swap completion lock poisoned")?
                    .insert(job.clone(), tokio::sync::watch::channel(false).0);
                // Keep already settled history in SQLite, not an old live cache.
                if let Ok(mut live) = this.live.lock() {
                    live.remove(&job);
                }
                if let Err(error) =
                    Self::spawn(this, &job, request, runs, None, taken.len(), &taken, None)
                {
                    if this.fault().is_some() {
                        return Err(error);
                    }
                    // A refused thread launch did nothing and left its fresh
                    // attempts deferred. The next pass may retry them.
                }
            }
        }
        Ok(())
    }
}
