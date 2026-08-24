//! Managed tmux workspace/session, work/window, and agent/pane lifecycle.
//!
//! Muxa's tmux policy is deliberately narrow:
//! - one managed session represents one workspace or project;
//! - one managed window binds the current run of a muxa Work;
//! - one managed pane represents one coding agent;
//!
//! Identity is stored in tmux user options so it survives muxad and MCP
//! process restarts without adding another database.

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use serde::Serialize;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

const WORKSPACE_ID_OPTION: &str = "@muxa_workspace_id";
const WORKSPACE_CWD_OPTION: &str = "@muxa_workspace_cwd";
const MANAGED_WORKSPACE_OPTION: &str = "@muxa_managed_workspace";
const WORK_ID_OPTION: &str = "@muxa_work_id";
const WORK_CWD_OPTION: &str = "@muxa_work_cwd";
const MANAGED_WORK_OPTION: &str = "@muxa_managed_work";
const MANAGED_AGENT_OPTION: &str = "@muxa_managed_agent";
const AGENT_OPTION: &str = "@muxa_agent";
const AGENT_ROLE_OPTION: &str = "@muxa_agent_role";
const AGENT_TASK_OPTION: &str = "@muxa_agent_task";
/// Stable per-window name for one pipeline agent. This is the key
/// `muxa work up` diffs on, which is why it lives on the pane rather
/// than in the daemon: it has to outlive muxad, the CLI process, and
/// the agent restarting inside the pane.
const AGENT_ALIAS_OPTION: &str = "@muxa_agent_alias";
const PANE_WORKSPACE_OPTION: &str = "@muxa_agent_workspace_id";
const PANE_WORK_OPTION: &str = "@muxa_agent_work_id";
const EXTERNAL_SOURCE_OPTION: &str = "@muxa_external_source";
const EXTERNAL_SCOPE_OPTION: &str = "@muxa_external_scope";
const EXTERNAL_STABLE_ID_OPTION: &str = "@muxa_external_stable_id";
const EXTERNAL_KEY_OPTION: &str = "@muxa_external_key";
const EXTERNAL_TITLE_OPTION: &str = "@muxa_external_title";
const EXTERNAL_URL_OPTION: &str = "@muxa_external_url";
const EXTERNAL_STATUS_OPTION: &str = "@muxa_external_status";

