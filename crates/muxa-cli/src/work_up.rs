//! `muxa work up` — bring a work item's tmux window to the state its
//! pipeline declares.
//!
//! `muxa work start` is the imperative primitive: one invocation, one
//! agent pane. This is the declarative one. A Work ID may resolve an external
//! issue reference; that context routes the Work to a workspace and pipeline,
//! and the pipeline says which agents the Work should have. Running it compares that against the panes
//! the window already has and creates the difference — so the first call
//! stands a team up and the second call is a no-op, not a duplicate team.
//!
//! The interesting seam is external issue lookup. Muxa does not talk to Linear,
//! Jira, or GitHub; it spends one headless agent turn asking an agent to do
//! it, because a user who already has a ticket-fetching skill has already
//! solved this problem once and should not solve it again inside muxa. See
//! [`muxa::pipeline`] for the contract and [`muxa::ask::one_shot`] for the
//! bridge.

use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use muxa::config::{Config, TicketConfig};
use muxa::event::AgentState;
use muxa::pipeline::{
    self, DesiredAgent, ExistingAgent, Plan, PlanStep, Ticket, Vars, WorktreePlan, REQUEST_KEY,
};
use muxa::pipeline_run::{PipelineAliasObservation, PipelineAliasStatus, PipelineRunRegistration};
use muxa::request::{ComposedRequest, RequestParts};
use muxa::work::WorkIdentity;
use std::collections::HashMap;

use crate::agent_launch::{AgentProgram, Placement, SplitDirection, StartRequest};

