//! `muxa swap` — move agents onto another `aas` account and resume them.
//!
//! A daemon that runs swaps as jobs gets the request: it plans, journals and
//! runs it under the same exclusion its automation uses, and this module
//! follows the job. A pane that runs its agent directly is left to this
//! process by the daemon and swapped here, bound to the agent the daemon
//! planned. A daemon from before swap jobs, or `--local`, means the whole
//! swap runs here, as [`muxa::account_swap::run`].

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args as ClapArgs, ValueEnum};
use muxa::account_swap::{
    self, AccountChoice, BareMode, ContinueWhen, DirectTarget, SwapConfig, SwapOutcome,
    SwapRequest, SwapScope, SwapStatus,
};
use muxa::event::AgentKind;
use muxa::ipc::{Client, RuntimeError};
use muxa::swap_service::{Accepted, EntryView, JobRef, JobView, SwapOverrides};
use std::fmt::Write as _;
use std::future::Future;
use std::path::Path;
use std::time::Duration;

/// Move agents whose account ran out onto another `aas` account, resuming
/// each conversation in place.
///
/// With no target, swaps every agent stopped by a usage cap. Each agent is
/// stopped, its launch line is typed back into the pane's shell as
/// `aas e <next> … --resume <id>`, and a follow-up prompt carries the
/// cut-off turn on. Defaults come from `[swap]` in config.toml.
#[derive(Debug, ClapArgs)]
#[command(after_help = "Examples:
  muxa swap                     swap every rate-limited agent (auto-picked account)
  muxa swap %5 --to jelly@claude
  muxa swap --all --spread      move every idle agent, spread across accounts
  muxa swap --all --force --provider codex
  muxa swap --dry-run           show the plan, touch nothing
  muxa swap --status cli-…      follow a swap the daemon is running or ran
  muxa swap --host rtzr --to work@claude   swap on a Fleet host")]
