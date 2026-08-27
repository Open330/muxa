//! Managed tmux workspace/session, work/window, and agent/pane lifecycle.
//!
//! Muxa's tmux policy is deliberately narrow:
//! - one managed session represents one workspace or project;
//! - one managed window binds the current run of a muxa Work;
//! - one managed pane represents one coding agent;
//!
//! Identity is stored in tmux user options so it survives muxad and MCP
//! process restarts without adding another database.

use crate::theme::TableTone;
use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use muxa::ipc::Client;
use muxa::work::{WorkRecord, WorkStage};
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
const AGENT_GENERATION_OPTION: &str = "@muxa_pipeline_generation";
const PANE_WORKSPACE_OPTION: &str = "@muxa_agent_workspace_id";
const PANE_WORK_OPTION: &str = "@muxa_agent_work_id";
const EXTERNAL_SOURCE_OPTION: &str = "@muxa_external_source";
const EXTERNAL_SCOPE_OPTION: &str = "@muxa_external_scope";
const EXTERNAL_STABLE_ID_OPTION: &str = "@muxa_external_stable_id";
/// Comma-separated pipeline aliases that have reported finishing.
///
/// On the window rather than the pane: an agent that has finished may well
/// have exited, and the fact that it finished has to outlive it. This is
/// the only completion signal muxa has — agent state cannot supply one,
/// because `idle` means both "done" and "paused mid-thought".
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
    /// tmux's stable `$N`, retained as this Run's physical coordinate.
    pub session_id: String,
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
    /// Whether muxa created this session, as opposed to adopting one the
    /// operator already had.
    ///
    /// Identity and ownership are different facts. A session muxa adopted
    /// carries a workspace id so its work windows are findable, but muxa
    /// must never close it: it is full of windows muxa did not open.
    pub managed: bool,
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
    SessionControl {
        action: AgentControlAction,
        session: String,
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
    /// Exact managed tmux pane id, for example %42. Conflicts with --session.
    #[arg(long, required_unless_present = "session", conflicts_with = "session")]
    pub pane: Option<String>,
    /// Muxa-owned native PTY session id or display name. Conflicts with --pane.
    #[arg(long, required_unless_present = "pane", conflicts_with = "pane")]
    pub session: Option<String>,
    /// Interrupt the current turn or terminate the whole pane/session.
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
        required_unless_present_any = ["auto", "buffer"],
        conflicts_with_all = ["auto", "buffer"]
    )]
    pub name: Option<String>,
    /// Read the name from a tmux paste buffer and delete it. Used by the
    /// generated `prefix + ,` binding so prompt text never enters a shell.
    #[arg(
        long,
        value_name = "BUFFER",
        hide = true,
        conflicts_with_all = ["name", "auto"]
    )]
    pub buffer: Option<String>,
    /// Exact window target such as @42. Defaults to the current tmux pane's window.
    #[arg(long, value_name = "TARGET")]
    pub window: Option<String>,
    /// Restore tmux's dynamic process-based automatic window name.
    #[arg(long, conflicts_with = "buffer")]
    pub auto: bool,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, clap::Args)]
pub struct WorkDoneArgs {
    /// Pipeline alias reporting done. Defaults to the alias recorded on
    /// the calling pane, so an agent can just run `muxa work done`.
    #[arg(long)]
    pub alias: Option<String>,
    /// tmux pane to read the alias and work from. Defaults to `TMUX_PANE`.
    #[arg(long)]
    pub pane: Option<String>,
    /// Run generation being completed. Defaults to the generation stamped on
    /// the calling pane. Useful when automation supplies an explicit pane.
    #[arg(long)]
    pub generation: Option<u64>,
    /// Restart this alias's completion generation and hold/reconcile its
    /// downstream aliases again.
    #[arg(long)]
    pub undo: bool,
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

#[derive(Debug, clap::Args)]
pub struct WorkspaceViewArgs {
    /// Session to view. Defaults to the session the calling client is in.
    #[arg(long)]
    pub session: Option<String>,
    /// tmux client to move, e.g. `/dev/pts/71`. Defaults to the calling one.
    #[arg(long)]
    pub client: Option<String>,
    /// Suffix that names the view. Defaults to the client's pid, which keeps
    /// one terminal to one view instead of a new session per jump.
    #[arg(long = "client-pid")]
    pub client_pid: Option<String>,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

/// Name for a client's private view of `session`.
///
/// `<session>~view~<suffix>` sorts next to the session it mirrors and reads as
/// what it is. Safe despite tmux matching session names by prefix, because an
/// exact match wins over prefix candidates — measured on tmux 3.4 with
/// `callabo`, `callabo-set` and `callabo~view~1734560` all present, `-t
/// callabo` resolves to `callabo`.
fn view_session_name(session: &str, suffix: &str) -> String {
    format!("{session}~view~{suffix}")
}

const VIEW_SESSION_FORMAT: &str =
    "#{session_id}\t#{session_name}\t#{session_attached}\t#{session_group}\t#{destroy-unattached}";

#[derive(Debug, Clone, PartialEq, Eq)]
struct ViewSession {
    id: String,
    name: String,
    attached: u32,
    group: Option<String>,
    destroy_unattached: bool,
}

fn parse_view_session(line: &str) -> Result<ViewSession> {
    let mut fields = line.trim_end_matches(['\r', '\n']).splitn(5, '\t');
    let id = fields.next().unwrap_or_default().to_string();
    let name = fields.next().unwrap_or_default().to_string();
    let attached = fields
        .next()
        .ok_or_else(|| anyhow::anyhow!("tmux did not report session_attached"))?
        .parse::<u32>()
        .context("tmux reported an invalid session_attached value")?;
    let group = fields
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let destroy_unattached = match fields.next().unwrap_or_default().trim() {
        "1" | "on" => true,
        "0" | "off" => false,
        value => bail!("tmux reported an invalid destroy-unattached value {value:?}"),
    };
    if id.is_empty() || name.is_empty() {
        bail!("tmux did not report a session id and name");
    }
    Ok(ViewSession {
        id,
        name,
        attached,
        group,
        destroy_unattached,
    })
}

fn list_view_sessions() -> Result<Vec<ViewSession>> {
    tmux_output(&["list-sessions", "-F", VIEW_SESSION_FORMAT])?
        .lines()
        .filter(|line| !line.is_empty())
        .map(parse_view_session)
        .collect()
}

fn resolve_view_session(target: &str) -> Result<ViewSession> {
    parse_view_session(&tmux_output(&[
        "display-message",
        "-p",
        "-t",
        target,
        VIEW_SESSION_FORMAT,
    ])?)
}

fn tmux_session_id_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let sequence = |id: &str| id.strip_prefix('$').and_then(|raw| raw.parse::<u64>().ok());
    match (sequence(left), sequence(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => left.cmp(right),
    }
}

/// Pick the same representative as topology folding. `session_group` keeps
/// the original name after `rename-session`, so the only durable ordering is
/// tmux's monotonically allocated numeric session id.
fn canonical_view_source(target: &ViewSession, sessions: &[ViewSession]) -> ViewSession {
    let Some(group) = target.group.as_deref() else {
        return target.clone();
    };
    sessions
        .iter()
        .filter(|session| session.group.as_deref() == Some(group))
        .min_by(|left, right| tmux_session_id_cmp(&left.id, &right.id))
        .cloned()
        .unwrap_or_else(|| target.clone())
}

fn expected_view_group(source: &ViewSession) -> &str {
    source.group.as_deref().unwrap_or(&source.name)
}

/// A renamed original session should lend its new name to future views. If
/// that original is gone and the oldest survivor is itself a generated view,
/// remove its final view suffix instead of producing
/// `base~view~1~view~2` on every regroup. The session-group name cannot be
/// used as the base because tmux does not update it after `rename-session`.
fn view_name_base(source: &ViewSession) -> &str {
    let differs_from_group = source
        .group
        .as_deref()
        .is_some_and(|group| group != source.name);
    if source.destroy_unattached || differs_from_group {
        if let Some((base, suffix)) = source.name.rsplit_once("~view~") {
            if !base.is_empty() && !suffix.is_empty() {
                return base;
            }
        }
    }
    &source.name
}

/// Reuse only a view created but not yet occupied by a racing invocation, or
/// the view the same client has already entered. An attached session owned by
/// another client is a name collision, not this terminal's private view.
fn reusable_view<'a>(
    sessions: &'a [ViewSession],
    name: &str,
    source: &ViewSession,
    client_session_id: &str,
) -> Option<&'a ViewSession> {
    sessions.iter().find(|session| {
        session.name == name
            && session.group.as_deref() == Some(expected_view_group(source))
            && (session.attached == 0 || session.id == client_session_id)
    })
}

