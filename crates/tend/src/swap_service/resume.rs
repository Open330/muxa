//! Admission of fresh attempts for untouched targets after restart.
use super::{
    refusal, registered_target, AccountChoice, Arc, AttemptRun, ContinueWhen, Engine, EngineStep,
    Ordering, Origin, Service, SwapRequest, SwapScope, TextRef,
};

/// Admission retries start at one second and stop growing at thirty.
#[derive(Default)]
pub(super) struct ResumeBackoff {
    delay: u64,
}

impl ResumeBackoff {
    pub(super) fn after_pass(&mut self, admitted: usize) -> std::time::Duration {
        self.delay = if admitted > 0 {
            1
        } else {
            self.delay.saturating_mul(2).clamp(1, 30)
        };
        std::time::Duration::from_secs(self.delay)
    }
}

impl<E: Engine> Service<E> {
    /// Only run while deferrals exist. A worker that defers again restarts
    /// admission; the final recheck closes the exit/worker handoff race.
    pub(super) fn start_resumer(this: &Arc<Self>) -> Result<(), String> {
        if this.draining.load(Ordering::SeqCst)
            || this.fault().is_some()
            || this
                .store
                .deferred_jobs()
                .map_err(|e| e.to_string())?
                .is_empty()
            || this.resumer_running.swap(true, Ordering::SeqCst)
        {
            return Ok(());
        }
        let weak = Arc::downgrade(this);
        let launched = std::thread::Builder::new()
            .name("swap-resume".into())
            .spawn(move || {
                let mut backoff = ResumeBackoff::default();
                loop {
                    let Some(service) = weak.upgrade() else {
                        break;
                    };
                    if service.draining.load(Ordering::SeqCst) || service.fault().is_some() {
                        service.resumer_running.store(false, Ordering::SeqCst);
                        break;
                    }
                    let admitted = match Self::resume_pending(&service) {
                        Ok(count) => count,
                        Err(error) => {
                            if !service.draining.load(Ordering::SeqCst) {
                                service.set_fault(format!(
                                    "could not resume deferred swap jobs: {error}"
                                ));
                            }
                            service.resumer_running.store(false, Ordering::SeqCst);
                            break;
                        }
                    };
                    let remaining = match service.store.deferred_jobs() {
                        Ok(jobs) => jobs,
                        Err(error) => {
                            service.set_fault(format!("could not read deferred swaps: {error}"));
                            service.resumer_running.store(false, Ordering::SeqCst);
                            break;
                        }
                    };
                    if remaining.is_empty() {
                        service.resumer_running.store(false, Ordering::SeqCst);
                        // A worker either sees false and launches a dispatcher,
                        // or leaves a new deferral for this last recheck.
                        match service.store.deferred_jobs() {
                            Ok(jobs)
                                if !jobs.is_empty() && !service.draining.load(Ordering::SeqCst) =>
                            {
                                if service.resumer_running.swap(true, Ordering::SeqCst) {
                                    break;
                                }
                                continue;
                            }
                            Ok(_) => break,
                            Err(error) => {
                                service.set_fault(format!(
                                    "could not recheck deferred swaps: {error}"
                                ));
                                break;
                            }
                        }
                    }
                    let delay = backoff.after_pass(admitted);
                    drop(service);
                    std::thread::sleep(delay);
                }
            });
        if let Err(error) = launched {
            this.resumer_running.store(false, Ordering::SeqCst);
            return Err(error.to_string());
        }
        Ok(())
    }

    #[allow(clippy::too_many_lines)] // admission, policy reconstruction, and fenced handover in order
    pub(super) fn resume_pending(this: &Arc<Self>) -> Result<usize, String> {
        let jobs = this.store.deferred_jobs().map_err(|e| e.to_string())?;
        if jobs.is_empty() {
            return Ok(0);
        }
        // A full provider read can be slow. New submissions can pass their
        // gate while it runs; all journal decisions are rechecked below.
        let snapshot = this.provider.snapshot().ok();
        let _gate = this
            .submit
            .lock()
            .map_err(|_| "swap submission lock poisoned")?;
        this.active()?;
        let mut admitted = 0;
        for job in jobs {
            // Exactly one worker per job, including mixed jobs.
            let worker_running = this.receiver(&job).is_some();
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
            // Expiry is journal-only and works even when the provider is down.
            let mut eligible = Vec::new();
            for source in deferred {
                if time::OffsetDateTime::now_utc().unix_timestamp()
                    >= this
                        .store
                        .resume_deadline(&job, source.target_id, source.attempt)
                        .map_err(|e| e.to_string())?
                {
                    skip(source, "resume_window_expired")?;
                } else {
                    eligible.push(source);
                }
            }
            if worker_running {
                continue;
            }
            let Some(snapshot) = snapshot.as_ref() else {
                continue;
            };
            if snapshot.node_id != plan.node_id {
                for target in eligible {
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
            for source in eligible {
                let Some(planned) = usize::try_from(source.target_id)
                    .ok()
                    .and_then(|id| plan.targets.get(id))
                else {
                    return Err(format!("job {job} has an invalid target id"));
                };
                if registered_target(
                    &snapshot.agents,
                    &planned.conversation_id,
                    &planned.pane,
                    &planned.endpoint,
                )
                .is_none()
                    || this.engine.pane_gone(planned, snapshot)
                {
                    skip(source, "target_gone")?;
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
                    this.engine.prepare_resume(&request, planned, snapshot);
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
                let count = runs.len();
                match Self::spawn(this, &job, request, runs, None, taken.len(), &taken, None) {
                    Ok(()) => admitted += count,
                    Err(error) if this.fault().is_some() => return Err(error),
                    Err(_) => {} // No effects: worker launch left fresh attempts deferred.
                }
            }
        }
        Ok(admitted)
    }
}
