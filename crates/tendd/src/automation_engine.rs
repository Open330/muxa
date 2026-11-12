//! The automation engine: plans firings from agent state and the rules,
//! admits and runs them (text, keys, judgment, account swaps), and records
//! each outcome in the ledger.

#![allow(clippy::wildcard_imports)] // moved from `main.rs`; uses the daemon's imports

use super::*;

/// Automation swaps admitted at once, across every rule and pane; one more
/// waits its turn. One per pane besides.
const AUTOMATION_SWAP_CAPACITY: usize = 2;

/// How long a firing that found its pane busy, or no room for a swap,
/// waits before it is checked again.
const AUTOMATION_BUSY_DEFER: time::Duration = time::Duration::seconds(5);

/// Which automation submissions could still accept a job. A key nothing is
/// submitting under is settled: if no job has it, none will.
#[derive(Default)]
pub(super) struct Submissions(
    std::sync::Mutex<HashMap<String, tokio::sync::watch::Receiver<bool>>>,
);

/// Held by a submission's blocking call; dropping it, however the call
/// ends, says that submission can accept nothing more.
pub(super) struct Submitting {
    key: String,
    registry: Arc<Submissions>,
    done: tokio::sync::watch::Sender<bool>,
}

impl Submissions {
    pub(super) fn begin(self: &Arc<Self>, key: &str) -> Submitting {
        let (done, watch) = tokio::sync::watch::channel(false);
        if let Ok(mut submitting) = self.0.lock() {
            submitting.insert(key.to_owned(), watch);
        }
        Submitting {
            key: key.to_owned(),
            registry: Arc::clone(self),
            done,
        }
    }

    /// Return once nothing is submitting under `key`.
    #[cfg_attr(not(feature = "aas"), allow(dead_code))]
    pub(super) async fn settled(&self, key: &str) {
        let watch = self
            .0
            .lock()
            .ok()
            .and_then(|submitting| submitting.get(key).cloned());
        if let Some(mut watch) = watch {
            let _ = watch.wait_for(|done| *done).await;
        }
    }
}

impl Drop for Submitting {
    fn drop(&mut self) {
        if let Ok(mut submitting) = self.registry.0.lock() {
            submitting.remove(&self.key);
        }
        let _ = self.done.send(true);
    }
}

/// A job that did what the automation asked: done, its one target swapped,
/// with no fault and no effect left uncertain.
fn swap_fired(view: &tend::swap_api::JobView) -> bool {
    view.state == "done"
        && view.fault.is_none()
        && matches!(
            view.entries.as_slice(),
            [entry] if entry.disposition.as_deref() == Some("swapped") && entry.uncertain_effects == 0
        )
}

/// A judgment's ledger detail, with what came of it.
fn judgment_detail(completed: &JudgedFiring, execution: &str) -> String {
    let condition = completed
        .rule
        .ask_condition
        .as_ref()
        .expect("judged rule has a condition");
    serde_json::json!({
        "judgment": completed.judgment,
        "condition": condition.prompt,
        "observe_only": condition.observe_only,
        "execution": execution,
    })
    .to_string()
}

/// The engine: watch state transitions, arm matching rules, hold their
/// firings until due, re-check them against the live store, and act.
///
/// Modelled on the collaboration waker directly above — same transition
/// subscription, same debounce-by-key discipline, same slow reconcile tick
/// as the recovery path. The differences are that firings are *scheduled*
/// rather than immediate (hence the heap) and that every decision is
/// recorded in the ledger.
///
/// Always spawned, even with no rules: a rule written through
/// `automation_set_rule` must take effect without a daemon restart, and an
/// engine with nothing to do costs one refcount bump per transition.
#[allow(clippy::too_many_arguments)] // Explicit shared services for the automation task.
pub(super) fn spawn_automation_task(
    automation: Arc<AutomationStore>,
    ask: Arc<AskStore>,
    store: tend::SharedStore,
    backends: Vec<tend::SharedBackend>,
    swap: tend::swap_api::SwapConfig,
    accounts: tend::accounts::AccountsConfig,
    collaboration: Arc<CollaborationStore>,
    swap_service: Option<Arc<DaemonSwap>>,
    shutdown_tx: &broadcast::Sender<()>,
) -> tokio::task::JoinHandle<()> {
    let mut shutdown_rx = shutdown_tx.subscribe();
    // Subscribe before the first authoritative scan so a transition racing
    // task startup is represented either by that scan or by a pending signal.
    let mut transitions = store.subscribe();
    let mut config_changes = automation.subscribe();
    tokio::spawn(async move {
        let mut engine = AutomationEngine::new(automation, store, backends);
        engine.ask = Some(ask);
        engine.swap = swap;
        engine.accounts = accounts;
        engine.collaboration = Some(collaboration);
        engine.swap_service = swap_service;
        engine.reattach_swaps().await;
        engine.rescan().await;
        let mut reconcile =
            tokio::time::interval(std::time::Duration::from_secs(AUTOMATION_RECONCILE_SECONDS));
        reconcile.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        reconcile.tick().await;
        loop {
            let idle = engine.time_until_next_firing();
            tokio::select! {
                transition = transitions.recv() => match transition {
                    Ok(transition) => engine.observe(&transition.agent).await,
                    // A lag means an arming transition may have been dropped.
                    // Re-read the authoritative state immediately.
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        tracing::debug!(dropped, "automation transition stream lagged");
                        engine.rescan().await;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                changed = config_changes.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    engine.pending.clear();
                    engine.armed.clear();
                    engine.queued_rules.clear();
                    engine.drain_reservations.clear();
                    // A rule was added, edited, enabled, or paused. Drop
                    // pending firings whose rule no longer exists and
                    // re-evaluate everything against the new set.
                    engine.rescan().await;
                }
                () = tokio::time::sleep(idle) => engine.fire_due().await,
                ended = engine.swap_ops.join_next_with_id(), if !engine.swap_ops.is_empty() => {
                    if let Some(ended) = ended {
                        engine.swap_task_ended(ended).await;
                    }
                }
                completed = engine.judgments.join_next(), if !engine.judgments.is_empty() => {
                    match completed {
                        Some(Ok(completed)) => engine.finish_judgment(completed).await,
                        Some(Err(error)) => tracing::warn!(%error, "automation judgment task failed; no action taken"),
                        None => {}
                    }
                }
                _ = reconcile.tick() => engine.rescan().await,
                _ = shutdown_rx.recv() => break,
            }
        }
    })
}

