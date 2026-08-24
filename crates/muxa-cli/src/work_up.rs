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
use muxa::pipeline::{
    self, DesiredAgent, ExistingAgent, Plan, PlanStep, Ticket, Vars, WorktreePlan, REQUEST_KEY,
};
use muxa::request::{ComposedRequest, RequestParts};

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
    /// Emit the structured result as JSON.
    #[arg(long)]
    pub json: bool,
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
    pub plan: Plan,
    pub launched: Vec<LaunchedAgent>,
    /// Aliases that were sent `--prompt`.
    pub reprompted: Vec<String>,
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
}

pub async fn run(args: UpArgs, config: &Config) -> Result<()> {
    let json = args.json;
    let dry_run = args.dry_run;
    let resolved = resolve(&args, config).await?;
    let result = apply(resolved, dry_run)?;
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
    let existing = existing_agents(&resolved.work, &resolved.workspace)?;
    let broadcast = resolved
        .request
        .as_ref()
        .map(|request| request.text.as_str());
    let plan = pipeline::plan(&resolved.desired, &existing, broadcast);

    if dry_run {
        return Ok(finish(
            resolved,
            plan,
            Vec::new(),
            Vec::new(),
            None,
            None,
            true,
        ));
    }

    let mut launched = Vec::new();
    let mut reprompted = Vec::new();
    for step in &plan.steps {
        match step {
            PlanStep::Launch(agent) => launched.push(launch(agent, &resolved)?),
            PlanStep::Reprompt {
                alias,
                pane,
                prompt,
            } => {
                send_prompt(pane, prompt)
                    .with_context(|| format!("send --prompt to {alias} in pane {pane}"))?;
                reprompted.push(alias.clone());
            }
            PlanStep::Keep { .. } => {}
        }
    }

    // Read identity back from tmux rather than from the launch results:
    // when every agent was already running, nothing was launched and there
    // is no result to read it from.
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
        // Splitting an existing window repeatedly halves whichever pane was
        // active, so geometry is only sane once every pane exists.
        apply_layout(window, layout)?;
    }
    Ok(finish(
        resolved, plan, launched, reprompted, session, window, false,
    ))
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
        plan,
        launched,
        reprompted,
    }
}

// ---------------------------------------------------------------- resolve

pub(crate) async fn resolve(args: &UpArgs, config: &Config) -> Result<Resolved> {
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

    let ticket = if args.no_ticket {
        None
    } else {
        resolve_ticket(&config.ticket, external_lookup, args.refresh).await?
    };

    // Two ids because the two forms are both load-bearing: `work` is what
    // muxa stores and displays, `id` is what goes in a branch name or a
    // directory.
    let mut vars = Vars::new().set("id", &id).set("work", &work);
    if let Some(ticket) = &ticket {
        vars = vars.with_ticket(ticket);
    }

    // The caller's request is a var like any other, so a pipeline template
    // can place it — and so it reaches the ticket-less path unchanged.
    let request = muxa::request::compose(
        RequestParts {
            skill: args.skill.as_deref(),
            body: args.body.as_deref(),
            context: args.context.as_deref(),
        },
        &config.message.skills,
    )?;
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

async fn resolve_ticket(config: &TicketConfig, id: &str, refresh: bool) -> Result<Option<Ticket>> {
    let Some((source_name, source)) = pipeline::select_source(config, id)? else {
        return Ok(None);
    };
    if !refresh {
        if let Some(mut ticket) = cached_ticket(id, config.cache_secs) {
            ticket.source = Some(source_name.to_string());
            return Ok(Some(ticket));
        }
    }
    // Say what is being spawned before spawning it. Only on a cache miss:
    // the common re-run costs nothing and does not need announcing.
    eprintln!(
        "resolving {id} via ticket source {source_name:?} — one headless {} turn, billed to your account",
        config.agent
    );
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

fn existing_agents(work: &str, workspace: &str) -> Result<Vec<ExistingAgent>> {
    Ok(crate::tmux_work::find_work_in(work, Some(workspace))?
        .map(|info| {
            info.agents
                .into_iter()
                .map(|agent| ExistingAgent {
                    pane: agent.pane,
                    program: agent.agent,
                    alias: agent.alias,
                })
                .collect()
        })
        .unwrap_or_default())
}

fn launch(agent: &DesiredAgent, resolved: &Resolved) -> Result<LaunchedAgent> {
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
    for step in &result.plan.steps {
        match step {
            PlanStep::Launch(agent) => {
                let pane = result
                    .launched
                    .iter()
                    .find(|launched| launched.alias == agent.alias)
                    .map_or_else(|| "-".to_string(), |launched| launched.pane.clone());
                println!(
                    "  + {:<10} {:<9} {:<12} {pane}",
                    agent.alias,
                    agent.program,
                    agent.role.as_deref().unwrap_or("-")
                );
            }
            PlanStep::Reprompt { alias, pane, .. } => {
                println!("  » {alias:<10} {:<9} {:<12} {pane}", "prompted", "");
            }
            PlanStep::Keep { alias, pane } => {
                println!("  = {alias:<10} {:<9} {:<12} {pane}", "running", "");
            }
        }
    }
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