const SESSION_FORMAT: &str = "#{session_name}\t#{session_id}\t#{@muxa_workspace_id}\t#{@muxa_workspace_cwd}\t#{@muxa_managed_workspace}\t#{session_attached}\t#{session_windows}";
const WINDOW_FORMAT: &str = "#{session_name}\t#{session_id}\t#{window_id}\t#{window_index}\t#{window_name}\t#{@muxa_work_id}\t#{@muxa_work_cwd}\t#{@muxa_managed_work}\t#{@muxa_external_source}\t#{@muxa_external_scope}\t#{@muxa_external_stable_id}\t#{@muxa_external_key}\t#{@muxa_external_title}\t#{@muxa_external_url}\t#{@muxa_external_status}";
const PANE_FORMAT: &str = "#{session_name}\t#{window_id}\t#{pane_id}\t#{@muxa_agent}\t#{@muxa_agent_role}\t#{@muxa_agent_task}\t#{pane_current_command}\t#{pane_current_path}\t#{@muxa_managed_agent}\t#{@muxa_agent_workspace_id}\t#{@muxa_agent_work_id}\t#{@muxa_agent_alias}";
const WINDOW_IDENTITY_FORMAT: &str =
    "#{window_id}\t#{session_id}\t#{session_name}\t#{window_name}\t#{automatic-rename}";

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ManagedAgentPane {
    pub pane: String,
    pub agent: String,
    /// Pipeline alias, when this pane was created by `muxa work up`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    pub command: String,
    pub cwd: PathBuf,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorkInfo {
    pub work: String,
    pub workspace: String,
    pub session: String,
    pub window: String,
    pub window_index: u32,
    pub window_name: String,
    pub cwd: PathBuf,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_item: Option<Box<ExternalItemInfo>>,
    pub agents: Vec<ManagedAgentPane>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ExternalItemInfo {
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stable_id: Option<String>,
    pub display_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorkspaceInfo {
    pub workspace: String,
    pub session: String,
    pub cwd: PathBuf,
    pub attached_clients: u32,
    pub windows: u32,
    pub works: Vec<WorkInfo>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WindowRenameResult {
    pub window_id: String,
    pub session_id: String,
    pub session_name: String,
    pub previous_name: String,
    pub name: String,
    pub automatic: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum AgentControlAction {
    Interrupt,
    Terminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManageAction {
    ListWorkspace,
    ShowWorkspace,
    ListWork,
    ShowWork,
    InterruptAgent,
    TerminateAgent,
    CloseWork,
    CloseWorkspace,
}

impl ManageAction {
    pub fn parse(value: &str) -> std::result::Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "list_workspace" | "list_workspaces" => Ok(Self::ListWorkspace),
            "show_workspace" => Ok(Self::ShowWorkspace),
            "list_work" | "list" => Ok(Self::ListWork),
            "show_work" | "show" => Ok(Self::ShowWork),
            "interrupt_agent" | "interrupt" | "abort" => Ok(Self::InterruptAgent),
            "terminate_agent" | "terminate" | "kill" => Ok(Self::TerminateAgent),
            "close_work" | "close" => Ok(Self::CloseWork),
            "close_workspace" => Ok(Self::CloseWorkspace),
            other => Err(format!(
                "unknown tmux action {other:?}; expected list_workspace, show_workspace, \
                 list_work, show_work, interrupt_agent, terminate_agent, close_work, \
                 or close_workspace"
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ManageRequest {
    pub action: ManageAction,
    pub pane: Option<String>,
    pub workspace: Option<String>,
    pub work: Option<String>,
    pub confirm: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum ManageResult {
    Workspaces {
        workspaces: Vec<WorkspaceInfo>,
    },
    Workspace {
        workspace: WorkspaceInfo,
    },
    Works {
        works: Vec<WorkInfo>,
    },
    Work {
        work: WorkInfo,
    },
    AgentControl {
        action: AgentControlAction,
        pane: String,
    },
    WorkClosed {
        work: String,
        workspace: String,
        session: String,
        window: String,
    },
    WorkspaceClosed {
        workspace: String,
        session: String,
    },
}

#[derive(Debug, clap::Args)]
pub struct AgentControlArgs {
    /// Exact managed tmux pane id, for example %42.
    #[arg(long)]
    pub pane: String,
    /// Interrupt the current turn or terminate the whole pane.
    #[arg(long, value_enum)]
    pub action: AgentControlAction,
    /// Confirm the destructive terminate action.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct WindowRenameArgs {
    /// New stable display name. Whitespace is normalized to `-`.
    #[arg(
        value_name = "NAME",
        required_unless_present = "auto",
        conflicts_with = "auto"
    )]
    pub name: Option<String>,
    /// Exact window target such as @42. Defaults to the current tmux pane's window.
    #[arg(long, value_name = "TARGET")]
    pub window: Option<String>,
    /// Restore tmux's dynamic process-based automatic window name.
    #[arg(long)]
    pub auto: bool,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct WorkListArgs {
    /// Limit work windows to one managed workspace.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct WorkShowArgs {
    /// Muxa Work id, for example TEST-0001.
    pub work: String,
    /// Workspace id when the same work id exists in more than one workspace.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct WorkCloseArgs {
    /// Muxa Work id, for example TEST-0001.
    pub work: String,
    /// Workspace id when the same work id exists in more than one workspace.
    #[arg(long)]
    pub workspace: Option<String>,
    /// Skip the confirmation prompt.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct WorkspaceListArgs {
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct WorkspaceShowArgs {
    /// Managed workspace/project id.
    pub workspace: String,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct WorkspaceCloseArgs {
    /// Managed workspace/project id.
    pub workspace: String,
    /// Skip the confirmation prompt.
    #[arg(long, short = 'y')]
    pub yes: bool,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

pub fn run_agent_control(args: AgentControlArgs) -> Result<()> {
    if args.action == AgentControlAction::Terminate
        && !confirm_destructive(
            args.yes,
            &format!("Terminate managed agent pane {}?", args.pane),
        )?
    {
        println!("cancelled");
        return Ok(());
    }
    let result = control_agent(&args.pane, args.action, args.yes)?;
    print_result(&result, args.json)
}

pub fn run_window_rename(args: WindowRenameArgs) -> Result<()> {
    let result = rename_window(args.window.as_deref(), args.name.as_deref(), args.auto)?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else if result.automatic {
        println!(
            "window {} ({}) now uses automatic name {:?}",
            result.window_id, result.session_name, result.name
        );
    } else {
        println!(
            "renamed window {} ({}) from {:?} to {:?}",
            result.window_id, result.session_name, result.previous_name, result.name
        );
    }
    Ok(())
}

pub fn run_work_list(args: WorkListArgs) -> Result<()> {
    let works = match args.workspace.as_deref() {
        Some(workspace) => find_workspace(workspace)?
            .map(|workspace| workspace.works)
            .unwrap_or_default(),
        None => list_works()?,
    };
    if args.json {
        println!("{}", serde_json::to_string_pretty(&json_works(&works))?);
    } else if works.is_empty() {
        println!("no muxa-managed work windows");
    } else {
        for work in works {
            println!(
                "{}  workspace={}  session={}  window={}  agents={}  cwd={}",
                work.work,
                work.workspace,
                work.session,
                work.window,
                work.agents.len(),
                work.cwd.display()
            );
        }
    }
    Ok(())
}

pub fn run_work_show(args: WorkShowArgs) -> Result<()> {
    let work = find_work_in(&args.work, args.workspace.as_deref())?
        .ok_or_else(|| anyhow::anyhow!("managed work {:?} not found", args.work))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&work)?);
    } else {
        println!(
            "{}  workspace={}  session={}  window={}  cwd={}",
            work.work,
            work.workspace,
            work.session,
            work.window,
            work.cwd.display()
        );
        for agent in &work.agents {
            println!(
                "  {}  {}{}{}{}",
                agent.pane,
                agent.agent,
                agent
                    .alias
                    .as_deref()
                    .map_or_else(String::new, |alias| format!(" alias={alias}")),
                agent
                    .role
                    .as_deref()
                    .map_or_else(String::new, |role| format!(" role={role}")),
                agent
                    .task
                    .as_deref()
                    .map_or_else(String::new, |task| format!(" task={task}"))
            );
        }
    }
    Ok(())
}

pub fn run_work_close(args: WorkCloseArgs) -> Result<()> {
    if !confirm_destructive(
        args.yes,
        &format!("Close work {} and all agent panes?", args.work),
    )? {
        println!("cancelled");
        return Ok(());
    }
    let result = close_work(&args.work, args.workspace.as_deref(), args.yes)?;
    print_result(&result, args.json)
}

pub fn run_workspace_list(args: WorkspaceListArgs) -> Result<()> {
    let workspaces = list_workspaces()?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json_workspaces(&workspaces))?
        );
    } else if workspaces.is_empty() {
        println!("no muxa-managed workspaces");
    } else {
        for workspace in workspaces {
            println!(
                "{}  session={}  works={}  cwd={}",
                workspace.workspace,
                workspace.session,
                workspace.works.len(),
                workspace.cwd.display()
            );
        }
    }
    Ok(())
}

pub fn run_workspace_show(args: WorkspaceShowArgs) -> Result<()> {
    let workspace = find_workspace(&args.workspace)?
        .ok_or_else(|| anyhow::anyhow!("managed workspace {:?} not found", args.workspace))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&workspace)?);
    } else {
        println!(
            "{}  session={}  works={}  cwd={}",
            workspace.workspace,
            workspace.session,
            workspace.works.len(),
            workspace.cwd.display()
        );
        for work in &workspace.works {
            println!(
                "  {}  window={}  agents={}  cwd={}",
                work.work,
                work.window,
                work.agents.len(),
                work.cwd.display()
            );
        }
    }
    Ok(())
}

pub fn run_workspace_close(args: WorkspaceCloseArgs) -> Result<()> {
    if !confirm_destructive(
        args.yes,
        &format!(
            "Close workspace {} and every work window and agent pane?",
            args.workspace
        ),
    )? {
        println!("cancelled");
        return Ok(());
    }
    let result = close_workspace(&args.workspace, args.yes)?;
    print_result(&result, args.json)
}

pub fn manage(request: ManageRequest) -> Result<ManageResult> {
    match request.action {
        ManageAction::ListWorkspace => Ok(ManageResult::Workspaces {
            workspaces: list_workspaces()?,
        }),
        ManageAction::ShowWorkspace => {
            let raw = required(
                request.workspace.as_deref(),
                "show_workspace requires workspace",
            )?;
            let workspace = find_workspace(raw)?
                .ok_or_else(|| anyhow::anyhow!("managed workspace {raw:?} not found"))?;
            Ok(ManageResult::Workspace { workspace })
        }
        ManageAction::ListWork => Ok(ManageResult::Works {
            works: match request.workspace.as_deref() {
                Some(workspace) => find_workspace(workspace)?
                    .map(|workspace| workspace.works)
                    .unwrap_or_default(),
                None => list_works()?,
            },
        }),
        ManageAction::ShowWork => {
            let raw = required(request.work.as_deref(), "show_work requires work")?;
            let work = find_work_in(raw, request.workspace.as_deref())?
                .ok_or_else(|| anyhow::anyhow!("managed work {raw:?} not found"))?;
            Ok(ManageResult::Work { work })
        }
        ManageAction::InterruptAgent => {
            let pane = required(request.pane.as_deref(), "interrupt_agent requires pane")?;
            control_agent(pane, AgentControlAction::Interrupt, true)
        }
        ManageAction::TerminateAgent => {
            let pane = required(request.pane.as_deref(), "terminate_agent requires pane")?;
            control_agent(pane, AgentControlAction::Terminate, request.confirm)
        }
        ManageAction::CloseWork => {
            let work = required(request.work.as_deref(), "close_work requires work")?;
            close_work(work, request.workspace.as_deref(), request.confirm)
        }
        ManageAction::CloseWorkspace => {
            let workspace = required(
                request.workspace.as_deref(),
                "close_workspace requires workspace",
            )?;
            close_workspace(workspace, request.confirm)
        }
    }
}

pub fn normalize_work_id(raw: &str) -> Result<String> {
    let work = raw.trim().to_ascii_uppercase();
    if work.is_empty() {
        bail!("work id cannot be empty");
    }
    if work.len() > 128 {
        bail!("work id is too long (max 128 bytes)");
    }
    if work
        .chars()
        .any(|ch| ch == '\t' || ch == '\n' || ch == '\r')
    {
        bail!("work id cannot contain tabs or newlines");
    }
    Ok(work)
}

pub fn normalize_workspace_id(raw: &str) -> Result<String> {
    let workspace = raw.trim().to_ascii_lowercase();
    if workspace.is_empty() {
        bail!("workspace id cannot be empty");
    }
    if workspace.len() > 128 {
        bail!("workspace id is too long (max 128 bytes)");
    }
    if workspace
        .chars()
        .any(|ch| ch == '\t' || ch == '\n' || ch == '\r')
    {
        bail!("workspace id cannot contain tabs or newlines");
    }
    Ok(workspace)
}

pub fn workspace_id_for_cwd(cwd: &Path) -> Result<String> {
    let name = cwd
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("workspace");
    normalize_workspace_id(name)
}

pub fn find_work_in(raw: &str, workspace: Option<&str>) -> Result<Option<WorkInfo>> {
    let wanted = normalize_work_id(raw)?;
    let workspace = workspace.map(normalize_workspace_id).transpose()?;
    let mut matches = list_works()?
        .into_iter()
        .filter(|work| {
            work.work == wanted
                && workspace
                    .as_deref()
                    .is_none_or(|workspace| work.workspace == workspace)
        })
        .collect::<Vec<_>>();
    if matches.len() > 1 {
        bail!("work {wanted} exists in multiple workspaces; specify --workspace");
    }
    Ok(matches.pop())
}

pub fn find_workspace(raw: &str) -> Result<Option<WorkspaceInfo>> {
    let wanted = normalize_workspace_id(raw)?;
    Ok(list_workspaces()?
        .into_iter()
        .find(|workspace| workspace.workspace == wanted))
}

pub fn list_works() -> Result<Vec<WorkInfo>> {
    Ok(list_workspaces()?
        .into_iter()
        .flat_map(|workspace| workspace.works)
        .collect())
}

pub fn list_workspaces() -> Result<Vec<WorkspaceInfo>> {
    let sessions = tmux_output_allow_no_server(&["list-sessions", "-F", SESSION_FORMAT])?;
    let windows = tmux_output_allow_no_server(&["list-windows", "-a", "-F", WINDOW_FORMAT])?;
    let panes = tmux_output_allow_no_server(&["list-panes", "-a", "-F", PANE_FORMAT])?;
    Ok(parse_workspaces(&sessions, &windows, &panes))
}

pub fn session_name_for_workspace(workspace: &str) -> Result<String> {
    let normalized = normalize_workspace_id(workspace)?;
    let base = sanitize_session_name(&normalized.to_ascii_lowercase());
    let existing = all_session_names()?;
    Ok(unique_name(base, |candidate| {
        existing.iter().any(|name| name == candidate)
    }))
}

pub fn window_name_for_work(work: &str) -> Result<String> {
    Ok(sanitize_window_name(&normalize_work_id(work)?))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WindowIdentity {
    window_id: String,
    session_id: String,
    session_name: String,
    name: String,
}

fn rename_window(
    target: Option<&str>,
    name: Option<&str>,
    automatic: bool,
) -> Result<WindowRenameResult> {
    let before = resolve_window_identity(target)?;
    if automatic {
        set_window_automatic_rename(&before.window_id, true)?;
    } else {
        let name = normalize_window_name(required(name, "window rename requires NAME or --auto")?)?;
        ensure_window_name_available(&before, &name)?;
        // Pin the mode explicitly. `rename-window` currently disables automatic
        // rename itself, but relying on that side effect makes the policy
        // sensitive to tmux behavior changes.
        set_window_automatic_rename(&before.window_id, false)?;
        tmux_status(&["rename-window", "-t", &before.window_id, &name])?;
    }
    let after = resolve_window_identity(Some(&before.window_id))?;
    Ok(WindowRenameResult {
        window_id: after.window_id,
        session_id: after.session_id,
        session_name: after.session_name,
        previous_name: before.name,
        name: after.name,
        automatic,
    })
}

fn resolve_window_identity(target: Option<&str>) -> Result<WindowIdentity> {
    let target = target
        .filter(|target| !target.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var("TMUX_PANE")
                .ok()
                .filter(|pane| !pane.trim().is_empty())
        })
        .ok_or_else(|| {
            anyhow::anyhow!("window target is required outside tmux; pass --window @N")
        })?;
    let output = tmux_output(&[
        "display-message",
        "-p",
        "-t",
        &target,
        WINDOW_IDENTITY_FORMAT,
    ])?;
    parse_window_identity(output.trim()).ok_or_else(|| {
        anyhow::anyhow!("tmux target {target:?} did not resolve to a complete window identity")
    })
}

fn parse_window_identity(line: &str) -> Option<WindowIdentity> {
    let mut fields = line.splitn(5, '\t');
    let window_id = fields.next()?.trim();
    let session_id = fields.next()?.trim();
    let session_name = fields.next()?;
    let name = fields.next()?;
    // Consume the mode field as part of validating the complete format even
    // though the caller already knows which mode it requested.
    fields.next()?;
    if !window_id.starts_with('@') || !session_id.starts_with('$') || session_name.is_empty() {
        return None;
    }
    Some(WindowIdentity {
        window_id: window_id.into(),
        session_id: session_id.into(),
        session_name: session_name.into(),
        name: name.into(),
    })
}

fn ensure_window_name_available(window: &WindowIdentity, name: &str) -> Result<()> {
    let rows = tmux_output(&[
        "list-windows",
        "-t",
        &window.session_id,
        "-F",
        "#{window_id}\t#{window_name}",
    ])?;
    if rows.lines().any(|line| {
        line.split_once('\t')
            .is_some_and(|(window_id, current)| window_id != window.window_id && current == name)
    }) {
        bail!(
            "window name {name:?} already exists in session {} ({})",
            window.session_name,
            window.session_id
        );
    }
    Ok(())
}

fn set_window_automatic_rename(window: &str, enabled: bool) -> Result<()> {
    set_option(
        OptionScope::Window,
        window,
        "automatic-rename",
        if enabled { "on" } else { "off" },
    )
}

pub fn mark_workspace(session: &str, workspace: &str, cwd: &Path) -> Result<()> {
    let workspace = normalize_workspace_id(workspace)?;
    let cwd = cwd
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("workspace cwd is not valid UTF-8: {}", cwd.display()))?;
    set_option(
        OptionScope::Session,
        session,
        WORKSPACE_ID_OPTION,
        &workspace,
    )?;
    set_option(OptionScope::Session, session, WORKSPACE_CWD_OPTION, cwd)?;
    set_option(OptionScope::Session, session, MANAGED_WORKSPACE_OPTION, "1")?;
    Ok(())
}

pub fn mark_work(window: &str, workspace: &str, work: &str, cwd: &Path) -> Result<()> {
    let workspace = normalize_workspace_id(workspace)?;
    let work = normalize_work_id(work)?;
    let cwd = cwd
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("work cwd is not valid UTF-8: {}", cwd.display()))?;
    set_option(OptionScope::Window, window, WORK_ID_OPTION, &work)?;
    set_option(OptionScope::Window, window, WORK_CWD_OPTION, cwd)?;
    set_option(OptionScope::Window, window, MANAGED_WORK_OPTION, "1")?;
    set_option(OptionScope::Window, window, WORKSPACE_ID_OPTION, &workspace)?;
    set_window_automatic_rename(window, false)?;
    Ok(())
}

/// Attach a provider-neutral external issue snapshot to a managed work
/// window. These options let muxad discover the association and persist it in
/// the durable work store without teaching muxad a Linear/GitHub/Jira client.
pub fn mark_work_external(window: &str, ticket: &muxa::pipeline::Ticket) -> Result<()> {
    let Some(source) = ticket.source.as_deref() else {
        return Ok(());
    };
    set_option(
        OptionScope::Window,
        window,
        EXTERNAL_SOURCE_OPTION,
        &metadata(source, 64)?,
    )?;
    set_option(
        OptionScope::Window,
        window,
        EXTERNAL_KEY_OPTION,
        &metadata(&ticket.id, 128)?,
    )?;
    for (key, value, max) in [
        (EXTERNAL_SCOPE_OPTION, ticket.scope.as_deref(), 256),
        (EXTERNAL_STABLE_ID_OPTION, ticket.stable_id.as_deref(), 256),
        (EXTERNAL_TITLE_OPTION, ticket.title.as_deref(), 512),
        (EXTERNAL_URL_OPTION, ticket.url.as_deref(), 2_048),
        (EXTERNAL_STATUS_OPTION, ticket.state.as_deref(), 128),
    ] {
        if let Some(value) = value.filter(|value| !value.trim().is_empty()) {
            set_option(
                OptionScope::Window,
                window,
                key,
                &external_metadata(value, max),
            )?;
        }
    }
    Ok(())
}

pub fn mark_agent(
    pane: &str,
    agent: &str,
    workspace: Option<&str>,
    work: Option<&str>,
    role: Option<&str>,
    task: Option<&str>,
    alias: Option<&str>,
) -> Result<()> {
    validate_pane_id(pane)?;
    set_option(OptionScope::Pane, pane, AGENT_OPTION, &metadata(agent, 64)?)?;
    set_option(OptionScope::Pane, pane, MANAGED_AGENT_OPTION, "1")?;
    if let Some(workspace) = workspace {
        set_option(
            OptionScope::Pane,
            pane,
            PANE_WORKSPACE_OPTION,
            &normalize_workspace_id(workspace)?,
        )?;
    }
    if let Some(work) = work {
        set_option(
            OptionScope::Pane,
            pane,
            PANE_WORK_OPTION,
            &normalize_work_id(work)?,
        )?;
    }
    if let Some(role) = role.filter(|value| !value.trim().is_empty()) {
        set_option(
            OptionScope::Pane,
            pane,
            AGENT_ROLE_OPTION,
            &metadata(role, 64)?,
        )?;
    }
    if let Some(task) = task.filter(|value| !value.trim().is_empty()) {
        set_option(
            OptionScope::Pane,
            pane,
            AGENT_TASK_OPTION,
            &metadata(task, 256)?,
        )?;
    }
    if let Some(alias) = alias.filter(|value| !value.trim().is_empty()) {
        set_option(
            OptionScope::Pane,
            pane,
            AGENT_ALIAS_OPTION,
            &metadata(alias, 64)?,
        )?;
    }
    Ok(())
}

pub fn window_id_for_pane(pane: &str) -> Result<String> {
    validate_pane_id(pane)?;
    let window = tmux_output(&["display-message", "-p", "-t", pane, "#{window_id}"])?;
    let window = window.trim();
    if !window.starts_with('@') {
        bail!("pane {pane} resolved to an invalid window id {window:?}");
    }
    Ok(window.to_string())
}

pub fn session_name_for_pane(pane: &str) -> Result<String> {
    validate_pane_id(pane)?;
    let session = tmux_output(&["display-message", "-p", "-t", pane, "#{session_name}"])?;
    let session = session.trim();
    if session.is_empty() {
        bail!("pane {pane} resolved to an empty session name");
    }
    Ok(session.to_string())
}

pub fn cleanup_pane(pane: &str) {
    if validate_pane_id(pane).is_ok() {
        let _ = muxa::tmux::tmux_command_scoped()
            .args(["kill-pane", "-t", pane])
            .status();
    }
}

fn control_agent(pane: &str, action: AgentControlAction, confirm: bool) -> Result<ManageResult> {
    validate_pane_id(pane)?;
    if action == AgentControlAction::Terminate && !confirm {
        bail!("terminate_agent requires confirm=true");
    }
    ensure_managed_agent(pane)?;
    let args = match action {
        AgentControlAction::Interrupt => vec!["send-keys", "-t", pane, "C-c"],
        AgentControlAction::Terminate => vec!["kill-pane", "-t", pane],
    };
    tmux_status(&args)?;
    Ok(ManageResult::AgentControl {
        action,
        pane: pane.to_string(),
    })
}

fn close_work(raw: &str, workspace: Option<&str>, confirm: bool) -> Result<ManageResult> {
    if !confirm {
        bail!("close_work requires confirm=true");
    }
    let work = find_work_in(raw, workspace)?
        .ok_or_else(|| anyhow::anyhow!("managed work {raw:?} not found"))?;
    tmux_status(&["kill-window", "-t", &work.window])?;
    Ok(ManageResult::WorkClosed {
        work: work.work,
        workspace: work.workspace,
        session: work.session,
        window: work.window,
    })
}

fn close_workspace(raw: &str, confirm: bool) -> Result<ManageResult> {
    if !confirm {
        bail!("close_workspace requires confirm=true");
    }
    let workspace = find_workspace(raw)?
        .ok_or_else(|| anyhow::anyhow!("managed workspace {raw:?} not found"))?;
    tmux_status(&["kill-session", "-t", &format!("={}", workspace.session)])?;
    Ok(ManageResult::WorkspaceClosed {
        workspace: workspace.workspace,
        session: workspace.session,
    })
}

fn ensure_managed_agent(pane: &str) -> Result<()> {
    let output = tmux_output(&[
        "display-message",
        "-p",
        "-t",
        pane,
        "#{@muxa_managed_agent}\t#{@muxa_agent}",
    ])?;
    let mut fields = output.trim().split('\t');
    if fields.next() != Some("1") || fields.next().is_none_or(str::is_empty) {
        bail!("pane {pane} is not a muxa-managed agent pane");
    }
    Ok(())
}

fn validate_pane_id(pane: &str) -> Result<()> {
    if pane
        .strip_prefix('%')
        .is_some_and(|digits| !digits.is_empty() && digits.chars().all(|ch| ch.is_ascii_digit()))
    {
        Ok(())
    } else {
        bail!("pane must be an exact tmux pane id such as %42")
    }
}

fn metadata(raw: &str, max: usize) -> Result<String> {
    let value = raw.trim();
    if value.is_empty() {
        bail!("metadata value cannot be empty");
    }
    if value.len() > max {
        bail!("metadata value is too long (max {max} bytes)");
    }
    if value
        .chars()
        .any(|ch| ch == '\t' || ch == '\n' || ch == '\r')
    {
        bail!("metadata value cannot contain tabs or newlines");
    }
    Ok(value.to_string())
}

fn external_metadata(raw: &str, max: usize) -> String {
    let normalized = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut end = normalized.len().min(max);
    while !normalized.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    normalized[..end].to_string()
}

fn parse_workspaces(sessions: &str, windows: &str, panes: &str) -> Vec<WorkspaceInfo> {
    let mut workspaces = Vec::new();
    for line in sessions.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 7 || fields[2].trim().is_empty() || fields[4] != "1" {
            continue;
        }
        let session = fields[0].to_string();
        let session_id = fields[1];
        let workspace = fields[2].trim().to_ascii_lowercase();
        let mut works = windows
            .lines()
            .filter_map(|line| parse_work_window(line, session_id, &workspace, panes))
            .collect::<Vec<_>>();
        works.sort_by(|left, right| left.work.cmp(&right.work));
        workspaces.push(WorkspaceInfo {
            workspace,
            session,
            cwd: PathBuf::from(fields[3]),
            attached_clients: fields[5].parse().unwrap_or(0),
            windows: fields[6].parse().unwrap_or(0),
            works,
        });
    }
    workspaces.sort_by(|left, right| left.workspace.cmp(&right.workspace));
    workspaces
}