/// Live state of the automation engine: the pending firing queue, the
/// in-flight key set, and a short-lived pane scan for the workspace/work
/// filters.
struct AutomationEngine {
    automation: Arc<AutomationStore>,
    store: tend::SharedStore,
    backends: Vec<tend::SharedBackend>,
    /// Firings waiting on their fire time, earliest first.
    pending: BinaryHeap<std::cmp::Reverse<PlannedFiring>>,
    /// `(rule, pane, episode)` already queued. Together with the ledger's
    /// own episode check this is what makes "never twice for one cap
    /// episode" hold both within a daemon run and across a restart.
    armed: HashSet<(String, String, String)>,
    queued_rules: HashMap<(String, String, String), (AutomationRule, u64)>,
    collaboration: Option<Arc<CollaborationStore>>,
    drain_reservations:
        HashMap<(String, String, String), tend::collaboration::WorkerAutomationReservation>,
    panes: Vec<tend::tmux::PaneInfo>,
    panes_read_at: Option<std::time::Instant>,
    ask: Option<Arc<AskStore>>,
    judgments: tokio::task::JoinSet<JudgedFiring>,
    /// `[swap]` defaults for the `swap_account` action.
    swap: tend::swap_api::SwapConfig,
    /// Daemon-owned swap jobs the `swap_account` action submits to; `None`
    /// when they could not be opened, in which case that action is refused.
    swap_service: Option<Arc<DaemonSwap>>,
    /// `[accounts]`, which the `swap_account` action moves agents between
    /// when a provider lists them.
    accounts: tend::accounts::AccountsConfig,
    /// The tasks that own admitted swap operations, each from its pane's
    /// guard to its job's end; the engine never waits on one.
    swap_ops: tokio::task::JoinSet<SwapEnded>,
    /// Which operation each of those tasks owns, so a task that fails still
    /// leaves its operation found.
    swap_tasks: HashMap<tokio::task::Id, String>,
    /// Admitted operations nothing has ended yet, by request key.
    swap_inflight: HashMap<String, SwapOperation>,
    /// Judged swap firings that found no room, tried again as room frees.
    deferred_judgments: std::collections::VecDeque<JudgedFiring>,
}

/// A swap the automation admitted: charged to its guards from then on,
/// owned by its own task until its job ends or it is released.
struct SwapOperation {
    /// Its `admitted` ledger entry, which the entry that ends it copies.
    admitted: AutomationLedgerEntry,
    /// Held while it runs, as it was while its firing waited.
    _drain_reservation: Option<tend::collaboration::WorkerAutomationReservation>,
}

/// What an operation's task came to.
pub(super) struct SwapEnded {
    pub(super) key: String,
    pub(super) job: Option<String>,
    pub(super) end: SwapEnd,
}

#[cfg_attr(not(feature = "aas"), allow(dead_code))]
pub(super) enum SwapEnd {
    /// No job was accepted under its key: nothing was done.
    Released(String),
    /// Its job ended so.
    Ended(tend::swap_api::JobView),
}

/// What came of performing a firing's action.
enum Performed {
    Done,
    Failed,
    /// Its pane was busy; nothing was done, and it is tried again later.
    Busy,
}

/// Where a swap firing stands after it asks to be admitted.
enum Admission {
    Admitted,
    /// No room yet; the firing is asked again later.
    Deferred,
    /// The guards or a fresh check refused it.
    Refused(String),
    /// This daemon cannot run swaps.
    Unavailable,
}

/// What a newly admitted operation's task needs to start its job.
#[cfg_attr(not(feature = "aas"), allow(dead_code))]
pub(super) struct SwapStart {
    pub(super) firing: PlannedFiring,
    pub(super) revision: u64,
    pub(super) revision_now: tokio::sync::watch::Receiver<u64>,
    pub(super) store: tend::SharedStore,
    pub(super) collaboration: Option<Arc<CollaborationStore>>,
    /// Its claim on the key, made before the operation was spawned and
    /// owned until the submission ends or the operation is dropped short
    /// of one: a later lookup by key waits for whichever comes first.
    pub(super) submitting: Submitting,
}

struct JudgedFiring {
    firing: PlannedFiring,
    rule: AutomationRule,
    subject: AutomationSubject,
    revision: u64,
    context: Option<JudgmentContext>,
    judgment: AutomationJudgment,
    drain_reservation: Option<tend::collaboration::WorkerAutomationReservation>,
}

