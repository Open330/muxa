//! The daemon's half of the account module: the swap job service it opens,
//! the automation's swap operations, and what the service reads of the
//! daemon. Built with the `aas` feature; `swap_runner_off.rs` stands in
//! without it.

use super::{
    paths, Arc, AutomationSubject, CollaborationStore, PlannedFiring, Result, WorkerIoGuard,
};
use crate::automation_engine::{Submissions, SwapEnd, SwapEnded, SwapStart};

/// Wait for running jobs to end, up to `timeout`.
pub(crate) async fn drain(swap: &DaemonSwap, timeout: std::time::Duration) -> bool {
    swap.service.drain(timeout).await
}

/// What IPC serves.
pub(crate) fn jobs(swap: &Arc<DaemonSwap>) -> Arc<dyn tend::swap_api::SwapJobs> {
    Arc::clone(&swap.service) as Arc<dyn tend::swap_api::SwapJobs>
}

/// The request key an automation firing's swap job is filed under.
pub(crate) fn request_key(
    node_id: &str,
    pane: &str,
    socket: Option<&str>,
    agent_session_id: &str,
    rule: &str,
    episode: &str,
) -> String {
    tend_aas::swap_service::automation_request_key(
        node_id,
        pane,
        socket,
        agent_session_id,
        rule,
        episode,
    )
}

/// The job store schema this build reads.
pub(crate) const JOB_SCHEMA: i64 = tend_aas::swap_service::JOB_SCHEMA;

/// The swap job service and the node it belongs to.
pub(crate) struct DaemonSwap {
    pub(crate) service: Arc<tend_aas::swap_service::SwapService>,
    pub(crate) node_id: String,
    /// Automation submissions still able to accept a job, by key, for as
    /// long as this daemon (and the service) lives.
    pub(crate) submissions: Arc<Submissions>,
}

/// Open the swap job service off the reactor. A node identity that cannot
/// be persisted disables swaps rather than inventing one: repeat requests
/// are recognised by it across restarts.
pub(crate) async fn open_swap_service(
    store: tend::SharedStore,
    backends: Vec<tend::SharedBackend>,
    swap: tend_aas::account_swap::SwapConfig,
    accounts: tend::accounts::AccountsConfig,
    config_path: Option<std::path::PathBuf>,
    collaboration: Arc<CollaborationStore>,
) -> Result<Arc<DaemonSwap>, String> {
    let runtime = tokio::runtime::Handle::current();
    let receipts = Arc::new(tend_aas::swap_service::PromptReceipts::new(
        store.prompt_sequence(),
    ));
    spawn_prompt_receipts(&store, Arc::clone(&receipts));
    let opened = tokio::task::spawn_blocking(move || {
        let path = paths::default_node_id_file().ok_or("no data directory for the node id")?;
        let node_id = tend::fleet::load_or_create_node_id(&path)
            .map_err(|error| format!("node id at {}: {error}", path.display()))?
            .as_str()
            .to_owned();
        let context = DaemonContext {
            runtime,
            store,
            backends,
            swap,
            accounts,
            config_path,
            node_id: node_id.clone(),
            collaboration,
            receipts,
        };
        tend_aas::swap_service::SwapService::open(None, Arc::new(context)).map(|service| {
            DaemonSwap {
                service: Arc::new(service),
                node_id,
                submissions: Arc::default(),
            }
        })
    })
    .await;
    match opened {
        Ok(Ok(swap)) => Ok(Arc::new(swap)),
        Ok(Err(error)) => {
            tracing::warn!(%error, "swap jobs disabled");
            Err(error)
        }
        Err(error) => {
            tracing::warn!(%error, "swap jobs disabled: could not open their store");
            Err(format!("could not open the swap job store: {error}"))
        }
    }
}