struct PreparedView {
    id: String,
    name: String,
    created: bool,
}

fn prepare_view(
    client: &str,
    source: &ViewSession,
    suffix: &str,
    sessions: &[ViewSession],
) -> Result<PreparedView> {
    let base_name = view_session_name(view_name_base(source), suffix);
    let current_id = resolve_view_session(client)?.id;
    let existing = reusable_view(sessions, &base_name, source, &current_id);
    let mut name = base_name;
    if existing.is_none() && sessions.iter().any(|session| session.name == name) {
        name = unique_name(name, |candidate| {
            sessions.iter().any(|session| session.name == candidate)
        });
    }
    if let Some(existing) = existing {
        return Ok(PreparedView {
            id: existing.id.clone(),
            name,
            created: false,
        });
    }

    let create = tmux_output(&[
        "new-session",
        "-dP",
        "-F",
        "#{session_id}",
        "-t",
        &source.id,
        "-s",
        &name,
    ]);
    match create {
        Ok(id) => Ok(PreparedView {
            id: id.trim().to_string(),
            name,
            created: true,
        }),
        Err(create_error) => {
            // Another hook invocation may have won the create between our
            // listing and `new-session`. Reuse only its unattached view or the
            // session this same client already entered.
            let refreshed = list_view_sessions()?;
            let refreshed_client_id = resolve_view_session(client)?.id;
            let Some(existing) = reusable_view(&refreshed, &name, source, &refreshed_client_id)
            else {
                return Err(create_error);
            };
            Ok(PreparedView {
                id: existing.id.clone(),
                name,
                created: false,
            })
        }
    }
}

fn activate_view(client: &str, original_session_id: &str, view: &PreparedView) -> Result<()> {
    // Move the client in BEFORE `destroy-unattached`. Setting that option on a
    // session that still has no client makes tmux reap it on the spot.
    if let Err(error) = tmux_status(&["switch-client", "-c", client, "-t", &view.id]) {
        if view.created {
            let _ = tmux_status(&["kill-session", "-t", &view.id]);
        }
        return Err(error);
    }
    if let Err(error) = tmux_status(&["set-option", "-t", &view.id, "destroy-unattached", "on"]) {
        if let Err(rollback) =
            tmux_status(&["switch-client", "-c", client, "-t", original_session_id])
        {
            bail!("{error}; could not return client to {original_session_id}: {rollback}");
        }
        if view.created {
            let _ = tmux_status(&["kill-session", "-t", &view.id]);
        }
        return Err(error);
    }
    Ok(())
}

/// Give one tmux client its own view of a session, so two terminals on one
/// workspace stop following each other's window switches.
///
/// A tmux session has a single current window shared by every client attached
/// to it. A *session group* is the only thing that separates them: the window
/// list stays shared, but each session in the group keeps its own current
/// window. This puts the client into one.
///
/// A no-op when the client is the session's only one — a lone terminal needs
/// no view, and creating one anyway would leave a second session in every
/// listing for nothing.
pub fn run_workspace_view(args: WorkspaceViewArgs) -> Result<()> {
    let client = match args.client.clone() {
        Some(client) => client,
        None => tmux_output(&["display-message", "-p", "#{client_name}"])?
            .trim()
            .to_string(),
    };
    if client.is_empty() {
        bail!("no tmux client to move; pass --client");
    }
    let client_session = resolve_view_session(&client)
        .with_context(|| format!("resolve the current session for client {client:?}"))?;
    let target = match args.session.as_deref() {
        Some(session) => resolve_view_session(session)
            .with_context(|| format!("resolve requested session {session:?}"))?,
        None => client_session.clone(),
    };
    let other_clients = client_session.attached.saturating_sub(1);
    if other_clients == 0 {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "client": client,
                    "session": target.name,
                    "view": serde_json::Value::Null,
                    "reason": "sole client",
                }))?
            );
        }
        return Ok(());
    }

    let suffix = match args.client_pid.clone() {
        Some(suffix) => suffix,
        None => tmux_output(&["display-message", "-p", "-t", &client, "#{client_pid}"])?
            .trim()
            .to_string(),
    };
    let suffix = if suffix.is_empty() {
        std::process::id().to_string()
    } else {
        suffix
    };
    let sessions = list_view_sessions()?;
    let source = canonical_view_source(&target, &sessions);
    let view = prepare_view(&client, &source, &suffix, &sessions)?;
    activate_view(&client, &client_session.id, &view)?;

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "client": client,
                "session": source.name,
                "view": view.name,
                "view_id": view.id,
            }))?
        );
    } else {
        println!("client {client} now views {} as {}", source.name, view.name);
    }
    Ok(())
}

pub async fn run_agent_control(args: AgentControlArgs, client: &Client) -> Result<()> {
    let result = if let Some(session) = args.session.as_deref() {
        let sessions = client
            .list_sessions()
            .await
            .context("listing native muxa sessions")?;
        let session_id = sessions
            .iter()
            .find(|candidate| {
                candidate.id == session || candidate.display_name.as_deref() == Some(session)
            })
            .map_or_else(|| session.to_string(), |candidate| candidate.id.clone());
        if args.action == AgentControlAction::Terminate
            && !confirm_destructive(
                args.yes,
                &format!("Terminate native agent session {session_id}?"),
            )?
        {
            println!("cancelled");
            return Ok(());
        }
        match args.action {
            AgentControlAction::Interrupt => client
                .write_session(&session_id, "\u{3}")
                .await
                .context("interrupting native agent session")?,
            AgentControlAction::Terminate => client
                .terminate_session(&session_id)
                .await
                .context("terminating native agent session")?,
        }
        ManageResult::SessionControl {
            action: args.action,
            session: session_id,
        }
    } else {
        let pane = args.pane.as_deref().expect("clap requires pane or session");
        if args.action == AgentControlAction::Terminate
            && !confirm_destructive(args.yes, &format!("Terminate managed agent pane {pane}?"))?
        {
            println!("cancelled");
            return Ok(());
        }
        control_agent(pane, args.action, args.yes)?
    };
    print_result(&result, args.json)
}