impl AutomationEngine {
    fn new(
        automation: Arc<AutomationStore>,
        store: tend::SharedStore,
        backends: Vec<tend::SharedBackend>,
    ) -> Self {
        Self {
            automation,
            store,
            backends,
            pending: BinaryHeap::new(),
            armed: HashSet::new(),
            queued_rules: HashMap::new(),
            collaboration: None,
            drain_reservations: HashMap::new(),
            panes: Vec::new(),
            panes_read_at: None,
            ask: None,
            judgments: tokio::task::JoinSet::new(),
            swap: tend::swap_api::SwapConfig::default(),
            swap_service: None,
            accounts: tend::accounts::AccountsConfig::default(),
            swap_ops: tokio::task::JoinSet::new(),
            swap_tasks: HashMap::new(),
            swap_inflight: HashMap::new(),
            deferred_judgments: std::collections::VecDeque::new(),
        }
    }

    /// How long to park before the next firing is due, bounded so a rule
    /// added at runtime is still picked up promptly.
    fn time_until_next_firing(&self) -> std::time::Duration {
        let mut cap = std::time::Duration::from_secs(AUTOMATION_MAX_IDLE_SECONDS);
        if !self.deferred_judgments.is_empty() {
            cap = cap.min(AUTOMATION_BUSY_DEFER.unsigned_abs());
        }
        let Some(next) = self.pending.peek() else {
            return cap;
        };
        let remaining = next.0.fire_at - time::OffsetDateTime::now_utc();
        if remaining.is_negative() {
            return std::time::Duration::ZERO;
        }
        remaining.unsigned_abs().min(cap)
    }

    /// Evaluate every enabled rule against one agent that just changed state.
    async fn observe(&mut self, agent: &tend::Agent) {
        let now = time::OffsetDateTime::now_utc();
        let (active, rules) = self.automation.active_rules(now).await;
        if !active || rules.is_empty() {
            return;
        }
        let subject = self.subject_for(&rules, agent).await;
        self.arm_all(&rules, std::slice::from_ref(&subject), now)
            .await;
    }

    /// Authoritative pass over the whole registry. Recovers the two cases a
    /// transition cannot cover: a row already capped when the daemon
    /// started, and a `RateLimited` landing on a row that was already in
    /// `Error` (no state change, so no transition is broadcast).
    async fn rescan(&mut self) {
        let now = time::OffsetDateTime::now_utc();
        let (active, rules) = self.automation.active_rules(now).await;
        // A firing whose rule was disabled or deleted must not survive.
        let live: HashSet<&str> = rules.iter().map(|rule| rule.name.as_str()).collect();
        self.pending
            .retain(|firing| live.contains(firing.0.rule.as_str()));
        self.armed
            .retain(|(rule, _, _)| live.contains(rule.as_str()));
        self.queued_rules
            .retain(|(rule, _, _), _| live.contains(rule.as_str()));
        self.drain_reservations
            .retain(|(rule, _, _), _| live.contains(rule.as_str()));
        if !active || rules.is_empty() {
            return;
        }
        let agents = self.store.snapshot().await;
        let subjects = if rules.iter().any(AutomationRule::needs_pane_metadata) {
            self.refresh_panes().await;
            subjects_from(&agents, &self.panes)
        } else {
            subjects_from(&agents, &[])
        };
        self.arm_all(&rules, &subjects, now).await;
    }

    async fn arm_all(
        &mut self,
        rules: &[AutomationRule],
        subjects: &[AutomationSubject],
        now: time::OffsetDateTime,
    ) {
        let revision = *self.automation.subscribe().borrow();
        let config = self.automation.config().await;
        let ledger = self.automation.ledger();
        for rule in rules {
            if config.rule_named(&rule.name) != Some(rule) {
                continue;
            }
            for subject in subjects {
                let Some(pane) = subject.pane.clone() else {
                    continue;
                };
                // Cheap pre-filter before touching the ledger. Most
                // evaluations are "this agent is not in the state this rule
                // watches", and a guard read scans up to 500 entries.
                if subject.current_event() != Some(rule.on) {
                    continue;
                }
                let episode = subject.episode();
                let key = (rule.name.clone(), pane.clone(), episode.clone());
                if self.armed.contains(&key) {
                    continue;
                }
                let guards = ledger.guard_state(&rule.name, &pane, &episode, now).await;
                match Scheduler::arm(&config, rule, subject, guards, now, jitter_ratio()) {
                    Decision::Fire(firing) => {
                        if let Some(collaboration) = &self.collaboration {
                            // A pane something is acting on (a swap may hold
                            // it for minutes) is armed on a later pass, never
                            // waited for here.
                            let Some(Some(reservation)) = collaboration
                                .try_reserve_worker_automation(
                                    &subject.agent_session_id,
                                    &pane,
                                    subject.socket.as_deref(),
                                )
                                .await
                            else {
                                continue;
                            };
                            self.drain_reservations.insert(key.clone(), reservation);
                        }
                        tracing::info!(
                            rule = %rule.name,
                            pane = %pane,
                            action = %firing.action,
                            fire_at = %firing.fire_at,
                            "automation armed",
                        );
                        self.queued_rules
                            .insert(key.clone(), (rule.clone(), revision));
                        self.armed.insert(key);
                        self.pending.push(std::cmp::Reverse(*firing));
                    }
                    // Arm-time skips are the common case (every rule sees
                    // every agent), so they stay in the trace rather than
                    // flooding the ledger. Fire-time skips are recorded.
                    Decision::Skip(reason) => tracing::trace!(
                        rule = %rule.name,
                        pane = %pane,
                        %reason,
                        "automation did not arm",
                    ),
                }
            }
        }
    }

