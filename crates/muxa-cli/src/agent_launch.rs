//! Deterministic tmux agent launcher shared by the CLI and MCP tool.
//!
//! This deliberately exposes an allowlist of known agent CLIs instead of an
//! arbitrary shell command. Agents can create a worker pane without spending
//! a model turn reconstructing tmux syntax, while callers still get a narrow,
//! predictable operation.

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use serde::Serialize;
use std::path::{Path, PathBuf};

use muxa::backend::HostKind;
use muxa::ipc::Client;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum AgentProgram {
    Claude,
    Codex,
    Gemini,
    /// The Antigravity CLI. Named for its binary (`agy`) rather than the
    /// product, mirroring `Gemini` → `gemini`; `antigravity` stays accepted
    /// as an alias on both the CLI flag and [`AgentProgram::parse`].
    #[value(name = "agy", alias = "antigravity")]
    #[serde(rename = "agy")]
    Antigravity,
    Opencode,
}

impl AgentProgram {
    pub fn parse(value: &str) -> std::result::Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "claude" | "claude_code" | "claude-code" => Ok(Self::Claude),
            "codex" | "cx" => Ok(Self::Codex),
            "gemini" | "gemini_cli" | "gemini-cli" => Ok(Self::Gemini),
            "agy" | "antigravity" => Ok(Self::Antigravity),
            "opencode" => Ok(Self::Opencode),
            _ => Err(format!(
                "unknown agent {value:?}; expected claude, codex, gemini, agy, or opencode"
            )),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Antigravity => "agy",
            Self::Opencode => "opencode",
        }
    }

    fn launch_command(self, prompt: Option<&str>) -> String {
        let quoted = prompt.map(shell_single_quote);
        match (self, quoted) {
            (Self::Claude, Some(prompt)) => {
                format!("claude --dangerously-skip-permissions {prompt}")
            }
            (Self::Claude, None) => "claude --dangerously-skip-permissions".into(),
            // cx in the user's shell is codex --yolo. Invoke the expanded
            // command so launch behavior does not depend on interactive zsh
            // alias loading inside tmux.
            (Self::Codex, Some(prompt)) => format!("codex --yolo {prompt}"),
            (Self::Codex, None) => "codex --yolo".into(),
            (Self::Gemini, Some(prompt)) => {
                format!("gemini --approval-mode yolo --skip-trust -i {prompt}")
            }
            (Self::Gemini, None) => "gemini --approval-mode yolo --skip-trust".into(),
            // agy's own flag spelling: it has no `--approval-mode`/`--skip-trust`,
            // and `-i` is its `--prompt-interactive` alias (so the pane stays
            // interactive after the first prompt, matching gemini's behaviour).
            (Self::Antigravity, Some(prompt)) => {
                format!("agy --dangerously-skip-permissions -i {prompt}")
            }
            (Self::Antigravity, None) => "agy --dangerously-skip-permissions".into(),
            (Self::Opencode, Some(prompt)) => format!("opencode --prompt {prompt}"),
            (Self::Opencode, None) => "opencode".into(),
        }
    }

    fn native_launch(self, prompt: Option<&str>) -> NativeLaunch {
        let mut args = match self {
            Self::Claude | Self::Antigravity => {
                vec!["--dangerously-skip-permissions".into()]
            }
            Self::Codex => vec!["--yolo".into()],
            Self::Gemini => vec![
                "--approval-mode".into(),
                "yolo".into(),
                "--skip-trust".into(),
            ],
            Self::Opencode => Vec::new(),
        };
        if let Some(prompt) = prompt {
            match self {
                Self::Claude | Self::Codex => args.push(prompt.into()),
                Self::Gemini | Self::Antigravity => {
                    args.push("-i".into());
                    args.push(prompt.into());
                }
                Self::Opencode => {
                    args.push("--prompt".into());
                    args.push(prompt.into());
                }
            }
        }
        NativeLaunch {
            command: self.label().into(),
            args,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NativeLaunch {
    command: String,
    args: Vec<String>,
}

/// Execution host selection for the user-facing agent launcher.
///
/// The first native slice deliberately exposes only hosts that can create a
/// new surface today. Observation-only pane backends remain available to the
/// daemon and will join this enum as their launch lifecycle lands.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum LaunchHost {
    /// Use tmux when this shell is in a real tmux pane; otherwise use muxa's PTY.
    #[default]
    Auto,
    /// Spawn a muxa-owned PTY session through muxad.
    Native,
    /// Use the existing managed tmux launcher.
    Tmux,
}

impl LaunchHost {
    fn resolve(self, detected: Option<HostKind>) -> Self {
        match self {
            Self::Auto if detected == Some(HostKind::Tmux) => Self::Tmux,
            Self::Auto => Self::Native,
            explicit => explicit,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    #[default]
    Pane,
    Window,
    Session,
}

impl Placement {
    pub fn parse(value: Option<&str>) -> std::result::Result<Self, String> {
        match value.unwrap_or("pane").trim().to_ascii_lowercase().as_str() {
            "pane" | "split" => Ok(Self::Pane),
            "window" => Ok(Self::Window),
            "session" => Ok(Self::Session),
            other => Err(format!(
                "unknown placement {other:?}; expected pane, window, or session"
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum SplitDirection {
    #[default]
    Right,
    Down,
}

impl SplitDirection {
    pub fn parse(value: Option<&str>) -> std::result::Result<Self, String> {
        match value
            .unwrap_or("right")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "right" | "horizontal" => Ok(Self::Right),
            "down" | "vertical" => Ok(Self::Down),
            other => Err(format!(
                "unknown direction {other:?}; expected right or down"
            )),
        }
    }
}

#[derive(Debug, clap::Args)]
pub struct StartArgs {
    /// Known agent CLI to launch. codex expands the local cx profile (codex --yolo).
    #[arg(long, value_enum)]
    pub agent: AgentProgram,
    /// Execution host. Auto uses tmux inside tmux and a muxa-owned PTY elsewhere.
    #[arg(long, value_enum, default_value = "auto")]
    pub host: LaunchHost,
    /// Create a split pane (default), window, or independent session.
    #[arg(long, value_enum, default_value = "pane")]
    pub placement: Placement,
    /// tmux pane/window target. Defaults to `TMUX_PANE`; unused for session placement.
    #[arg(long)]
    pub target: Option<String>,
    /// Working directory. Defaults to the current directory.
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Initial task. Omit to start an interactive agent with no first prompt.
    #[arg(long)]
    pub prompt: Option<String>,
    /// Window/session name. Session placement derives it from cwd when omitted.
    #[arg(long)]
    pub name: Option<String>,
    /// Managed workspace/project. Valid only together with --work.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Managed Work. Reuses its current Run window or creates it once.
    #[arg(long)]
    pub work: Option<String>,
    /// Optional agent role stored on the pane, for example reviewer.
    #[arg(long)]
    pub role: Option<String>,
    /// Optional short task label stored on the pane.
    #[arg(long)]
    pub task: Option<String>,
    /// Stable per-work name for this pane, used by `muxa work up` to tell
    /// an agent it already started from one it still has to.
    #[arg(long)]
    pub alias: Option<String>,
    /// Split to the right (default) or below the target pane.
    #[arg(long, value_enum, default_value = "right")]
    pub direction: SplitDirection,
    /// Emit the structured result as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct WorkStartArgs {
    /// Work id. Its current Run reuses or creates one managed tmux window.
    pub work: String,
    /// Workspace/project session. Defaults to the work directory name.
    #[arg(long)]
    pub workspace: Option<String>,
    /// First allowlisted agent, or another agent to add when the work exists.
    #[arg(long, value_enum)]
    pub agent: AgentProgram,
    /// Work directory. Defaults to current directory; checked when the work window is reused.
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// Initial task.
    #[arg(long)]
    pub prompt: Option<String>,
    /// Optional agent role, for example implementer or reviewer.
    #[arg(long)]
    pub role: Option<String>,
    /// Optional short task label.
    #[arg(long)]
    pub task: Option<String>,
    /// Stable per-work name for this pane, used by `muxa work up`.
    #[arg(long)]
    pub alias: Option<String>,
    /// Split a reused work window to the right or below.
    #[arg(long, value_enum, default_value = "right")]
    pub direction: SplitDirection,
    /// Emit the structured result as JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone)]
pub struct StartRequest {
    pub agent: AgentProgram,
    pub placement: Placement,
    pub target: Option<String>,
    pub cwd: Option<PathBuf>,
    pub prompt: Option<String>,
    pub name: Option<String>,
    pub workspace: Option<String>,
    pub work: Option<String>,
    pub role: Option<String>,
    pub task: Option<String>,
    pub alias: Option<String>,
    /// Durable pipeline generation stamped on an aliased pane. A later
    /// `work done` reads it back so an old pane cannot complete a new run.
    pub generation: Option<u64>,
    pub direction: SplitDirection,
    /// The daemon this launch belongs to.
    ///
    /// Reserving an alias used to go to `paths::default_socket()` regardless,
    /// so `--alias` against any other daemon reserved the name in the wrong
    /// room — or, with no daemon on the default socket at all, failed and
    /// rolled the whole launch back.
    pub socket: PathBuf,
}

impl StartRequest {
    /// The request a `muxa agent start` invocation describes, against the
    /// daemon the CLI resolved.
    pub fn from_args(args: &StartArgs, socket: &Path) -> Self {
        Self {
            agent: args.agent,
            placement: args.placement,
            target: args.target.clone(),
            cwd: args.cwd.clone(),
            prompt: args.prompt.clone(),
            name: args.name.clone(),
            workspace: args.workspace.clone(),
            work: args.work.clone(),
            role: args.role.clone(),
            task: args.task.clone(),
            alias: args.alias.clone(),
            generation: None,
            direction: args.direction,
            socket: socket.to_path_buf(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StartResult {
    pub host: LaunchHost,
    pub pane: String,
    pub agent: AgentProgram,
    pub placement: Placement,
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work: Option<String>,
    pub created_workspace: bool,
    pub created_work: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    pub cwd: PathBuf,
    pub prompt_supplied: bool,
}

/// Stable `muxa agent start --json` envelope across execution hosts.
///
/// Host-specific coordinates remain optional, but every key is serialized so
/// consumers can parse one schema and branch only on `host`. `StartResult`
/// remains the tmux lifecycle result used by managed Work/MCP internals.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct AgentStartOutput {
    schema_version: u8,
    host: LaunchHost,
    agent: AgentProgram,
    placement: Placement,
    pane: Option<String>,
    session: Option<String>,
    window: Option<String>,
    name: Option<String>,
    workspace: Option<String>,
    work: Option<String>,
    created_workspace: bool,
    created_work: bool,
    role: Option<String>,
    task: Option<String>,
    alias: Option<String>,
    cwd: PathBuf,
    prompt_supplied: bool,
}

impl AgentStartOutput {
    fn tmux(result: &StartResult) -> Self {
        Self {
            schema_version: 1,
            host: result.host,
            agent: result.agent,
            placement: result.placement,
            pane: Some(result.pane.clone()),
            session: result.session.clone(),
            window: result.window.clone(),
            name: result.name.clone(),
            workspace: result.workspace.clone(),
            work: result.work.clone(),
            created_workspace: result.created_workspace,
            created_work: result.created_work,
            role: result.role.clone(),
            task: result.task.clone(),
            alias: result.alias.clone(),
            cwd: result.cwd.clone(),
            prompt_supplied: result.prompt_supplied,
        }
    }
}

pub async fn run(args: StartArgs, client: &Client, socket_path: &Path) -> Result<()> {
    match args.host.resolve(muxa::backend::detect_host_env()) {
        LaunchHost::Native => run_native(args, client, socket_path).await,
        LaunchHost::Tmux => run_tmux(args, socket_path),
        LaunchHost::Auto => unreachable!("auto launch host is resolved above"),
    }
}

fn run_tmux(args: StartArgs, socket: &Path) -> Result<()> {
    let json = args.json;
    let result = start(StartRequest::from_args(&args, socket))?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&AgentStartOutput::tmux(&result))?
        );
    } else {
        println!(
            "started {} in {} {} (cwd {})",
            result.agent.label(),
            match result.placement {
                Placement::Pane => "pane",
                Placement::Window => "window pane",
                Placement::Session => "session pane",
            },
            result.pane,
            result.cwd.display()
        );
    }
    Ok(())
}

async fn run_native(args: StartArgs, client: &Client, socket_path: &Path) -> Result<()> {
    if args.work.is_some() || args.workspace.is_some() {
        bail!(
            "managed --work/--workspace launch is not available on the native host yet; use --host tmux or `muxa run`"
        );
    }
    if args.target.is_some() || args.placement != Placement::Pane {
        bail!(
            "--target and non-pane --placement require tmux; omit them for a native session or use --host tmux"
        );
    }
    if args.direction != SplitDirection::Right {
        bail!("--direction requires tmux; omit it for a native session or use --host tmux");
    }
    if args.role.is_some() || args.task.is_some() || args.alias.is_some() {
        bail!(
            "--role, --task, and --alias require a managed Work binding, which is not available on the native host yet"
        );
    }

    let cwd_source = args.cwd.unwrap_or(std::env::current_dir()?);
    let cwd = std::fs::canonicalize(&cwd_source)
        .with_context(|| format!("resolve cwd {}", cwd_source.display()))?;
    if !cwd.is_dir() {
        bail!("cwd is not a directory: {}", cwd.display());
    }
    let prompt = args
        .prompt
        .as_deref()
        .filter(|prompt| !prompt.trim().is_empty());
    let launch = args.agent.native_launch(prompt);
    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    let session = client
        .spawn_session(muxa::SpawnSession {
            command: launch.command,
            args: launch.args,
            env: crate::caller_env(socket_path),
            cwd: Some(cwd.clone()),
            name: args
                .name
                .clone()
                .or_else(|| Some(args.agent.label().into())),
            cols: Some(cols),
            rows: Some(rows),
        })
        .await
        .context("spawning native muxa agent session")?;
    let result = AgentStartOutput {
        schema_version: 1,
        host: LaunchHost::Native,
        agent: args.agent,
        placement: Placement::Session,
        pane: None,
        session: Some(session.id),
        window: None,
        name: session.display_name,
        workspace: None,
        work: None,
        created_workspace: false,
        created_work: false,
        role: None,
        task: None,
        alias: None,
        cwd,
        prompt_supplied: prompt.is_some(),
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "started {} in native session {} (cwd {})\nattach with: muxa attach {}",
            result.agent.label(),
            result
                .session
                .as_deref()
                .expect("native result has a session"),
            result.cwd.display(),
            result
                .session
                .as_deref()
                .expect("native result has a session"),
        );
    }
    Ok(())
}

pub fn run_work_start(args: WorkStartArgs, socket: &Path) -> Result<()> {
    let json = args.json;
    let result = start(StartRequest {
        socket: socket.to_path_buf(),
        agent: args.agent,
        placement: Placement::Pane,
        target: None,
        cwd: args.cwd,
        prompt: args.prompt,
        name: None,
        workspace: args.workspace,
        work: Some(args.work),
        role: args.role,
        task: args.task,
        alias: args.alias,
        generation: None,
        direction: args.direction,
    })?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        println!(
            "{} agent {} in workspace {} work {} (session {}, window {}, cwd {})",
            if result.created_work {
                "created"
            } else {
                "added"
            },
            result.pane,
            result.workspace.as_deref().unwrap_or("-"),
            result.work.as_deref().unwrap_or("-"),
            result.session.as_deref().unwrap_or("-"),
            result.window.as_deref().unwrap_or("-"),
            result.cwd.display()
        );
    }
    Ok(())
}

/// Start one allowlisted agent in a detached tmux surface and return its exact
/// pane id. The operation is synchronous and should be wrapped in
/// `spawn_blocking` by async callers.
#[allow(clippy::too_many_lines)] // launch, metadata stamping, and rollback form one physical transaction
pub fn start(mut request: StartRequest) -> Result<StartResult> {
    let PreparedLaunch {
        cwd,
        workspace,
        work,
        created_workspace,
        created_work,
        adopted_session,
    } = prepare_launch(&mut request)?;
    let prompt = request
        .prompt
        .as_deref()
        .filter(|prompt| !prompt.trim().is_empty());
    let command = request.agent.launch_command(prompt);
    let args = tmux_args(&request, &cwd, &command)?;
    let output = muxa::tmux::tmux_command_scoped()
        .args(&args)
        .output()
        .context("run tmux agent launcher")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "tmux {} failed{}",
            args.first().map_or("command", String::as_str),
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        );
    }
    let pane = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('%'))
        .ok_or_else(|| anyhow::anyhow!("tmux created the surface but returned no pane id"))?
        .to_string();

    let managed = work.is_some();
    let session = managed
        .then(|| crate::tmux_work::session_name_for_pane(&pane))
        .transpose()?;
    let window = managed
        .then(|| crate::tmux_work::window_id_for_pane(&pane))
        .transpose()?;

    let mark = (|| {
        if let (Some(session), Some(workspace)) = (&adopted_session, workspace.as_deref()) {
            crate::tmux_work::adopt_workspace(session, workspace)?;
        }
        if created_workspace {
            crate::tmux_work::mark_workspace(
                session
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("created workspace has no tmux session"))?,
                workspace
                    .as_deref()
                    .expect("created workspace has workspace id"),
                &cwd,
            )?;
        }
        if created_work {
            let window = window
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("created work has no tmux window id"))?;
            crate::tmux_work::mark_work(
                window,
                workspace.as_deref().expect("created work has workspace id"),
                work.as_deref().expect("created work has id"),
                &cwd,
            )?;
        }
        crate::tmux_work::mark_agent(
            &pane,
            request.agent.label(),
            workspace.as_deref(),
            work.as_deref(),
            request.role.as_deref(),
            request.task.as_deref(),
            request.alias.as_deref(),
            request.generation,
            &request.socket,
        )
    })();
    if let Err(error) = mark {
        crate::tmux_work::cleanup_pane(&pane);
        return Err(error).context("record muxa tmux metadata");
    }
    // Minting belongs to the agent's session-start hook, which reaches the
    // room's arbiter. Doing it here too would allocate from a namespace this
    // process cannot see all of — the bug the arbiter exists to close — so an
    // unaliased launch reports no handle and the hook names the pane a moment
    // later.
    let alias = request.alias;

    Ok(StartResult {
        host: LaunchHost::Tmux,
        pane,
        agent: request.agent,
        placement: request.placement,
        name: request.name,
        workspace,
        work,
        created_workspace,
        created_work,
        session,
        window,
        role: request.role,
        task: request.task,
        alias,
        cwd,
        prompt_supplied: prompt.is_some(),
    })
}

struct PreparedLaunch {
    cwd: PathBuf,
    /// Session muxa put this work into without having created it. It gets a
    /// workspace identity, never the managed flag.
    adopted_session: Option<String>,
    workspace: Option<String>,
    work: Option<String>,
    created_workspace: bool,
    created_work: bool,
}

fn prepare_launch(request: &mut StartRequest) -> Result<PreparedLaunch> {
    if request.workspace.is_some() && request.work.is_none() {
        bail!("--workspace is valid only together with --work");
    }
    let work = request
        .work
        .as_deref()
        .map(crate::tmux_work::normalize_work_id)
        .transpose()?;

    if work.is_some()
        && (request.placement != Placement::Pane
            || request.target.is_some()
            || request.name.is_some())
    {
        bail!("--work uses its managed workspace and window; do not combine it with --placement, --target, or --name");
    }

    let requested_cwd = request.cwd.take();
    let initial_cwd_source = requested_cwd
        .clone()
        .map_or_else(std::env::current_dir, Ok)
        .context("resolve current directory")?;
    let initial_cwd = std::fs::canonicalize(&initial_cwd_source)
        .with_context(|| format!("resolve cwd {}", initial_cwd_source.display()))?;
    if !initial_cwd.is_dir() {
        bail!("cwd is not a directory: {}", initial_cwd.display());
    }

    let workspace = work
        .as_ref()
        .map(|_| match request.workspace.as_deref() {
            Some(workspace) => crate::tmux_work::normalize_workspace_id(workspace),
            None => crate::tmux_work::workspace_id_for_cwd(&initial_cwd),
        })
        .transpose()?;
    let existing_workspace = workspace
        .as_deref()
        .map(crate::tmux_work::find_workspace)
        .transpose()?
        .flatten();
    let existing_work = match (work.as_deref(), workspace.as_deref()) {
        (Some(work), Some(workspace)) => crate::tmux_work::find_work_in(work, Some(workspace))?,
        _ => None,
    };

    let cwd_source = existing_work
        .as_ref()
        .map_or(initial_cwd.as_path(), |existing| existing.cwd.as_path());
    let cwd = std::fs::canonicalize(cwd_source)
        .with_context(|| format!("resolve cwd {}", cwd_source.display()))?;
    if let (Some(existing), Some(requested)) = (&existing_work, requested_cwd) {
        let requested = std::fs::canonicalize(&requested)
            .with_context(|| format!("resolve cwd {}", requested.display()))?;
        if requested != cwd {
            bail!(
                "work {} already uses cwd {}; requested {}",
                existing.work,
                cwd.display(),
                requested.display()
            );
        }
    }

    // A session already named after this workspace is this workspace. Making
    // `callabo-2` beside it splits one workspace across two sessions, which
    // is the opposite of what session=workspace means.
    let adopted_session = match (&workspace, &existing_workspace) {
        (Some(workspace), None) => crate::tmux_work::adoptable_session(workspace)?,
        _ => None,
    };
    let created_workspace =
        workspace.is_some() && existing_workspace.is_none() && adopted_session.is_none();
    let created_work = work.is_some() && existing_work.is_none();
    if let Some(existing) = &existing_work {
        request.placement = Placement::Pane;
        request.target = Some(existing.window.clone());
        request.name = Some(existing.window_name.clone());
    } else if let Some(existing) = &existing_workspace {
        request.placement = Placement::Window;
        request.target = Some(existing.session.clone());
        request.name = Some(crate::tmux_work::window_name_for_work(
            work.as_deref().expect("managed work has id"),
        )?);
    } else if let Some(session) = adopted_session.as_deref() {
        request.placement = Placement::Window;
        request.target = Some(session.to_string());
        request.name = Some(crate::tmux_work::window_name_for_work(
            work.as_deref().expect("managed work has id"),
        )?);
    } else if let Some(workspace) = workspace.as_deref() {
        request.placement = Placement::Session;
        request.target = None;
        request.name = Some(crate::tmux_work::session_name_for_workspace(workspace)?);
    }

    resolve_placement_target(request, &cwd)?;
    Ok(PreparedLaunch {
        cwd,
        adopted_session,
        workspace,
        work,
        created_workspace,
        created_work,
    })
}

fn resolve_placement_target(request: &mut StartRequest, cwd: &Path) -> Result<()> {
    // new-window rejects a pane id even though pane ids are the stable target
    // Muxa exposes to callers. Resolve either a pane or window input to the
    // owning tmux session and let tmux choose an unused window index.
    if request.placement == Placement::Window {
        let target = request
            .target
            .clone()
            .or_else(|| std::env::var("TMUX_PANE").ok())
            .filter(|target| !target.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("window placement needs --target or TMUX_PANE"))?;
        request.target = Some(resolve_window_session(&target)?);
    }
    if request.placement == Placement::Session {
        if request.target.is_some() {
            bail!("session placement does not accept --target");
        }
        let base = request
            .name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .map_or_else(
                || session_base_name(cwd.to_string_lossy().as_ref()),
                sanitize_session_name,
            );
        let existing = existing_session_names();
        request.name = Some(unique_session_name(base, |candidate| {
            existing.iter().any(|name| name == candidate)
        }));
    }
    Ok(())
}

fn resolve_window_session(target: &str) -> Result<String> {
    let output = muxa::tmux::tmux_command_scoped()
        .args(["display-message", "-p", "-t", target, "#{session_name}"])
        .output()
        .context("resolve tmux window target")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "cannot resolve tmux target {target:?}{}",
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        );
    }
    let session = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if session.is_empty() {
        bail!("tmux target {target:?} resolved to an empty session");
    }
    Ok(session)
}