fn parse_work_window(
    line: &str,
    session_id: &str,
    workspace: &str,
    panes: &str,
) -> Option<WorkInfo> {
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() < 8
        || fields[1] != session_id
        || fields[5].trim().is_empty()
        || fields[7] != "1"
    {
        return None;
    }
    let work = fields[5].trim().to_ascii_uppercase();
    let window = fields[2].to_string();
    let agents = panes
        .lines()
        .filter_map(|line| parse_agent_pane(line, &window, workspace, &work))
        .collect();
    Some(WorkInfo {
        work,
        workspace: workspace.to_string(),
        session: fields[0].to_string(),
        window,
        window_index: fields[3].parse().unwrap_or(0),
        window_name: fields[4].to_string(),
        cwd: PathBuf::from(fields[6]),
        external_item: parse_external_item(&fields),
        agents,
    })
}

fn parse_external_item(fields: &[&str]) -> Option<Box<ExternalItemInfo>> {
    let source = fields.get(8)?.trim();
    let display_key = fields.get(11).map_or("", |value| value.trim());
    if source.is_empty() || display_key.is_empty() {
        return None;
    }
    Some(Box::new(ExternalItemInfo {
        source: source.into(),
        scope: optional_field(fields, 9),
        stable_id: optional_field(fields, 10),
        display_key: display_key.into(),
        title: optional_field(fields, 12),
        url: optional_field(fields, 13),
        status: optional_field(fields, 14),
    }))
}