    /// Act on every firing whose time has come, re-checking each against
    /// the live store first.
    async fn fire_due(&mut self) {
        // Judged firings that found no room or a busy pane, asked again.
        for completed in std::mem::take(&mut self.deferred_judgments) {
            self.finish_judgment(completed).await;
        }
        let now = time::OffsetDateTime::now_utc();
        let mut due = Vec::new();
        while self
            .pending
            .peek()
            .is_some_and(|firing| firing.0.fire_at <= now)
        {
            if let Some(std::cmp::Reverse(firing)) = self.pending.pop() {
                due.push(firing);
            }
        }
        for firing in due {
            self.armed.remove(&(
                firing.rule.clone(),
                firing.pane.clone(),
                firing.episode.clone(),
            ));
            self.fire(&firing, now).await;
            let key = (
                firing.rule.clone(),
                firing.pane.clone(),
                firing.episode.clone(),
            );
            if !self.armed.contains(&key) {
                self.drain_reservations.remove(&key);
            }
        }
    }

    async fn fire(&mut self, firing: &PlannedFiring, now: time::OffsetDateTime) {
        let key = (
            firing.rule.clone(),
            firing.pane.clone(),
            firing.episode.clone(),
        );
        let Some((queued_rule, revision)) = self.queued_rules.remove(&key) else {
            return;
        };
        let config = self.automation.config().await;
        let Some(rule) = config.rule_named(&firing.rule).cloned() else {
            // The rule was removed while this firing waited.
            return;
        };
        let subject = self.live_subject(&rule, &firing.agent_session_id).await;
        let ledger = self.automation.ledger();
        if queued_rule != rule
            || revision != *self.automation.subscribe().borrow()
            || subject
                .as_ref()
                .is_some_and(|subject| subject.socket != firing.socket)
        {
            ledger
                .append(ledger_entry(
                    firing,
                    now,
                    AutomationOutcome::Skipped,
                    Some("Queued rule or pane endpoint changed".into()),
                ))
                .await;
            return;
        }
        let guards = ledger
            .guard_state(&firing.rule, &firing.pane, &firing.episode, now)
            .await;
        match Scheduler::confirm(&config, &rule, firing, subject.as_ref(), guards, now) {
            Decision::Skip(reason) => {
                tracing::info!(
                    rule = %firing.rule,
                    pane = %firing.pane,
                    %reason,
                    "automation skipped at fire time",
                );
                ledger
                    .append(ledger_entry(
                        firing,
                        now,
                        AutomationOutcome::Skipped,
                        Some(reason.to_string()),
                    ))
                    .await;
            }
            Decision::Fire(confirmed) => {
                if rule.ask_condition.is_some() {
                    if let Some(subject) = subject {
                        self.start_judgment(*confirmed, rule, subject, now, revision)
                            .await;
                    }
                    return;
                }
                if confirmed.action == AutomationAction::SwapAccount {
                    self.fire_swap(*confirmed, rule, revision, key, now).await;
                    return;
                }
                let delivered = match self.perform(&confirmed).await {
                    Performed::Done => true,
                    Performed::Failed => false,
                    Performed::Busy => {
                        self.requeue(*confirmed, rule, revision, now + AUTOMATION_BUSY_DEFER);
                        return;
                    }
                };
                let outcome = if delivered {
                    AutomationOutcome::Fired
                } else {
                    AutomationOutcome::Failed
                };
                let detail = if delivered {
                    confirmed.text.clone()
                } else {
                    Some(SkipReason::ActionFailed.to_string())
                };
                tracing::info!(
                    rule = %confirmed.rule,
                    pane = %confirmed.pane,
                    action = %confirmed.action,
                    %outcome,
                    "automation acted",
                );
                ledger
                    .append(ledger_entry(&confirmed, now, outcome, detail))
                    .await;
            }
        }
    }

    /// A confirmed swap firing: admitted, deferred for room, or recorded as
    /// refused.
    async fn fire_swap(
        &mut self,
        confirmed: PlannedFiring,
        rule: AutomationRule,
        revision: u64,
        key: (String, String, String),
        now: time::OffsetDateTime,
    ) {
        let ledger = self.automation.ledger();
        let mut drain = self.drain_reservations.remove(&key);
        let admission = self
            .admit_swap(
                &confirmed,
                &rule,
                revision,
                confirmed.text.clone(),
                false,
                &mut drain,
            )
            .await;
        match admission {
            Admission::Admitted => {}
            Admission::Deferred => {
                if let Some(drain) = drain {
                    self.drain_reservations.insert(key, drain);
                }
                self.requeue(confirmed, rule, revision, now + AUTOMATION_BUSY_DEFER);
            }
            Admission::Refused(reason) => {
                ledger
                    .append(ledger_entry(
                        &confirmed,
                        now,
                        AutomationOutcome::Skipped,
                        Some(reason),
                    ))
                    .await;
            }
            Admission::Unavailable => {
                ledger
                    .append(ledger_entry(
                        &confirmed,
                        now,
                        AutomationOutcome::Failed,
                        Some(SkipReason::ActionFailed.to_string()),
                    ))
                    .await;
            }
        }
    }