fn tmux_args(request: &StartRequest, cwd: &Path, command: &str) -> Result<Vec<String>> {
    let cwd = cwd
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("cwd is not valid UTF-8: {}", cwd.display()))?;
    let current_target = || {
        request
            .target
            .clone()
            .or_else(|| std::env::var("TMUX_PANE").ok())
            .filter(|target| !target.trim().is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "{} placement needs --target or TMUX_PANE",
                    match request.placement {
                        Placement::Pane => "pane",
                        Placement::Window => "window",
                        Placement::Session => "session",
                    }
                )
            })
    };

    let args = match request.placement {
        Placement::Pane => {
            let target = current_target()?;
            let split = match request.direction {
                SplitDirection::Right => "-h",
                SplitDirection::Down => "-v",
            };
            vec![
                "split-window".into(),
                split.into(),
                "-d".into(),
                "-P".into(),
                "-F".into(),
                "#{pane_id}".into(),
                "-t".into(),
                target,
                "-c".into(),
                cwd.into(),
                command.into(),
            ]
        }
        Placement::Window => {
            let target = current_target()?;
            let mut args = vec![
                "new-window".into(),
                "-d".into(),
                "-P".into(),
                "-F".into(),
                "#{pane_id}".into(),
                "-t".into(),
                target,
            ];
            if let Some(name) = request
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
            {
                args.extend(["-n".into(), name.into()]);
            }
            args.extend(["-c".into(), cwd.into(), command.into()]);
            args
        }
        Placement::Session => {
            if request.target.is_some() {
                bail!("session placement does not accept --target");
            }
            let name = request
                .name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .map_or_else(|| session_base_name(cwd), sanitize_session_name);
            let mut args = vec![
                "new-session".into(),
                "-d".into(),
                "-P".into(),
                "-F".into(),
                "#{pane_id}".into(),
                "-s".into(),
                name,
            ];
            if let Some(work) = request.work.as_deref() {
                args.extend(["-n".into(), crate::tmux_work::window_name_for_work(work)?]);
            }
            args.extend(["-c".into(), cwd.into(), command.into()]);
            args
        }
    };
    Ok(args)
}