#[derive(Debug, clap::Args)]
#[allow(clippy::struct_excessive_bools)] // independent CLI flags, not a state machine
pub struct UpArgs {
    /// Stable Muxa Work id, for example auth-cleanup.
    pub work: String,
    /// Optional external issue key to resolve and link, for example CAL-1234.
    /// When omitted, the Work id is still looked up for compatibility; use
    /// `--no-ticket` for a strictly local Work.
    #[arg(long, value_name = "ISSUE")]
    pub external: Option<String>,
    /// Pipeline to staff the window with. Defaults to the matching route's.
    #[arg(long)]
    pub pipeline: Option<String>,
    /// Workspace/project session. Defaults to the route's, then the cwd name.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Work directory. Overrides the route; refused when the route creates
    /// a worktree, which computes its own.
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// What the work is. One input, two deliveries: an agent that has no
    /// pane yet gets it in its launch prompt, and one already running gets
    /// it typed into its pane. Starting work and steering work in flight
    /// are the same request against different state.
    #[arg(long, visible_alias = "prompt")]
    pub body: Option<String>,
    /// Registered `[message.skills]` template, with or without a leading
    /// `/`. Expanded ahead of `--body`, the same as `muxa_call_peer`.
    #[arg(long)]
    pub skill: Option<String>,
    /// Bounded extra context, labelled in the prompt so an agent can tell
    /// it from the instruction.
    #[arg(long)]
    pub context: Option<String>,
    /// Resolve and diff, then print what would happen and create nothing.
    /// Ticket lookup still runs on a cache miss, and that is a billed agent
    /// turn; `--no-ticket` skips it entirely.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip external issue lookup and launch on the Work id alone.
    #[arg(long)]
    pub no_ticket: bool,
    /// Ignore a cached external issue and ask the resolver again.
    #[arg(long)]
    pub refresh: bool,
    /// Print the exact prompt each agent would receive, before spending
    /// anything on it.
    #[arg(long)]
    pub show_prompts: bool,
    /// Skip the confirmation before a billed ticket lookup.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Emit the structured result as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct ReconcileArgs {
    /// Reconcile every durable Run with a dependency-ready alias.
    #[arg(long)]
    pub all: bool,
    #[arg(long, requires = "work")]
    pub workspace: Option<String>,
    #[arg(long, requires = "workspace")]
    pub work: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LaunchedAgent {
    pub alias: String,
    pub pane: String,
    pub program: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UpResult {
    pub work: String,
    pub workspace: String,
    pub pipeline: String,
    pub cwd: PathBuf,
    pub dry_run: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket: Option<Ticket>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreePlan>,
    /// True when this run created the worktree rather than reusing it.
    pub created_worktree: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layout: Option<String>,
    /// The composed request, when the caller supplied one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request: Option<ComposedRequest>,
    /// Dependency layout of the pipeline that produced this plan, so a
    /// reader (or the dashboard) sees what runs together without
    /// re-deriving it from the config.
    pub graph: Vec<muxa::pipeline::GraphNode>,
    pub plan: Plan,
    pub launched: Vec<LaunchedAgent>,
    /// Aliases that were sent `--prompt`.
    pub reprompted: Vec<String>,
    /// Aliases that have reported `muxa work done` on this work, newest run
    /// included. Carried so a reader can tell a converged pipeline from a
    /// stalled one: both leave every pane `idle`, and without this the two
    /// render identically.
    pub done: Vec<String>,
}

/// Everything decided before a single tmux command runs.
///
/// Split out so the async half (ticket lookup) and the blocking half
/// (every tmux call) stay separable: the CLI runs them back to back,
/// while the MCP server hands the blocking half to `spawn_blocking`
/// rather than stalling its reactor on a subprocess.
pub(crate) struct Resolved {
    work: String,
    workspace: String,
    pipeline: String,
    cwd: PathBuf,
    ticket: Option<Ticket>,
    worktree: Option<WorktreePlan>,
    created_worktree: bool,
    /// Whether the caller actually asked for this directory, as opposed to
    /// happening to stand in it. Only a pinned cwd is asserted against the
    /// work window's recorded one.
    cwd_pinned: bool,
    layout: Option<String>,
    request: Option<ComposedRequest>,
    desired: Vec<DesiredAgent>,
    /// Existing daemon-owned state, used by dry-run rendering. A real apply
    /// performs an atomic register and reads the returned Run instead.
    durable_run: Option<muxa::pipeline_run::PipelineRun>,
    /// pane → what the daemon says that agent is doing. Empty when the
    /// daemon is unreachable, which degrades to the old pane-only view
    /// rather than failing the launch.
    states: HashMap<String, AgentState>,
}

pub async fn run(
    args: UpArgs,
    config: &Config,
    config_path: Option<PathBuf>,
    client: Option<&muxa::ipc::Client>,
) -> Result<()> {
    let json = args.json;
    let dry_run = args.dry_run;
    let show_prompts = args.show_prompts;
    let resolved = resolve_or_onboard(&args, config, config_path, client).await?;
    if show_prompts {
        print_prompts(&resolved.desired);
    }
    let result = if dry_run {
        apply(resolved, true)?
    } else {
        let client = client.ok_or_else(|| {
            anyhow::anyhow!("muxa work up requires muxad for durable pipeline state")
        })?;
        apply_durable(resolved, client).await?
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        print_result(&result);
    }
    Ok(())
}

/// The blocking half: read the window, diff it against the pipeline, and
/// act on the difference.
pub(crate) fn apply(resolved: Resolved, dry_run: bool) -> Result<UpResult> {
    if !dry_run {
        bail!("non-dry pipeline reconciliation must use muxad's durable Run state");
    }
    let existing = existing_agents(&resolved.work, &resolved.workspace, &resolved.states)?;
    // The durable Run is the completion record: it outlives the agents that
    // reported, so a re-run reads what previous ones said. Before a Run is
    // registered nothing has reported yet, which is what an empty set means.
    let done: Vec<String> = resolved.durable_run.as_ref().map_or_else(Vec::new, |run| {
        run.aliases
            .values()
            .filter(|state| state.status == PipelineAliasStatus::Done)
            .map(|state| state.alias.clone())
            .collect()
    });
    let broadcast = resolved
        .request
        .as_ref()
        .map(|request| request.text.as_str());
    let plan = pipeline::plan(&resolved.desired, &existing, broadcast, &done);

    Ok(finish(
        resolved,
        plan,
        Vec::new(),
        Vec::new(),
        None,
        None,
        true,
        done,
    ))
}

/// Register desired state with muxad, atomically claim dependency-ready
/// aliases, then report the physical launch/re-prompt result. Completion and
/// dependency gating are never inferred from the live pane set here.
#[allow(clippy::too_many_lines)] // register, claim, physical apply, and report are one reconciliation flow
pub(crate) async fn apply_durable(
    resolved: Resolved,
    client: &muxa::ipc::Client,
) -> Result<UpResult> {
    let existing = existing_agents(&resolved.work, &resolved.workspace, &resolved.states)?;
    let work_info = crate::tmux_work::find_work_in(&resolved.work, Some(&resolved.workspace))?;
    let observed = existing
        .iter()
        .filter_map(|agent| {
            let alias = agent.alias.clone()?;
            Some(PipelineAliasObservation {
                alias,
                pane: agent.pane.clone(),
                status: match agent.state {
                    Some(AgentState::WaitingInput | AgentState::WaitingChoice) => {
                        PipelineAliasStatus::Blocked
                    }
                    Some(AgentState::Error | AgentState::Stopped) => PipelineAliasStatus::Failed,
                    Some(AgentState::Starting | AgentState::Working | AgentState::Idle) | None => {
                        PipelineAliasStatus::Running
                    }
                },
            })
        })
        .collect::<Vec<_>>();
    // `--body`/`--prompt` is a real restart of every existing pipeline
    // participant. The store expands these roots transitively and advances
    // the Run generation before any prompt can be delivered.
    let mut invalidate = resolved.request.as_ref().map_or_else(Vec::new, |_| {
        existing
            .iter()
            .filter_map(|agent| agent.alias.clone())
            .collect()
    });
    // An explicit `work up` is also the retry control for a launch that
    // failed before producing a pane. Pane-backed failures remain visible
    // for the operator to inspect/close; blindly typing a prompt into a
    // stopped shell could execute it as a command.
    if let Some(previous) = resolved.durable_run.as_ref() {
        invalidate.extend(
            previous
                .aliases
                .values()
                .filter(|state| state.status == PipelineAliasStatus::Failed && state.pane.is_none())
                .map(|state| state.alias.clone()),
        );
    }
    invalidate.sort();
    invalidate.dedup();
    let identity = WorkIdentity::new(resolved.workspace.clone(), resolved.work.clone());
    let registration = PipelineRunRegistration {
        identity: identity.clone(),
        pipeline: resolved.pipeline.clone(),
        desired: resolved.desired.clone(),
        cwd: resolved.cwd.clone(),
        window_id: work_info.as_ref().map(|work| work.window.clone()),
        observed,
        invalidate,
    };
    let mut run = client
        .pipeline_register(&registration)
        .await
        .context("register durable pipeline Run with muxad")?;
    // Migration/adoption path: panes created before durable Runs have no
    // generation option yet. Stamp every active alias with its own expected
    // generation; pending invalidated descendants keep the old value until
    // dependency reconciliation reaches them.
    for agent in &existing {
        let Some(alias) = agent.alias.as_deref() else {
            continue;
        };
        let Some(state) = run.aliases.get(alias) else {
            continue;
        };
        if !state.reconcile_pending {
            crate::tmux_work::mark_agent_generation(&agent.pane, state.generation)
                .with_context(|| format!("stamp pipeline generation on alias {alias:?}"))?;
        }
    }
    let done = run
        .aliases
        .values()
        .filter(|state| state.status == PipelineAliasStatus::Done)
        .map(|state| state.alias.clone())
        .collect::<Vec<_>>();
    let broadcast = resolved
        .request
        .as_ref()
        .map(|request| request.text.as_str());
    let plan = pipeline::plan(&resolved.desired, &existing, broadcast, &done);
    let claims = client
        .pipeline_claim(&identity, run.generation)
        .await
        .context("claim dependency-ready pipeline aliases")?;
    let mut launched = Vec::new();
    let mut reprompted = Vec::new();
    for claim in claims {
        let outcome = if let Some(pane) = claim.pane.as_deref() {
            let result = (|| {
                crate::tmux_work::mark_agent_generation(pane, claim.generation)?;
                if let Some(prompt) = claim.agent.prompt.as_deref() {
                    send_prompt(pane, prompt).with_context(|| {
                        format!(
                            "re-prompt pipeline alias {:?} in pane {pane}",
                            claim.agent.alias
                        )
                    })?;
                }
                reprompted.push(claim.agent.alias.clone());
                Ok((pane.to_string(), run.window_id.clone()))
            })();
            result
        } else {
            match recover_unreported_alias(&identity, &claim.agent.alias, claim.generation) {
                Ok(Some(target)) => Ok(target),
                Ok(None) => launch(&claim.agent, &resolved, claim.generation).map(|agent| {
                    let pane = agent.pane.clone();
                    launched.push(agent);
                    let window =
                        crate::tmux_work::find_work_in(&resolved.work, Some(&resolved.workspace))
                            .ok()
                            .flatten()
                            .map(|work| work.window);
                    (pane, window)
                }),
                Err(error) => Err(error),
            }
        };
        match outcome {
            Ok((pane, window)) => {
                run = client
                    .pipeline_report(
                        &identity,
                        &claim.agent.alias,
                        claim.generation,
                        PipelineAliasStatus::Running,
                        Some(&pane),
                        None,
                        window.as_deref(),
                    )
                    .await
                    .with_context(|| {
                        format!("report pipeline alias {:?} running", claim.agent.alias)
                    })?;
            }
            Err(error) => {
                let detail = format!("{error:#}");
                let _ = client
                    .pipeline_report(
                        &identity,
                        &claim.agent.alias,
                        claim.generation,
                        PipelineAliasStatus::Failed,
                        claim.pane.as_deref(),
                        Some(&detail),
                        run.window_id.as_deref(),
                    )
                    .await;
                return Err(error);
            }
        }
    }

    let (session, window) =
        match crate::tmux_work::find_work_in(&resolved.work, Some(&resolved.workspace))? {
            Some(info) => (Some(info.session), Some(info.window)),
            None => (None, None),
        };
    if let (Some(window), Some(ticket)) = (window.as_deref(), resolved.ticket.as_ref()) {
        crate::tmux_work::mark_work_external(window, ticket)
            .with_context(|| format!("record external issue on work window {window}"))?;
    }
    if let (Some(window), Some(layout)) = (window.as_deref(), resolved.layout.as_deref()) {
        apply_layout(window, layout)?;
    }
    let done = run
        .aliases
        .values()
        .filter(|state| state.status == PipelineAliasStatus::Done)
        .map(|state| state.alias.clone())
        .collect();
    Ok(finish(
        resolved, plan, launched, reprompted, session, window, false, done,
    ))
}

/// Reconcile one already-registered Run from its persisted desired agents.
/// This is used immediately after a completion event and by muxad's restart
/// safety-net worker, so neither path needs to resolve the ticket again.
pub(crate) async fn reconcile_run(
    client: &muxa::ipc::Client,
    identity: &WorkIdentity,
    generation: u64,
) -> Result<Vec<LaunchedAgent>> {
    let claims = client
        .pipeline_claim(identity, generation)
        .await
        .context("claim dependency-ready pipeline aliases")?;
    let mut launched = Vec::new();
    for claim in claims {
        let outcome: Result<(String, Option<String>)> = if let Some(pane) = claim.pane.as_deref() {
            (|| {
                crate::tmux_work::mark_agent_generation(pane, claim.generation)?;
                if let Some(prompt) = claim.agent.prompt.as_deref() {
                    send_prompt(pane, prompt).with_context(|| {
                        format!(
                            "re-prompt pipeline alias {:?} in pane {pane}",
                            claim.agent.alias
                        )
                    })?;
                }
                Ok((pane.to_string(), claim.window_id.clone()))
            })()
        } else {
            (|| {
                if let Some(target) =
                    recover_unreported_alias(identity, &claim.agent.alias, claim.generation)?
                {
                    return Ok(target);
                }
                {
                    let program = AgentProgram::parse(&claim.agent.program).map_err(|error| {
                        anyhow::anyhow!("pipeline agent {:?}: {error}", claim.agent.alias)
                    })?;
                    let direction = SplitDirection::parse(claim.agent.direction.as_deref())
                        .map_err(|error| {
                            anyhow::anyhow!("pipeline agent {:?}: {error}", claim.agent.alias)
                        })?;
                    crate::agent_launch::start(StartRequest {
                        agent: program,
                        placement: Placement::Pane,
                        target: None,
                        cwd: Some(claim.cwd.clone()),
                        prompt: claim.agent.prompt.clone(),
                        name: None,
                        workspace: Some(identity.workspace_id.clone()),
                        work: Some(identity.work_id.clone()),
                        role: claim.agent.role.clone(),
                        task: claim.agent.task.clone(),
                        alias: Some(claim.agent.alias.clone()),
                        generation: Some(claim.generation),
                        direction,
                    })
                    .with_context(|| format!("launch pipeline agent {:?}", claim.agent.alias))
                    .map(|result| {
                        launched.push(LaunchedAgent {
                            alias: claim.agent.alias.clone(),
                            pane: result.pane.clone(),
                            program: claim.agent.program.clone(),
                            role: claim.agent.role.clone(),
                        });
                        (result.pane, result.window)
                    })
                }
            })()
        };
        match outcome {
            Ok((pane, window)) => {
                client
                    .pipeline_report(
                        identity,
                        &claim.agent.alias,
                        claim.generation,
                        PipelineAliasStatus::Running,
                        Some(&pane),
                        None,
                        window.as_deref(),
                    )
                    .await
                    .with_context(|| {
                        format!("report pipeline alias {:?} running", claim.agent.alias)
                    })?;
            }
            Err(error) => {
                let detail = format!("{error:#}");
                let _ = client
                    .pipeline_report(
                        identity,
                        &claim.agent.alias,
                        claim.generation,
                        PipelineAliasStatus::Failed,
                        claim.pane.as_deref(),
                        Some(&detail),
                        claim.window_id.as_deref(),
                    )
                    .await;
                return Err(error);
            }
        }
    }
    Ok(launched)
}

pub async fn run_reconcile(args: ReconcileArgs, client: &muxa::ipc::Client) -> Result<()> {
    let runs = client
        .pipeline_runs()
        .await
        .context("list durable pipeline Runs")?;
    let selected = runs.into_iter().filter(|run| {
        if args.all {
            return run.has_ready_alias();
        }
        args.workspace
            .as_deref()
            .zip(args.work.as_deref())
            .is_some_and(|(workspace, work)| {
                run.identity.workspace_id.eq_ignore_ascii_case(workspace)
                    && run.identity.work_id.eq_ignore_ascii_case(work)
            })
    });
    for run in selected {
        if let Err(error) = reconcile_run(client, &run.identity, run.generation).await {
            tracing::warn!(
                work = %run.identity.key(),
                generation = run.generation,
                %error,
                "pipeline reconciliation failed",
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn finish(
    resolved: Resolved,
    plan: Plan,
    launched: Vec<LaunchedAgent>,
    reprompted: Vec<String>,
    session: Option<String>,
    window: Option<String>,
    dry_run: bool,
    done: Vec<String>,
) -> UpResult {
    UpResult {
        work: resolved.work,
        workspace: resolved.workspace,
        pipeline: resolved.pipeline,
        cwd: resolved.cwd,
        dry_run,
        ticket: resolved.ticket,
        worktree: resolved.worktree,
        created_worktree: resolved.created_worktree,
        session,
        window,
        layout: resolved.layout,
        request: resolved.request,
        graph: muxa::pipeline::graph(&resolved.desired),
        plan,
        launched,
        reprompted,
        done,
    }
}

// ---------------------------------------------------------------- resolve

pub(crate) async fn resolve(
    args: &UpArgs,
    config: &Config,
    client: Option<&muxa::ipc::Client>,
) -> Result<Resolved> {
    let states = agent_states(client).await;
    let work = crate::tmux_work::normalize_work_id(&args.work)?;
    let id = work.to_ascii_lowercase();
    if args.no_ticket
        && args
            .external
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
    {
        bail!("--external cannot be combined with --no-ticket");
    }
    let explicit_external = args
        .external
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let external_lookup = explicit_external.unwrap_or(&id);

    // An explicit --pipeline is its own routing decision, so it stands in
    // for a missing rule rather than being refused by one. That keeps the
    // first run of `muxa work up --pipeline x` working on an empty config,
    // which is where most people meet this command.
    let fallback = muxa::config::RouteConfig::default();
    // An explicitly linked issue may carry the routing prefix while the
    // durable Work id stays provider-neutral (`auth-cleanup` + `CAL-1234`).
    // Legacy calls without `--external` continue routing on the Work id.
    let route_selector = explicit_external.unwrap_or(&work);
    let route = match pipeline::select_route(&config.route, route_selector)? {
        Some(route) => route,
        None if args.pipeline.is_some() => &fallback,
        None => {
            return Err(pipeline::PipelineError::NoRoute(work.clone()).into());
        }
    };

    // Ask for the request first: it is free, and it is the last thing the
    // operator can change once money starts moving.
    let request = compose_request(args, config, &work)?;

    let ticket = if args.no_ticket {
        None
    } else {
        resolve_ticket(&config.ticket, external_lookup, args.refresh, args.yes).await?
    };

    // Two ids because the two forms are both load-bearing: `work` is what
    // muxa stores and displays, `id` is what goes in a branch name or a
    // directory.
    let mut vars = Vars::new().set("id", &id).set("work", &work);
    if let Some(ticket) = &ticket {
        vars = vars.with_ticket(ticket);
    }

    vars.set_opt(
        REQUEST_KEY,
        request.as_ref().map(|request| request.text.as_str()),
    );

    // Find the work window before choosing a directory, not after: a work
    // item that already exists has a cwd, and that answer beats whichever
    // directory the operator happens to be standing in.
    let workspace_hint = match args.workspace.as_deref() {
        Some(workspace) => Some(crate::tmux_work::normalize_workspace_id(workspace)?),
        None => match route.workspace.as_deref() {
            Some(workspace) => {
                let rendered = vars.render(workspace);
                // A workspace template that wants `{{cwd}}` cannot be
                // resolved yet; look the work up across workspaces instead.
                if rendered.contains("{{") {
                    None
                } else {
                    Some(crate::tmux_work::normalize_workspace_id(&rendered)?)
                }
            }
            None => None,
        },
    };
    let existing = crate::tmux_work::find_work_in(&work, workspace_hint.as_deref())?;

    let (cwd, worktree, created_worktree) = resolve_cwd(args, route, &vars, existing.as_ref())?;
    vars.set_opt("cwd", cwd.to_str());

    let workspace = match (workspace_hint, &existing) {
        (Some(workspace), _) => workspace,
        (None, Some(existing)) => existing.workspace.clone(),
        (None, None) => crate::tmux_work::workspace_id_for_cwd(&cwd)?,
    };
    vars.set_opt("workspace", Some(&workspace));

    let name = args
        .pipeline
        .as_deref()
        .or(route.pipeline.as_deref())
        .ok_or_else(|| anyhow::Error::from(pipeline::PipelineError::NoPipeline(work.clone())))?
        .to_string();
    let spec = config.pipeline.get(&name).ok_or_else(|| {
        anyhow::Error::from(pipeline::PipelineError::UnknownPipeline(name.clone()))
    })?;
    let desired = pipeline::desired_agents(&name, spec, &vars)?;
    let durable_run = match client {
        Some(client) => client.pipeline_runs().await.ok().and_then(|runs| {
            runs.into_iter()
                .find(|run| run.identity.workspace_id == workspace && run.identity.work_id == work)
        }),
        None => None,
    };

    Ok(Resolved {
        work,
        workspace,
        pipeline: name,
        cwd,
        ticket,
        worktree,
        created_worktree,
        cwd_pinned: cwd_is_pinned(args, route),
        layout: spec.layout.clone(),
        request,
        desired,
        durable_run,
        states,
    })
}

/// Whether the caller named a directory, as opposed to happening to stand
/// in one. Only a named directory is worth asserting against the work
/// window's recorded cwd; standing somewhere else must not fail a re-run.
fn cwd_is_pinned(args: &UpArgs, route: &muxa::config::RouteConfig) -> bool {
    args.cwd.is_some() || route.cwd.is_some() || route.worktree.is_some()
}

fn resolve_cwd(
    args: &UpArgs,
    route: &muxa::config::RouteConfig,
    vars: &Vars,
    existing: Option<&crate::tmux_work::WorkInfo>,
) -> Result<(PathBuf, Option<WorktreePlan>, bool)> {
    if route.worktree.is_some() && route.prepare.is_some() {
        bail!("route sets both `worktree` and `prepare`; pick whichever owns provisioning");
    }
    if let Some(command) = route.prepare.as_deref() {
        return prepared_cwd(args, route, vars, existing, command);
    }
    if let Some(config) = &route.worktree {
        if args.cwd.is_some() {
            bail!(
                "route for this work id creates a git worktree; --cwd would point the agents somewhere else"
            );
        }
        let plan = pipeline::worktree_plan(config, vars, &|value| expand_tilde(value));
        let created = ensure_worktree(&plan)?;
        let cwd = std::fs::canonicalize(&plan.path)
            .with_context(|| format!("resolve worktree {}", plan.path.display()))?;
        return Ok((cwd, Some(plan), created));
    }
    let source = match (&args.cwd, route.cwd.as_deref()) {
        (Some(cwd), _) => cwd.clone(),
        (None, Some(template)) => PathBuf::from(expand_tilde(&vars.render(template))),
        // Unpinned: the work window's own directory if it has one, so a
        // re-run from anywhere converges instead of arguing about cwd.
        (None, None) => match existing {
            Some(existing) => existing.cwd.clone(),
            None => std::env::current_dir().context("resolve current directory")?,
        },
    };
    let cwd = std::fs::canonicalize(&source)
        .with_context(|| format!("resolve cwd {}", source.display()))?;
    if !cwd.is_dir() {
        bail!("cwd is not a directory: {}", cwd.display());
    }
    Ok((cwd, None, false))
}

/// Provision through the route's `prepare` command, then work in `cwd`.
///
/// Run only when the work window does not exist yet: re-running `muxa
/// work up` must converge, and a provisioning command asked to create
/// something twice usually fails — loudly and for the wrong reason.
fn prepared_cwd(
    args: &UpArgs,
    route: &muxa::config::RouteConfig,
    vars: &Vars,
    existing: Option<&crate::tmux_work::WorkInfo>,
    command: &str,
) -> Result<(PathBuf, Option<WorktreePlan>, bool)> {
    if let Some(existing) = existing {
        // Already provisioned; adopt what it recorded.
        return Ok((existing.cwd.clone(), None, false));
    }
    let command = vars.render(command);
    if command.contains("{{") {
        bail!("prepare command still has unresolved placeholders: {command}");
    }
    let target = match (&args.cwd, route.cwd.as_deref()) {
        (Some(cwd), _) => cwd.clone(),
        (None, Some(template)) => PathBuf::from(expand_tilde(&vars.render(template))),
        (None, None) => bail!("route uses `prepare`; set `cwd` so muxa knows where it lands"),
    };
    if args.dry_run {
        println!("would run: {command}");
        // Nothing exists yet, so canonicalize would fail. Report the
        // intended path rather than inventing one.
        return Ok((target, None, false));
    }
    if !target.exists() {
        println!("preparing: {command}");
        run_prepare(&command)?;
    }
    let cwd = std::fs::canonicalize(&target).with_context(|| {
        format!(
            "prepare ran but {} does not exist; check the route's cwd",
            target.display()
        )
    })?;
    Ok((cwd, None, true))
}

fn run_prepare(command: &str) -> Result<()> {
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .status()
        .with_context(|| format!("running prepare: {command}"))?;
    if !status.success() {
        bail!("prepare command failed ({status}): {command}");
    }
    Ok(())
}

pub(crate) fn expand_tilde(value: &str) -> String {
    let Some(rest) = value.strip_prefix('~') else {
        return value.to_string();
    };
    // Only a bare `~` or `~/` — `~user` is a shell feature muxa has no
    // business guessing at.
    if !rest.is_empty() && !rest.starts_with('/') {
        return value.to_string();
    }
    dirs::home_dir().map_or_else(
        || value.to_string(),
        |home| format!("{}{rest}", home.display()),
    )
}

// ---------------------------------------------------------------- ticket

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct CachedTicket {
    fetched_at: i64,
    ticket: Ticket,
}

async fn resolve_ticket(
    config: &TicketConfig,
    id: &str,
    refresh: bool,
    assume_yes: bool,
) -> Result<Option<Ticket>> {
    let Some((source_name, source)) = pipeline::select_source(config, id)? else {
        return Ok(None);
    };
    if !refresh {
        if let Some(mut ticket) = cached_ticket(id, config.cache_secs) {
            ticket.source = Some(source_name.to_string());
            return Ok(Some(ticket));
        }
    }
    // Ask before spending, not after. Only on a cache miss: the common
    // re-run costs nothing, so the prompt appears exactly when money would
    // move — including under `--dry-run`, which skips creating panes but
    // still pays for the lookup.
    if !confirm_lookup(id, source_name, &config.agent, assume_yes)? {
        return Ok(None);
    }
    let prompt = Vars::new().set("id", id).render(&source.prompt);
    let cwd = config
        .cwd
        .clone()
        .map(|cwd| PathBuf::from(expand_tilde(&cwd.to_string_lossy())))
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    let answer = muxa::ask::one_shot(muxa::ask::OneShot {
        agent: &config.agent,
        prompt: &prompt,
        cwd: &cwd,
        permission_mode: config.permission_mode,
        additional_dirs: &config.additional_dirs,
        timeout: Duration::from_secs(config.timeout_secs),
    })
    .await
    .with_context(|| {
        format!("ticket source {source_name:?} could not look up {id} (use --no-ticket to launch without it)")
    })?;
    if let Some(cost) = answer.cost_usd {
        eprintln!("that lookup cost ${cost:.4}.");
    }
    let mut ticket = Ticket::parse_reply(id, &answer.text).with_context(|| {
        format!("ticket source {source_name:?} answered for {id} but not with a ticket")
    })?;
    ticket.source = Some(source_name.to_string());
    store_ticket(id, &ticket);
    Ok(Some(ticket))
}

fn ticket_cache_path(id: &str) -> Option<PathBuf> {
    let slug: String = id
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect();
    Some(
        dirs::cache_dir()?
            .join("muxa")
            .join("tickets")
            .join(format!("{slug}.json")),
    )
}

fn cached_ticket(id: &str, ttl_secs: u64) -> Option<Ticket> {
    if ttl_secs == 0 {
        return None;
    }
    let raw = std::fs::read_to_string(ticket_cache_path(id)?).ok()?;
    let cached: CachedTicket = serde_json::from_str(&raw).ok()?;
    let age = time::OffsetDateTime::now_utc().unix_timestamp() - cached.fetched_at;
    (age >= 0 && age <= i64::try_from(ttl_secs).unwrap_or(i64::MAX)).then_some(cached.ticket)
}

/// Best-effort: a ticket that cannot be cached is still a ticket.
fn store_ticket(id: &str, ticket: &Ticket) {
    let Some(path) = ticket_cache_path(id) else {
        return;
    };
    let entry = CachedTicket {
        fetched_at: time::OffsetDateTime::now_utc().unix_timestamp(),
        ticket: ticket.clone(),
    };
    let Ok(body) = serde_json::to_string(&entry) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, body);
}

// -------------------------------------------------------------- worktree

/// Create the work's git worktree, or confirm the one already there.
/// Returns whether this call created it.
fn ensure_worktree(plan: &WorktreePlan) -> Result<bool> {
    if plan.path.exists() {
        if !plan.path.join(".git").exists() {
            bail!(
                "{} already exists and is not a git worktree",
                plan.path.display()
            );
        }
        return Ok(false);
    }
    if !plan.repo.join(".git").exists() {
        bail!(
            "worktree repo {} is not a git repository",
            plan.repo.display()
        );
    }
    if let Some(parent) = plan.path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let path = plan.path.to_string_lossy().into_owned();
    let args: Vec<String> = if branch_exists(&plan.repo, &plan.branch) {
        // Reattaching an existing branch: `git worktree add` refuses if it
        // is checked out somewhere else, which is the right answer.
        vec!["worktree".into(), "add".into(), path, plan.branch.clone()]
    } else {
        let base = plan
            .base
            .clone()
            .or_else(|| default_base(&plan.repo))
            .unwrap_or_else(|| "HEAD".to_string());
        vec![
            "worktree".into(),
            "add".into(),
            "-b".into(),
            plan.branch.clone(),
            path,
            base,
        ]
    };
    git(&plan.repo, &args).with_context(|| format!("create worktree {}", plan.path.display()))?;
    Ok(true)
}

fn branch_exists(repo: &Path, branch: &str) -> bool {
    git(
        repo,
        &[
            "rev-parse".into(),
            "--verify".into(),
            "--quiet".into(),
            format!("refs/heads/{branch}"),
        ],
    )
    .is_ok()
}

fn default_base(repo: &Path) -> Option<String> {
    git(
        repo,
        &[
            "symbolic-ref".into(),
            "--quiet".into(),
            "--short".into(),
            "refs/remotes/origin/HEAD".into(),
        ],
    )
    .ok()
    .map(|out| out.trim().to_string())
    .filter(|out| !out.is_empty())
}

fn git(repo: &Path, args: &[String]) -> Result<String> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("run git")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "git {} failed{}",
            args.first().map_or("command", String::as_str),
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// ----------------------------------------------------------------- apply

/// Read the window's panes and, for each, what the daemon says the agent
/// on it is doing.
///
/// A pane proves someone was started there. Whether they are working,
/// idle, or stuck on a permission prompt is a different question, and the
/// daemon is the only thing that can answer it.
/// Ask the daemon what every tracked agent is doing, keyed by pane.
///
/// Best-effort: `muxa work up` does not otherwise need the daemon, and a
/// launch should not fail because the control plane is down. Without it,
/// the plan falls back to "a pane exists", which is what it always did.
async fn agent_states(client: Option<&muxa::ipc::Client>) -> HashMap<String, AgentState> {
    let Some(client) = client else {
        return HashMap::new();
    };
    client.snapshot().await.map_or_else(
        |_| HashMap::new(),
        |agents| {
            agents
                .into_iter()
                .filter_map(|agent| agent.pane.map(|pane| (pane, agent.state)))
                .collect()
        },
    )
}

fn existing_agents(
    work: &str,
    workspace: &str,
    states: &HashMap<String, AgentState>,
) -> Result<Vec<ExistingAgent>> {
    Ok(crate::tmux_work::find_work_in(work, Some(workspace))?
        .map(|info| {
            info.agents
                .into_iter()
                .map(|agent| ExistingAgent {
                    state: states.get(&agent.pane).copied(),
                    pane: agent.pane,
                    program: agent.agent,
                    alias: agent.alias,
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Resolve, and on a first run with nothing configured, offer to set it
/// up rather than failing with instructions the operator then has to
/// follow by hand.
///
/// "No route matches" is only an error for someone who already has a
/// config. For everyone else it is the first thing muxa ever says to
/// them, and answering it with TOML syntax is how a feature goes unused.
/// `muxa work init` is already the conversation that fixes it, so this
/// hands over to it and retries once with the config it wrote.
async fn resolve_or_onboard(
    args: &UpArgs,
    config: &Config,
    config_path: Option<PathBuf>,
    client: Option<&muxa::ipc::Client>,
) -> Result<Resolved> {
    let error = match resolve(args, config, client).await {
        Ok(resolved) => return Ok(resolved),
        Err(error) => error,
    };
    let unconfigured = error
        .downcast_ref::<pipeline::PipelineError>()
        .is_some_and(|error| matches!(error, pipeline::PipelineError::NoRoute(_)));
    if !unconfigured || !offer_onboarding()? {
        return Err(error);
    }
    crate::work_init::run(
        crate::work_init::InitArgs {
            describe: None,
            agent: None,
            dry_run: false,
            yes: false,
        },
        config,
        config_path.clone(),
    )
    .await?;
    // Reload: the file just changed underneath the config this process
    // started with.
    let config = Config::load_or_default(config_path.as_deref())
        .context("re-reading the config muxa work init just wrote")?;
    resolve(args, &config, client).await
}

/// Ask whether to set muxa up now. Non-interactive callers keep the plain
/// error — a script wants a failure it can read, not a prompt.
fn offer_onboarding() -> Result<bool> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(false);
    }
    println!("muxa has no work pipeline configured yet.");
    Ok(cliclack::confirm("Set one up now?")
        .initial_value(true)
        .interact()?)
}

/// Build the caller's request, asking for one when nothing was supplied.
fn compose_request(args: &UpArgs, config: &Config, work: &str) -> Result<Option<ComposedRequest>> {
    let body = match args.body.clone() {
        Some(body) => Some(body),
        None if args.skill.is_none() && args.context.is_none() => ask_for_request(work)?,
        None => None,
    };
    Ok(muxa::request::compose(
        RequestParts {
            skill: args.skill.as_deref(),
            body: body.as_deref(),
            context: args.context.as_deref(),
        },
        &config.message.skills,
    )?)
}

/// Ask before the lookup spends a turn.
///
/// `--dry-run` does not exempt this: it skips creating panes, not the
/// agent turn that resolves the ticket, and an operator who reads "dry
/// run" as "nothing happens" would be paying without being asked.
fn confirm_lookup(id: &str, source: &str, agent: &str, assume_yes: bool) -> Result<bool> {
    use std::io::IsTerminal;
    println!("{id} is not cached. Looking it up costs one headless {agent} turn, billed to your account (source {source:?}).");
    if assume_yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("looking up {id} spends a billed agent turn; pass --yes, or --no-ticket to skip it");
    }
    Ok(cliclack::confirm("Look it up?")
        .initial_value(true)
        .interact()?)
}

/// Ask what the agents should work on. Empty is a valid answer: a
/// ticket-driven pipeline already knows the task.
fn ask_for_request(work: &str) -> Result<Option<String>> {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Ok(None);
    }
    let text: String = cliclack::input(format!("What should the agents do for {work}?"))
        .placeholder("leave empty to use the pipeline's own instructions")
        .required(false)
        .interact()?;
    let text = text.trim().to_string();
    Ok((!text.is_empty()).then_some(text))
}

fn launch(agent: &DesiredAgent, resolved: &Resolved, generation: u64) -> Result<LaunchedAgent> {
    let program = AgentProgram::parse(&agent.program)
        .map_err(|error| anyhow::anyhow!("pipeline agent {:?}: {error}", agent.alias))?;
    let direction = SplitDirection::parse(agent.direction.as_deref())
        .map_err(|error| anyhow::anyhow!("pipeline agent {:?}: {error}", agent.alias))?;
    let result = crate::agent_launch::start(StartRequest {
        agent: program,
        placement: Placement::Pane,
        target: None,
        // Supplying a cwd asserts it against the work window's recorded one,
        // so only assert what the caller actually named. Unpinned, the
        // launcher adopts the existing work's directory — the same answer
        // `resolve` reported.
        cwd: resolved.cwd_pinned.then(|| resolved.cwd.clone()),
        prompt: agent.prompt.clone(),
        name: None,
        workspace: Some(resolved.workspace.clone()),
        work: Some(resolved.work.clone()),
        role: agent.role.clone(),
        task: agent.task.clone(),
        alias: Some(agent.alias.clone()),
        generation: Some(generation),
        direction,
    })
    .with_context(|| format!("launch pipeline agent {:?}", agent.alias))?;
    Ok(LaunchedAgent {
        alias: agent.alias.clone(),
        pane: result.pane,
        program: agent.program.clone(),
        role: agent.role.clone(),
    })
}

/// Recover the pane created in the narrow crash window between physical
/// launch and `pipeline_report`. The durable claim lease is intentionally
/// retryable, but retrying must adopt that pane instead of launching the
/// same alias twice.
fn recover_unreported_alias(
    identity: &WorkIdentity,
    alias: &str,
    generation: u64,
) -> Result<Option<(String, Option<String>)>> {
    let Some(work) =
        crate::tmux_work::find_work_in(&identity.work_id, Some(&identity.workspace_id))?
    else {
        return Ok(None);
    };
    let mut matching = work
        .agents
        .iter()
        .filter(|agent| agent.alias.as_deref() == Some(alias));
    let Some(agent) = matching.next() else {
        return Ok(None);
    };
    if matching.next().is_some() {
        bail!("cannot recover pipeline alias {alias:?}: multiple matching panes already exist");
    }
    crate::tmux_work::mark_agent_generation(&agent.pane, generation)
        .with_context(|| format!("adopt pipeline alias {alias:?} in pane {}", agent.pane))?;
    Ok(Some((agent.pane.clone(), Some(work.window))))
}

/// One line per pipeline alias, grouped by dependency depth.
///
/// The graph and the plan used to print as two blocks, so every agent
/// appeared twice — once as a shape, once as a row — and neither view was
/// complete on its own. They are the same thing: what runs when, and where
/// it currently is. Grouping only kicks in when the pipeline actually has
/// edges; a flat pipeline stays a flat list rather than gaining ceremony
/// it does not need.
///
/// Glyphs stay inside Geometric Shapes U+25A0–25CF. The half-filled
/// circles just past that range fall back to a different font in most
/// terminals and render at the wrong size.
fn print_plan_rows(result: &UpResult) {
    let nodes = &result.graph;
    let layered = nodes.iter().any(|node| node.depth > 0);
    let mut depth = usize::MAX;
    for node in nodes {
        let Some(step) = result
            .plan
            .steps
            .iter()
            .find(|step| step.alias() == node.alias)
        else {
            continue;
        };
        if layered && node.depth != depth {
            depth = node.depth;
            println!(
                "  {}",
                if depth == 0 {
                    "now".to_string()
                } else {
                    format!("then · after {}", node.after.join(", "))
                }
            );
        }
        let indent = if layered { "   " } else { "  " };
        let (glyph, status, detail) = describe(step, result);
        println!(
            "{indent}{glyph} {:<10} {:<8} {:<10} {status:<9} {detail}",
            node.alias,
            node.program,
            node.role.as_deref().unwrap_or("-"),
        );
    }
    for extra in &result.plan.unclaimed {
        println!(
            "  ◇ {:<10} {:<8} {:<10} {:<9} {}",
            extra.alias.as_deref().unwrap_or("(no alias)"),
            extra.program,
            "-",
            "unclaimed",
            extra.pane
        );
    }
}

/// Glyph, status word, and trailing detail for one alias.
fn describe(step: &PlanStep, result: &UpResult) -> (char, String, String) {
    match step {
        PlanStep::Launch(agent) => {
            let pane = result
                .launched
                .iter()
                .find(|launched| launched.alias == agent.alias)
                .map_or_else(String::new, |launched| launched.pane.clone());
            (
                '●',
                if result.dry_run {
                    "will start".into()
                } else {
                    "started".into()
                },
                pane,
            )
        }
        PlanStep::Keep { pane, state, .. } => {
            // Only while it is actually at rest: an agent that reported done
            // and was then re-prompted is working again, and saying `done`
            // over live work would be a lie the operator acts on.
            let at_rest = state.is_none_or(|state| state == AgentState::Idle);
            if at_rest && result.done.iter().any(|alias| alias == step.alias()) {
                ('◉', "done".into(), pane.clone())
            } else {
                (
                    '●',
                    state.map_or("running", state_label).to_string(),
                    pane.clone(),
                )
            }
        }
        PlanStep::Reprompt { pane, .. } => ('●', "prompted".into(), pane.clone()),
        PlanStep::Waiting { waiting_on, .. } => (
            '○',
            "waiting".into(),
            format!("for {}", waiting_on.join(", ")),
        ),
        PlanStep::Attention { pane, state, .. } => (
            '◆',
            state.map_or("blocked", state_label).to_string(),
            format!("{pane}  needs you"),
        ),
    }
}

#[cfg(test)]
mod done_view_tests {
    use super::*;

    fn result_with(done: &[&str]) -> UpResult {
        UpResult {
            work: "CAL-1".into(),
            workspace: "ws".into(),
            pipeline: "pair".into(),
            cwd: std::path::PathBuf::from("/repo"),
            dry_run: false,
            ticket: None,
            worktree: None,
            created_worktree: false,
            session: None,
            window: None,
            layout: None,
            request: None,
            graph: Vec::new(),
            plan: Plan {
                steps: Vec::new(),
                unclaimed: Vec::new(),
            },
            launched: Vec::new(),
            reprompted: Vec::new(),
            done: done.iter().map(|alias| (*alias).to_string()).collect(),
        }
    }

    fn kept(alias: &str, state: Option<AgentState>) -> PlanStep {
        PlanStep::Keep {
            alias: alias.to_string(),
            pane: "%9".into(),
            state,
        }
    }

    /// The gap this closes: a converged pipeline and a stalled one both leave
    /// every pane `idle`, so the plan view rendered them identically and a
    /// finished review sat unnoticed.
    #[test]
    fn a_reported_agent_reads_as_done_not_idle() {
        let result = result_with(&["review"]);
        let (glyph, status, _) = describe(&kept("review", Some(AgentState::Idle)), &result);
        assert_eq!(status, "done");
        assert_eq!(glyph, '\u{25c9}');

        let (_, status, _) = describe(&kept("impl", Some(AgentState::Idle)), &result);
        assert_eq!(status, "idle", "an alias that never reported is just idle");
    }

    /// Done is a claim about work at rest. An agent re-prompted after
    /// reporting is working again, and rendering that as `done` would be a
    /// lie the operator acts on.
    #[test]
    fn live_work_outranks_a_stale_done_marker() {
        let result = result_with(&["review"]);
        for state in [AgentState::Working, AgentState::Starting] {
            let (_, status, _) = describe(&kept("review", Some(state)), &result);
            assert_eq!(status, state_label(state));
        }
    }
}

fn state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Starting => "starting",
        AgentState::Working => "working",
        AgentState::Idle => "idle",
        AgentState::WaitingInput => "waiting",
        AgentState::WaitingChoice => "choosing",
        AgentState::Error => "error",
        AgentState::Stopped => "stopped",
    }
}

fn send_prompt(pane: &str, text: &str) -> Result<()> {
    if !muxa::tmux::send_text(pane, text) {
        bail!("tmux refused the prompt; the pane may be gone");
    }
    // The agent CLIs treat text and submit as separate events; typing into
    // a TUI that is still redrawing swallows the newline.
    std::thread::sleep(muxa::backend::PROMPT_SUBMIT_GRACE);
    let output = muxa::tmux::tmux_command_scoped()
        .args(["send-keys", "-t", pane, "Enter"])
        .output()
        .context("submit prompt")?;
    if !output.status.success() {
        bail!("tmux could not submit the prompt to {pane}");
    }
    Ok(())
}

fn apply_layout(window: &str, layout: &str) -> Result<()> {
    let output = muxa::tmux::tmux_command_scoped()
        .args(["select-layout", "-t", window, layout])
        .output()
        .context("apply pipeline layout")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!("tmux select-layout {layout} failed: {stderr}");
    }
    Ok(())
}

// ---------------------------------------------------------------- output

/// Show what each agent would actually receive. The rendered prompt is
/// the thing worth reviewing before spending a turn on it — a leftover
/// `{{ticket.title}}` or a role instruction that says the wrong thing is
/// obvious here and invisible in the plan summary.
fn print_prompts(desired: &[DesiredAgent]) {
    println!("prompts that would be sent:\n");
    for agent in desired {
        println!(
            "─── {} ({}{}) ───",
            agent.alias,
            agent.program,
            agent
                .role
                .as_deref()
                .map_or_else(String::new, |role| format!(", {role}"))
        );
        match agent.prompt.as_deref() {
            Some(prompt) => println!("{prompt}\n"),
            None => println!("(no prompt — starts interactive)\n"),
        }
    }
}

fn print_result(result: &UpResult) {
    let verb = if result.dry_run { "would be" } else { "is" };
    println!(
        "work {} {verb} in workspace {} via pipeline {}",
        result.work, result.workspace, result.pipeline
    );
    let worktree = match (&result.worktree, result.created_worktree) {
        (Some(plan), true) => format!(" (worktree {}, created)", plan.branch),
        (Some(plan), false) => format!(" (worktree {}, reused)", plan.branch),
        (None, _) => String::new(),
    };
    println!("  cwd      {}{worktree}", result.cwd.display());
    match &result.ticket {
        Some(ticket) => {
            let title = ticket.title.as_deref().unwrap_or("(no title)");
            let state = ticket
                .state
                .as_deref()
                .map_or_else(String::new, |state| format!("  [{state}]"));
            println!(
                "  external {}:{} {title}{state}",
                ticket.source.as_deref().unwrap_or("issue"),
                ticket.id
            );
            if let Some(url) = &ticket.url {
                println!("           {url}");
            }
        }
        None => println!("  external (not resolved)"),
    }
    if let Some(request) = &result.request {
        let mut lines = request.text.lines();
        let first = lines.next().unwrap_or_default();
        let clipped: String = first.chars().take(58).collect();
        println!(
            "  request  {}{clipped}{}",
            request
                .skill
                .as_deref()
                .map_or_else(String::new, |skill| format!("{skill} ")),
            if clipped.len() < first.len() || lines.next().is_some() {
                " …"
            } else {
                ""
            }
        );
    }
    print_plan_rows(result);
    for extra in &result.plan.unclaimed {
        println!(
            "  ? {:<10} {:<9} {:<12} {}",
            extra.alias.as_deref().unwrap_or("(no alias)"),
            extra.program,
            "unclaimed",
            extra.pane
        );
    }
    if let Some(layout) = &result.layout {
        println!("  layout   {layout}");
    }
    if result.plan.waiting() > 0 {
        println!(
            "\n{} agent(s) wait on an upstream alias; they start once it reports `muxa work done`.",
            result.plan.waiting()
        );
    }
    if result.plan.attention() > 0 {
        println!(
            "\n{} agent(s) are waiting on you; muxa did not type over their prompt.",
            result.plan.attention()
        );
    }
    if result.dry_run {
        println!("\ndry run: nothing was created. Re-run without --dry-run to apply.");
    } else if result.plan.converged() && result.launched.is_empty() {
        println!("\nalready converged: every pipeline agent has a pane.");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn up_args(cwd: Option<&str>) -> UpArgs {
        UpArgs {
            work: "cal-1".into(),
            external: None,
            pipeline: None,
            workspace: None,
            cwd: cwd.map(PathBuf::from),
            body: None,
            skill: None,
            context: None,
            dry_run: false,
            show_prompts: false,
            yes: false,
            no_ticket: true,
            refresh: false,
            json: false,
        }
    }

    #[test]
    fn a_route_cannot_own_provisioning_two_ways() {
        let both = muxa::config::RouteConfig {
            cwd: Some("/tmp/{{id}}".into()),
            prepare: Some("echo hi".into()),
            worktree: Some(muxa::config::WorktreeConfig {
                repo: "/repo".into(),
                ..muxa::config::WorktreeConfig::default()
            }),
            ..muxa::config::RouteConfig::default()
        };
        let error = resolve_cwd(&up_args(None), &both, &Vars::new(), None)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("pick whichever owns provisioning"),
            "{error}"
        );
    }

    #[test]
    fn prepare_needs_somewhere_to_land() {
        // The directory does not exist until the command has run, so muxa
        // cannot discover it — the route has to say.
        let route = muxa::config::RouteConfig {
            prepare: Some("echo hi".into()),
            ..muxa::config::RouteConfig::default()
        };
        let error = resolve_cwd(&up_args(None), &route, &Vars::new(), None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("set `cwd`"), "{error}");
    }

    #[test]
    fn an_unresolved_placeholder_never_reaches_the_shell() {
        // `{{ticket.branch}}` with no ticket would otherwise be passed to
        // sh verbatim and create a workspace named after the placeholder.
        let route = muxa::config::RouteConfig {
            prepare: Some("provision {{id}} {{ticket.branch}}".into()),
            cwd: Some("/tmp/{{id}}".into()),
            ..muxa::config::RouteConfig::default()
        };
        let vars = Vars::new().set("id", "cal-1");
        let error = resolve_cwd(&up_args(None), &route, &vars, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("unresolved placeholders"), "{error}");
        assert!(error.contains("ticket.branch"), "{error}");
    }

    #[test]
    fn an_already_provisioned_work_is_not_provisioned_again() {
        // Re-running must converge. A provisioning command asked to create
        // something twice usually fails, loudly and for the wrong reason.
        let route = muxa::config::RouteConfig {
            prepare: Some("false".into()), // would fail if it ran
            cwd: Some("/tmp/{{id}}".into()),
            ..muxa::config::RouteConfig::default()
        };
        let existing = crate::tmux_work::WorkInfo {
            work: "CAL-1".into(),
            workspace: "callabo".into(),
            session: "callabo".into(),
            session_id: "$1".into(),
            window: "@1".into(),
            window_index: 0,
            window_name: "CAL-1".into(),
            cwd: PathBuf::from("/tmp/already-here"),
            external_item: None,
            agents: Vec::new(),
        };
        let (cwd, worktree, created) =
            resolve_cwd(&up_args(None), &route, &Vars::new(), Some(&existing)).expect("adopts");
        assert_eq!(cwd, PathBuf::from("/tmp/already-here"));
        assert!(worktree.is_none());
        assert!(!created);
    }

    #[test]
    fn only_a_named_directory_is_asserted_against_the_work_window() {
        let bare = muxa::config::RouteConfig::default();
        // Standing somewhere is not a claim about where the work lives, so
        // a re-run from another directory must converge, not argue.
        assert!(!cwd_is_pinned(&up_args(None), &bare));
        assert!(cwd_is_pinned(&up_args(Some("/tmp")), &bare));

        let routed = muxa::config::RouteConfig {
            cwd: Some("/srv/{{id}}".into()),
            ..muxa::config::RouteConfig::default()
        };
        assert!(cwd_is_pinned(&up_args(None), &routed));

        let worktree = muxa::config::RouteConfig {
            worktree: Some(muxa::config::WorktreeConfig {
                repo: "/repo".into(),
                ..muxa::config::WorktreeConfig::default()
            }),
            ..muxa::config::RouteConfig::default()
        };
        assert!(cwd_is_pinned(&up_args(None), &worktree));
    }

    #[test]
    fn tilde_expands_only_for_the_current_user() {
        let home = dirs::home_dir().expect("home");
        assert_eq!(
            expand_tilde("~/workspace/callabo"),
            format!("{}/workspace/callabo", home.display())
        );
        assert_eq!(expand_tilde("~"), home.display().to_string());
        // `~someone-else` is a shell feature, left alone rather than guessed.
        assert_eq!(expand_tilde("~bob/x"), "~bob/x");
        assert_eq!(expand_tilde("/abs/path"), "/abs/path");
    }

    #[test]
    fn a_ticket_cache_path_is_safe_for_any_work_id() {
        let path = ticket_cache_path("cal-1234").expect("cache dir");
        assert!(path.ends_with("muxa/tickets/cal-1234.json"), "{path:?}");
        let nasty = ticket_cache_path("../../etc/passwd").expect("cache dir");
        let name = nasty.file_name().expect("file name").to_string_lossy();
        assert_eq!(name, "------etc-passwd.json");
        assert!(
            nasty.ends_with("muxa/tickets/------etc-passwd.json"),
            "{nasty:?}"
        );
    }

    #[test]
    fn a_cache_ttl_of_zero_never_serves_a_hit() {
        assert!(cached_ticket("cal-does-not-exist", 0).is_none());
    }
}