pub fn run_window_rename(args: WindowRenameArgs) -> Result<()> {
    let name = match args.buffer.as_deref() {
        Some(buffer) => Some(take_window_name_buffer(buffer)?),
        None => args.name.clone(),
    };
    let result = rename_window(args.window.as_deref(), name.as_deref(), args.auto)?;
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

/// Move one command-prompt response across the tmux boundary without ever
/// interpolating it into a shell command. The binding stores the response in
/// a per-client named buffer; this reads and immediately deletes that buffer.
fn take_window_name_buffer(buffer: &str) -> Result<String> {
    if buffer.trim().is_empty() {
        bail!("tmux buffer name cannot be empty");
    }
    let value = tmux_output(&["show-buffer", "-b", buffer])?;
    tmux_status(&["delete-buffer", "-b", buffer])
        .with_context(|| format!("delete consumed tmux buffer {buffer:?}"))?;
    Ok(value.trim_end_matches(['\r', '\n']).to_string())
}

/// Report that this agent has finished its part, which is what opens an
/// `after` edge for whatever waits on it.
///
/// muxa cannot observe "done" any other way: agent state says `idle` both
/// when an agent has finished and when it is between thoughts, and a pane
/// stays open either way. So the agent says so, and muxa records the claim
/// rather than inferring one.
pub async fn run_work_done(args: WorkDoneArgs, client: &muxa::ipc::Client) -> Result<()> {
    let pane = args
        .pane
        .clone()
        .or_else(|| std::env::var("TMUX_PANE").ok())
        .filter(|pane| !pane.trim().is_empty())
        .ok_or_else(|| anyhow::anyhow!("no pane to read; run inside tmux or pass --pane"))?;
    validate_pane_id(&pane)?;
    let alias = match args.alias.clone() {
        Some(alias) => alias,
        None => pane_option(&pane, AGENT_ALIAS_OPTION)?
            .ok_or_else(|| anyhow::anyhow!("pane {pane} has no pipeline alias; pass --alias"))?,
    }
    .trim()
    .to_ascii_lowercase();
    let workspace = pane_option(&pane, PANE_WORKSPACE_OPTION)?.ok_or_else(|| {
        anyhow::anyhow!("pane {pane} has no pipeline workspace; run `muxa work up` first")
    })?;
    let work = pane_option(&pane, PANE_WORK_OPTION)?.ok_or_else(|| {
        anyhow::anyhow!("pane {pane} has no pipeline Work; run `muxa work up` first")
    })?;
    let generation = match args.generation {
        Some(generation) => generation,
        None => pane_option(&pane, AGENT_GENERATION_OPTION)?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "pane {pane} has no pipeline generation; run `muxa work up` to adopt it"
                )
            })?
            .parse::<u64>()
            .with_context(|| format!("pane {pane} has an invalid pipeline generation"))?,
    };
    let identity = muxa::work::WorkIdentity::new(workspace, work);
    let run = if args.undo {
        client
            .pipeline_invalidate(&identity, &alias, generation)
            .await
            .with_context(|| format!("invalidate pipeline alias {alias:?}"))?
    } else {
        client
            .pipeline_done(&identity, &alias, generation)
            .await
            .with_context(|| format!("atomically complete pipeline alias {alias:?}"))?
    };
    // Completion is the event that drives reconciliation; callers never need
    // to remember a second `work up`. The daemon also subscribes to this same
    // durable revision as a crash/restart safety net.
    crate::work_up::reconcile_run(client, &identity, run.generation).await?;
    let done = run
        .aliases
        .values()
        .filter(|state| state.status == muxa::pipeline_run::PipelineAliasStatus::Done)
        .map(|state| state.alias.clone())
        .collect::<Vec<_>>();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "work": identity,
                "alias": alias,
                "generation": run.generation,
                "done": done,
            }))?
        );
    } else if args.undo {
        println!("{alias} is no longer reported done");
    } else {
        println!("{alias} reported done ({})", done.join(", "));
    }
    Ok(())
}

/// Longest alias muxa will mint before giving up, and the widest base it
/// will build one from. The collaboration store caps an alias at 32 bytes
/// (`valid_identity_token`), so a base is trimmed with room left for the
/// collision suffix rather than minting a name the daemon would reject.
const DEFAULT_ALIAS_BASE_MAX: usize = 28;
/// How many `claude`, `claude2`, `claude3`… candidates to try. A room with
/// 64 agents of one kind is not a room; this is a loop bound, not a policy.
const DEFAULT_ALIAS_TRIES: usize = 64;
/// How many claim/verify rounds to spend settling a naming race. One round
/// per racer is enough to converge, so this bounds how many agents may start
/// in one window in the same instant before muxa stops trying.
const DEFAULT_ALIAS_ROUNDS: usize = 16;

/// Give `pane` a room-local handle if nothing has named it yet. Returns the
/// alias the pane now answers to, or `None` when it already had one or the
/// room has no free name left.
///
/// The addressing vocabulary was never what was missing — `resolve_target`
/// has understood `@alias` for as long as collaboration has existed. What
/// was missing is that nothing *minted* one: an alias appeared only if
/// `muxa work up` stamped a pipeline name or the agent called
/// `muxa identity set` on itself, which an agent started by hand never
/// does. So in practice every peer call fell back to `%1242` — unique,
/// correct, and unreadable. You cannot tell which pane it is without
/// asking tmux, and you certainly cannot remember it between two calls.
///
/// The name goes on the pane rather than into the daemon, for the same
/// reason the pipeline alias does: it has to outlive muxad, this CLI
/// process, and the agent restarting in place, so the *slot* keeps its
/// name across all three. It dies with the pane, which is exactly right —
/// a handle is worth keeping only while the thing it addresses exists.
pub fn ensure_default_alias(pane: &str, base: &str) -> Result<Option<String>> {
    validate_pane_id(pane)?;
    // The overwhelmingly common case: a pane that was named the first time
    // its agent started, on every session start since. One tmux call, out.
    if pane_option(pane, AGENT_ALIAS_OPTION)?.is_some() {
        return Ok(None);
    }
    let window = window_id_for_pane(pane)?;

    // Agents starting in one window at the same moment all read an empty
    // room and all pick `claude`. tmux user options have no compare-and-set,
    // so the race is settled after the fact: claim, re-read, and yield to
    // any lower-numbered pane holding the same name.
    //
    // It has to be a loop rather than one retry. With three racers, the two
    // that yield both re-pick from a read taken before either wrote, so they
    // land on the same `claude2` and — with nothing checking a second time —
    // keep it. Each round does settle the lowest unsettled racer, though,
    // and a pane that settles never moves again, so N racers converge in at
    // most N rounds.
    for _ in 0..DEFAULT_ALIAS_ROUNDS {
        let Some(alias) = pick_default_alias(base, &aliases_besides(&window, pane)?) else {
            return Ok(None);
        };
        set_option(OptionScope::Pane, pane, AGENT_ALIAS_OPTION, &alias)?;
        if !yields_to_lower(&alias, pane, &aliases_besides(&window, pane)?) {
            return Ok(Some(alias));
        }
    }

    // Out of rounds with a name somebody lower still holds. Give it up
    // rather than leave the duplicate standing: a contested `@claude2` is
    // refused as ambiguous for *both* panes, so keeping ours would take down
    // the pane that legitimately owns it too. Unset, this pane simply falls
    // back to being addressed as `%1242`.
    unset_option(OptionScope::Pane, pane, AGENT_ALIAS_OPTION)?;
    Ok(None)
}