fn optional_field(fields: &[&str], index: usize) -> Option<String> {
    fields
        .get(index)
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn parse_agent_pane(
    line: &str,
    window: &str,
    workspace: &str,
    work: &str,
) -> Option<ManagedAgentPane> {
    let fields: Vec<&str> = line.split('\t').collect();
    if fields.len() < 12
        || fields[1] != window
        || fields[3].trim().is_empty()
        || fields[8] != "1"
        || !fields[9].eq_ignore_ascii_case(workspace)
        || !fields[10].eq_ignore_ascii_case(work)
    {
        return None;
    }
    Some(ManagedAgentPane {
        pane: fields[2].to_string(),
        agent: fields[3].to_string(),
        alias: option(fields[11]),
        role: option(fields[4]),
        task: option(fields[5]),
        command: fields[6].to_string(),
        cwd: PathBuf::from(fields[7]),
    })
}

fn option(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.to_string())
}

fn all_session_names() -> Result<Vec<String>> {
    Ok(
        tmux_output_allow_no_server(&["list-sessions", "-F", "#{session_name}"])?
            .lines()
            .map(str::to_string)
            .collect(),
    )
}

fn sanitize_session_name(name: &str) -> String {
    let mut cleaned = String::new();
    for ch in name.trim().chars() {
        let ch = if ch == '.' || ch == ':' || ch.is_whitespace() {
            '-'
        } else {
            ch
        };
        if ch != '-' || !cleaned.ends_with('-') {
            cleaned.push(ch);
        }
    }
    let cleaned = cleaned.trim_matches('-').to_string();
    if cleaned.is_empty() {
        "workspace".into()
    } else {
        cleaned
    }
}