fn shell_single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn session_base_name(cwd: &str) -> String {
    Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| "agent".into(), sanitize_session_name)
}

fn sanitize_session_name(name: &str) -> String {
    let cleaned: String = name
        .trim()
        .chars()
        .map(|ch| if ch == '.' || ch == ':' { '-' } else { ch })
        .collect();
    if cleaned.is_empty() {
        "agent".into()
    } else {
        cleaned
    }
}

fn existing_session_names() -> Vec<String> {
    muxa::tmux::tmux_command_scoped()
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn unique_session_name(base: String, exists: impl Fn(&str) -> bool) -> String {
    if !exists(&base) {
        return base;
    }
    (2..10_000)
        .map(|suffix| format!("{base}-{suffix}"))
        .find(|candidate| !exists(candidate))
        .unwrap_or_else(|| format!("{base}-overflow"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(agent: AgentProgram, placement: Placement) -> StartRequest {
        StartRequest {
            socket: muxa::paths::default_socket(),
            agent,
            placement,
            target: Some("%9".into()),
            cwd: Some(PathBuf::from("/tmp")),
            prompt: Some("review June's changes; don't edit".into()),
            name: None,
            workspace: None,
            work: None,
            role: None,
            task: None,
            alias: None,
            generation: None,
            direction: SplitDirection::Right,
        }
    }

    #[test]
    fn codex_profile_expands_cx_and_quotes_the_prompt() {
        assert_eq!(
            AgentProgram::Codex.launch_command(Some("review June's changes; don't edit")),
            "codex --yolo 'review June'\\''s changes; don'\\''t edit'"
        );
    }

    #[test]
    fn native_launch_keeps_the_prompt_as_one_argv_value() {
        let launch = AgentProgram::Codex.native_launch(Some("review June's changes; don't edit"));
        assert_eq!(launch.command, "codex");
        assert_eq!(launch.args, ["--yolo", "review June's changes; don't edit"]);

        let gemini = AgentProgram::Gemini.native_launch(Some("review it"));
        assert_eq!(gemini.command, "gemini");
        assert_eq!(
            gemini.args,
            ["--approval-mode", "yolo", "--skip-trust", "-i", "review it"]
        );
    }

    #[test]
    fn agent_start_json_uses_one_schema_for_tmux_and_native() {
        let tmux = AgentStartOutput {
            schema_version: 1,
            host: LaunchHost::Tmux,
            agent: AgentProgram::Codex,
            placement: Placement::Pane,
            pane: Some("%7".into()),
            session: Some("$1".into()),
            window: Some("@2".into()),
            name: Some("review".into()),
            workspace: None,
            work: None,
            created_workspace: false,
            created_work: false,
            role: None,
            task: None,
            alias: None,
            cwd: PathBuf::from("/repo"),
            prompt_supplied: true,
        };
        let native = AgentStartOutput {
            host: LaunchHost::Native,
            placement: Placement::Session,
            pane: None,
            session: Some("pty-7".into()),
            window: None,
            ..tmux.clone()
        };
        let tmux = serde_json::to_value(tmux).unwrap();
        let native = serde_json::to_value(native).unwrap();

        let tmux_keys = tmux.as_object().unwrap().keys().collect::<Vec<_>>();
        let native_keys = native.as_object().unwrap().keys().collect::<Vec<_>>();
        assert_eq!(tmux_keys, native_keys);
        assert_eq!(tmux["pane"], "%7");
        assert!(native["pane"].is_null());
        assert_eq!(native["session"], "pty-7");
        assert_eq!(native["placement"], "session");
    }

    #[test]
    fn auto_launch_uses_tmux_only_for_a_tmux_shell() {
        assert_eq!(
            LaunchHost::Auto.resolve(Some(HostKind::Tmux)),
            LaunchHost::Tmux
        );
        assert_eq!(LaunchHost::Auto.resolve(None), LaunchHost::Native);
        assert_eq!(
            LaunchHost::Auto.resolve(Some(HostKind::Rmux)),
            LaunchHost::Native
        );
        assert_eq!(
            LaunchHost::Native.resolve(Some(HostKind::Tmux)),
            LaunchHost::Native
        );
    }

    #[test]
    fn pane_plan_is_detached_and_returns_the_pane_id_format() {
        let request = request(AgentProgram::Codex, Placement::Pane);
        let args = tmux_args(&request, Path::new("/tmp"), "codex --yolo").unwrap();
        assert_eq!(args[0], "split-window");
        assert!(args.iter().any(|arg| arg == "-d"));
        assert!(args.windows(2).any(|pair| pair == ["-F", "#{pane_id}"]));
        assert!(args.windows(2).any(|pair| pair == ["-t", "%9"]));
        assert_eq!(args.last().unwrap(), "codex --yolo");
    }

    #[test]
    fn window_and_session_plans_use_the_requested_surface() {
        let mut window = request(AgentProgram::Claude, Placement::Window);
        window.target = Some("muxa".into());
        window.name = Some("review".into());
        let args = tmux_args(&window, Path::new("/tmp"), "claude").unwrap();
        assert_eq!(args[0], "new-window");
        assert!(args.windows(2).any(|pair| pair == ["-n", "review"]));
        assert!(args.windows(2).any(|pair| pair == ["-t", "muxa"]));

        let mut session = request(AgentProgram::Gemini, Placement::Session);
        session.target = None;
        session.name = Some("cal.7041:review".into());
        let args = tmux_args(&session, Path::new("/tmp"), "gemini").unwrap();
        assert_eq!(args[0], "new-session");
        assert!(args
            .windows(2)
            .any(|pair| pair == ["-s", "cal-7041-review"]));
    }

    #[test]
    fn managed_workspace_session_names_its_first_work_window() {
        let mut session = request(AgentProgram::Codex, Placement::Session);
        session.target = None;
        session.name = Some("muxa".into());
        session.workspace = Some("muxa".into());
        session.work = Some("TEST-0001".into());
        let args = tmux_args(&session, Path::new("/tmp"), "codex --yolo").unwrap();
        assert!(args.windows(2).any(|pair| pair == ["-s", "muxa"]));
        assert!(args.windows(2).any(|pair| pair == ["-n", "TEST-0001"]));
    }

    #[test]
    fn parsers_accept_short_agent_and_placement_aliases() {
        assert_eq!(AgentProgram::parse("cx").unwrap(), AgentProgram::Codex);
        assert_eq!(Placement::parse(Some("split")).unwrap(), Placement::Pane);
        assert_eq!(
            SplitDirection::parse(Some("vertical")).unwrap(),
            SplitDirection::Down
        );
    }

    #[test]
    fn repeated_session_names_receive_a_stable_suffix() {
        let existing = ["review", "review-2"];
        assert_eq!(
            unique_session_name("review".into(), |name| existing.contains(&name)),
            "review-3"
        );
    }
}