/// `(pane id, alias)` for every *other* pane in `window` that carries one.
fn aliases_besides(window: &str, pane: &str) -> Result<Vec<(String, String)>> {
    let raw = tmux_output(&[
        "list-panes",
        "-t",
        window,
        "-F",
        "#{pane_id}\t#{@muxa_agent_alias}",
    ])?;
    Ok(parse_window_aliases(&raw, pane))
}

fn parse_window_aliases(raw: &str, pane: &str) -> Vec<(String, String)> {
    raw.lines()
        .filter_map(|line| {
            let (other, alias) = line.split_once('\t')?;
            let (other, alias) = (other.trim(), alias.trim());
            (other != pane && !alias.is_empty()).then(|| (other.to_string(), alias.to_string()))
        })
        .collect()
}

/// `claude`, then `claude2`, `claude3`… — the first name in the series the
/// room is not already using.
///
/// The first agent of a kind gets the bare name deliberately. A room
/// usually holds one of each, `@claude` is what people actually type, and
/// the MCP instructions already tell agents to route by exactly that name.
/// Numbering only on collision keeps the common case memorable instead of
/// making everyone track whether they are 1 or 2.
fn pick_default_alias(base: &str, taken: &[(String, String)]) -> Option<String> {
    let base = sanitize_alias_base(base)?;
    (1..=DEFAULT_ALIAS_TRIES).find_map(|n| {
        let candidate = if n == 1 {
            base.clone()
        } else {
            format!("{base}{n}")
        };
        taken
            .iter()
            .all(|(_, name)| !name.eq_ignore_ascii_case(&candidate))
            .then_some(candidate)
    })
}

/// Reduce an agent-kind label to something `valid_identity_token` accepts,
/// so a name muxa mints is never one the collaboration store then refuses.
fn sanitize_alias_base(base: &str) -> Option<String> {
    let mut cleaned: String = base
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
        .collect();
    cleaned.truncate(DEFAULT_ALIAS_BASE_MAX);
    (!cleaned.is_empty()).then_some(cleaned)
}

/// Whether `pane` has to give up `alias` because a pane tmux numbered
/// earlier is holding the same name. The tie-break has to be total and
/// stable, or two racers each decide the *other* one yields.
fn yields_to_lower(alias: &str, pane: &str, others: &[(String, String)]) -> bool {
    others.iter().any(|(other, name)| {
        name.eq_ignore_ascii_case(alias) && pane_ordinal(other) < pane_ordinal(pane)
    })
}

/// tmux hands out pane ids monotonically, so the numeric part orders two
/// panes by which existed first. Unparseable ids sort last, which keeps an
/// unexpected id shape from winning a contest it should lose.
fn pane_ordinal(pane: &str) -> u64 {
    pane.strip_prefix('%')
        .and_then(|digits| digits.parse().ok())
        .unwrap_or(u64::MAX)
}

fn pane_option(pane: &str, key: &str) -> Result<Option<String>> {
    let raw = tmux_output(&["display-message", "-p", "-t", pane, &format!("#{{{key}}}")])?;
    Ok(option(raw.trim()))
}

pub async fn run_work_list(args: WorkListArgs, client: &muxa::ipc::Client) -> Result<()> {
    let works = match args.workspace.as_deref() {
        Some(workspace) => find_workspace(workspace)?
            .map(|workspace| workspace.works)
            .unwrap_or_default(),
        None => list_works()?,
    };
    let visible_runs = client
        .pipeline_runs()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|run| {
            args.workspace
                .as_deref()
                .is_none_or(|workspace| run.identity.workspace_id == workspace)
        })
        .collect::<Vec<_>>();
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "works": works,
                "pipeline_runs": visible_runs,
            }))?
        );
    } else if works.is_empty() && visible_runs.is_empty() {
        println!("no muxa-managed work or durable pipeline Runs");
    } else {
        let theme = crate::theme::for_theme(
            muxa::config::Config::load_or_default(None)
                .map(|cfg| cfg.ui.theme)
                .unwrap_or_default(),
            crate::use_colors(),
        );
        print!(
            "{}",
            work_list_table(&works, &work_annotations(), &visible_runs, theme)
        );
    }
    Ok(())
}