fn sanitize_window_name(name: &str) -> String {
    sanitize_session_name(name)
}

fn normalize_window_name(raw: &str) -> Result<String> {
    let value = metadata(raw, 64)?;
    if value.chars().any(char::is_control) {
        bail!("window name cannot contain control characters");
    }
    let mut normalized = String::new();
    for ch in value.chars() {
        if ch.is_whitespace() {
            if !normalized.ends_with('-') {
                normalized.push('-');
            }
        } else {
            normalized.push(ch);
        }
    }
    let normalized = normalized.trim_matches('-').to_string();
    if normalized.is_empty() {
        bail!("window name cannot be empty");
    }
    if normalized.len() > 64 {
        bail!("window name is too long after normalization (max 64 bytes)");
    }
    Ok(normalized)
}

fn unique_name(base: String, exists: impl Fn(&str) -> bool) -> String {
    if !exists(&base) {
        return base;
    }
    (2..10_000)
        .map(|suffix| format!("{base}-{suffix}"))
        .find(|candidate| !exists(candidate))
        .unwrap_or_else(|| format!("{base}-overflow"))
}

#[derive(Debug, Clone, Copy)]
enum OptionScope {
    Session,
    Window,
    Pane,
}

fn set_option(scope: OptionScope, target: &str, key: &str, value: &str) -> Result<()> {
    let mut args = vec!["set-option"];
    match scope {
        OptionScope::Session => {}
        OptionScope::Window => args.push("-w"),
        OptionScope::Pane => args.push("-p"),
    }
    args.extend(["-t", target, key, value]);
    tmux_status(&args)
}