#[allow(clippy::struct_excessive_bools)] // independent CLI switches
pub struct Args {
    /// Panes (`%5`, `herdr:p9`) or provider session ids to swap.
    #[arg(value_name = "TARGET", conflicts_with_all = ["all", "limited"])]
    targets: Vec<String>,
    /// Every Claude / Codex agent, not only the rate-limited ones.
    #[arg(long, conflicts_with = "limited")]
    all: bool,
    /// Only agents stopped by a usage cap (the default with no target).
    #[arg(long)]
    limited: bool,
    /// Only this provider's agents.
    #[arg(long, value_enum)]
    provider: Option<Provider>,
    /// Account to move to: an `aas` name, or `auto` (the default) for the
    /// best-ranked account with quota left.
    #[arg(long, value_name = "ACCOUNT", conflicts_with = "spread")]
    to: Option<String>,
    /// Round-robin the agents over every account with quota left.
    #[arg(long)]
    spread: bool,
    /// Also stop agents that are mid-turn.
    #[arg(long)]
    force: bool,
    /// Agents started without `aas`: leave them (`skip`) or relaunch them
    /// under it (`wrap`). Default from `[swap] bare`.
    #[arg(long, value_enum)]
    bare: Option<Bare>,
    /// Follow-up prompt typed after the resume — the wording only; who gets
    /// it is `--continue-when`. Default from `[swap] continue_text`.
    #[arg(
        long = "continue-text",
        value_name = "TEXT",
        conflicts_with_all = ["no_continue", "continue_legacy"]
    )]
    continue_text: Option<String>,
    /// Deprecated: use `--continue-text`. Sets the wording only — it no
    /// longer sends to every swapped agent; add `--continue-when always` for
    /// that.
    #[arg(long = "continue", value_name = "TEXT", conflicts_with = "no_continue")]
    continue_legacy: Option<String>,
    /// Which swapped agents get the follow-up: `limited` (their turn was cut
    /// off by a usage cap or by this swap), `always`, or `never`. Default
    /// from `[swap] continue_when`.
    #[arg(long = "continue-when", value_enum, conflicts_with = "no_continue")]
    continue_when: Option<When>,
    /// Send no follow-up prompt (`--continue-when never`).
    #[arg(long)]
    no_continue: bool,
    /// What the relaunch must show before the follow-up is typed:
    /// `binding` (its own arguments resume exactly that conversation) or
    /// `verified` (also read from the stopped process, and confirmed on the
    /// assigned account). Default from `[swap] continue_evidence`.
    #[arg(long = "continue-evidence", value_enum)]
    continue_evidence: Option<Evidence>,
    /// Print the plan without stopping or typing anything.
    #[arg(long)]
    dry_run: bool,
    #[arg(long)]
    json: bool,
    /// Run the swap on this Fleet host (a control-mode `[fleet.hosts]`
    /// alias) instead of here: its daemon, its agents, its config.
    #[arg(long, value_name = "ALIAS")]
    host: Option<String>,
    /// Leave this pane alone whatever the scope (repeatable).
    #[arg(long, value_name = "PANE")]
    exclude: Vec<String>,
    /// Run the swap in this process rather than as a daemon job: no journal,
    /// no exclusion against the daemon's own automation.
    #[arg(long)]
    local: bool,
    /// Show a daemon swap job by its request key (printed when it started)
    /// or job id, and wait for it to end.
    #[arg(
        long,
        value_name = "KEY",
        conflicts_with_all = [
            "targets", "all", "limited", "provider", "to", "spread", "force", "bare",
            "continue_text", "continue_legacy", "continue_when", "no_continue",
            "continue_evidence", "dry_run",
            "exclude", "local",
        ]
    )]
    status: Option<String>,
    /// Retry a daemon swap job that has ended (by key or job id): only a
    /// target that failed before its relaunch, while the very agent it
    /// planned still runs, is swapped again, on the account first assigned,
    /// with no follow-up; the others are only looked at. `--force` and
    /// `--continue-evidence` may be given.
    #[arg(
        long,
        value_name = "KEY",
        conflicts_with_all = [
            "targets", "all", "limited", "provider", "to", "spread", "bare",
            "continue_text", "continue_legacy", "continue_when", "no_continue",
            "dry_run", "exclude", "local", "status",
        ]
    )]
    retry: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Provider {
    Claude,
    Codex,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Bare {
    Skip,
    Wrap,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Evidence {
    Binding,
    Verified,
}

impl Evidence {
    fn as_str(self) -> &'static str {
        match self {
            Self::Binding => "binding",
            Self::Verified => "verified",
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum When {
    Limited,
    Always,
    Never,
}

impl From<When> for ContinueWhen {
    fn from(when: When) -> Self {
        match when {
            When::Limited => Self::Limited,
            When::Always => Self::Always,
            When::Never => Self::Never,
        }
    }
}

impl Args {
    /// The follow-up wording from either spelling.
    fn follow_up_text(&self) -> Option<&String> {
        self.continue_text
            .as_ref()
            .or(self.continue_legacy.as_ref())
    }
}

/// Printed when the deprecated spelling is used: it no longer means "send
/// to every agent", and a caller relying on that should say so.
const CONTINUE_DEPRECATED: &str = "muxa swap: --continue is deprecated; use --continue-text. \
     It now keeps the configured continue_when policy (only agents whose turn was cut off); \
     add --continue-when always to send it to every swapped agent";

fn request(args: &Args, config: &SwapConfig) -> SwapRequest {
    let scope = if !args.targets.is_empty() {
        SwapScope::Targets(args.targets.clone())
    } else if args.all {
        SwapScope::All
    } else {
        SwapScope::Limited
    };
    let mut request = SwapRequest::new(scope, config);
    request.provider = args.provider.map(|provider| match provider {
        Provider::Claude => AgentKind::ClaudeCode,
        Provider::Codex => AgentKind::Codex,
    });
    request.to = if args.spread {
        AccountChoice::Spread
    } else {
        AccountChoice::parse(args.to.as_deref().unwrap_or("auto"))
    };
    request.force = args.force;
    if let Some(bare) = args.bare {
        request.bare = match bare {
            Bare::Skip => BareMode::Skip,
            Bare::Wrap => BareMode::Wrap,
        };
    }
    // The wording and who gets it are separate: naming the prompt does not
    // widen it to agents that had nothing cut off.
    if let Some(text) = args.follow_up_text() {
        request.continue_text = Some(text.clone()).filter(|text| !text.trim().is_empty());
    }
    if args.no_continue {
        request.continue_when = ContinueWhen::Never;
    } else if let Some(when) = args.continue_when {
        request.continue_when = when.into();
    }
    if let Some(evidence) = args.continue_evidence {
        request.continue_evidence = match evidence {
            Evidence::Binding => account_swap::ContinueEvidence::Binding,
            Evidence::Verified => account_swap::ContinueEvidence::Verified,
        };
    }
    request.dry_run = args.dry_run;
    request.exclude.clone_from(&args.exclude);
    request
}

pub(crate) async fn run(
    client: &Client,
    args: Args,
    config: &SwapConfig,
    accounts: &muxa::accounts::AccountsConfig,
    explicit_config: Option<&Path>,
) -> Result<()> {
    if args.continue_legacy.is_some() {
        eprintln!("{CONTINUE_DEPRECATED}");
    }
    if let Some(host) = muxa::work_control::remote_host_alias(args.host.as_deref()) {
        return run_remote(client, &args, host).await;
    }
    // Reading a job never depends on the service taking new ones.
    if let Some(job) = &args.status {
        return show_status(client, job, args.json).await;
    }
    if let Some(parent) = &args.retry {
        return run_retry(client, &args, parent, explicit_config).await;
    }
    if args.local {
        return run_local(client, &args, config, accounts).await;
    }
    let hello = client.hello(HELLO_TIMEOUT).await.map_err(|error| {
        anyhow!(
            "could not ask the daemon how it runs swaps: {error}\n\
             pass --local to swap in this process instead"
        )
    })?;
    let route = match route(&hello.capabilities) {
        // No marker: an old daemon, or one that serves swap jobs from before
        // the marker and cannot take one now. Only a daemon that does not
        // know the request at all is old enough to bypass.
        Route::Legacy => probe(&client.swap_job_status(&JobRef::Key(PROBE_KEY.into())).await),
        route => route,
    };
    match route {
        Route::Daemon => run_routed(client, &args, config, accounts, explicit_config).await,
        Route::Unavailable => bail!("{UNAVAILABLE}"),
        Route::Legacy => {
            eprintln!("muxa swap: this daemon predates swap jobs; swapping in this process");
            run_local(client, &args, config, accounts).await
        }
    }
}

const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// How often a running job is read back.
const POLL: Duration = Duration::from_millis(500);

const UNAVAILABLE: &str = "swap jobs are unavailable on this daemon right now \
     (starting, halted or shutting down; see `muxa doctor`). Nothing was sent.\n\
     Retry shortly, or pass --local to swap in this process instead";

/// Where a swap runs, from what the daemon advertised.
#[derive(Debug, PartialEq, Eq)]
enum Route {
    /// As a daemon job.
    Daemon,
    /// The daemon runs swaps as jobs but cannot take one now: refuse, never
    /// run around its exclusion.
    Unavailable,
    /// A daemon from before swap jobs: here, as it always was.
    Legacy,
}

/// A key no job is made under, read to learn whether the daemon knows swap
/// jobs at all.
const PROBE_KEY: &str = "cli-probe";

/// What reading [`PROBE_KEY`] says, with no swap tag advertised: only the
/// daemon's "unknown request" is a daemon from before swap jobs. Any other
/// answer, a not-found included, is a service that exists and is not
/// taking jobs now; a failed read cannot tell, so it refuses too.
fn probe(read: &Result<JobView, RuntimeError>) -> Route {
    match read {
        Err(RuntimeError::Daemon(error)) if error.contains("unknown variant `swap_job_status`") => {
            Route::Legacy
        }
        _ => Route::Unavailable,
    }
}

fn route(capabilities: &[String]) -> Route {
    let has = |name: &str| capabilities.iter().any(|c| c == name);
    if has(muxa::ipc::SWAP_JOBS_CAPABILITY) && has(muxa::ipc::SWAP_PLAN_CAPABILITY) {
        Route::Daemon
    } else if has(muxa::ipc::SWAP_SERVICE_CAPABILITY) {
        Route::Unavailable
    } else {
        Route::Legacy
    }
}

/// The whole swap in this process.
async fn run_local(
    client: &Client,
    args: &Args,
    config: &SwapConfig,
    accounts: &muxa::accounts::AccountsConfig,
) -> Result<()> {
    let request = request(args, config);
    let agents = client.snapshot().await.context("listing agents")?;
    let backends = muxa::backend::active_backends();
    let config = config.clone();
    let accounts = accounts.clone();
    let outcomes = tokio::task::spawn_blocking(move || {
        account_swap::run(&request, &agents, &backends, &config, &accounts)
    })
    .await
    .context("swap worker")?;
    report(&outcomes, args.json)?;
    if outcomes.iter().any(|o| o.status == SwapStatus::Failed) {
        bail!("some agents could not be swapped");
    }
    Ok(())
}

fn report(outcomes: &[SwapOutcome], json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(outcomes)?);
    } else {
        print!("{}", render(outcomes));
    }
    Ok(())
}

/// The request exactly as typed, nothing filled in from configuration: its
/// digest is what makes a repeated send the same request.
fn overrides(args: &Args, explicit_config: Option<&Path>) -> SwapOverrides {
    SwapOverrides {
        targets: args.targets.clone(),
        all: args.all,
        limited: args.limited,
        provider: args.provider.map(|provider| {
            match provider {
                Provider::Claude => "claude",
                Provider::Codex => "codex",
            }
            .to_owned()
        }),
        to: args.to.clone(),
        spread: args.spread,
        force: args.force,
        bare: args.bare.map(|bare| {
            match bare {
                Bare::Skip => "skip",
                Bare::Wrap => "wrap",
            }
            .to_owned()
        }),
        continue_text: args.follow_up_text().cloned(),
        continue_when: args.continue_when.map(|when| {
            match when {
                When::Limited => "limited",
                When::Always => "always",
                When::Never => "never",
            }
            .to_owned()
        }),
        no_continue: args.no_continue,
        continue_evidence: args.continue_evidence.map(|e| e.as_str().to_owned()),
        exclude: args.exclude.clone(),
        caller_config: explicit_config.map(Path::to_path_buf),
    }
}

/// What the CLI needs of the daemon's swap jobs; a trait so the job flow
/// can be tested without one.
trait SwapDaemon {
    fn status(&self, job: &JobRef) -> impl Future<Output = Result<JobView, RuntimeError>> + Send;
}

impl SwapDaemon for Client {
    fn status(&self, job: &JobRef) -> impl Future<Output = Result<JobView, RuntimeError>> + Send {
        self.swap_job_status(job)
    }
}

/// How following a daemon job ended.
#[derive(Debug)]
enum JobEnd {
    /// The job ended, or the service faulted under it.
    Ended(JobView),
    /// The daemon answered the start with a refusal and holds no job for
    /// the key: nothing was accepted.
    Refused(String),
    /// Whether the job exists, or how far it got, is not known.
    Unknown(String),
    /// Ctrl-C: waiting stopped; the job, if accepted, runs on.
    Interrupted,
}

/// Whether there is nothing more to wait for: the job ended, or the
/// service faulted (a faulted job may stay `running` with nothing left to
/// run it).
fn settled(view: &JobView) -> bool {
    view.fault.is_some() || !matches!(view.state.as_str(), "accepted" | "running")
}

/// Start `key` and follow it to its end. Ctrl-C is armed before the start
/// is sent: an interrupted start, like a lost answer, leaves the key as the
/// only handle, and nothing here ever sends the request again.
async fn follow_job(
    daemon: &impl SwapDaemon,
    key: &str,
    start: impl Future<Output = Result<Accepted, RuntimeError>>,
    cancel: impl Future<Output = ()>,
    poll: Duration,
) -> JobEnd {
    tokio::pin!(cancel);
    let started = tokio::select! {
        biased;
        () = &mut cancel => return JobEnd::Interrupted,
        started = start => started,
    };
    let mut view = match started {
        Ok(accepted) => accepted.view,
        Err(start) => {
            let refused = matches!(start, RuntimeError::Daemon(_));
            let by_key_ref = JobRef::Key(key.to_owned());
            let by_key = tokio::select! {
                biased;
                () = &mut cancel => return JobEnd::Interrupted,
                view = daemon.status(&by_key_ref) => view,
            };
            match by_key {
                Ok(view) => view,
                // A refusal is an answer, and no job holds the key: refused.
                // Anything else may still be accepted later.
                Err(status) if refused && not_found(&status) => {
                    return JobEnd::Refused(start.to_string());
                }
                Err(status) => {
                    return JobEnd::Unknown(format!(
                        "starting it: {start}; reading it back: {status}"
                    ));
                }
            }
        }
    };
    let job = JobRef::Id(view.job_id.clone());
    loop {
        if settled(&view) {
            return JobEnd::Ended(view);
        }
        let read = tokio::select! {
            biased;
            () = &mut cancel => return JobEnd::Interrupted,
            read = async {
                tokio::time::sleep(poll).await;
                daemon.status(&job).await
            } => read,
        };
        match read {
            Ok(next) => view = next,
            Err(error) => return JobEnd::Unknown(format!("reading it back: {error}")),
        }
    }
}

/// The store's "no such job", as the daemon words it.
fn not_found(error: &RuntimeError) -> bool {
    matches!(error, RuntimeError::Daemon(message)
        if message.starts_with("swap job ") && message.ends_with(" not found"))
}

/// `--retry`: only a daemon can retry its job; nothing falls back to a
/// fresh or in-process swap.
async fn run_retry(
    client: &Client,
    args: &Args,
    parent: &str,
    explicit_config: Option<&Path>,
) -> Result<()> {
    let hello = client.hello(HELLO_TIMEOUT).await.map_err(|error| {
        anyhow!("could not ask the daemon whether it can retry swap jobs: {error}")
    })?;
    if !hello
        .capabilities
        .iter()
        .any(|c| c == muxa::ipc::SWAP_RETRY_CAPABILITY)
    {
        bail!(
            "this daemon cannot retry swap jobs right now (too old, or its swap service is \
             unavailable); nothing was sent"
        );
    }
    let overrides = muxa::swap_service::RetryOverrides {
        continue_evidence: args.continue_evidence.map(|e| e.as_str().to_owned()),
        force: args.force,
        caller_config: explicit_config.map(Path::to_path_buf),
    };
    let key = format!("cli-{}", uuid::Uuid::new_v4());
    eprintln!("muxa swap: retry {key} of {parent} (see it with `muxa swap --status {key}`)");
    let cancel = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let parent = job_ref(parent);
    let start = client.swap_job_retry(&key, &parent, &overrides);
    let view = match follow_job(client, &key, start, cancel, POLL).await {
        JobEnd::Ended(view) => view,
        JobEnd::Refused(error) => bail!("the daemon refused this retry: {error}"),
        JobEnd::Unknown(error) => bail!(
            "{error}\nthe retry may or may not have been accepted; do not retry again before \
             checking `muxa swap --status {key}`"
        ),
        JobEnd::Interrupted => bail!(
            "stopped waiting. The retry, if accepted, runs on under the daemon: \
             `muxa swap --status {key}`"
        ),
    };
    let outcomes = job_outcomes(&view);
    report(&outcomes, args.json)?;
    job_verdict(&view, &key)
}

/// The daemon-owned swap, with any direct panes swapped here afterwards.
async fn run_routed(
    client: &Client,
    args: &Args,
    config: &SwapConfig,
    accounts: &muxa::accounts::AccountsConfig,
    explicit_config: Option<&Path>,
) -> Result<()> {
    let overrides = overrides(args, explicit_config);
    let plan = client.swap_plan(&overrides).await.map_err(|error| {
        anyhow!(
            "the daemon could not plan this swap: {error}\nnothing was sent; \
             pass --local to swap in this process instead"
        )
    })?;
    if !plan.executable {
        bail!(
            "swap jobs are unavailable on this daemon: {}\nnothing was sent; \
             pass --local to swap in this process instead",
            plan.unavailable.as_deref().unwrap_or("unknown reason")
        );
    }
    let direct = plan.direct_targets.clone();
    let local_request = request(args, config);
    if args.dry_run {
        let local = run_planned_direct(client, &local_request, config, accounts, &direct).await?;
        let outcomes = merge(plan.outcomes, &direct, local);
        report(&outcomes, args.json)?;
        return Ok(());
    }
    let mut daemon_overrides = overrides;
    for target in &direct {
        if !daemon_overrides.exclude.contains(&target.pane) {
            daemon_overrides.exclude.push(target.pane.clone());
        }
    }
    let key = format!("cli-{}", uuid::Uuid::new_v4());
    eprintln!("muxa swap: request {key} (see it with `muxa swap --status {key}`)");
    let cancel = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let start = client.swap_job_start(&key, &daemon_overrides);
    let view = match follow_job(client, &key, start, cancel, POLL).await {
        JobEnd::Ended(view) => view,
        JobEnd::Refused(error) => bail!("the daemon refused this swap: {error}"),
        JobEnd::Unknown(error) => bail!(
            "{error}\nthe swap may or may not have been accepted; do not retry before \
             checking `muxa swap --status {key}`{}",
            skipped_direct(&direct)
        ),
        JobEnd::Interrupted => bail!(
            "stopped waiting. The swap, if accepted, runs on under the daemon: \
             `muxa swap --status {key}`{}",
            skipped_direct(&direct)
        ),
    };
    let clean = view.state == "done" && view.fault.is_none();
    let daemon_half = job_outcomes(&view);
    let local = if direct.is_empty() {
        Vec::new()
    } else if clean {
        eprintln!(
            "muxa swap: {} pane(s) run their agent directly; the daemon left them to this \
             process, which swaps them now as planned",
            direct.len()
        );
        run_planned_direct(client, &local_request, config, accounts, &direct).await?
    } else {
        eprintln!(
            "muxa swap: the daemon job did not end cleanly{}",
            skipped_direct(&direct)
        );
        Vec::new()
    };
    let outcomes = merge(daemon_half, &direct, local);
    report(&outcomes, args.json)?;
    job_verdict(&view, &key)?;
    if outcomes.iter().any(|o| o.status == SwapStatus::Failed) {
        bail!("some agents could not be swapped");
    }
    Ok(())
}

fn skipped_direct(direct: &[DirectTarget]) -> String {
    if direct.is_empty() {
        return String::new();
    }
    let panes: Vec<&str> = direct.iter().map(|target| target.pane.as_str()).collect();
    format!(
        "\nthe directly-run pane(s) {} were not swapped",
        panes.join(", ")
    )
}

/// The planned direct panes, swapped here: the request's own scope, bound
/// to exactly the agents the daemon planned.
async fn run_planned_direct(
    client: &Client,
    request: &SwapRequest,
    config: &SwapConfig,
    accounts: &muxa::accounts::AccountsConfig,
    direct: &[DirectTarget],
) -> Result<Vec<SwapOutcome>> {
    if direct.is_empty() {
        return Ok(Vec::new());
    }
    let agents = client.snapshot().await.context("listing agents")?;
    let backends = muxa::backend::active_backends();
    let (request, config, accounts, direct) = (
        request.clone(),
        config.clone(),
        accounts.clone(),
        direct.to_vec(),
    );
    tokio::task::spawn_blocking(move || {
        account_swap::run_bound(&request, &agents, &backends, &config, &accounts, &direct)
    })
    .await
    .context("swap worker")
}

/// The daemon's report with each direct pane's placeholder (excluded from
/// the job, or skipped in its plan) replaced by what happened to it here,
/// in place; a direct outcome with no placeholder goes last.
fn merge(
    daemon: Vec<SwapOutcome>,
    direct: &[DirectTarget],
    local: Vec<SwapOutcome>,
) -> Vec<SwapOutcome> {
    let mut local: Vec<Option<SwapOutcome>> = local.into_iter().map(Some).collect();
    let mut merged = Vec::with_capacity(daemon.len() + local.len());
    for outcome in daemon {
        // The job's exclusion of the pane, or the plan's refusal of it:
        // a genuine skip on that pane stays.
        let placeholder = outcome.status == SwapStatus::Skipped
            && (outcome.note.contains("excluded")
                || outcome.note.contains("runs its agent directly"))
            && direct
                .iter()
                .any(|target| outcome.pane.as_deref() == Some(target.pane.as_str()));
        if placeholder {
            let pane = outcome.pane.clone();
            if let Some(mine) = local
                .iter_mut()
                .find(|mine| mine.as_ref().is_some_and(|mine| mine.pane == pane))
                .and_then(Option::take)
            {
                merged.push(mine);
            }
            continue;
        }
        merged.push(outcome);
    }
    merged.extend(local.into_iter().flatten());
    merged
}

/// One report row per job entry, in request order. The journal is the
/// authority: an entry it does not record as swapped is never shown as
/// swapped, whatever the executor's own report said.
fn job_outcomes(view: &JobView) -> Vec<SwapOutcome> {
    let mut entries: Vec<&EntryView> = view.entries.iter().collect();
    entries.sort_by_key(|entry| entry.report_index);
    entries.into_iter().map(entry_outcome).collect()
}

fn entry_outcome(entry: &EntryView) -> SwapOutcome {
    let status = match entry.disposition.as_deref() {
        Some("swapped") => SwapStatus::Swapped,
        Some("skipped") => SwapStatus::Skipped,
        _ => SwapStatus::Failed,
    };
    let mut outcome = entry.outcome.clone().unwrap_or_else(|| SwapOutcome {
        pane: entry.pane.clone(),
        agent_session_id: String::new(),
        kind: AgentKind::Unknown,
        from: None,
        to: None,
        status,
        command: None,
        note: String::new(),
        continue_reason: None,
        session_id_source: None,
        conversation_verified: None,
        conversation_unverified_reason: None,
        registered: None,
        continue_withheld: None,
    });
    if outcome.status != status || entry.outcome.is_none() {
        let what = entry.disposition.as_deref().unwrap_or("not finished");
        let reason = entry
            .reason
            .as_deref()
            .map_or_else(String::new, |reason| format!(" ({reason})"));
        outcome.note = if outcome.note.is_empty() {
            format!("{what}{reason}")
        } else {
            format!("{what}{reason}: {}", outcome.note)
        };
        outcome.status = status;
    }
    if let Some(reason) = entry
        .continuation
        .as_deref()
        .filter(|decision| !matches!(*decision, "allowed" | "not_due"))
    {
        outcome.continue_withheld = Some(reason.to_owned());
    }
    if entry.uncertain_effects > 0 {
        let _ = write!(
            outcome.note,
            "; {} effect(s) may or may not have happened",
            entry.uncertain_effects
        );
    }
    outcome
}

/// An error unless the job ended cleanly.
fn job_verdict(view: &JobView, key: &str) -> Result<()> {
    if let Some(fault) = &view.fault {
        bail!(
            "swap job {} ({key}) stopped: the daemon's swap service faulted: {fault}",
            view.job_id
        );
    }
    if view.state != "done" {
        bail!(
            "swap job {} ({key}) ended {}{}",
            view.job_id,
            view.state,
            view.reason
                .as_deref()
                .map_or_else(String::new, |reason| format!(": {reason}"))
        );
    }
    if view.entries.iter().any(|entry| entry.uncertain_effects > 0) {
        bail!(
            "swap job {} ({key}) has effects whose result was never recorded; check those panes",
            view.job_id
        );
    }
    if job_outcomes(view)
        .iter()
        .any(|outcome| outcome.status == SwapStatus::Failed)
    {
        bail!(
            "swap job {} ({key}): some agents could not be swapped",
            view.job_id
        );
    }
    Ok(())
}

/// A job id as the store makes them: `swap-` and a UUID.
fn job_ref(text: &str) -> JobRef {
    let id = text
        .strip_prefix("swap-")
        .is_some_and(|uuid| uuid::Uuid::parse_str(uuid).is_ok());
    if id {
        JobRef::Id(text.to_owned())
    } else {
        JobRef::Key(text.to_owned())
    }
}

/// How watching a job by `--status` ended.
#[derive(Debug)]
enum Watch {
    Settled(JobView),
    /// Ctrl-C; the job, if found, by id.
    Interrupted(Option<String>),
    Failed(RuntimeError),
}

/// Read the job (an id that is not found is tried as a key) and follow it
/// to its end. Ctrl-C ends any wait, a slow read included.
async fn watch_job(
    daemon: &impl SwapDaemon,
    text: &str,
    cancel: impl Future<Output = ()>,
    poll: Duration,
) -> Watch {
    tokio::pin!(cancel);
    let first = job_ref(text);
    let read = async {
        match daemon.status(&first).await {
            Err(error) if matches!(first, JobRef::Id(_)) && not_found(&error) => {
                daemon.status(&JobRef::Key(text.to_owned())).await
            }
            read => read,
        }
    };
    let mut view = tokio::select! {
        biased;
        () = &mut cancel => return Watch::Interrupted(None),
        read = read => match read {
            Ok(view) => view,
            Err(error) => return Watch::Failed(error),
        },
    };
    let job = JobRef::Id(view.job_id.clone());
    while !settled(&view) {
        let read = tokio::select! {
            biased;
            () = &mut cancel => return Watch::Interrupted(Some(view.job_id)),
            read = async {
                tokio::time::sleep(poll).await;
                daemon.status(&job).await
            } => read,
        };
        match read {
            Ok(next) => view = next,
            Err(error) => return Watch::Failed(error),
        }
    }
    Watch::Settled(view)
}

/// `--status`: follow the job to its end and report it.
async fn show_status(client: &Client, text: &str, json: bool) -> Result<()> {
    let cancel = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    let view = match watch_job(client, text, cancel, POLL).await {
        Watch::Settled(view) => view,
        Watch::Interrupted(Some(job)) => bail!("stopped waiting; swap job {job} runs on"),
        Watch::Interrupted(None) => bail!("stopped waiting"),
        Watch::Failed(error) => bail!("{error}"),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&view)?);
    } else {
        println!("job {}  {}", view.job_id, view.state);
        print!("{}", render(&job_outcomes(&view)));
    }
    job_verdict(&view, text)
}