    /// Put `firing` back in the queue, due at `at`, to be checked afresh.
    fn requeue(
        &mut self,
        mut firing: PlannedFiring,
        rule: AutomationRule,
        revision: u64,
        at: time::OffsetDateTime,
    ) {
        firing.fire_at = at;
        let key = (
            firing.rule.clone(),
            firing.pane.clone(),
            firing.episode.clone(),
        );
        self.queued_rules.insert(key.clone(), (rule, revision));
        self.armed.insert(key);
        self.pending.push(std::cmp::Reverse(firing));
    }

    async fn start_judgment(
        &mut self,
        firing: PlannedFiring,
        rule: AutomationRule,
        subject: AutomationSubject,
        now: time::OffsetDateTime,
        revision: u64,
    ) {
        let slot = self.automation.try_judgment_slot();
        if self.judgments.len() >= 2 || slot.is_err() {
            self.requeue(firing, rule, revision, now + time::Duration::seconds(1));
            return;
        }
        let permit = slot.expect("judgment slot checked");
        if revision != *self.automation.subscribe().borrow() {
            return;
        }
        let condition = rule
            .ask_condition
            .as_ref()
            .expect("Ask condition checked")
            .clone();
        let ledger = self.automation.ledger();
        let reservation = ledger_entry(
            &firing,
            now,
            AutomationOutcome::Judging,
            Some(format!(
                "Ask provider {}: judgment reserved",
                condition.provider
            )),
        );
        if let Err(reason) = ledger
            .reserve_judgment(reservation, condition.max_per_hour, rule.cooldown())
            .await
        {
            ledger
                .append(ledger_entry(
                    &firing,
                    now,
                    AutomationOutcome::Skipped,
                    Some(reason),
                ))
                .await;
            return;
        }
        let ask = self.ask.clone();
        let backends = self.backends.clone();
        let drain_reservation = self.drain_reservations.remove(&(
            firing.rule.clone(),
            firing.pane.clone(),
            firing.episode.clone(),
        ));
        self.judgments.spawn(async move {
            let _permit = permit;
            let context = automation_judge::capture_context(&subject, &backends).await;
            let judgment = match (&ask, &context) {
                (Some(ask), Ok(context)) => {
                    automation_judge::evaluate(ask, &condition, context).await
                }
                (_, Err(_)) => {
                    AutomationJudgment::unknown(&condition, "Current pane context is unavailable")
                }
                (None, _) => AutomationJudgment::unknown(&condition, "Ask is unavailable"),
            };
            JudgedFiring {
                firing,
                rule,
                subject,
                revision,
                context: context.ok(),
                judgment,
                drain_reservation,
            }
        });
    }

    async fn finish_judgment(&mut self, mut completed: JudgedFiring) {
        let condition = completed
            .rule
            .ask_condition
            .as_ref()
            .expect("judged rule has a condition");
        let mut execution = "not_matched".to_string();
        let mut outcome = AutomationOutcome::Judged;
        if condition.observe_only {
            execution = "observe_only".into();
        } else if completed.judgment.decision == JudgmentDecision::Match {
            match self.confirm_judgment(&completed).await {
                Ok(()) if completed.firing.action == AutomationAction::SwapAccount => {
                    let detail = judgment_detail(&completed, "admitted");
                    let (firing, rule) = (completed.firing.clone(), completed.rule.clone());
                    let admission = self
                        .admit_swap(
                            &firing,
                            &rule,
                            completed.revision,
                            Some(detail),
                            true,
                            &mut completed.drain_reservation,
                        )
                        .await;
                    match admission {
                        // Its own end is recorded when its job ends.
                        Admission::Admitted => return,
                        Admission::Deferred => {
                            self.deferred_judgments.push_back(completed);
                            return;
                        }
                        Admission::Refused(reason) => execution = reason,
                        Admission::Unavailable => {
                            execution = "action_failed".into();
                            outcome = AutomationOutcome::Failed;
                        }
                    }
                }
                Ok(()) => match self.perform(&completed.firing).await {
                    Performed::Done => {
                        execution = "fired".into();
                        outcome = AutomationOutcome::Fired;
                    }
                    Performed::Failed => {
                        execution = "action_failed".into();
                        outcome = AutomationOutcome::Failed;
                    }
                    Performed::Busy => {
                        self.deferred_judgments.push_back(completed);
                        return;
                    }
                },
                Err(reason) => execution = reason,
            }
        }
        let detail = judgment_detail(&completed, &execution);
        self.automation
            .ledger()
            .append(ledger_entry(
                &completed.firing,
                time::OffsetDateTime::now_utc(),
                outcome,
                Some(detail),
            ))
            .await;
    }