/// Render the work table.
///
/// Pure on its inputs so the layout is testable without a tmux server; the
/// caller supplies the works, their annotations, and the theme.
fn work_list_table(
    works: &[WorkInfo],
    records: &[WorkRecord],
    runs: &[muxa::pipeline_run::PipelineRun],
    theme: crate::theme::CliTheme,
) -> String {
    use comfy_table::{ContentArrangement, Table};

    let staged: Vec<Option<String>> = works
        .iter()
        .map(|work| {
            stage_for(work, records)
                .filter(|stage| *stage != WorkStage::Auto)
                .map(|stage| stage.label().to_string())
        })
        .collect();
    let run_stages = runs
        .iter()
        .map(|run| {
            stage_for_identity(&run.identity, records)
                .filter(|stage| *stage != WorkStage::Auto)
                .map(|stage| stage.label().to_string())
        })
        .collect::<Vec<_>>();
    // The column only earns its width when something is actually staged.
    let show_stage = staged.iter().chain(&run_stages).any(Option::is_some);

    let mut header = vec!["WORK", "WORKSPACE", "GEN", "ALIASES", "DONE"];
    if show_stage {
        header.push("STAGE");
    }
    header.push("CWD");

    let mut table = Table::new();
    table
        .load_preset(comfy_table::presets::UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(
            header
                .into_iter()
                .map(|label| theme.cell(label, TableTone::Header))
                .collect::<Vec<_>>(),
        );

    for (work, stage) in works.iter().zip(&staged) {
        let run = runs.iter().find(|run| {
            run.identity.workspace_id == work.workspace && run.identity.work_id == work.work
        });
        let completion = run.map(muxa::pipeline_run::PipelineRun::completion);
        let mut row = vec![
            theme.cell(&work.work, TableTone::Accent),
            theme.cell(&work.workspace, TableTone::Dim),
            run.map_or_else(
                || theme.right_cell("-", TableTone::Dim),
                |run| theme.right_cell(run.generation, TableTone::Dim),
            ),
            theme.cell(
                run.map_or_else(|| agent_summary(work), pipeline_alias_summary),
                TableTone::Tmux,
            ),
            match completion {
                // No Run, or a Run that named no agents: nothing here was ever
                // asked to report, and `0/N` would say the opposite.
                None | Some((_, 0)) => theme.right_cell("-", TableTone::Dim),
                Some((done, total)) if done == total => {
                    theme.right_cell(format!("{done}/{total}"), TableTone::Good)
                }
                Some((done, total)) => theme.right_cell(format!("{done}/{total}"), TableTone::Warn),
            },
        ];
        if show_stage {
            row.push(theme.cell(stage.as_deref().unwrap_or("-"), TableTone::Dim));
        }
        row.push(theme.cell(shorten_home(&work.cwd), TableTone::Dim));
        table.add_row(row);
    }
    for (run, stage) in runs.iter().zip(&run_stages).filter(|(run, _)| {
        !works.iter().any(|work| {
            run.identity.workspace_id == work.workspace && run.identity.work_id == work.work
        })
    }) {
        let (done, total) = run.completion();
        let mut row = vec![
            theme.cell(&run.identity.work_id, TableTone::Accent),
            theme.cell(&run.identity.workspace_id, TableTone::Dim),
            theme.right_cell(run.generation, TableTone::Dim),
            theme.cell(pipeline_alias_summary(run), TableTone::Tmux),
            if done == total {
                theme.right_cell(format!("{done}/{total}"), TableTone::Good)
            } else {
                theme.right_cell(format!("{done}/{total}"), TableTone::Warn)
            },
        ];
        if show_stage {
            row.push(theme.cell(stage.as_deref().unwrap_or("-"), TableTone::Dim));
        }
        row.push(theme.cell(shorten_home(&run.cwd), TableTone::Dim));
        table.add_row(row);
    }
    format!("{table}\n")
}

fn pipeline_alias_summary(run: &muxa::pipeline_run::PipelineRun) -> String {
    run.desired
        .iter()
        .filter_map(|agent| {
            run.aliases
                .get(&agent.alias)
                .map(|state| format!("{}:{}", agent.alias, state.status))
        })
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
}

/// Who is in the window: pipeline aliases when it has them, otherwise the
/// agent programs, collapsed so three claudes read as `claude x3` rather than
/// as a list.
fn agent_summary(work: &WorkInfo) -> String {
    let aliases: Vec<&str> = work
        .agents
        .iter()
        .filter_map(|agent| agent.alias.as_deref())
        .collect();
    if !aliases.is_empty() {
        return aliases.join(" \u{b7} ");
    }
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for agent in &work.agents {
        match counts.iter_mut().find(|(name, _)| *name == agent.agent) {
            Some((_, count)) => *count += 1,
            None => counts.push((agent.agent.as_str(), 1)),
        }
    }
    counts
        .into_iter()
        .map(|(name, count)| {
            if count > 1 {
                format!("{name} x{count}")
            } else {
                name.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
}

/// `/home/june/x` -> `~/x`, so the column spends its width on the part that
/// differs between rows.
fn shorten_home(path: &Path) -> String {
    let Some(home) = dirs::home_dir() else {
        return path.display().to_string();
    };
    path.strip_prefix(&home).map_or_else(
        |_| path.display().to_string(),
        |rest| {
            if rest.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rest.display())
            }
        },
    )
}

pub fn run_work_show(args: WorkShowArgs) -> Result<()> {
    let work = find_work_in(&args.work, args.workspace.as_deref())?
        .ok_or_else(|| anyhow::anyhow!("managed work {:?} not found", args.work))?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&work)?);
    } else {
        let records = work_annotations();
        println!(
            "{}  workspace={}  session={}  window={}  cwd={}{}",
            work.work,
            work.workspace,
            work.session,
            work.window,
            work.cwd.display(),
            stage_suffix(&work, &records)
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

/// A session muxa may put this workspace's work windows into without
/// having created it: one already named after the workspace.
///
/// Refusing to adopt splits a workspace across `callabo` and `callabo-2`,
/// which contradicts the whole model — one session is one workspace. The
/// safety that mattered was never "do not touch it"; it is that
/// `close_workspace` refuses a session muxa did not create.
pub fn adoptable_session(workspace: &str) -> Result<Option<String>> {
    let normalized = normalize_workspace_id(workspace)?;
    let wanted = sanitize_session_name(&normalized.to_ascii_lowercase());
    Ok(all_session_names()?
        .into_iter()
        .find(|name| name == &wanted))
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

/// Give an adopted session a workspace identity without claiming it.
///
/// Deliberately does not set `@muxa_managed_workspace`: that flag is what
/// `close_workspace` reads, and muxa must not offer to kill a session full
/// of windows somebody else opened.
pub fn adopt_workspace(session: &str, workspace: &str) -> Result<()> {
    let workspace = normalize_workspace_id(workspace)?;
    set_option(
        OptionScope::Session,
        session,
        WORKSPACE_ID_OPTION,
        &workspace,
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
        // Write every field, including empty ones. Otherwise changing the
        // linked issue could combine its key with the previous issue's URL,
        // title, or provider status left behind on this Run window.
        set_option(
            OptionScope::Window,
            window,
            key,
            &external_option_value(value, max),
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)] // pane metadata is an explicit flat tmux option contract
pub fn mark_agent(
    pane: &str,
    agent: &str,
    workspace: Option<&str>,
    work: Option<&str>,
    role: Option<&str>,
    task: Option<&str>,
    alias: Option<&str>,
    generation: Option<u64>,
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
    if let Some(generation) = generation {
        set_option(
            OptionScope::Pane,
            pane,
            AGENT_GENERATION_OPTION,
            &generation.to_string(),
        )?;
    }
    Ok(())
}

pub fn mark_agent_generation(pane: &str, generation: u64) -> Result<()> {
    validate_pane_id(pane)?;
    set_option(
        OptionScope::Pane,
        pane,
        AGENT_GENERATION_OPTION,
        &generation.to_string(),
    )
}

/// The stage a human (or the dashboard) recorded for this logical Work.
/// A new Run in another tmux window retains the same stage because execution
/// coordinates are deliberately not part of Work identity.
fn stage_for(work: &WorkInfo, records: &[WorkRecord]) -> Option<WorkStage> {
    stage_for_identity(
        &muxa::work::WorkIdentity::new(&work.workspace, &work.work),
        records,
    )
}

fn stage_for_identity(
    identity: &muxa::work::WorkIdentity,
    records: &[WorkRecord],
) -> Option<WorkStage> {
    records
        .iter()
        .find(|record| &record.identity == identity)
        .map(|record| record.metadata.stage)
}

/// Load the dashboard's annotations for display. Read-only on purpose:
/// `WorkStore` rewrites the whole file on save, so a second writer outside
/// muxad's mutex would drop records the dashboard holds in memory.
fn work_annotations() -> Vec<WorkRecord> {
    muxa::dashboard::load_work_records(muxa::paths::default_dashboard_work_file())
}

fn stage_suffix(work: &WorkInfo, records: &[WorkRecord]) -> String {
    stage_for(work, records)
        .filter(|stage| *stage != WorkStage::Auto)
        .map_or_else(String::new, |stage| format!("  stage={}", stage.label()))
}

#[cfg(test)]
mod work_list_view_tests {
    use super::*;

    fn agent(pane: &str, program: &str, alias: Option<&str>) -> ManagedAgentPane {
        ManagedAgentPane {
            pane: pane.into(),
            agent: program.into(),
            alias: alias.map(Into::into),
            role: None,
            task: None,
            command: program.into(),
            cwd: PathBuf::from("/repo"),
        }
    }

    fn run(work: &str, aliases: &[(&str, bool)]) -> muxa::pipeline_run::PipelineRun {
        use muxa::pipeline_run::{PipelineAliasState, PipelineAliasStatus};
        muxa::pipeline_run::PipelineRun {
            identity: muxa::work::WorkIdentity::new("callabo", work),
            pipeline: "pair".into(),
            desired: aliases
                .iter()
                .map(|(alias, _)| muxa::pipeline::DesiredAgent {
                    alias: (*alias).to_string(),
                    program: "claude".into(),
                    role: None,
                    task: None,
                    prompt: None,
                    direction: None,
                    after: Vec::new(),
                })
                .collect(),
            cwd: PathBuf::from("/repo"),
            generation: 1,
            window_id: None,
            aliases: aliases
                .iter()
                .map(|(alias, done)| {
                    (
                        (*alias).to_string(),
                        PipelineAliasState {
                            alias: (*alias).to_string(),
                            status: if *done {
                                PipelineAliasStatus::Done
                            } else {
                                PipelineAliasStatus::Pending
                            },
                            generation: 1,
                            completion_generation: None,
                            pane: None,
                            error: None,
                            reconcile_pending: false,
                            claim_started_at: None,
                            updated_at: time::OffsetDateTime::UNIX_EPOCH,
                        },
                    )
                })
                .collect(),
            updated_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn work(name: &str, agents: Vec<ManagedAgentPane>) -> WorkInfo {
        WorkInfo {
            work: name.into(),
            workspace: "callabo".into(),
            session: "callabo".into(),
            session_id: "$1".into(),
            window: "@1".into(),
            window_index: 0,
            window_name: name.into(),
            cwd: PathBuf::from("/repo"),
            external_item: None,
            agents,
        }
    }

    fn durable_run() -> muxa::pipeline_run::PipelineRun {
        use muxa::pipeline_run::{PipelineAliasState, PipelineAliasStatus, PipelineRun};
        let now = time::OffsetDateTime::UNIX_EPOCH;
        let desired = [("impl", Vec::new()), ("review", vec!["impl".to_string()])]
            .into_iter()
            .map(|(alias, after)| muxa::pipeline::DesiredAgent {
                alias: alias.to_string(),
                program: "codex".to_string(),
                role: None,
                task: None,
                prompt: None,
                direction: None,
                after,
            })
            .collect();
        let state = |alias: &str, status, completion_generation| PipelineAliasState {
            alias: alias.to_string(),
            status,
            generation: 1,
            completion_generation,
            pane: (alias == "impl").then(|| "%1".to_string()),
            error: None,
            reconcile_pending: false,
            claim_started_at: None,
            updated_at: now,
        };
        PipelineRun {
            identity: muxa::work::WorkIdentity::new("callabo", "CAL-1"),
            pipeline: "delivery".to_string(),
            desired,
            cwd: PathBuf::from("/repo"),
            generation: 1,
            window_id: Some("@1".to_string()),
            aliases: std::collections::BTreeMap::from([
                (
                    "impl".to_string(),
                    state("impl", PipelineAliasStatus::Done, Some(1)),
                ),
                (
                    "review".to_string(),
                    state("review", PipelineAliasStatus::Pending, None),
                ),
            ]),
            updated_at: now,
        }
    }

    /// `done` records aliases, so only aliased panes can ever be in it. A pane
    /// the operator split by hand must not inflate the denominator and make a
    #[test]
    fn agents_read_as_aliases_or_collapsed_programs() {
        let piped = work(
            "CAL-1",
            vec![
                agent("%1", "claude", Some("impl")),
                agent("%2", "codex", Some("review")),
            ],
        );
        assert_eq!(agent_summary(&piped), "impl \u{b7} review");

        let manual = work(
            "RELEASE-1",
            vec![
                agent("%1", "claude", None),
                agent("%2", "claude", None),
                agent("%3", "codex", None),
            ],
        );
        assert_eq!(agent_summary(&manual), "claude x2 \u{b7} codex");
    }

    /// A converged pipeline and a stalled one differ by one cell, so that cell
    /// has to be present and correct.
    #[test]
    fn table_shows_the_completion_ratio() {
        let works = vec![
            work(
                "CAL-1",
                vec![
                    agent("%1", "claude", Some("impl")),
                    agent("%2", "codex", Some("review")),
                ],
            ),
            work("RELEASE-1", vec![agent("%3", "claude", None)]),
        ];
        let runs = vec![run("CAL-1", &[("impl", true), ("review", true)])];
        let out = work_list_table(&works, &[], &runs, crate::theme::CliTheme::plain());
        assert!(out.contains("CAL-1"), "{out}");
        assert!(out.contains("2/2"), "{out}");
        // The stage column stays out of the way when nothing is staged.
        assert!(!out.contains("STAGE"), "{out}");
    }

    /// The denominator is what the pipeline *asked for*, not what has been
    /// launched — otherwise a pair whose reviewer has not started yet reads
    /// `1/1`, which is indistinguishable from converged.
    #[test]
    fn an_unlaunched_agent_still_counts_against_the_total() {
        let works = vec![work("CAL-1", vec![agent("%1", "claude", Some("impl"))])];
        let runs = vec![run("CAL-1", &[("impl", true), ("review", false)])];
        let out = work_list_table(&works, &[], &runs, crate::theme::CliTheme::plain());
        assert!(out.contains("1/2"), "{out}");
    }

    /// A window with no Run has no completion record at all. `0/N` would say
    /// nothing finished; the truth is that nothing here was ever asked to.
    #[test]
    fn a_work_without_a_run_reports_no_ratio() {
        let works = vec![work("CAL-1", vec![agent("%1", "claude", Some("impl"))])];
        let out = work_list_table(&works, &[], &[], crate::theme::CliTheme::plain());
        assert!(!out.contains("0/1"), "{out}");
        assert!(out.contains("CAL-1"), "{out}");
    }

    #[test]
    fn table_uses_durable_desired_aliases_before_downstream_has_a_pane() {
        let works = vec![work("CAL-1", vec![agent("%1", "codex", Some("impl"))])];
        let out = work_list_table(
            &works,
            &[],
            &[durable_run()],
            crate::theme::CliTheme::plain(),
        );
        assert!(out.contains("1/2"), "{out}");
        assert!(out.contains("impl:done"), "{out}");
        assert!(out.contains("review:pending"), "{out}");
        assert!(!out.contains("1/1"), "{out}");
    }

    #[test]
    fn table_keeps_a_durable_run_visible_without_a_tmux_window() {
        let out = work_list_table(&[], &[], &[durable_run()], crate::theme::CliTheme::plain());
        assert!(out.contains("CAL-1"), "{out}");
        assert!(out.contains("callabo"), "{out}");
        assert!(out.contains("impl:done"), "{out}");
        assert!(out.contains("review:pending"), "{out}");
        assert!(out.contains("1/2"), "{out}");
    }
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
    if !workspace.managed {
        bail!(
            "workspace {raw:?} lives in session {:?}, which muxa adopted rather than created; \
             close its work windows instead, or kill the session yourself",
            workspace.session
        );
    }
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

fn external_option_value(raw: Option<&str>, max: usize) -> String {
    raw.filter(|value| !value.trim().is_empty())
        .map_or_else(String::new, |value| external_metadata(value, max))
}

fn parse_workspaces(sessions: &str, windows: &str, panes: &str) -> Vec<WorkspaceInfo> {
    let mut workspaces = Vec::new();
    for line in sessions.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        // A workspace id is enough to be findable; `managed` decides what
        // muxa may do to the session, not whether it can see it.
        if fields.len() < 7 || fields[2].trim().is_empty() {
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
            managed: fields[4] == "1",
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
        session_id: session_id.to_string(),
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

/// Normalize a user-supplied name: whitespace to `-`, no control characters,
/// bounded length. Shared with `watch`'s rename form so both entry points
/// produce the same name for the same keystrokes.
pub(crate) fn normalize_window_name(raw: &str) -> Result<String> {
    let value = metadata(raw, 64)?;
    if value.chars().any(char::is_control) {
        bail!("window name cannot contain control characters");
    }
    if value.contains("#{") || value.contains("#(") {
        bail!("window name cannot contain tmux format expansions");
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

fn unset_option(scope: OptionScope, target: &str, key: &str) -> Result<()> {
    let mut args = vec!["set-option", "-u"];
    match scope {
        OptionScope::Session => {}
        OptionScope::Window => args.push("-w"),
        OptionScope::Pane => args.push("-p"),
    }
    args.extend(["-t", target, key]);
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
            ManageResult::SessionControl { action, session } => {
                println!("{action:?} native agent session {session}");
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

    fn view_session(
        id: &str,
        name: &str,
        attached: u32,
        group: Option<&str>,
        destroy_unattached: bool,
    ) -> ViewSession {
        ViewSession {
            id: id.into(),
            name: name.into(),
            attached,
            group: group.map(str::to_string),
            destroy_unattached,
        }
    }

    #[test]
    fn view_session_name_sorts_beside_the_session_it_mirrors() {
        // `<session>~view~<pid>` rather than `view~<pid>~<session>`: the view
        // lands next to its session in `list-sessions` and in watch's tree.
        // Safe despite tmux's prefix matching because an exact name match wins
        // — `-t callabo` resolves to `callabo`, not to this.
        assert_eq!(
            view_session_name("callabo", "1734560"),
            "callabo~view~1734560"
        );
    }

    #[test]
    fn view_session_name_keeps_one_terminal_to_one_view() {
        // The suffix is the client's pid, so the same terminal regrouping
        // twice asks for the same name instead of leaving a trail of sessions
        // behind every jump.
        assert_eq!(
            view_session_name("muxa", "42"),
            view_session_name("muxa", "42")
        );
        assert_ne!(
            view_session_name("muxa", "42"),
            view_session_name("muxa", "43")
        );
    }

    #[test]
    fn canonical_view_source_survives_base_session_rename() {
        let renamed = view_session("$99", "renamed", 2, Some("base"), false);
        let later_view = view_session("$107", "base~view~42", 1, Some("base"), true);
        let sessions = vec![later_view.clone(), renamed.clone()];

        assert_eq!(canonical_view_source(&later_view, &sessions), renamed);
    }

    #[test]
    fn orphaned_view_strips_one_view_suffix_without_nesting() {
        let orphan = view_session("$107", "base~view~42", 2, Some("base"), true);
        assert_eq!(view_name_base(&orphan), "base");
        assert_eq!(
            view_session_name(view_name_base(&orphan), "43"),
            "base~view~43"
        );

        let leaked_orphan = view_session("$108", "base~view~44", 2, Some("base"), false);
        assert_eq!(view_name_base(&leaked_orphan), "base");

        let renamed_orphan = view_session("$109", "renamed~view~45", 2, Some("base"), false);
        assert_eq!(view_name_base(&renamed_orphan), "renamed");
        assert_eq!(
            view_session_name(view_name_base(&renamed_orphan), "46"),
            "renamed~view~46"
        );

        let renamed_base = view_session("$99", "renamed", 2, Some("base"), false);
        assert_eq!(view_name_base(&renamed_base), "renamed");

        let deliberate_name = view_session(
            "$100",
            "project~view~draft",
            2,
            Some("project~view~draft"),
            false,
        );
        assert_eq!(view_name_base(&deliberate_name), "project~view~draft");
    }

    #[test]
    fn reusable_view_does_not_take_over_another_clients_session() {
        let source = view_session("$1", "base", 2, Some("base"), false);
        let occupied = view_session("$2", "base~view~42", 1, Some("base"), true);
        let sessions = vec![source.clone(), occupied.clone()];

        assert!(reusable_view(&sessions, &occupied.name, &source, "$1").is_none());
        assert_eq!(
            reusable_view(&sessions, &occupied.name, &source, "$2"),
            Some(&occupied)
        );

        let available = view_session("$3", "base~view~43", 0, Some("base"), false);
        let sessions = vec![source.clone(), available.clone()];
        assert_eq!(
            reusable_view(&sessions, &available.name, &source, "$1"),
            Some(&available)
        );
    }

    #[test]
    fn parse_view_session_refuses_a_malformed_attached_count() {
        assert!(parse_view_session("$1\tbase\tnot-a-number\t\toff").is_err());
    }

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
        assert!(normalize_window_name("#(printf hidden)").is_err());
        assert!(normalize_window_name("#{session_name}").is_err());
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

    fn work_at(workspace: &str, work: &str) -> WorkInfo {
        WorkInfo {
            work: work.into(),
            workspace: workspace.into(),
            session: "callabo".into(),
            session_id: "$1".into(),
            window: "@7".into(),
            window_index: 0,
            window_name: work.into(),
            cwd: PathBuf::from("/work"),
            external_item: None,
            agents: Vec::new(),
        }
    }

    fn record_at(workspace: &str, work: &str, stage: WorkStage) -> WorkRecord {
        WorkRecord {
            identity: muxa::work::WorkIdentity::new(workspace, work),
            metadata: muxa::work::WorkMetadata {
                title: None,
                goal: None,
                next_action: None,
                stage,
                updated_at: time::OffsetDateTime::UNIX_EPOCH,
            },
            external_items: Vec::new(),
            legacy_binding: None,
            created_at: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn a_stage_follows_the_logical_work_across_runs() {
        let work = work_at("callabo", "CAL-1234");
        let records = vec![
            record_at("callabo", "CAL-1234", WorkStage::Review),
            record_at("callabo", "OTHER", WorkStage::Done),
            record_at("other", "CAL-1234", WorkStage::Blocked),
        ];
        assert_eq!(stage_for(&work, &records), Some(WorkStage::Review));
        assert_eq!(stage_for(&work_at("callabo", "MISSING"), &records), None);
        assert_eq!(stage_for(&work, &[]), None);
    }

    #[test]
    fn the_default_stage_adds_no_column() {
        let work = work_at("callabo", "CAL-1234");
        let records = vec![record_at("callabo", "CAL-1234", WorkStage::Auto)];
        // `auto` means "nobody has said anything"; printing it would be noise.
        assert_eq!(stage_suffix(&work, &records), "");
        let staged = vec![record_at("callabo", "CAL-1234", WorkStage::InProgress)];
        assert_eq!(stage_suffix(&work, &staged), "  stage=in_progress");
    }

    #[test]
    fn an_external_issue_refresh_clears_absent_fields_and_normalizes_present_ones() {
        assert_eq!(external_option_value(None, 32), "");
        assert_eq!(external_option_value(Some("   "), 32), "");
        assert_eq!(
            external_option_value(Some("  Needs\n review  "), 32),
            "Needs review"
        );
    }

    #[test]
    fn an_adopted_session_is_findable_but_not_closable() {
        // Identity and ownership are separate facts. muxa must be able to
        // find work in a session it adopted, and must never offer to kill
        // it — it is full of windows muxa did not open.
        let sessions = "callabo\t$1\tcallabo\t/repo\t\t1\t3\n\
                        owned\t$2\towned\t/repo\t1\t1\t1\n";
        let workspaces = parse_workspaces(sessions, "", "");
        let adopted = workspaces
            .iter()
            .find(|w| w.workspace == "callabo")
            .expect("an adopted session is still visible");
        assert!(!adopted.managed, "adoption must not claim ownership");
        let owned = workspaces
            .iter()
            .find(|w| w.workspace == "owned")
            .expect("a created session is visible too");
        assert!(owned.managed);
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

    fn taken(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(pane, alias)| ((*pane).to_string(), (*alias).to_string()))
            .collect()
    }

    #[test]
    fn the_first_agent_of_a_kind_gets_the_bare_name() {
        // `@claude` is what people type and what the MCP instructions
        // already tell agents to route by; numbering it `claude1` from the
        // start would make the common case the awkward one.
        assert_eq!(pick_default_alias("claude", &[]).unwrap(), "claude");
    }

    #[test]
    fn a_second_agent_of_a_kind_is_numbered_from_two() {
        let room = taken(&[("%1", "claude")]);
        assert_eq!(pick_default_alias("claude", &room).unwrap(), "claude2");
        let room = taken(&[("%1", "claude"), ("%2", "claude2")]);
        assert_eq!(pick_default_alias("claude", &room).unwrap(), "claude3");
    }

    #[test]
    fn a_freed_name_is_reused_rather_than_left_as_a_hole() {
        // Aliases die with their panes, so `claude` going away means the
        // next agent is `claude`, not `claude3` forever.
        let room = taken(&[("%2", "claude2")]);
        assert_eq!(pick_default_alias("claude", &room).unwrap(), "claude");
    }

    #[test]
    fn a_name_taken_by_another_kind_is_still_taken() {
        // Aliases share one namespace per room: `resolve_target` matches on
        // the name alone, so a codex pane already called `claude` (renamed
        // by hand, say) has to push the real claude to `claude2`.
        let room = taken(&[("%1", "CLAUDE")]);
        assert_eq!(pick_default_alias("claude", &room).unwrap(), "claude2");
    }

    #[test]
    fn minted_names_are_ones_the_collaboration_store_accepts() {
        // `valid_identity_token`: 1-32 of [alnum . _ -].
        let long = "a".repeat(80);
        let alias = pick_default_alias(&long, &[]).unwrap();
        assert!(alias.len() <= DEFAULT_ALIAS_BASE_MAX);
        assert_eq!(sanitize_alias_base("Claude Code!").unwrap(), "claudecode");
        assert_eq!(sanitize_alias_base(" \t "), None);
    }

    #[test]
    fn a_full_room_mints_nothing_rather_than_looping() {
        let room: Vec<(String, String)> = (1..=DEFAULT_ALIAS_TRIES + 1)
            .map(|n| {
                let alias = if n == 1 {
                    "claude".to_string()
                } else {
                    format!("claude{n}")
                };
                (format!("%{n}"), alias)
            })
            .collect();
        assert_eq!(pick_default_alias("claude", &room), None);
    }

    #[test]
    fn window_aliases_skip_our_own_pane_and_the_unnamed() {
        let raw = "%1\tclaude\n%2\t\n%3\treviewer\n";
        assert_eq!(
            parse_window_aliases(raw, "%1"),
            taken(&[("%3", "reviewer")]),
            "our own row and the empty one are both dropped"
        );
    }

    /// One round of the settle loop under the worst schedule: every racer
    /// reads the room as it stood before any of them wrote, then all the
    /// writes land, then each verifies against the result. Returns the room
    /// and who settled — composing the same two decisions
    /// (`pick_default_alias`, `yields_to_lower`) the real loop runs.
    fn settle_round(
        racers: &[String],
        room: &mut Vec<(String, String)>,
        settled: &mut Vec<String>,
    ) {
        let snapshot = room.clone();
        let writes: Vec<(String, String)> = racers
            .iter()
            .filter(|pane| !settled.contains(pane))
            .map(|pane| {
                let others: Vec<_> = snapshot
                    .iter()
                    .filter(|(other, _)| other != pane)
                    .cloned()
                    .collect();
                let alias = pick_default_alias("claude", &others).expect("a free name");
                (pane.clone(), alias)
            })
            .collect();
        for (pane, alias) in &writes {
            room.retain(|(held, _)| held != pane);
            room.push((pane.clone(), alias.clone()));
        }
        for (pane, alias) in &writes {
            let others: Vec<_> = room
                .iter()
                .filter(|(other, _)| other != pane)
                .cloned()
                .collect();
            if !yields_to_lower(alias, pane, &others) {
                settled.push(pane.clone());
            }
        }
    }

    fn racers(n: usize) -> Vec<String> {
        (1..=n).map(|i| format!("%{i}")).collect()
    }

    #[test]
    fn a_single_retry_leaves_the_losers_colliding() {
        // Why the settle is a loop and not the one retry this started as.
        // Round one: everyone claims `claude`, and only the lowest-numbered
        // racer survives the check. Round two: the three that yielded each
        // re-pick from a read taken before any of them wrote, so all three
        // choose `claude2` — and the old code returned right there, with no
        // second check. This is the shape the live 7-pane barrier test
        // produced before the fix.
        let panes = racers(4);
        let (mut room, mut settled) = (Vec::new(), Vec::new());
        settle_round(&panes, &mut room, &mut settled);
        assert_eq!(settled, vec!["%1"], "only the lowest ordinal settles");
        settle_round(&panes, &mut room, &mut settled);
        let losers: Vec<&str> = room
            .iter()
            .filter(|(pane, _)| pane != "%1")
            .map(|(_, alias)| alias.as_str())
            .collect();
        assert_eq!(losers, ["claude2", "claude2", "claude2"], "{room:?}");
    }

    #[test]
    fn repeated_rounds_settle_every_racer_on_its_own_name() {
        // A settled pane never moves again, so each round frees the next
        // racer: N racers converge in at most N rounds, well inside the
        // bound.
        let panes = racers(7);
        let (mut room, mut settled) = (Vec::new(), Vec::new());
        let mut rounds = 0;
        while settled.len() < panes.len() && rounds < DEFAULT_ALIAS_ROUNDS {
            settle_round(&panes, &mut room, &mut settled);
            rounds += 1;
        }
        assert_eq!(settled.len(), panes.len(), "did not converge: {room:?}");
        assert!(
            rounds <= panes.len(),
            "{rounds} rounds for {} racers",
            panes.len()
        );
        let mut names: Vec<&str> = room.iter().map(|(_, alias)| alias.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(
            names.len(),
            panes.len(),
            "duplicate names survived: {room:?}"
        );
    }

    #[test]
    fn the_tie_break_never_makes_both_racers_yield() {
        // If two panes each decided the other one wins, the loop would swap
        // names forever instead of converging.
        let room = vec![("%1".to_string(), "claude".to_string())];
        assert!(yields_to_lower("claude", "%2", &room));
        let room = vec![("%2".to_string(), "claude".to_string())];
        assert!(!yields_to_lower("claude", "%1", &room));
    }

    #[test]
    fn pane_ordinal_orders_by_age_and_sorts_junk_last() {
        assert!(pane_ordinal("%9") < pane_ordinal("%10"));
        assert!(pane_ordinal("%10") < pane_ordinal("nonsense"));
    }
}