/// The same swap as a `muxa swap … --json` argv for a Fleet host, every
/// value as one `--flag=value` word as the relay's allowlist expects.
fn remote_argv(args: &Args) -> Vec<String> {
    let mut words = vec!["swap".to_owned()];
    words.extend(args.targets.iter().cloned());
    for (on, flag) in [
        (args.all, "--all"),
        (args.limited, "--limited"),
        (args.spread, "--spread"),
        (args.force, "--force"),
        (args.no_continue, "--no-continue"),
        (args.dry_run, "--dry-run"),
        (args.local, "--local"),
    ] {
        if on {
            words.push(flag.to_owned());
        }
    }
    if let Some(provider) = args.provider {
        words.push(format!(
            "--provider={}",
            match provider {
                Provider::Claude => "claude",
                Provider::Codex => "codex",
            }
        ));
    }
    if let Some(to) = &args.to {
        words.push(format!("--to={to}"));
    }
    if let Some(job) = &args.status {
        words.push(format!("--status={job}"));
    }
    if let Some(job) = &args.retry {
        words.push(format!("--retry={job}"));
    }
    if let Some(bare) = args.bare {
        words.push(format!(
            "--bare={}",
            match bare {
                Bare::Skip => "skip",
                Bare::Wrap => "wrap",
            }
        ));
    }
    // The new spellings only: a host too old to know them refuses the run
    // rather than reading `--continue` as "send to every agent".
    // A blank wording sends nothing here, so it is `--no-continue` there:
    // the allowlist takes no empty values, and the host's own default text
    // must not stand in for one the caller cleared.
    let blank = args
        .follow_up_text()
        .is_some_and(|text| text.trim().is_empty());
    if blank && !args.no_continue {
        words.push("--no-continue".to_owned());
    } else if let Some(text) = args.follow_up_text() {
        words.push(format!("--continue-text={text}"));
    }
    if let Some(when) = args.continue_when.filter(|_| !blank) {
        words.push(format!(
            "--continue-when={}",
            match when {
                When::Limited => "limited",
                When::Always => "always",
                When::Never => "never",
            }
        ));
    }
    if let Some(evidence) = args.continue_evidence {
        words.push(format!("--continue-evidence={}", evidence.as_str()));
    }
    words.extend(args.exclude.iter().map(|pane| format!("--exclude={pane}")));
    words.push("--json".to_owned());
    words
}