    async fn confirm_judgment(&mut self, completed: &JudgedFiring) -> Result<(), String> {
        if *self.automation.subscribe().borrow() != completed.revision {
            return Err("configuration_changed".into());
        }
        self.panes_read_at = None;
        let subject = self
            .live_subject(&completed.rule, &completed.firing.agent_session_id)
            .await
            .ok_or("pane_gone")?;
        if subject != completed.subject {
            return Err("agent_state_changed".into());
        }
        let fresh = automation_judge::capture_context(&subject, &self.backends)
            .await
            .map_err(|_| "context_unavailable")?;
        if completed.context.as_ref() != Some(&fresh) {
            return Err("context_changed".into());
        }
        let config = self.automation.config().await;
        let Some(rule) = config.rule_named(&completed.firing.rule) else {
            return Err("rule_removed".into());
        };
        if rule != &completed.rule || *self.automation.subscribe().borrow() != completed.revision {
            return Err("configuration_changed".into());
        }
        let now = time::OffsetDateTime::now_utc();
        let latest = self
            .live_subject(rule, &completed.firing.agent_session_id)
            .await;
        if latest.as_ref() != Some(&completed.subject) {
            return Err("agent_state_changed".into());
        }
        let mut guards = self
            .automation
            .ledger()
            .guard_state(
                &completed.firing.rule,
                &completed.firing.pane,
                &completed.firing.episode,
                now,
            )
            .await;
        guards.episode_handled = false;
        match Scheduler::confirm(
            &config,
            rule,
            &completed.firing,
            latest.as_ref(),
            guards,
            now,
        ) {
            Decision::Fire(_) => Ok(()),
            Decision::Skip(reason) => Err(reason.to_string()),
        }
    }

    /// Carry out one action. Every arm keeps the injection inside this
    /// function, so "what an automation can do to a pane" has exactly one
    /// place to read.
    async fn perform(&mut self, firing: &PlannedFiring) -> Performed {
        match firing.action {
            // A notice, not a keystroke: nothing is typed into the pane, so
            // it takes no hold on it. The ledger entry is the durable half,
            // and `tend automation log` is where an operator reads it back.
            AutomationAction::Notify => {
                tracing::warn!(
                    rule = %firing.rule,
                    pane = %firing.pane,
                    agent = %firing.agent,
                    message = firing.text.as_deref().unwrap_or_default(),
                    "automation notice",
                );
                return Performed::Done;
            }
            // Admitted, never performed here: see `admit_swap`.
            AutomationAction::SwapAccount => {
                tracing::error!(
                    rule = %firing.rule,
                    pane = %firing.pane,
                    "an automation swap reached perform; it is admitted instead"
                );
                return Performed::Failed;
            }
            _ => {}
        }
        // A pane something else is acting on is tried again later, never
        // waited for: a swap can hold it for minutes.
        let io_guard = if let Some(collaboration) = &self.collaboration {
            let Some(gate) = collaboration.try_worker_action_guard(&firing.pane).await else {
                return Performed::Busy;
            };
            if collaboration
                .worker_input_blocked(&firing.pane, firing.socket.as_deref())
                .await
            {
                return Performed::Failed;
            }
            Some(Arc::new(gate))
        } else {
            None
        };
        let done = match firing.action {
            AutomationAction::PressEnter => {
                self.confirm_selected_yes(firing, io_guard.clone()).await
            }
            AutomationAction::SendPrompt => {
                let Some(text) = firing.text.as_deref() else {
                    return Performed::Failed;
                };
                if !send_automation_text(&self.backends, firing, text, io_guard.clone()).await {
                    return Performed::Failed;
                }
                if !firing.submit {
                    return Performed::Done;
                }
                // Native hosts use explicit paste boundaries; only hosts
                // without that support need a best-effort fallback delay.
                tokio::time::sleep(tend::backend::prompt_submit_grace(&firing.pane)).await;
                send_automation_text(&self.backends, firing, "\r", io_guard.clone()).await
            }
            // ETX — the same byte `tend agent control --action interrupt`
            // writes into a native PTY session.
            AutomationAction::Interrupt => {
                send_automation_text(&self.backends, firing, "\u{3}", io_guard.clone()).await
            }
            AutomationAction::Notify | AutomationAction::SwapAccount => false,
        };
        if done {
            Performed::Done
        } else {
            Performed::Failed
        }
    }

    /// Admit a swap for `firing`, never waiting on anything that can hold:
    /// with room for it (globally and on its pane) and the guards still
    /// passing under the ledger's lock, it is recorded as `admitted`,
    /// charged once, and handed to a task that takes its pane, checks it is
    /// still due, starts its job and follows it to its end.
    async fn admit_swap(
        &mut self,
        firing: &PlannedFiring,
        rule: &AutomationRule,
        revision: u64,
        detail: Option<String>,
        judged: bool,
        drain: &mut Option<tend::collaboration::WorkerAutomationReservation>,
    ) -> Admission {
        let Some(swap) = self.swap_service.clone() else {
            tracing::warn!(
                rule = %firing.rule,
                pane = %firing.pane,
                "automation swap refused: swap jobs are unavailable on this daemon"
            );
            return Admission::Unavailable;
        };
        if self.swap_inflight.len() >= AUTOMATION_SWAP_CAPACITY
            || self
                .swap_inflight
                .values()
                .any(|op| op.admitted.pane == firing.pane)
        {
            return Admission::Deferred;
        }
        let key = swap_runner::request_key(
            &swap.node_id,
            &firing.pane,
            firing.socket.as_deref(),
            &firing.agent_session_id,
            &firing.rule,
            &firing.episode,
        );
        let config = self.automation.config().await;
        let subject = self.live_subject(rule, &firing.agent_session_id).await;
        if *self.automation.subscribe().borrow() != revision
            || config.rule_named(&firing.rule) != Some(rule)
        {
            return Admission::Refused("configuration_changed".into());
        }
        let now = time::OffsetDateTime::now_utc();
        let mut admitted = ledger_entry(firing, now, AutomationOutcome::Admitted, detail);
        admitted.operation = Some(key.clone());
        let check = |mut guards: tend::automation::GuardState| {
            // A judged firing's own judgment already holds its episode.
            if judged {
                guards.episode_handled = false;
            }
            match Scheduler::confirm(&config, rule, firing, subject.as_ref(), guards, now) {
                Decision::Fire(_) => Ok(()),
                Decision::Skip(reason) => Err(reason.to_string()),
            }
        };
        if let Err(reason) = self
            .automation
            .ledger()
            .reserve_operation(admitted.clone(), check)
            .await
        {
            return Admission::Refused(reason);
        }
        tracing::info!(
            rule = %firing.rule,
            pane = %firing.pane,
            %key,
            "automation swap admitted",
        );
        self.swap_inflight.insert(
            key.clone(),
            SwapOperation {
                admitted,
                _drain_reservation: drain.take(),
            },
        );
        let start = SwapStart {
            firing: firing.clone(),
            revision,
            revision_now: self.automation.subscribe(),
            store: self.store.clone(),
            collaboration: self.collaboration.clone(),
            submitting: swap.submissions.begin(&key),
        };
        self.spawn_swap_task(&swap, key, Some(start));
        Admission::Admitted
    }