fn tmux_status(args: &[&str]) -> Result<()> {
    let output = muxa::tmux::tmux_command_scoped()
        .args(args)
        .output()
        .with_context(|| format!("run tmux {}", args.first().unwrap_or(&"command")))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    bail!(
        "tmux {} failed{}",
        args.first().unwrap_or(&"command"),
        if stderr.is_empty() {
            String::new()
        } else {
            format!(": {stderr}")
        }
    )
}

fn tmux_output(args: &[&str]) -> Result<String> {
    let output = muxa::tmux::tmux_command_scoped()
        .args(args)
        .output()
        .with_context(|| format!("run tmux {}", args.first().unwrap_or(&"command")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        bail!(
            "tmux {} failed{}",
            args.first().unwrap_or(&"command"),
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn tmux_output_allow_no_server(args: &[&str]) -> Result<String> {
    match tmux_output(args) {
        Ok(output) => Ok(output),
        Err(error)
            if error.to_string().contains("no server running")
                || error.to_string().contains("no sessions") =>
        {
            Ok(String::new())
        }
        Err(error) => Err(error),
    }
}

fn confirm_destructive(yes: bool, prompt: &str) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("confirmation requires an interactive terminal; pass --yes");
    }
    Ok(cliclack::confirm(prompt).initial_value(false).interact()?)
}

fn required<'a>(value: Option<&'a str>, message: &str) -> Result<&'a str> {
    value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!(message.to_string()))
}