/// What Ctrl-C does to a swap on a Fleet host: the host's own `muxa swap`
/// executes there through Fleet's work-command transport, and this client
/// only stops waiting for it. Whether that run stops too depends on how the
/// wait ends (relay, SSH, timeout), so the notice claims neither.
fn remote_wait_notice(host: &str) -> String {
    format!(
        "swapping on {host}: Ctrl-C stops this client from waiting; it does not reliably \
         cancel the swap on {host}, which may still be stopping and relaunching agents. \
         Check {host} (`muxa fleet status`) before retrying."
    )
}

#[derive(clap::Parser)]
#[command(name = "swap")]
struct SwapCli {
    #[command(flatten)]
    args: Args,
}

/// The canonical `swap … --json` argv for `words` (a `muxa swap` command
/// line without the program name), checked by the same parser the CLI uses
/// so conflicting options fail here, before any process is spawned.
pub(crate) fn checked_argv(words: &[String]) -> std::result::Result<Vec<String>, String> {
    use clap::Parser as _;
    let cli =
        SwapCli::try_parse_from(std::iter::once("swap".to_owned()).chain(words.iter().cloned()))
            .map_err(|error| {
                error
                    .to_string()
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .to_owned()
            })?;
    Ok(remote_argv(&cli.args))
}