    fn spawn_swap_task(&mut self, swap: &Arc<DaemonSwap>, key: String, start: Option<SwapStart>) {
        let task = self
            .swap_ops
            .spawn(run_swap_operation(Arc::clone(swap), key.clone(), start));
        self.swap_tasks.insert(task.id(), key);
    }

    /// Follow again every operation an earlier engine admitted and nothing
    /// ended: never starting it again, only reading what its job did.
    async fn reattach_swaps(&mut self) {
        for admitted in self.automation.ledger().unresolved_operations().await {
            let Some(key) = admitted.operation.clone() else {
                continue;
            };
            if self.swap_inflight.contains_key(&key) {
                continue;
            }
            tracing::info!(rule = %admitted.rule, pane = %admitted.pane, %key, "automation swap reattached");
            self.swap_inflight.insert(
                key.clone(),
                SwapOperation {
                    admitted,
                    _drain_reservation: None,
                },
            );
            match self.swap_service.clone() {
                Some(swap) => self.spawn_swap_task(&swap, key, None),
                // Its admission stays charged until a daemon that runs swap
                // jobs reads what became of it.
                None => {
                    tracing::warn!(%key, "automation swap left unresolved: swap jobs are unavailable");
                }
            }
        }
    }

    async fn swap_task_ended(
        &mut self,
        ended: Result<(tokio::task::Id, SwapEnded), tokio::task::JoinError>,
    ) {
        match ended {
            Ok((id, ended)) => {
                self.swap_tasks.remove(&id);
                self.finish_swap_operation(ended).await;
            }
            Err(error) => {
                let Some(key) = self.swap_tasks.remove(&error.id()) else {
                    return;
                };
                // A task that failed loses nothing: its operation is
                // followed again by its key, never started again.
                tracing::warn!(%key, %error, "automation swap task failed; following its job again");
                if let Some(swap) = self.swap_service.clone() {
                    self.spawn_swap_task(&swap, key, None);
                }
            }
        }
        // Room may have freed for a judged swap that found none.
        for completed in std::mem::take(&mut self.deferred_judgments) {
            self.finish_judgment(completed).await;
        }
    }

    /// Record how an operation ended, once, as the entry that closes its
    /// admission: `fired` only for a job that ended done with its one
    /// target swapped, cleanly.
    async fn finish_swap_operation(&mut self, ended: SwapEnded) {
        let Some(operation) = self.swap_inflight.remove(&ended.key) else {
            return;
        };
        let admitted = operation.admitted;
        let (outcome, execution) = match &ended.end {
            SwapEnd::Released(reason) => (AutomationOutcome::Skipped, reason.clone()),
            SwapEnd::Ended(view) if swap_fired(view) => (AutomationOutcome::Fired, "fired".into()),
            SwapEnd::Ended(view) => (
                AutomationOutcome::Failed,
                format!(
                    "{}: job {} ended {}{}",
                    SkipReason::ActionFailed,
                    view.job_id,
                    view.state,
                    view.fault
                        .as_deref()
                        .map(|fault| format!(" ({fault})"))
                        .unwrap_or_default()
                ),
            ),
        };
        // A judged swap keeps its judgment, with what came of it.
        let detail = match admitted
            .detail
            .as_deref()
            .and_then(|detail| serde_json::from_str::<serde_json::Value>(detail).ok())
        {
            Some(mut judged) if judged.get("judgment").is_some() => {
                judged["execution"] = serde_json::json!(execution);
                Some(judged.to_string())
            }
            _ if outcome == AutomationOutcome::Fired => admitted.detail.clone(),
            _ => Some(execution),
        };
        tracing::info!(
            rule = %admitted.rule,
            pane = %admitted.pane,
            key = %ended.key,
            job = ended.job.as_deref().unwrap_or("none"),
            %outcome,
            "automation account swap",
        );
        self.automation
            .ledger()
            .append(AutomationLedgerEntry {
                fired_at: time::OffsetDateTime::now_utc(),
                outcome,
                detail,
                job: ended.job,
                ..admitted
            })
            .await;
    }