/// One admitted swap operation, from its pane's guard to its job's end.
/// With `start`, it is new: it takes the pane, checks the firing is still
/// due and starts the job under `key`. Without, it only finds the job an
/// earlier run started under `key`. Either way it then follows that job
/// for as long as it runs: a wait that runs out is waited again, never
/// taken for an end.
pub(crate) async fn run_swap_operation(
    swap: Arc<DaemonSwap>,
    key: String,
    start: Option<SwapStart>,
) -> SwapEnded {
    let service = &swap.service;
    let released = |reason: String| SwapEnded {
        key: key.clone(),
        job: None,
        end: SwapEnd::Released(reason),
    };
    let job = match start {
        Some(start) => match start_swap_job(&swap, &key, start).await {
            Ok(job) => job,
            Err(reason) => return released(reason),
        },
        None => match find_swap_job(&swap, &key).await {
            Some(job) => job,
            None => return released("no job was accepted under its key".into()),
        },
    };
    let mut pause = std::time::Duration::from_secs(5);
    loop {
        match service.wait(&job, AUTOMATION_SWAP_WAIT).await {
            Ok(view) if job_settled(&view) => {
                return SwapEnded {
                    key,
                    job: Some(job),
                    end: SwapEnd::Ended(view),
                };
            }
            Ok(view) => {
                tracing::info!(%key, %job, state = %view.state, "automation swap still running");
            }
            Err(error) => {
                tracing::info!(%key, %job, %error, "automation swap still being followed");
            }
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(AUTOMATION_SWAP_BACKOFF_MAX);
    }
}

/// Take the pane, check the firing is still due, and start its job: the
/// job's id, or why nothing was started.
async fn start_swap_job(swap: &DaemonSwap, key: &str, start: SwapStart) -> Result<String, String> {
    let SwapStart {
        firing,
        revision,
        revision_now,
        store,
        collaboration,
        submitting,
    } = start;
    let gate = match &collaboration {
        Some(collaboration) => {
            let gate = collaboration.worker_action_guard(&firing.pane).await;
            if collaboration
                .worker_input_blocked(&firing.pane, firing.socket.as_deref())
                .await
            {
                return Err("the worker's input is blocked".into());
            }
            Some(Arc::new(gate))
        }
        None => None,
    };
    // The guard may have been waited on: the firing has to still be due,
    // for the same configuration, agent and server, now that it is held.
    swap_still_due_now(&store, &revision_now, &firing, revision).await?;
    let overrides = tend_aas::swap_service::SwapOverrides {
        targets: vec![firing.agent_session_id.clone()],
        to: firing.account.clone(),
        force: true,
        continue_text: firing.text.clone(),
        ..tend_aas::swap_service::SwapOverrides::default()
    };
    // The guard moves into the swap job: no copy of it is kept here while
    // the job runs.
    let guard = gate.map(|gate| {
        Box::new(PaneGuard {
            pane: firing.pane.clone(),
            socket: firing.socket.clone(),
            _gate: gate,
        }) as Box<dyn tend_aas::swap_service::TargetGuard>
    });
    let still_due = automation_still_due(
        tokio::runtime::Handle::current(),
        store,
        revision_now,
        &firing,
        revision,
    );
    let submit = Arc::clone(&swap.service);
    let submit_key = key.to_owned();
    // The claim moves into the call: whoever looks for this key later
    // waits for the call, even if this task is gone by then.
    let accepted = tokio::task::spawn_blocking(move || {
        let _submitting = submitting;
        submit.submit_automation(&submit_key, &overrides, guard, Some(still_due))
    })
    .await;
    match accepted {
        Ok(Ok(accepted)) => Ok(accepted.job_id),
        // Refused, or not known to have been: the key says which.
        Ok(Err(error)) => find_swap_job(swap, key).await.ok_or(error),
        Err(error) => find_swap_job(swap, key)
            .await
            .ok_or_else(|| error.to_string()),
    }
}

/// The job started under `key`, if one was. Asked only once nothing is
/// submitting under it any more (a submission still planning may yet
/// accept one); a store that does not answer is asked again, and only "not
/// found" then says no job was accepted.
async fn find_swap_job(swap: &DaemonSwap, key: &str) -> Option<String> {
    use tend_aas::swap_service::SwapJobs as _;
    let mut pause = std::time::Duration::from_secs(1);
    loop {
        swap.submissions.settled(key).await;
        let read = Arc::clone(&swap.service);
        let job = tend_aas::swap_service::JobRef::Key(key.to_owned());
        match tokio::task::spawn_blocking(move || read.status(&job)).await {
            Ok(Ok(view)) => return Some(view.job_id),
            Ok(Err(error)) if error.contains("not found") => return None,
            Ok(Err(error)) => {
                tracing::warn!(%key, %error, "automation swap lookup failed; asking again");
            }
            Err(error) => {
                tracing::warn!(%key, %error, "automation swap lookup failed; asking again");
            }
        }
        tokio::time::sleep(pause).await;
        pause = (pause * 2).min(AUTOMATION_SWAP_BACKOFF_MAX);
    }
}

/// Feed every agent prompt hook the store receives into `receipts`, its
/// text dropped, for a swap follow-up's evidence. A lagging receiver only
/// loses evidence, never claims any.
pub(crate) fn spawn_prompt_receipts(
    store: &tend::SharedStore,
    receipts: Arc<tend_aas::swap_service::PromptReceipts>,
) {
    let mut prompts = store.subscribe_prompts();
    tokio::spawn(async move {
        loop {
            match prompts.recv().await {
                Ok(record) => receipts.record(&record),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

/// What the swap service reads of the daemon. Its methods block on the
/// runtime, so the service only ever calls them from its own threads.
pub(crate) struct DaemonContext {
    pub(crate) runtime: tokio::runtime::Handle,
    pub(crate) store: tend::SharedStore,
    pub(crate) backends: Vec<tend::SharedBackend>,
    pub(crate) swap: tend_aas::account_swap::SwapConfig,
    pub(crate) accounts: tend::accounts::AccountsConfig,
    pub(crate) config_path: Option<std::path::PathBuf>,
    pub(crate) node_id: String,
    pub(crate) collaboration: Arc<CollaborationStore>,
    pub(crate) receipts: Arc<tend_aas::swap_service::PromptReceipts>,
}

impl tend_aas::swap_service::ContextProvider for DaemonContext {
    fn turn_probe(&self) -> Option<Arc<dyn tend_aas::account_swap::TurnProbe>> {
        Some(Arc::clone(&self.receipts) as Arc<dyn tend_aas::account_swap::TurnProbe>)
    }

    fn snapshot(&self) -> Result<tend_aas::swap_service::Snapshot, String> {
        Ok(tend_aas::swap_service::Snapshot {
            node_id: self.node_id.clone(),
            agents: self.runtime.block_on(self.store.snapshot()),
            backends: self.backends.clone(),
            config: self.swap.clone(),
            accounts: self.accounts.clone(),
            config_path: self.config_path.clone(),
        })
    }

    /// The same exclusion every other writer to the pane takes, and the
    /// same refusal of a pane whose input is blocked or draining.
    fn acquire_guard(
        &self,
        pane: &str,
        socket: Option<&str>,
    ) -> Result<Box<dyn tend_aas::swap_service::TargetGuard>, String> {
        let collaboration = Arc::clone(&self.collaboration);
        let pane = pane.to_owned();
        let socket = socket.map(str::to_owned);
        self.runtime.block_on(async move {
            let gate = collaboration.worker_action_guard(&pane).await;
            if collaboration
                .worker_input_blocked(&pane, socket.as_deref())
                .await
            {
                return Err("input to this pane is blocked or draining".to_owned());
            }
            Ok(Box::new(PaneGuard {
                pane,
                socket,
                _gate: Arc::new(gate),
            })
                as Box<dyn tend_aas::swap_service::TargetGuard>)
        })
    }
}

/// A held pane guard. The daemon's guard is per pane name, so with no
/// server it covers that pane on any; with one, only that endpoint — a
/// stale server never authorises another.
pub(crate) struct PaneGuard {
    pub(crate) pane: String,
    pub(crate) socket: Option<String>,
    pub(crate) _gate: WorkerIoGuard,
}

impl tend_aas::swap_service::TargetGuard for PaneGuard {
    fn covers(&self, endpoint: &str) -> bool {
        match &self.socket {
            Some(socket) => endpoint == tend_aas::swap_service::endpoint(&self.pane, Some(socket)),
            None => endpoint
                .strip_prefix(self.pane.as_str())
                .is_some_and(|rest| rest.starts_with('@')),
        }
    }
}

/// How long one wait on an automation's swap job lasts. Running out only
/// ends that wait: the job carries on under the service and its operation
/// is waited on again.
pub(crate) const AUTOMATION_SWAP_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

/// The longest pause between waits on an operation whose job or store does
/// not answer.
pub(crate) const AUTOMATION_SWAP_BACKOFF_MAX: std::time::Duration =
    std::time::Duration::from_secs(300);

/// The automation's word, asked by the swap service from its own threads,
/// that a firing is still wanted: the rule's configuration unchanged, the
/// agent on the same pane and server in the same episode, its condition
/// still holding. Asked after the plan is made and before the target starts.
pub(crate) fn automation_still_due(
    runtime: tokio::runtime::Handle,
    store: tend::SharedStore,
    revision_now: tokio::sync::watch::Receiver<u64>,
    firing: &PlannedFiring,
    revision: u64,
) -> tend::swap_api::StillDue {
    still_due_from(
        move |session| runtime.block_on(store.by_session(session)),
        revision_now,
        firing,
        revision,
    )
}

/// [`automation_still_due`] over any way of reading the agent's row.
pub(crate) fn still_due_from(
    read: impl Fn(&str) -> Option<tend::Agent> + Send + 'static,
    revision_now: tokio::sync::watch::Receiver<u64>,
    firing: &PlannedFiring,
    revision: u64,
) -> tend::swap_api::StillDue {
    let session = firing.agent_session_id.clone();
    let episode = firing.episode.clone();
    let pane = firing.pane.clone();
    let socket = firing.socket.clone();
    let condition = firing.only_if_still;
    Box::new(move || {
        let unchanged = || {
            if *revision_now.borrow() == revision {
                Ok(())
            } else {
                Err("the automation configuration changed".to_owned())
            }
        };
        unchanged()?;
        let agent = read(&session);
        still_due_with(
            agent.as_ref(),
            &pane,
            socket.as_deref(),
            &episode,
            condition,
        )?;
        // The store read can wait: a configuration change meanwhile counts.
        unchanged()
    })
}

/// [`automation_still_due`], asked from the reactor.
async fn swap_still_due_now(
    store: &tend::SharedStore,
    revision_now: &tokio::sync::watch::Receiver<u64>,
    firing: &PlannedFiring,
    revision: u64,
) -> Result<(), String> {
    let unchanged = || {
        if *revision_now.borrow() == revision {
            Ok(())
        } else {
            Err("the automation configuration changed".to_owned())
        }
    };
    unchanged()?;
    let agent = store.by_session(&firing.agent_session_id).await;
    still_due_with(
        agent.as_ref(),
        &firing.pane,
        firing.socket.as_deref(),
        &firing.episode,
        firing.only_if_still,
    )?;
    unchanged()
}

/// A job that will not change any more.
pub(crate) fn job_settled(view: &tend::swap_api::JobView) -> bool {
    view.fault.is_some() || !matches!(view.state.as_str(), "accepted" | "running")
}

/// The agent half of [`automation_still_due`], read from its row.
pub(crate) fn still_due_with(
    agent: Option<&tend::Agent>,
    pane: &str,
    socket: Option<&str>,
    episode: &str,
    condition: tend::automation::AutomationCondition,
) -> Result<(), String> {
    let agent = agent.ok_or_else(|| "the agent is gone".to_owned())?;
    let subject = AutomationSubject::from_agent(agent);
    if subject.pane.as_deref() != Some(pane) || subject.socket.as_deref() != socket {
        return Err("the agent moved".into());
    }
    if subject.episode() != episode {
        return Err("the agent's state changed".into());
    }
    if !subject.satisfies(condition) {
        return Err("its condition no longer holds".into());
    }
    Ok(())
}