/// `muxa swap --host`: the host's own `muxa swap` does the work, through
/// the Fleet transport, and its report is printed here.
async fn run_remote(client: &Client, args: &Args, host: &str) -> Result<()> {
    if !args.dry_run && args.status.is_none() {
        eprintln!("{}", remote_wait_notice(host));
    }
    let output = client
        .work_command(Some(host), &remote_argv(args), None)
        .await
        .with_context(|| format!("running the swap on {host}"))?;
    // The host's own notices (its request key above all) are for the user.
    for line in output
        .stderr
        .lines()
        .filter(|line| line.starts_with("muxa swap:"))
    {
        eprintln!("{host}: {line}");
    }
    if args.status.is_some() {
        let Ok(view) = serde_json::from_str::<JobView>(&output.stdout) else {
            bail!(
                "{host}: swap status failed (exit {}): {}",
                output.exit_code,
                output.stderr.trim()
            );
        };
        if args.json {
            println!("{}", serde_json::to_string_pretty(&view)?);
        } else {
            println!("{host}: job {}  {}", view.job_id, view.state);
            print!("{}", render(&job_outcomes(&view)));
        }
        job_verdict(&view, &format!("on {host}"))?;
        if output.exit_code != 0 {
            bail!("{host}: {}", output.stderr.trim());
        }
        return Ok(());
    }
    let Ok(outcomes) = serde_json::from_str::<Vec<SwapOutcome>>(&output.stdout) else {
        bail!(
            "{host}: swap failed (exit {}): {}",
            output.exit_code,
            output.stderr.trim()
        );
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&outcomes)?);
    } else {
        print!("{}", render(&outcomes));
    }
    if outcomes.iter().any(|o| o.status == SwapStatus::Failed) {
        bail!("some agents on {host} could not be swapped");
    }
    // A host's daemon job can end unclean with no failed row (faulted,
    // uncertain effects): its exit status says so.
    if output.exit_code != 0 {
        bail!(
            "the swap on {host} did not end cleanly (exit {}): {}\n\
             check it with `muxa swap --host {host} --status <key>`",
            output.exit_code,
            output.stderr.trim()
        );
    }
    Ok(())
}