    /// Capture immediately before Enter; a waiting state alone cannot identify
    /// the selected option. Recheck state/config after the blocking capture.
    async fn confirm_selected_yes(
        &self,
        firing: &PlannedFiring,
        io_guard: Option<WorkerIoGuard>,
    ) -> bool {
        let revision = *self.automation.subscribe().borrow();
        let Some(kind) = tend::backend::pane_id_host_kind(&firing.pane) else {
            return false;
        };
        let Some(backend) = self
            .backends
            .iter()
            .find(|backend| backend.kind() == kind)
            .cloned()
        else {
            return false;
        };
        let pane = firing.pane.clone();
        let socket = firing.socket.clone();
        let capture =
            tokio::task::spawn_blocking(move || backend.capture_pane_on(socket.as_deref(), &pane));
        let Ok(Ok(Some(screen))) =
            tokio::time::timeout(std::time::Duration::from_secs(3), capture).await
        else {
            return false;
        };
        if !tend::automation_choice::affirmative_choice_visible(&screen) {
            return false;
        }
        let config = self.automation.config().await;
        let Some(rule) = config.rule_named(&firing.rule) else {
            return false;
        };
        if revision != *self.automation.subscribe().borrow() {
            return false;
        }
        if !config.active_at(time::OffsetDateTime::now_utc())
            || !rule.enabled
            || rule.action != AutomationAction::PressEnter
        {
            return false;
        }

        let Some(agent) = self.store.by_session(&firing.agent_session_id).await else {
            return false;
        };
        let subject = AutomationSubject::from_agent(&agent);
        if subject.pane.as_deref() != Some(firing.pane.as_str())
            || subject.socket != firing.socket
            || subject.episode() != firing.episode
            || !subject.satisfies(tend::automation::AutomationCondition::WaitingInput)
        {
            return false;
        }
        // A rule that names the command confirms only that command: the one
        // the agent's hook reported must be the one in the dialog on this
        // capture, and the pattern must cover it in full.
        if let Some(pattern) = &rule.screen {
            if !tend::automation_choice::approval_matches(
                &screen,
                agent.pending_command.as_deref(),
                pattern,
            ) {
                return false;
            }
        }
        send_automation_text(&self.backends, firing, "\r", io_guard.clone()).await
    }

    /// The registry row behind a firing, as the live store has it now.
    async fn live_subject(
        &mut self,
        rule: &AutomationRule,
        agent_session_id: &str,
    ) -> Option<AutomationSubject> {
        let agent = self.store.by_session(agent_session_id).await?;
        Some(self.subject_for(std::slice::from_ref(rule), &agent).await)
    }

    async fn subject_for(
        &mut self,
        rules: &[AutomationRule],
        agent: &tend::Agent,
    ) -> AutomationSubject {
        if rules.iter().any(AutomationRule::needs_pane_metadata) {
            self.refresh_panes().await;
            let agents = [agent.clone()];
            let mut subjects = subjects_from(&agents, &self.panes);
            if let Some(subject) = subjects.pop() {
                return subject;
            }
        }
        AutomationSubject::from_agent(agent)
    }

    /// Refresh the pane scan when it has gone stale. Only reached when a
    /// rule actually reads workspace/work metadata.
    async fn refresh_panes(&mut self) {
        let fresh = self.panes_read_at.is_some_and(|at| {
            at.elapsed() < std::time::Duration::from_secs(AUTOMATION_PANE_CACHE_SECONDS)
        });
        if fresh {
            return;
        }
        let backends = self.backends.clone();
        let scan = tokio::task::spawn_blocking(move || {
            backends
                .iter()
                .flat_map(|backend| backend.list_panes())
                .collect::<Vec<_>>()
        });
        self.panes = tokio::time::timeout(std::time::Duration::from_secs(3), scan)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or_default();
        self.panes_read_at = Some(std::time::Instant::now());
    }
}

fn ledger_entry(
    firing: &PlannedFiring,
    at: time::OffsetDateTime,
    outcome: AutomationOutcome,
    detail: Option<String>,
) -> AutomationLedgerEntry {
    AutomationLedgerEntry {
        rule: firing.rule.clone(),
        pane: firing.pane.clone(),
        agent: firing.agent,
        fired_at: at,
        action: firing.action,
        outcome,
        detail,
        episode: Some(firing.episode.clone()),
        operation: None,
        job: None,
    }
}

/// A random ratio in `[0, 1)` for the per-firing jitter. Derived from a v4
/// UUID so the engine needs no RNG dependency of its own; the scheduler
/// takes the ratio as an argument precisely so tests can pin it.
fn jitter_ratio() -> f64 {
    let bits = u64::from(uuid::Uuid::new_v4().as_fields().0);
    #[allow(clippy::cast_precision_loss)]
    {
        bits as f64 / f64::from(u32::MAX)
    }
}

/// Inject literal text into an automation firing's pane. A backend without
/// `send_text` (zellij) simply refuses, which the caller records as a
/// failure rather than a silent success.
async fn send_automation_text(
    backends: &[tend::SharedBackend],
    firing: &PlannedFiring,
    text: &str,
    io_guard: Option<WorkerIoGuard>,
) -> bool {
    send_pane_text(
        backends,
        &firing.pane,
        firing.socket.as_deref(),
        text,
        io_guard,
    )
    .await
}

#[cfg(test)]
#[path = "automation_engine_tests.rs"]
mod tests;