fn json_works(works: &[WorkInfo]) -> serde_json::Value {
    serde_json::json!({ "works": works })
}

fn json_workspaces(workspaces: &[WorkspaceInfo]) -> serde_json::Value {
    serde_json::json!({ "workspaces": workspaces })
}

fn print_result(result: &ManageResult, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(result)?);
    } else {
        match result {
            ManageResult::AgentControl { action, pane } => {
                println!("{action:?} agent pane {pane}");
            }
            ManageResult::WorkClosed {
                work,
                workspace,
                session,
                window,
            } => {
                println!(
                    "closed work {work} (workspace {workspace}, session {session}, window {window})"
                );
            }
            ManageResult::WorkspaceClosed { workspace, session } => {
                println!("closed workspace {workspace} (session {session})");
            }
            ManageResult::Workspaces { .. }
            | ManageResult::Workspace { .. }
            | ManageResult::Works { .. }
            | ManageResult::Work { .. } => {
                println!("{}", serde_json::to_string_pretty(result)?);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_and_work_ids_are_normalized_and_tmux_safe() {
        assert_eq!(normalize_workspace_id(" Muxa ").unwrap(), "muxa");
        assert_eq!(normalize_work_id(" test-0001 ").unwrap(), "TEST-0001");
        assert!(normalize_work_id("bad\nid").is_err());
        assert_eq!(
            window_name_for_work("TEST.0001: Review").unwrap(),
            "TEST-0001-REVIEW"
        );
    }

    #[test]
    fn explicit_window_names_preserve_case_and_normalize_whitespace() {
        assert_eq!(
            normalize_window_name("  CAL-7175  auth refactor  ").unwrap(),
            "CAL-7175-auth-refactor"
        );
        assert_eq!(
            normalize_window_name("topology-watch").unwrap(),
            "topology-watch"
        );
        assert!(normalize_window_name("\n").is_err());
    }

    #[test]
    fn window_identity_parser_requires_stable_native_ids() {
        assert_eq!(
            parse_window_identity("@42\t$7\tmuxa\tCAL-7175\t0"),
            Some(WindowIdentity {
                window_id: "@42".into(),
                session_id: "$7".into(),
                session_name: "muxa".into(),
                name: "CAL-7175".into(),
            })
        );
        assert!(parse_window_identity("CAL-7175\t$7\tmuxa\tname\t0").is_none());
        assert!(parse_window_identity("@42\tmuxa\tmuxa\tname\t0").is_none());
    }

    #[test]
    fn parser_keeps_only_managed_workspace_work_and_agent_hierarchy() {
        let sessions = "muxa\t$1\tmuxa\t/repo\t1\t1\t2\n\
                        legacy\t$2\t\t/tmp\t\t0\t1\n";
        let windows = "muxa\t$1\t@1\t0\ttest-0001\tTEST-0001\t/repo/wt\t1\tlinear\tCAL\tstable-1\tCAL-7093\tDashboard work\thttps://linear.app/CAL-7093\tstarted\n\
                       muxa\t$1\t@2\t1\tplain\t\t/repo\t\n\
                       legacy\t$2\t@3\t0\tlegacy\tOLD-1\t/tmp\t1\n";
        // Trailing column is the pipeline alias: set on %1, empty on the
        // hand-started %4 that `muxa work up` would report as unclaimed.
        let panes = "muxa\t@1\t%1\tcodex\timplementer\tmain\tcodex\t/repo/wt\t1\tmuxa\tTEST-0001\timpl\n\
                     muxa\t@1\t%2\tcodex\treviewer\twrong\tcodex\t/repo/wt\t1\tmuxa\tTEST-9999\t\n\
                     muxa\t@1\t%3\tcodex\treviewer\tunmanaged\tcodex\t/repo/wt\t\tmuxa\tTEST-0001\t\n\
                     muxa\t@1\t%4\tclaude\t\t\tclaude\t/repo/wt\t1\tmuxa\tTEST-0001\t\n";
        let workspaces = parse_workspaces(sessions, windows, panes);
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0].workspace, "muxa");
        assert_eq!(workspaces[0].works.len(), 1);
        assert_eq!(workspaces[0].works[0].work, "TEST-0001");
        assert_eq!(workspaces[0].works[0].window, "@1");
        assert_eq!(
            workspaces[0].works[0]
                .external_item
                .as_ref()
                .map(|item| (item.source.as_str(), item.display_key.as_str())),
            Some(("linear", "CAL-7093"))
        );
        assert_eq!(workspaces[0].works[0].agents.len(), 2);
        assert_eq!(workspaces[0].works[0].agents[0].pane, "%1");
        assert_eq!(
            workspaces[0].works[0].agents[0].alias.as_deref(),
            Some("impl")
        );
        // A managed pane nobody aliased still parses; it simply has no key
        // for the pipeline diff to claim it by.
        assert_eq!(workspaces[0].works[0].agents[1].pane, "%4");
        assert!(workspaces[0].works[0].agents[1].alias.is_none());
    }

    #[test]
    fn destructive_management_requires_explicit_confirmation() {
        let request = ManageRequest {
            action: ManageAction::TerminateAgent,
            pane: Some("%42".into()),
            workspace: None,
            work: None,
            confirm: false,
        };
        assert!(manage(request)
            .unwrap_err()
            .to_string()
            .contains("confirm=true"));
        assert_eq!(
            ManageAction::parse("close_work").unwrap(),
            ManageAction::CloseWork
        );
    }

    #[test]
    fn exact_pane_ids_are_required() {
        assert!(validate_pane_id("%42").is_ok());
        assert!(validate_pane_id("42").is_err());
        assert!(validate_pane_id("%4x").is_err());
    }
}