fn render(outcomes: &[SwapOutcome]) -> String {
    if outcomes.is_empty() {
        return "nothing to swap\n".to_owned();
    }
    let mut out = String::new();
    for outcome in outcomes {
        let pane = outcome.pane.as_deref().unwrap_or("-");
        let moved = match (&outcome.from, &outcome.to) {
            (from, Some(to)) => format!("{} -> {to}", from.as_deref().unwrap_or("(bare)")),
            (Some(from), None) => from.clone(),
            (None, None) => String::new(),
        };
        let _ = writeln!(
            out,
            "{:<8} {pane:<10} {:<11} {moved}",
            outcome.status.to_string(),
            outcome.kind.to_string(),
        );
        if let Some(command) = &outcome.command {
            let _ = writeln!(out, "         $ {command}");
        }
        if !outcome.note.is_empty() {
            let _ = writeln!(out, "         {}", outcome.note);
        }
        // Read back from a journal, the note may not say it.
        if let Some(reason) = outcome
            .continue_withheld
            .as_deref()
            .filter(|reason| !outcome.note.contains(reason))
        {
            let _ = writeln!(out, "         follow-up withheld ({reason})");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }

    fn parse(argv: &[&str]) -> SwapRequest {
        let cli = Cli::parse_from(std::iter::once("swap").chain(argv.iter().copied()));
        request(&cli.args, &SwapConfig::default())
    }

    fn request_from(args: &Args, config: &SwapConfig) -> SwapRequest {
        request(args, config)
    }

    #[test]
    fn host_runs_the_same_swap_there_as_one_word_per_value() {
        let cli = Cli::parse_from([
            "swap",
            "%5",
            "--force",
            "--provider",
            "claude",
            "--to",
            "work@claude",
            "--continue-text",
            "keep going",
            "--continue-when",
            "always",
            "--host",
            "rtzr",
        ]);
        let argv = remote_argv(&cli.args);
        assert_eq!(
            argv,
            [
                "swap",
                "%5",
                "--force",
                "--provider=claude",
                "--to=work@claude",
                "--continue-text=keep going",
                "--continue-when=always",
                "--json"
            ]
        );
        muxa::work_control::validate_swap_argv(&argv).unwrap();
    }

    /// The notice names the host and promises neither outcome.
    #[test]
    fn a_remote_swap_says_ctrl_c_only_stops_the_wait() {
        let notice = remote_wait_notice("rtzr");
        assert!(notice.contains("rtzr"), "{notice}");
        assert!(notice.contains("does not reliably cancel"), "{notice}");
        assert!(!notice.contains("cancelled"), "{notice}");
    }

    #[test]
    fn conflicting_options_fail_before_anything_is_sent() {
        let words = |w: &[&str]| w.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        assert!(checked_argv(&words(&["%5", "--all"])).is_err());
        assert!(checked_argv(&words(&["--to", "x", "--spread"])).is_err());
        assert_eq!(
            checked_argv(&words(&["--all", "--exclude", "%9"])).unwrap(),
            ["swap", "--all", "--exclude=%9", "--json"]
        );
    }

    #[test]
    fn no_target_means_the_rate_limited_agents() {
        let request = parse(&[]);
        assert_eq!(request.scope, SwapScope::Limited);
        assert_eq!(request.to, AccountChoice::Auto);
        assert_eq!(request.continue_when, ContinueWhen::Limited);
    }

    #[test]
    fn targets_all_and_account_flags_map_onto_the_request() {
        let request = parse(&["%5", "--to", "jelly@claude", "--bare", "wrap"]);
        assert_eq!(request.scope, SwapScope::Targets(vec!["%5".into()]));
        assert_eq!(request.to, AccountChoice::Named("jelly@claude".into()));
        assert_eq!(request.bare, BareMode::Wrap);

        let request = parse(&["--all", "--spread", "--force", "--provider", "codex"]);
        assert_eq!(request.scope, SwapScope::All);
        assert_eq!(request.to, AccountChoice::Spread);
        assert!(request.force);
        assert_eq!(request.provider, Some(AgentKind::Codex));
    }

    /// Naming the wording keeps the configured policy: it used to switch it
    /// to `always`, which woke every idle agent a swap touched.
    #[test]
    fn the_wording_and_who_gets_it_are_set_separately() {
        let request = parse(&["--continue-text", "keep going"]);
        assert_eq!(request.continue_text.as_deref(), Some("keep going"));
        assert_eq!(request.continue_when, ContinueWhen::Limited);

        let request = parse(&["--continue-when", "always"]);
        assert_eq!(
            request.continue_text,
            SwapRequest::new(SwapScope::Limited, &SwapConfig::default()).continue_text
        );
        assert_eq!(request.continue_when, ContinueWhen::Always);

        let request = parse(&["--continue-text", "go", "--continue-when", "never"]);
        assert_eq!(request.continue_when, ContinueWhen::Never);
        assert_eq!(parse(&["--no-continue"]).continue_when, ContinueWhen::Never);

        // A configured policy survives a wording-only override.
        let config = SwapConfig {
            continue_when: ContinueWhen::Never,
            ..SwapConfig::default()
        };
        let cli = Cli::parse_from(["swap", "--continue-text", "go"]);
        assert_eq!(
            request_from(&cli.args, &config).continue_when,
            ContinueWhen::Never
        );
        let config = SwapConfig {
            continue_when: ContinueWhen::Always,
            ..SwapConfig::default()
        };
        assert_eq!(
            request_from(&cli.args, &config).continue_when,
            ContinueWhen::Always
        );
    }

    /// The old spelling is the wording only now, and a remote run carries
    /// the new spellings a host too old to know them refuses.
    #[test]
    fn the_deprecated_continue_no_longer_means_every_agent() {
        let request = parse(&["--continue", "keep going"]);
        assert_eq!(request.continue_text.as_deref(), Some("keep going"));
        assert_eq!(request.continue_when, ContinueWhen::Limited);

        let words = |w: &[&str]| w.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            checked_argv(&words(&["--continue", "go"])).unwrap(),
            ["swap", "--continue-text=go", "--json"]
        );
        assert!(checked_argv(&words(&["--continue", "a", "--continue-text", "b"])).is_err());
        assert!(checked_argv(&words(&["--no-continue", "--continue-when", "always"])).is_err());
        assert!(checked_argv(&words(&["--continue-when", "sometimes"])).is_err());
        let argv = checked_argv(&words(&[
            "--continue-text",
            "go",
            "--continue-when",
            "limited",
        ]))
        .unwrap();
        muxa::work_control::validate_swap_argv(&argv).unwrap();
        // No policy named: none is sent, so the host keeps its own default.
        assert!(!checked_argv(&words(&["--continue-text", "go"]))
            .unwrap()
            .iter()
            .any(|word| word.starts_with("--continue-when")));
    }

    /// A blank wording sends nothing, here and on a Fleet host alike.
    #[test]
    fn a_blank_wording_sends_nothing_here_or_on_a_host() {
        assert_eq!(parse(&["--continue-text", " "]).continue_text, None);
        let words = |w: &[&str]| w.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        for blank in [
            words(&["--continue-text", " "]),
            words(&["--continue", ""]),
            words(&["--continue-text", "", "--continue-when", "always"]),
        ] {
            let argv = checked_argv(&blank).unwrap();
            assert!(argv.contains(&"--no-continue".to_owned()), "{argv:?}");
            assert!(
                !argv.iter().any(|w| w.starts_with("--continue")),
                "{argv:?}"
            );
            muxa::work_control::validate_swap_argv(&argv).unwrap();
        }
    }

    // --- daemon routing -------------------------------------------------

    fn caps(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// No swap tags at all is a daemon from before swap jobs; the service
    /// marker without them is a service that cannot take one now, which
    /// must never be run around.
    #[test]
    fn an_unavailable_service_is_not_mistaken_for_an_old_daemon() {
        assert_eq!(route(&caps(&["collaboration"])), Route::Legacy);
        assert_eq!(route(&caps(&["swap_service_v1"])), Route::Unavailable);
        assert_eq!(
            route(&caps(&["swap_service_v1", "swap_jobs_v1"])),
            Route::Unavailable
        );
        assert_eq!(
            route(&caps(&["swap_service_v1", "swap_jobs_v1", "swap_plan_v1"])),
            Route::Daemon
        );
    }

    /// The request as typed: no `auto` the user never wrote, no defaults,
    /// and the config file only when one was named.
    #[test]
    fn overrides_carry_only_what_was_typed() {
        let cli = Cli::parse_from(["swap", "%5", "--force"]);
        let typed = overrides(&cli.args, None);
        assert_eq!(typed.targets, ["%5"]);
        assert!(typed.force);
        assert_eq!(typed.to, None);
        assert_eq!(typed.continue_when, None);
        assert_eq!(typed.caller_config, None);
        let named = overrides(&cli.args, Some(Path::new("/etc/muxa.toml")));
        assert_eq!(
            named.caller_config.as_deref(),
            Some(Path::new("/etc/muxa.toml"))
        );
        let cli = Cli::parse_from(["swap", "--to", "a@claude", "--continue-when", "always"]);
        let typed = overrides(&cli.args, None);
        assert_eq!(typed.to.as_deref(), Some("a@claude"));
        assert_eq!(typed.continue_when.as_deref(), Some("always"));
    }

    /// The evidence policy is typed, carried to the daemon and to a host,
    /// and a withheld follow-up recorded in the journal reaches the report.
    #[test]
    fn continue_evidence_travels_and_a_withheld_follow_up_is_reported() {
        let cli = Cli::parse_from(["swap", "%5", "--continue-evidence", "verified"]);
        assert_eq!(
            request(&cli.args, &SwapConfig::default()).continue_evidence,
            account_swap::ContinueEvidence::Verified
        );
        assert_eq!(
            overrides(&cli.args, None).continue_evidence.as_deref(),
            Some("verified")
        );
        assert_eq!(
            overrides(&Cli::parse_from(["swap"]).args, None).continue_evidence,
            None
        );
        let words = |w: &[&str]| w.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        let argv = checked_argv(&words(&["--continue-evidence", "binding"])).unwrap();
        assert!(
            argv.contains(&"--continue-evidence=binding".to_owned()),
            "{argv:?}"
        );
        muxa::work_control::validate_swap_argv(&argv).unwrap();
        assert!(checked_argv(&words(&["--continue-evidence", "maybe"])).is_err());

        let mut entry = EntryView {
            report_index: 0,
            pane: Some("%1".into()),
            disposition: Some("swapped".into()),
            reason: None,
            uncertain_effects: 0,
            outcome: None,
            continuation: Some("binding_missing".into()),
        };
        assert_eq!(
            entry_outcome(&entry).continue_withheld.as_deref(),
            Some("binding_missing")
        );
        // Read back after a restart (no executor report), it is shown.
        let shown = render(&[entry_outcome(&entry)]);
        assert!(
            shown.contains("follow-up withheld (binding_missing)"),
            "{shown}"
        );
        entry.continuation = Some("allowed".into());
        assert_eq!(entry_outcome(&entry).continue_withheld, None);
        assert!(!render(&[entry_outcome(&entry)]).contains("withheld"));
    }

    #[test]
    fn a_retry_takes_only_force_evidence_and_json() {
        for argv in [
            &["swap", "--retry", "k", "%5"][..],
            &["swap", "--retry", "k", "--all"],
            &["swap", "--retry", "k", "--to", "a"],
            &["swap", "--retry", "k", "--local"],
            &["swap", "--retry", "k", "--dry-run"],
            &["swap", "--retry", "k", "--status", "k"],
        ] {
            assert!(Cli::try_parse_from(argv).is_err(), "{argv:?}");
        }
        assert!(Cli::try_parse_from([
            "swap",
            "--retry",
            "k",
            "--force",
            "--continue-evidence",
            "verified",
            "--json"
        ])
        .is_ok());
        let words = |w: &[&str]| w.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        let argv = checked_argv(&words(&["--retry", "cli-1", "--force"])).unwrap();
        assert_eq!(argv, ["swap", "--force", "--retry=cli-1", "--json"]);
        muxa::work_control::validate_swap_argv(&argv).unwrap();
    }

    #[test]
    fn status_takes_no_action_flags() {
        for argv in [
            &["swap", "--status", "k", "%5"][..],
            &["swap", "--status", "k", "--all"],
            &["swap", "--status", "k", "--local"],
            &["swap", "--status", "k", "--dry-run"],
        ] {
            assert!(Cli::try_parse_from(argv).is_err(), "{argv:?}");
        }
        assert!(Cli::try_parse_from(["swap", "--status", "k", "--json"]).is_ok());
    }

    #[test]
    fn a_job_id_is_read_as_one_and_anything_else_as_a_key() {
        let id = format!("swap-{}", uuid::Uuid::new_v4());
        assert_eq!(job_ref(&id), JobRef::Id(id.clone()));
        assert_eq!(job_ref("cli-123"), JobRef::Key("cli-123".into()));
        assert_eq!(job_ref("swap-nope"), JobRef::Key("swap-nope".into()));
    }

    #[test]
    fn local_and_status_reach_a_host() {
        let words = |w: &[&str]| w.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        let argv = checked_argv(&words(&["--local", "%5"])).unwrap();
        assert_eq!(argv, ["swap", "%5", "--local", "--json"]);
        muxa::work_control::validate_swap_argv(&argv).unwrap();
        let argv = checked_argv(&words(&["--status", "cli-1"])).unwrap();
        assert_eq!(argv, ["swap", "--status=cli-1", "--json"]);
        muxa::work_control::validate_swap_argv(&argv).unwrap();
    }

    fn view(state: &str, fault: Option<&str>) -> JobView {
        JobView {
            job_id: "swap-job".into(),
            node_id: "node".into(),
            state: state.into(),
            revision: 1,
            reason: None,
            entries: Vec::new(),
            fault: fault.map(str::to_owned),
        }
    }

    /// A scripted daemon: each start answer, and status answers in order.
    struct Scripted {
        start: Mutex<Option<Result<Accepted, RuntimeError>>>,
        statuses: Mutex<std::collections::VecDeque<Result<JobView, RuntimeError>>>,
        starts: std::sync::atomic::AtomicUsize,
        hold_start: bool,
    }

    use std::sync::Mutex;

    impl Scripted {
        fn new(
            start: Result<Accepted, RuntimeError>,
            statuses: Vec<Result<JobView, RuntimeError>>,
        ) -> Self {
            Self {
                start: Mutex::new(Some(start)),
                statuses: Mutex::new(statuses.into()),
                starts: std::sync::atomic::AtomicUsize::new(0),
                hold_start: false,
            }
        }
    }

    impl Scripted {
        /// The one start this daemon answers.
        fn start(&self) -> impl Future<Output = Result<Accepted, RuntimeError>> + Send {
            self.starts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let answer = self.start.lock().unwrap().take().expect("one start only");
            let hold = self.hold_start;
            async move {
                if hold {
                    std::future::pending::<()>().await;
                }
                answer
            }
        }
    }

    impl SwapDaemon for Scripted {
        fn status(&self, _: &JobRef) -> impl Future<Output = Result<JobView, RuntimeError>> + Send {
            let answer = self
                .statuses
                .lock()
                .unwrap()
                .pop_front()
                .expect("no more status answers");
            async move { answer }
        }
    }

    fn accepted(state: &str) -> Accepted {
        Accepted {
            job_id: "swap-job".into(),
            existing: false,
            view: view(state, None),
        }
    }

    async fn follow(daemon: &Scripted) -> JobEnd {
        follow_job(
            daemon,
            "cli-k",
            daemon.start(),
            std::future::pending(),
            Duration::from_millis(1),
        )
        .await
    }

    #[test]
    fn only_a_daemon_that_does_not_know_swap_jobs_is_bypassed() {
        let daemon = |error: &str| Err(RuntimeError::Daemon(error.into()));
        assert_eq!(
            probe(&daemon(
                "bad request: unknown variant `swap_job_status`, expected one of `hello`"
            )),
            Route::Legacy
        );
        // A daemon serving swap jobs from before the marker, not taking one now.
        assert_eq!(
            probe(&daemon("swap job cli-probe not found")),
            Route::Unavailable
        );
        assert_eq!(
            probe(&daemon("swap jobs are unavailable on this daemon")),
            Route::Unavailable
        );
        assert_eq!(probe(&Err(RuntimeError::Disconnected)), Route::Unavailable);
        assert_eq!(
            probe(&Err(RuntimeError::Timeout(Duration::from_secs(1)))),
            Route::Unavailable
        );
        assert_eq!(probe(&Ok(view("done", None))), Route::Unavailable);
    }

    /// A job that ended with a failed row fails, however it is read.
    #[test]
    fn a_done_job_with_a_failed_row_is_a_failure() {
        let mut job = view("done", None);
        job.entries.push(EntryView {
            report_index: 0,
            pane: Some("%1".into()),
            disposition: Some("failed".into()),
            reason: Some("candidates_unavailable".into()),
            uncertain_effects: 0,
            outcome: None,
            continuation: None,
        });
        assert!(job_verdict(&job, "k").is_err());
        job.entries[0].disposition = Some("kept".into());
        assert!(job_verdict(&job, "k").is_err());
        job.entries[0].disposition = Some("swapped".into());
        assert!(job_verdict(&job, "k").is_ok());
    }

    /// A status read that never answers still ends on Ctrl-C.
    #[tokio::test]
    async fn ctrl_c_ends_a_held_status_read() {
        struct Held;
        impl SwapDaemon for Held {
            fn status(
                &self,
                _: &JobRef,
            ) -> impl Future<Output = Result<JobView, RuntimeError>> + Send {
                std::future::pending()
            }
        }
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let watch = watch_job(
            &Held,
            "cli-k",
            async move {
                let _ = rx.await;
            },
            Duration::from_millis(1),
        );
        tokio::pin!(watch);
        tokio::select! {
            biased;
            _ = &mut watch => panic!("ended while the read was held"),
            () = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        tx.send(()).unwrap();
        let ended = tokio::time::timeout(Duration::from_secs(2), watch)
            .await
            .expect("Ctrl-C must end a held read");
        assert!(matches!(ended, Watch::Interrupted(None)));
    }

    /// An id that is not found is read as a key; any other error is final.
    #[tokio::test]
    async fn status_tries_an_unknown_id_as_a_key_only_when_not_found() {
        let id = format!("swap-{}", uuid::Uuid::new_v4());
        let missing = || Err(RuntimeError::Daemon(format!("swap job {id} not found")));
        let daemon = Scripted::new(
            Ok(accepted("done")),
            vec![missing(), Ok(view("done", None))],
        );
        assert!(matches!(
            watch_job(&daemon, &id, std::future::pending(), Duration::from_millis(1)).await,
            Watch::Settled(v) if v.state == "done"
        ));
        let daemon = Scripted::new(
            Ok(accepted("done")),
            vec![Err(RuntimeError::Disconnected), Ok(view("done", None))],
        );
        assert!(matches!(
            watch_job(
                &daemon,
                &id,
                std::future::pending(),
                Duration::from_millis(1)
            )
            .await,
            Watch::Failed(RuntimeError::Disconnected)
        ));
    }

    #[tokio::test]
    async fn a_job_is_followed_to_its_end() {
        let daemon = Scripted::new(
            Ok(accepted("running")),
            vec![Ok(view("running", None)), Ok(view("done", None))],
        );
        assert!(matches!(follow(&daemon).await, JobEnd::Ended(v) if v.state == "done"));
    }

    /// A fault ends the wait even with the job still marked running.
    #[tokio::test]
    async fn a_faulted_job_ends_the_wait() {
        let daemon = Scripted::new(
            Ok(accepted("running")),
            vec![Ok(view("running", Some("journal")))],
        );
        let JobEnd::Ended(view) = follow(&daemon).await else {
            panic!("a fault must end the wait");
        };
        assert!(job_verdict(&view, "cli-k").is_err());
    }

    /// A lost answer to an accepted start is recovered by the key, never by
    /// sending the request again.
    #[tokio::test]
    async fn a_lost_acknowledgement_is_recovered_by_key() {
        let daemon = Scripted::new(
            Err(RuntimeError::Disconnected),
            vec![Ok(view("running", None)), Ok(view("done", None))],
        );
        assert!(matches!(follow(&daemon).await, JobEnd::Ended(v) if v.state == "done"));
        assert_eq!(daemon.starts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Not found after a lost answer is not a refusal: the start may still
    /// be planning.
    #[tokio::test]
    async fn an_unanswered_start_with_no_job_yet_is_unknown_not_refused() {
        let missing = || Err(RuntimeError::Daemon("swap job cli-k not found".into()));
        let daemon = Scripted::new(Err(RuntimeError::Disconnected), vec![missing()]);
        assert!(matches!(follow(&daemon).await, JobEnd::Unknown(_)));
        let daemon = Scripted::new(Err(RuntimeError::Daemon("busy".into())), vec![missing()]);
        assert!(matches!(follow(&daemon).await, JobEnd::Refused(e) if e == "busy"));
    }

    /// Ctrl-C during the start stops the wait at once.
    #[tokio::test]
    async fn ctrl_c_during_the_start_only_stops_waiting() {
        let mut daemon = Scripted::new(Ok(accepted("running")), Vec::new());
        daemon.hold_start = true;
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let cancel = async move {
            let _ = rx.await;
        };
        let follow = follow_job(
            &daemon,
            "cli-k",
            daemon.start(),
            cancel,
            Duration::from_millis(1),
        );
        tokio::pin!(follow);
        tokio::select! {
            biased;
            _ = &mut follow => panic!("ended while the start was held"),
            () = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
        tx.send(()).unwrap();
        assert!(matches!(follow.await, JobEnd::Interrupted));
        assert_eq!(daemon.starts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    fn outcome(pane: &str, status: SwapStatus, note: &str) -> SwapOutcome {
        SwapOutcome {
            pane: Some(pane.into()),
            agent_session_id: format!("s{pane}"),
            kind: AgentKind::ClaudeCode,
            from: None,
            to: None,
            status,
            command: None,
            note: note.into(),
            continue_reason: None,
            session_id_source: None,
            conversation_verified: None,
            conversation_unverified_reason: None,
            registered: None,
            continue_withheld: None,
        }
    }

    fn direct(pane: &str) -> DirectTarget {
        DirectTarget {
            pane: pane.into(),
            socket: Some("default".into()),
            endpoint: format!("{pane}@default"),
            conversation_id: format!("s{pane}"),
        }
    }

    /// One row per agent, in order: each direct pane's placeholder becomes
    /// what happened to it here, and a genuine skip on that pane stays.
    #[test]
    fn a_split_swap_reports_each_agent_once_in_order() {
        let daemon = vec![
            outcome("%1", SwapStatus::Swapped, "resumed"),
            outcome("%2", SwapStatus::Skipped, "skipped: excluded"),
            outcome("%2", SwapStatus::Skipped, "skipped: mid-turn"),
            outcome(
                "%3",
                SwapStatus::Skipped,
                "skipped: this pane runs its agent directly",
            ),
        ];
        let local = vec![
            outcome("%2", SwapStatus::Swapped, "here"),
            outcome("%3", SwapStatus::Failed, "here"),
        ];
        let merged = merge(daemon, &[direct("%2"), direct("%3")], local);
        let shape: Vec<_> = merged
            .iter()
            .map(|o| (o.pane.clone().unwrap(), o.status, o.note.clone()))
            .collect();
        assert_eq!(
            shape,
            [
                ("%1".into(), SwapStatus::Swapped, "resumed".into()),
                ("%2".into(), SwapStatus::Swapped, "here".into()),
                ("%2".into(), SwapStatus::Skipped, "skipped: mid-turn".into()),
                ("%3".into(), SwapStatus::Failed, "here".into()),
            ]
        );
    }

    /// The journal decides: an executor report of "swapped" on an entry the
    /// journal left uncertain is not shown as swapped.
    #[test]
    fn the_journal_outranks_the_executors_report() {
        let entry = EntryView {
            report_index: 0,
            pane: Some("%1".into()),
            disposition: Some("needs_reconciliation".into()),
            reason: Some("interrupted".into()),
            uncertain_effects: 2,
            outcome: Some(outcome("%1", SwapStatus::Swapped, "resumed")),
            continuation: None,
        };
        let shown = entry_outcome(&entry);
        assert_eq!(shown.status, SwapStatus::Failed);
        assert!(
            shown.note.contains("needs_reconciliation"),
            "{}",
            shown.note
        );
        assert!(shown.note.contains("2 effect(s)"), "{}", shown.note);
        let mut job = view("done", None);
        job.entries.push(entry);
        assert!(job_verdict(&job, "k").is_err());
        assert!(job_verdict(&view("done", None), "k").is_ok());
        assert!(job_verdict(&view("needs_reconciliation", None), "k").is_err());
    }

    #[test]
    fn report_shows_the_move_and_the_typed_line() {
        let rendered = render(&[SwapOutcome {
            pane: Some("%5".into()),
            agent_session_id: "sid".into(),
            kind: AgentKind::ClaudeCode,
            from: Some("e-ed@claude".into()),
            to: Some("jelly@claude".into()),
            status: SwapStatus::Planned,
            command: Some("aas e jelly@claude --resume sid".into()),
            note: String::new(),
            continue_reason: None,
            session_id_source: None,
            conversation_verified: None,
            conversation_unverified_reason: None,
            registered: None,
            continue_withheld: None,
        }]);
        assert!(
            rendered.contains("e-ed@claude -> jelly@claude"),
            "{rendered}"
        );
        assert!(
            rendered.contains("$ aas e jelly@claude --resume sid"),
            "{rendered}"
        );
        assert_eq!(render(&[]), "nothing to swap\n");
    }
}
