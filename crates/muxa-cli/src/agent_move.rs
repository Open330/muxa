//! Move a live PTY, never restart its agent. Shared by the popup and watch.
use crate::mux_control::Control;
use anyhow::{ensure, Context, Result};
use clap::{Args, ValueEnum};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use muxa::{BackendEndpoint, HostKind, PaneKey, SessionKey, WindowKey};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
    Frame,
};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

const FORMAT: &str = "#{session_id}\t#{session_name}\t#{window_id}\t#{window_name}\t#{pane_id}\t#{pane_pid}\t#{pane_current_path}\t#{@muxa_agent_alias}\t#{@muxa_agent_workspace_id}\t#{@muxa_agent_work_id}\t#{@muxa_workspace_id}\t#{@muxa_work_id}\t#{@muxa_managed_workspace}\t#{@muxa_managed_work}\t#{@muxa_pipeline_generation}";
mod window_move;

const HISTORY: &str = "@muxa_move_origin";
const PANE_OPTIONS: [&str; 3] = [
    "@muxa_agent_workspace_id",
    "@muxa_agent_work_id",
    "@muxa_pipeline_generation",
];

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum HostArg {
    Tmux,
    Rmux,
}

#[derive(Debug, Clone, Args)]
#[allow(clippy::struct_excessive_bools)] // independent CLI switches
pub struct MoveArgs {
    /// Exact pane id. Defaults to the invoking terminal's active pane.
    #[arg(long)]
    pub pane: Option<String>,
    /// Move every pane and the existing window layout together.
    #[arg(long)]
    pub whole_window: bool,
    /// Exact source window id for a whole-window move.
    #[arg(long, conflicts_with = "pane")]
    pub source_window: Option<String>,
    /// Explicit server endpoint (watch pins this automatically).
    #[arg(long = "server-socket")]
    pub server_socket: Option<String>,
    #[arg(long, value_enum)]
    pub host: Option<HostArg>,
    /// Open the interactive move menu in a popup.
    #[arg(long, conflicts_with_all = ["new_session", "session", "window", "undo"])]
    pub popup: bool,
    /// Only switch the client that opened the menu.
    #[arg(long)]
    pub caller_client: Option<String>,
    /// Create a session and move into its first window.
    #[arg(long, conflicts_with_all = ["session", "window", "undo"])]
    pub new_session: Option<String>,
    /// Add a new window in an existing session (exact name or $id).
    #[arg(long, conflicts_with_all = ["window", "undo"])]
    pub session: Option<String>,
    /// Move into an existing window (stable @id).
    #[arg(long, conflicts_with = "undo")]
    pub window: Option<String>,
    /// Name of a new window; omitted names use the agent alias.
    #[arg(long, requires = "session")]
    pub new_window: Option<String>,
    #[arg(long, value_enum, default_value = "right")]
    pub direction: MoveDirection,
    /// Keep the current screen after moving.
    #[arg(long)]
    pub stay: bool,
    /// Return to the recorded previous location.
    #[arg(long)]
    pub undo: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum MoveDirection {
    Right,
    Left,
    Down,
    Up,
}

impl MoveDirection {
    fn next(self) -> Self {
        match self {
            Self::Right => Self::Down,
            Self::Down => Self::Left,
            Self::Left => Self::Up,
            Self::Up => Self::Right,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Location {
    session_id: String,
    session_name: String,
    window_id: String,
    window_name: String,
    pane: String,
    pid: String,
    cwd: String,
    alias: String,
    pane_workspace: String,
    pane_work: String,
    workspace: String,
    work: String,
    managed_workspace: bool,
    managed_work: bool,
    generation: String,
}

impl Location {
    fn parse(line: &str) -> Option<Self> {
        let f: Vec<_> = line.split('\t').collect();
        if f.len() != 15
            || !f[0].starts_with('$')
            || !f[2].starts_with('@')
            || !f[4].starts_with('%')
        {
            return None;
        }
        Some(Self {
            session_id: f[0].into(),
            session_name: f[1].into(),
            window_id: f[2].into(),
            window_name: f[3].into(),
            pane: f[4].into(),
            pid: f[5].into(),
            cwd: f[6].into(),
            alias: f[7].into(),
            pane_workspace: f[8].into(),
            pane_work: f[9].into(),
            workspace: f[10].into(),
            work: f[11].into(),
            managed_workspace: f[12] == "1",
            managed_work: f[13] == "1",
            generation: f[14].into(),
        })
    }

    fn target(&self) -> String {
        format!("{}:{}.{}", self.session_id, self.window_id, self.pane)
    }

    fn key(&self, endpoint: &BackendEndpoint) -> PaneKey {
        PaneKey {
            window: WindowKey {
                session: SessionKey {
                    endpoint: endpoint.clone(),
                    session_id: self.session_id.clone(),
                },
                window_id: self.window_id.clone(),
            },
            pane_id: if endpoint.host == HostKind::Rmux {
                format!("rmux:{}", self.pane)
            } else {
                self.pane.clone()
            },
        }
    }

    fn options(&self) -> [&str; 3] {
        [&self.pane_workspace, &self.pane_work, &self.generation]
    }
}

fn inventory(control: &Control) -> Result<Vec<Location>> {
    let output = control.output(&["list-panes", "-a", "-F", FORMAT])?;
    let rows: Vec<_> = output.lines().filter_map(Location::parse).collect();
    ensure!(!rows.is_empty(), "server returned no pane locations");
    Ok(rows)
}

fn native(pane: &str) -> &str {
    pane.strip_prefix("rmux:").unwrap_or(pane)
}

fn locate<'a>(rows: &'a [Location], key: &PaneKey) -> Result<&'a Location> {
    rows.iter()
        .find(|p| {
            p.pane == native(&key.pane_id)
                && p.window_id == key.window.window_id
                && p.session_id == key.window.session.session_id
        })
        .context("source pane changed location or closed; reopen the move menu")
}

fn valid_name(name: &str) -> Result<String> {
    let name = name.trim();
    ensure!(
        !name.is_empty() && name.len() <= 128,
        "name must contain 1–128 bytes"
    );
    ensure!(
        !name
            .chars()
            .any(|c| c.is_control() || matches!(c, ':' | '.' | '\\' | ';')),
        "name cannot contain control characters, colon, dot, backslash, or semicolon"
    );
    ensure!(
        !name.starts_with(['$', '@', '%', '-']),
        "name cannot begin with $, @, %, or -"
    );
    ensure!(
        !name.contains("#(") && !name.contains("#{"),
        "name cannot contain tmux format expressions"
    );
    Ok(name.to_string())
}

#[derive(Debug, Clone)]
pub enum Destination {
    NewSession(String),
    NewWindow {
        session: String,
        name: Option<String>,
    },
    Window {
        session: String,
        window: String,
    },
    Undo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveScope {
    Pane,
    Window,
}

#[derive(Debug, Clone)]
pub struct MoveRequest {
    pub source: PaneKey,
    source_pid: String,
    scope: MoveScope,
    window_panes: Vec<(String, String)>,
    pub destination: Destination,
    pub direction: MoveDirection,
    pub follow: bool,
}

#[derive(Debug)]
pub struct MoveOutcome {
    pub pane: PaneKey,
    pub follow: bool,
    pub message: String,
}

fn set_option(control: &Control, scope: &str, target: &str, key: &str, value: &str) -> Result<()> {
    let mut args = vec!["set-option"];
    if !scope.is_empty() {
        args.push(scope);
    }
    args.extend(["-t", target, key, value]);
    control.run(&args)
}

fn pane_options(control: &Control, target: &str, values: [&str; 3]) -> Result<()> {
    for (key, value) in PANE_OPTIONS.into_iter().zip(values) {
        set_option(control, "-p", target, key, value)?;
    }
    Ok(())
}

fn new_window(control: &Control, session: &str, name: &str, cwd: &str) -> Result<Location> {
    let created = control.output(&[
        "new-window",
        "-d",
        "-P",
        "-F",
        FORMAT,
        "-t",
        &format!("{session}:"),
        "-n",
        name,
        "-c",
        cwd,
    ])?;
    Location::parse(created.trim_end_matches('\n')).context("could not read new window identity")
}

fn new_session(control: &Control, name: &str, cwd: &str) -> Result<Location> {
    let created = control.output(&[
        "new-session",
        "-d",
        "-P",
        "-F",
        FORMAT,
        "-s",
        name,
        "-c",
        cwd,
    ])?;
    Location::parse(created.trim_end_matches('\n')).context("could not read new session identity")
}

fn mark_destination(
    control: &Control,
    dest: &Location,
    source: &Location,
    new_session: bool,
) -> Result<()> {
    if new_session && source.managed_workspace {
        let workspace = crate::tmux_work::normalize_workspace_id(&dest.session_name)?;
        for (option, value) in [
            ("@muxa_workspace_id", workspace.as_str()),
            ("@muxa_workspace_cwd", source.cwd.as_str()),
            ("@muxa_managed_workspace", "1"),
        ] {
            set_option(control, "", &dest.session_id, option, value)?;
        }
    }
    if source.managed_work
        && dest.workspace.is_empty()
        && !(new_session && source.managed_workspace)
    {
        // Adopt a workspace identity without claiming ownership of a session
        // the user created. Close-workspace must still refuse that session.
        set_option(
            control,
            "",
            &dest.session_id,
            "@muxa_workspace_id",
            &crate::tmux_work::normalize_workspace_id(&dest.session_name)?,
        )?;
    }
    if source.managed_work || dest.managed_workspace {
        let work = crate::tmux_work::normalize_work_id(&dest.window_name)?;
        for (option, value) in [
            ("@muxa_work_id", work.as_str()),
            ("@muxa_work_cwd", source.cwd.as_str()),
            ("@muxa_managed_work", "1"),
        ] {
            set_option(
                control,
                "-w",
                &format!("{}:{}", dest.session_id, dest.window_id),
                option,
                value,
            )?;
        }
    }
    set_option(
        control,
        "-w",
        &format!("{}:{}", dest.session_id, dest.window_id),
        "automatic-rename",
        "off",
    )
}

/// Preflight and setup happen before transfer. Failures before transfer restore
/// pane options and remove only the empty surface this operation created.
#[allow(clippy::too_many_lines)] // one transaction keeps setup, transfer and rollback together
pub fn execute(request: MoveRequest) -> Result<MoveOutcome> {
    if request.scope == MoveScope::Window {
        return window_move::execute(request);
    }
    let endpoint = &request.source.window.session.endpoint;
    ensure!(
        crate::mux_control::supported(endpoint.host),
        "moving is supported on tmux and rmux"
    );
    let control = Control::Endpoint(endpoint.clone());
    let rows = inventory(&control)?;
    let source = locate(&rows, &request.source)?.clone();
    ensure!(
        source.pid == request.source_pid,
        "source process changed; reopen the move menu"
    );
    let old_history = control
        .output(&["show-options", "-p", "-v", "-t", &source.target(), HISTORY])
        .unwrap_or_default();
    let undo: Option<Location> = if matches!(request.destination, Destination::Undo) {
        Some(
            serde_json::from_str(old_history.trim())
                .context("no valid previous location to restore")?,
        )
    } else {
        None
    };
    let default_name = if source.alias.is_empty() {
        format!("agent-{}", source.pane.trim_start_matches('%'))
    } else {
        valid_name(&source.alias)?
    };
    let mut created: Option<(Location, bool)> = None;
    let mut source_shell: Option<String> = None;
    let mut moved = false;
    let result = (|| -> Result<MoveOutcome> {
        let mut dest = match &request.destination {
            Destination::NewSession(name) => {
                let name = valid_name(name)?;
                ensure!(
                    !rows.iter().any(|p| p.session_name == name),
                    "session {name} already exists"
                );
                let dest = new_session(&control, &name, &source.cwd)?;
                created = Some((dest.clone(), true));
                control.run(&["rename-window", "-t", &dest.target(), &default_name])?;
                let mut dest = dest;
                dest.window_name.clone_from(&default_name);
                mark_destination(&control, &dest, &source, true)?;
                dest
            }
            Destination::NewWindow { session, name } => {
                let session = rows
                    .iter()
                    .find(|p| &p.session_id == session || &p.session_name == session)
                    .context("destination session no longer exists")?;
                let base = valid_name(name.as_deref().unwrap_or(&default_name))?;
                let mut name = base.clone();
                let mut suffix = 2;
                while rows
                    .iter()
                    .any(|p| p.session_id == session.session_id && p.window_name == name)
                {
                    name = format!("{base}-{suffix}");
                    suffix += 1;
                }
                let dest = new_window(&control, &session.session_id, &name, &source.cwd)?;
                created = Some((dest.clone(), false));
                mark_destination(&control, &dest, &source, false)?;
                dest
            }
            Destination::Window { session, window } => rows
                .iter()
                .find(|p| {
                    &p.session_id == session && &p.window_id == window && p.pane != source.pane
                })
                .context("destination window no longer has another pane")?
                .clone(),
            Destination::Undo => {
                let origin = undo.as_ref().expect("undo decoded");
                if let Some(dest) = rows.iter().find(|p| {
                    p.session_id == origin.session_id
                        && p.window_id == origin.window_id
                        && p.pane != source.pane
                }) {
                    dest.clone()
                } else {
                    // Recreate a disappeared window in the original surviving
                    // session. Do not guess by session name after an id vanished.
                    ensure!(
                        rows.iter().any(|p| p.session_id == origin.session_id),
                        "previous session was closed; choose a destination manually"
                    );
                    let dest = new_window(
                        &control,
                        &origin.session_id,
                        &origin.window_name,
                        &origin.cwd,
                    )?;
                    created = Some((dest.clone(), false));
                    mark_destination(&control, &dest, origin, false)?;
                    if origin.managed_work {
                        set_option(
                            &control,
                            "-w",
                            &dest.target(),
                            "@muxa_work_id",
                            &origin.work,
                        )?;
                    }
                    dest
                }
            }
        };
        ensure!(
            dest.window_id != source.window_id,
            "agent is already in that window"
        );
        // Read fresh metadata after marks were written; ids stay pinned.
        let fresh = inventory(&control)?;
        dest = fresh
            .iter()
            .find(|p| p.pane == dest.pane && p.session_id == dest.session_id)
            .context("destination pane closed")?
            .clone();
        let current_source =
            locate(&fresh, &request.source).context("source moved during preparation")?;
        ensure!(
            current_source.pid == source.pid,
            "source process changed during preparation"
        );
        if !source.alias.is_empty() {
            ensure!(
                !fresh
                    .iter()
                    .any(|p| p.window_id == dest.window_id && p.alias == source.alias),
                "destination already contains alias {}; rename it before moving",
                source.alias
            );
        }
        let target_options = undo.as_ref().map_or(
            [dest.workspace.as_str(), dest.work.as_str(), ""],
            Location::options,
        );
        pane_options(&control, &source.target(), target_options)?;
        set_option(
            &control,
            "-p",
            &source.target(),
            HISTORY,
            &serde_json::to_string(&source)?,
        )?;
        let source_target = source.target();
        let mut args = vec!["join-pane", "-d", "-s", source_target.as_str(), "-t"];
        let target = dest.target();
        args.push(&target);
        args.push(
            if matches!(
                request.direction,
                MoveDirection::Right | MoveDirection::Left
            ) {
                "-h"
            } else {
                "-v"
            },
        );
        if matches!(request.direction, MoveDirection::Left | MoveDirection::Up) {
            args.push("-b");
        }
        let sole_pane = fresh
            .iter()
            .filter(|p| p.window_id == source.window_id)
            .all(|p| p.pane == source.pane);
        let sole_window = fresh
            .iter()
            .filter(|p| p.session_id == source.session_id)
            .all(|p| p.window_id == source.window_id);
        if sole_pane && sole_window {
            // Destroying the session underneath its last pane detaches clients
            // and can terminate the popup before follow/cleanup. Keep a shell
            // there so the operator's terminal and undo location survive.
            let shell = control.output(&[
                "split-window",
                "-d",
                "-P",
                "-F",
                "#{pane_id}",
                "-t",
                &source.target(),
                "-c",
                &source.cwd,
            ])?;
            let shell = shell.trim();
            ensure!(
                shell.starts_with('%') && shell != source.pane,
                "could not identify source shell"
            );
            source_shell = Some(format!(
                "{}:{}.{}",
                source.session_id, source.window_id, shell
            ));
        }
        control.run(&args)?;
        moved = true;
        let mut warnings = Vec::new();
        if let Some((placeholder, _)) = &created {
            if let Err(error) = control.run(&["kill-pane", "-t", &placeholder.target()]) {
                warnings.push(format!("empty pane cleanup failed: {error}"));
            }
        }
        let actual = if let Some(actual) = inventory(&control).ok().and_then(|rows| {
            rows.into_iter()
                .find(|p| p.pane == source.pane && p.session_id == dest.session_id)
        }) {
            if actual.pid != source.pid {
                warnings.push("pane process changed after transfer".into());
            }
            actual
        } else {
            warnings.push("transfer succeeded, but its location could not be refreshed".into());
            let mut actual = dest.clone();
            actual.pane.clone_from(&source.pane);
            actual
        };
        let mut message = format!(
            "moved to {} / {} — agent continues; move menu → undo returns",
            actual.session_name, actual.window_name
        );
        if !warnings.is_empty() {
            let _ = write!(message, " ({})", warnings.join("; "));
        }
        if source_shell.is_some() {
            message.push_str("; original session keeps a shell");
        }
        Ok(MoveOutcome {
            pane: actual.key(endpoint),
            follow: request.follow && warnings.is_empty(),
            message,
        })
    })();
    if result.is_err() && !moved {
        let _ = pane_options(&control, &source.target(), source.options());
        let _ = set_option(
            &control,
            "-p",
            &source.target(),
            HISTORY,
            old_history.trim_end(),
        );
        if let Some(shell) = source_shell {
            let _ = control.run(&["kill-pane", "-t", &shell]);
        }
        if let Some((created, session)) = created {
            let _ = if session {
                control.run(&["kill-session", "-t", &created.session_id])
            } else {
                control.run(&[
                    "kill-window",
                    "-t",
                    &format!("{}:{}", created.session_id, created.window_id),
                ])
            };
        }
    }
    result
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChoiceKind {
    Action,
    Session,
    Window,
}

#[derive(Debug, Clone)]
struct Choice {
    label: String,
    search: String,
    session: Option<String>,
    kind: ChoiceKind,
    destination: Option<Destination>,
}

fn destination_choices(rows: &[Location], source: &Location, undo: bool) -> Vec<Choice> {
    let mut choices = vec![Choice {
        label: "+ New session…".into(),
        search: "new session".into(),
        session: None,
        kind: ChoiceKind::Action,
        destination: Some(Destination::NewSession(String::new())),
    }];
    if undo {
        choices.push(Choice {
            label: "↶ Undo previous move".into(),
            search: "undo previous move".into(),
            session: None,
            kind: ChoiceKind::Action,
            destination: Some(Destination::Undo),
        });
    }
    let mut sessions: Vec<_> = rows
        .iter()
        .filter(|row| !row.session_name.contains("~view~") || row.session_id == source.session_id)
        .collect();
    sessions.sort_by_key(|row| {
        (
            row.session_id != source.session_id,
            row.session_name.to_lowercase(),
            row.session_id.clone(),
        )
    });
    sessions.dedup_by(|a, b| a.session_id == b.session_id);
    for session in sessions {
        let current = session.session_id == source.session_id;
        choices.push(Choice {
            label: format!(
                "SESSION {}{}",
                session.session_name,
                if current { " · current" } else { "" }
            ),
            search: session.session_name.to_lowercase(),
            session: Some(session.session_id.clone()),
            kind: ChoiceKind::Session,
            destination: None,
        });
        choices.push(Choice {
            label: "+ New window".into(),
            search: format!("{} new window", session.session_name).to_lowercase(),
            session: Some(session.session_id.clone()),
            kind: ChoiceKind::Action,
            destination: Some(Destination::NewWindow {
                session: session.session_id.clone(),
                name: None,
            }),
        });
        let mut windows: Vec<_> = rows
            .iter()
            .filter(|row| row.session_id == session.session_id)
            .collect();
        windows.sort_by_key(|row| {
            row.window_id
                .trim_start_matches('@')
                .parse::<u64>()
                .unwrap_or(u64::MAX)
        });
        windows.dedup_by(|a, b| a.window_id == b.window_id);
        for window in windows {
            let panes = rows
                .iter()
                .filter(|row| {
                    row.session_id == session.session_id && row.window_id == window.window_id
                })
                .count();
            let current = window.window_id == source.window_id;
            let work = if window.work.is_empty() {
                String::new()
            } else {
                format!(" · Work {}", window.work)
            };
            choices.push(Choice {
                label: format!(
                    "WINDOW {} · {} {}{}{}",
                    window.window_name,
                    panes,
                    if panes == 1 { "pane" } else { "panes" },
                    work,
                    if current { " · source" } else { "" }
                ),
                search: format!(
                    "{} {} {}",
                    session.session_name, window.window_name, window.work
                )
                .to_lowercase(),
                session: Some(session.session_id.clone()),
                kind: ChoiceKind::Window,
                destination: (!current).then(|| Destination::Window {
                    session: session.session_id.clone(),
                    window: window.window_id.clone(),
                }),
            });
        }
    }
    choices
}

#[derive(Debug)]
pub struct MoveMenu {
    source: PaneKey,
    pid: String,
    title: String,
    choices: Vec<Choice>,
    rows: Vec<Location>,
    scope: MoveScope,
    collapsed: std::collections::BTreeSet<String>,
    selected: usize,
    query: String,
    naming: bool,
    pub follow: bool,
    direction: MoveDirection,
    pub error: Option<String>,
}

pub enum MenuEvent {
    None,
    Cancel,
    Submit(Box<MoveRequest>),
}

fn window_members(rows: &[Location], key: &PaneKey) -> Vec<(String, String)> {
    let mut members: Vec<_> = rows
        .iter()
        .filter(|row| {
            row.session_id == key.window.session.session_id && row.window_id == key.window.window_id
        })
        .map(|row| (row.pane.clone(), row.pid.clone()))
        .collect();
    members.sort();
    members
}

fn menu_title(location: &Location, scope: MoveScope) -> String {
    if scope == MoveScope::Window {
        format!(
            "Move whole WINDOW {} · {}",
            location.window_name, location.session_name
        )
    } else {
        format!(
            "Move pane {} · {} / {}",
            if location.alias.is_empty() {
                &location.pane
            } else {
                &location.alias
            },
            location.session_name,
            location.window_name
        )
    }
}

fn scoped_choices(
    control: &Control,
    rows: &[Location],
    source: &Location,
    scope: MoveScope,
) -> Vec<Choice> {
    let (option, level) = if scope == MoveScope::Window {
        (window_move::HISTORY, "-w")
    } else {
        (HISTORY, "-p")
    };
    let undo = control
        .output(&["show-options", level, "-v", "-t", &source.target(), option])
        .is_ok_and(|value| {
            if scope == MoveScope::Window {
                serde_json::from_str::<window_move::History>(value.trim()).is_ok()
            } else {
                serde_json::from_str::<Location>(value.trim()).is_ok()
            }
        });
    let mut choices = destination_choices(rows, source, undo);
    if scope == MoveScope::Window {
        choices.retain(|choice| !matches!(&choice.destination, Some(Destination::NewWindow { session, .. }) if rows.iter().any(|row| &row.session_id == session && row.window_id == source.window_id)));
        for choice in &mut choices {
            if choice.kind == ChoiceKind::Window {
                choice.destination = None;
            }
            if matches!(choice.destination, Some(Destination::NewWindow { .. })) {
                choice.label = "+ Move whole window here".into();
                choice.search.push(' ');
                let names = rows
                    .iter()
                    .filter(|row| choice.session.as_deref() == Some(row.session_id.as_str()))
                    .map(|row| row.window_name.to_lowercase())
                    .collect::<Vec<_>>()
                    .join(" ");
                choice.search.push_str(&names);
            }
        }
    }
    choices
}

pub async fn run_window(
    mut args: MoveArgs,
    daemon_socket: Option<std::path::PathBuf>,
) -> Result<()> {
    args.whole_window = true;
    run(args, daemon_socket).await
}

impl MoveMenu {
    pub fn open(source: PaneKey) -> Result<Self> {
        Self::open_scope(source, MoveScope::Pane)
    }

    pub fn open_scope(source: PaneKey, scope: MoveScope) -> Result<Self> {
        ensure!(
            crate::mux_control::supported(source.window.session.endpoint.host),
            "moving is supported on tmux and rmux"
        );
        let control = Control::Endpoint(source.window.session.endpoint.clone());
        let rows = inventory(&control)?;
        let location = locate(&rows, &source)?;
        let choices = scoped_choices(&control, &rows, location, scope);
        let selected = choices.iter().position(|choice| scope == MoveScope::Pane && matches!(&choice.destination, Some(Destination::NewWindow { session, .. }) if session == &location.session_id)).unwrap_or(0);
        Ok(Self {
            title: menu_title(location, scope),
            pid: location.pid.clone(),
            source,
            choices,
            rows,
            scope,
            collapsed: std::collections::BTreeSet::new(),
            selected,
            query: String::new(),
            naming: false,
            follow: true,
            direction: MoveDirection::Right,
            error: None,
        })
    }

    fn visible(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        self.choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| {
                if query.is_empty() {
                    return choice.kind == ChoiceKind::Session
                        || choice
                            .session
                            .as_ref()
                            .is_none_or(|id| !self.collapsed.contains(id));
                }
                if choice.kind == ChoiceKind::Session {
                    return self.choices.iter().any(|child| {
                        child.session == choice.session && child.search.contains(&query)
                    });
                }
                choice.search.contains(&query)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn fold(&mut self, expand: bool) {
        let visible = self.visible();
        let Some(&index) = visible.get(self.selected) else {
            return;
        };
        let Some(session) = self.choices[index].session.clone() else {
            return;
        };
        if expand {
            self.collapsed.remove(&session);
        } else {
            self.collapsed.insert(session.clone());
            if let Some(parent) = self.visible().iter().position(|&i| {
                self.choices[i].kind == ChoiceKind::Session
                    && self.choices[i].session.as_ref() == Some(&session)
            }) {
                self.selected = parent;
            }
        }
    }

    fn row_label(&self, index: usize, visible: &[usize]) -> String {
        let choice = &self.choices[index];
        if choice.kind == ChoiceKind::Session {
            let closed = self.query.is_empty()
                && choice
                    .session
                    .as_ref()
                    .is_some_and(|id| self.collapsed.contains(id));
            return format!("{} {}", if closed { "▸" } else { "▾" }, choice.label);
        }
        if choice.session.is_none() {
            return choice.label.clone();
        }
        let last = !visible
            .iter()
            .any(|&i| i > index && self.choices[i].session == choice.session);
        format!("  {} {}", if last { "└─" } else { "├─" }, choice.label)
    }

    fn toggle_scope(&mut self) -> Result<()> {
        let scope = if self.scope == MoveScope::Pane {
            MoveScope::Window
        } else {
            MoveScope::Pane
        };
        let control = Control::Endpoint(self.source.window.session.endpoint.clone());
        let rows = inventory(&control)?;
        let location = locate(&rows, &self.source)?;
        ensure!(
            location.pid == self.pid,
            "source process changed; reopen the move menu"
        );
        self.title = menu_title(location, scope);
        self.choices = scoped_choices(&control, &rows, location, scope);
        self.scope = scope;
        self.rows = rows;
        self.query.clear();
        self.selected = 0;
        self.collapsed.clear();
        Ok(())
    }

    pub fn on_key(&mut self, key: KeyEvent) -> MenuEvent {
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            return MenuEvent::None;
        }
        self.error = None;
        match key.code {
            KeyCode::Esc => {
                if self.naming || !self.query.is_empty() {
                    self.naming = false;
                    self.query.clear();
                    self.selected = 0;
                } else {
                    return MenuEvent::Cancel;
                }
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return MenuEvent::Cancel
            }
            KeyCode::Char('w') if key.modifiers.contains(KeyModifiers::CONTROL) && !self.naming => {
                if let Err(error) = self.toggle_scope() {
                    self.error = Some(error.to_string());
                }
            }
            KeyCode::Tab => self.follow = !self.follow,
            KeyCode::Left | KeyCode::Right
                if !self.naming && key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.direction = self.direction.next();
            }
            KeyCode::Left if !self.naming => self.fold(false),
            KeyCode::Right if !self.naming => self.fold(true),
            KeyCode::Up if !self.naming => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down if !self.naming => {
                self.selected = (self.selected + 1).min(self.visible().len().saturating_sub(1));
            }
            KeyCode::Backspace => {
                self.query.pop();
                if !self.naming {
                    self.selected = 0;
                }
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(c);
                if !self.naming {
                    self.selected = 0;
                }
            }
            KeyCode::Enter => {
                let destination = if self.naming {
                    match valid_name(&self.query) {
                        Ok(name) => Destination::NewSession(name),
                        Err(error) => {
                            self.error = Some(error.to_string());
                            return MenuEvent::None;
                        }
                    }
                } else {
                    let Some(index) = self.visible().get(self.selected).copied() else {
                        return MenuEvent::None;
                    };
                    if self.choices[index].kind == ChoiceKind::Session {
                        let closed = self.choices[index]
                            .session
                            .as_ref()
                            .is_some_and(|id| self.collapsed.contains(id));
                        self.fold(closed);
                        return MenuEvent::None;
                    }
                    let Some(destination) = self.choices[index].destination.clone() else {
                        self.error = Some(
                            "Choose a session action or an available destination window".into(),
                        );
                        return MenuEvent::None;
                    };
                    if matches!(destination, Destination::NewSession(_)) {
                        self.naming = true;
                        self.query.clear();
                        return MenuEvent::None;
                    }
                    destination
                };
                return MenuEvent::Submit(Box::new(MoveRequest {
                    source: self.source.clone(),
                    source_pid: self.pid.clone(),
                    scope: self.scope,
                    window_panes: window_members(&self.rows, &self.source),
                    destination,
                    direction: self.direction,
                    follow: self.follow,
                }));
            }
            _ => {}
        }
        MenuEvent::None
    }

    pub fn paste(&mut self, text: &str) {
        self.query.extend(text.chars().filter(|c| !c.is_control()));
        self.error = None;
        if !self.naming {
            self.selected = 0;
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.title.clone());
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let chunks = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(3),
        ])
        .split(inner);
        frame.render_widget(
            Paragraph::new(format!(
                "{}: {}",
                if self.naming {
                    "New session name"
                } else {
                    "Search destinations"
                },
                self.query
            )),
            chunks[0],
        );
        if self.naming {
            let description = if self.scope == MoveScope::Window {
                "Create a session and move the whole window into it.\nAll panes and their layout stay together."
            } else {
                "Create a session and move this running agent into it.\nIts working directory and conversation stay the same."
            };
            frame.render_widget(Paragraph::new(description), chunks[1]);
        } else {
            let visible = self.visible();
            let choices: Vec<_> = visible
                .iter()
                .map(|&i| {
                    let choice = &self.choices[i];
                    let style = match choice.kind {
                        ChoiceKind::Session => Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                        ChoiceKind::Window if choice.destination.is_none() => {
                            Style::default().add_modifier(Modifier::DIM)
                        }
                        ChoiceKind::Action => Style::default().fg(Color::Green),
                        ChoiceKind::Window => Style::default(),
                    };
                    ListItem::new(self.row_label(i, &visible)).style(style)
                })
                .collect();
            let mut state = ListState::default().with_selected(Some(self.selected));
            frame.render_stateful_widget(
                List::new(choices)
                    .highlight_style(Style::default().fg(Color::Black).bg(Color::Cyan))
                    .highlight_symbol("› "),
                chunks[1],
                &mut state,
            );
        }
        let scope_hint = if self.scope == MoveScope::Window {
            "whole window · Ctrl-W: pane".to_string()
        } else {
            format!(
                "Ctrl-←/→ split: {:?} · Ctrl-W: whole window",
                self.direction
            )
        };
        let footer = vec![
            Line::from(format!(
                "Enter choose/fold · Esc cancel · Tab {}",
                if self.follow {
                    "follow ✓"
                } else {
                    "stay here ✓"
                }
            )),
            Line::from(format!("←/→ fold/expand · {scope_hint}")),
            Line::styled(
                self.error.clone().unwrap_or_default(),
                Style::default().fg(Color::Red),
            ),
        ];
        frame.render_widget(Paragraph::new(footer), chunks[2]);
    }
}

fn endpoint(args: &MoveArgs) -> Result<BackendEndpoint> {
    let host = match args.host {
        Some(HostArg::Rmux) => HostKind::Rmux,
        None if args.pane.as_deref().is_some_and(|p| p.starts_with("rmux:"))
            || muxa::backend::rmux::endpoint_from_env().is_some() =>
        {
            HostKind::Rmux
        }
        Some(HostArg::Tmux) | None => HostKind::Tmux,
    };
    let socket = args
        .server_socket
        .clone()
        .or_else(|| match host {
            HostKind::Rmux => muxa::backend::rmux::endpoint_from_env(),
            _ => std::env::var("TMUX")
                .ok()
                .and_then(|v| v.split(',').next().map(str::to_string)),
        })
        .context("no server endpoint; specify --server-socket and --host")?;
    Ok(BackendEndpoint { host, socket })
}

fn resolve_source_pane(control: &Control, args: &MoveArgs) -> Result<String> {
    let pane = if let Some(window) = &args.source_window {
        let windows = inventory(control)?;
        let source = windows
            .iter()
            .find(|row| &row.window_id == window)
            .context("source window is not live")?;
        let target = control.output(&[
            "display-message",
            "-p",
            "-t",
            &format!("{}:{}", source.session_id, source.window_id),
            "#{pane_id}",
        ])?;
        target.trim().to_string()
    } else if let Some(pane) = &args.pane {
        native(pane).to_string()
    } else {
        let mut query = vec!["display-message", "-p", "#{pane_id}"];
        if let Some(client) = &args.caller_client {
            query.extend(["-c", client]);
        }
        control.output(&query)?.trim().to_string()
    };
    Ok(pane)
}

pub async fn run(args: MoveArgs, daemon_socket: Option<std::path::PathBuf>) -> Result<()> {
    let endpoint = endpoint(&args)?;
    if let Some(client) = args.caller_client.as_ref() {
        let _ = crate::CALLER_CLIENT.set(client.clone());
    }
    let control = Control::Endpoint(endpoint.clone());
    ensure!(
        args.whole_window || args.source_window.is_none(),
        "--source-window requires a whole-window move"
    );
    let pane = resolve_source_pane(&control, &args)?;
    ensure!(
        pane.len() > 1 && pane.starts_with('%') && pane[1..].chars().all(|c| c.is_ascii_digit()),
        "expected an exact pane id"
    );
    let rows = inventory(&control)?;
    let location = rows
        .iter()
        .find(|p| p.pane == pane)
        .context("source pane is not live")?;
    if args.popup {
        return crate::mux_control::agent_move_popup(
            &endpoint,
            args.caller_client.as_deref(),
            &pane,
            args.pane.is_none() && args.source_window.is_none(),
        );
    }
    let key = location.key(&endpoint);
    let direct = if let Some(name) = args.new_session {
        Some(Destination::NewSession(name))
    } else if let Some(session) = args.session {
        Some(Destination::NewWindow {
            session,
            name: args.new_window,
        })
    } else if let Some(window) = args.window {
        let dest = rows
            .iter()
            .find(|p| p.window_id == window)
            .context("destination window is not live")?;
        Some(Destination::Window {
            session: dest.session_id.clone(),
            window,
        })
    } else if args.undo {
        Some(Destination::Undo)
    } else {
        None
    };
    let outcome = if let Some(destination) = direct {
        Some(execute(MoveRequest {
            source: key.clone(),
            source_pid: location.pid.clone(),
            scope: if args.whole_window {
                MoveScope::Window
            } else {
                MoveScope::Pane
            },
            window_panes: window_members(&rows, &key),
            destination,
            direction: args.direction,
            follow: !args.stay,
        })?)
    } else {
        let mut menu = if args.whole_window {
            MoveMenu::open_scope(key, MoveScope::Window)?
        } else {
            MoveMenu::open(key)?
        };
        menu.follow = !args.stay;
        menu.direction = args.direction;
        interactive(&mut menu)?
    };
    if let Some(outcome) = outcome {
        let client = muxa::ipc::Client::new(
            daemon_socket
                .or_else(|| std::env::var_os("MUXA_SOCKET").map(std::path::PathBuf::from))
                .unwrap_or_else(muxa::paths::default_socket),
        );
        refresh_locations(&client, &outcome.pane).await;
        if outcome.follow {
            follow(&outcome.pane)?;
        } else {
            println!("{}", outcome.message);
        }
    }
    Ok(())
}

pub async fn refresh_locations(client: &muxa::ipc::Client, pane: &PaneKey) {
    // Older daemons discover the move in their periodic reconciler. A failed
    // metadata refresh must never cause the physical move to be retried.
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        client.refresh_pane_locations(&pane.pane_id),
    )
    .await;
}

pub fn follow(pane: &PaneKey) -> Result<()> {
    if let Some(client) = crate::CALLER_CLIENT.get() {
        return crate::tmux_work::focus_moved_pane(
            &Control::Endpoint(pane.window.session.endpoint.clone()),
            client,
            &pane.window.session.session_id,
            &pane.window.window_id,
            &pane.pane_id,
        );
    }
    crate::jump_to_topology_pane(pane);
    Ok(())
}

fn interactive(menu: &mut MoveMenu) -> Result<Option<MoveOutcome>> {
    use crossterm::{
        execute,
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    };
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
            let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
        }
    }
    ensure!(
        std::io::IsTerminal::is_terminal(&std::io::stdin()),
        "interactive move requires a terminal"
    );
    enable_raw_mode()?;
    let _restore = Restore;
    execute!(std::io::stdout(), EnterAlternateScreen)?;
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))?;
    loop {
        terminal.draw(|f| menu.render(f, f.area()))?;
        let event = crossterm::event::read()?;
        if let Event::Paste(text) = &event {
            menu.paste(text);
        }
        if let Event::Key(key) = event {
            match menu.on_key(key) {
                MenuEvent::Cancel => return Ok(None),
                MenuEvent::Submit(request) => match execute(*request) {
                    Ok(outcome) => return Ok(Some(outcome)),
                    Err(error) => menu.error = Some(error.to_string()),
                },
                MenuEvent::None => {}
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    struct Server {
        _directory: tempfile::TempDir,
        endpoint: BackendEndpoint,
    }

    impl Server {
        fn start(host: HostKind) -> Result<Self> {
            let directory = tempfile::tempdir()?;
            let endpoint = BackendEndpoint {
                host,
                socket: directory
                    .path()
                    .join("mux.sock")
                    .to_string_lossy()
                    .into_owned(),
            };
            let mut command = crate::mux_control::command(&endpoint).map_err(anyhow::Error::msg)?;
            let output = command
                .args([
                    "-f",
                    "/dev/null",
                    "new-session",
                    "-d",
                    "-s",
                    "source",
                    "-n",
                    "main",
                    "printf 'conversation-preserved\\n'; exec sleep 300",
                ])
                .output()?;
            ensure!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(Self {
                _directory: directory,
                endpoint,
            })
        }
        fn control(&self) -> Control {
            Control::Endpoint(self.endpoint.clone())
        }
        fn row(&self, key: &PaneKey) -> Location {
            let rows = inventory(&self.control()).unwrap();
            locate(&rows, key).unwrap().clone()
        }
        fn request(&self, key: &PaneKey, destination: Destination) -> MoveRequest {
            MoveRequest {
                source: key.clone(),
                source_pid: self.row(key).pid,
                scope: MoveScope::Pane,
                window_panes: window_members(&inventory(&self.control()).unwrap(), key),
                destination,
                direction: MoveDirection::Right,
                follow: false,
            }
        }
    }
    impl Drop for Server {
        fn drop(&mut self) {
            let _ = self.control().run(&["kill-server"]);
        }
    }

    fn available(host: HostKind) -> bool {
        match host {
            HostKind::Tmux => muxa::tmux::tmux_binary().is_file(),
            HostKind::Rmux => muxa::backend::rmux::binary_available(),
            _ => false,
        }
    }

    #[allow(clippy::too_many_lines)] // the same end-to-end contract runs on both servers
    fn exercise_live_moves(host: HostKind) -> Result<()> {
        if !available(host) {
            eprintln!("{host} binary unavailable; integration skipped");
            return Ok(());
        }
        let server = Server::start(host)?;
        let control = server.control();
        let source = inventory(&control)?[0].clone();
        // Leave another pane in the original window so undo can restore its
        // exact location, rather than guessing at a recycled session name.
        control.run(&["split-window", "-d", "-t", &source.target(), "sleep 300"])?;
        for (scope, option, value) in [
            ("", "@muxa_workspace_id", "source"),
            ("", "@muxa_managed_workspace", "1"),
            ("-w", "@muxa_work_id", "ORIGINAL"),
            ("-w", "@muxa_managed_work", "1"),
            ("-p", "@muxa_agent_workspace_id", "source"),
            ("-p", "@muxa_agent_work_id", "ORIGINAL"),
            ("-p", "@muxa_pipeline_generation", "7"),
            ("-p", "@muxa_agent_alias", "worker"),
            ("-p", "@muxa_agent_role", "implementer"),
            ("-p", "@test_conversation_id", "same-thread"),
        ] {
            set_option(&control, scope, &source.target(), option, value)?;
        }
        let key = source.key(&server.endpoint);
        let pid = source.pid.clone();
        let detached = execute(server.request(&key, Destination::NewSession("detached".into())))?;
        let row = server.row(&detached.pane);
        assert_eq!(row.pid, pid);
        assert_eq!(row.pane, source.pane);
        assert_eq!(row.pane_workspace, "detached");
        assert_eq!(row.pane_work, "WORKER");
        assert!(row.generation.is_empty());
        assert_eq!(
            control
                .output(&[
                    "show-options",
                    "-p",
                    "-v",
                    "-t",
                    &row.target(),
                    "@test_conversation_id"
                ])?
                .trim(),
            "same-thread"
        );
        assert_eq!(
            control
                .output(&[
                    "show-options",
                    "-p",
                    "-v",
                    "-t",
                    &row.target(),
                    "@muxa_agent_role"
                ])?
                .trim(),
            "implementer"
        );
        assert!(control
            .output(&["capture-pane", "-p", "-t", &row.target()])?
            .contains("conversation-preserved"));
        assert_eq!(
            inventory(&control)?
                .iter()
                .filter(|p| p.session_id == row.session_id)
                .count(),
            1,
            "placeholder was removed"
        );

        let restored = execute(server.request(&detached.pane, Destination::Undo))?;
        let row = server.row(&restored.pane);
        assert_eq!(row.pid, pid);
        assert_eq!(row.window_id, source.window_id);
        assert_eq!(row.pane_work, "ORIGINAL");
        assert_eq!(row.generation, "7");

        let dest = new_window(&control, &source.session_id, "review", &source.cwd)?;
        set_option(&control, "-w", &dest.target(), "@muxa_work_id", "REVIEW")?;
        set_option(&control, "-w", &dest.target(), "@muxa_managed_work", "1")?;
        let moved = execute(server.request(
            &restored.pane,
            Destination::Window {
                session: dest.session_id.clone(),
                window: dest.window_id.clone(),
            },
        ))?;
        assert_eq!(server.row(&moved.pane).pane_work, "REVIEW");
        assert_eq!(server.row(&moved.pane).pid, pid);
        let restored = execute(server.request(&moved.pane, Destination::Undo))?;
        // A duplicate alias refuses the operation before touching the source.
        set_option(
            &control,
            "-p",
            &dest.target(),
            "@muxa_agent_alias",
            "worker",
        )?;
        let windows_before = inventory(&control)?.len();
        assert!(execute(server.request(
            &restored.pane,
            Destination::Window {
                session: dest.session_id,
                window: dest.window_id
            }
        ))
        .unwrap_err()
        .to_string()
        .contains("alias"));
        assert_eq!(inventory(&control)?.len(), windows_before);
        assert_eq!(server.row(&restored.pane).pane_work, "ORIGINAL");

        let next = execute(server.request(
            &restored.pane,
            Destination::NewWindow {
                session: source.session_id.clone(),
                name: None,
            },
        ))?;
        assert_eq!(server.row(&next.pane).pid, pid);
        assert_ne!(next.pane.window.window_id, source.window_id);
        // Old menu requests cannot move the pane a second time.
        assert!(execute(
            server.request(&next.pane, Destination::NewSession("already-exists".into()))
        )
        .is_ok());
        assert!(MoveMenu::open(next.pane).is_err());
        Ok(())
    }

    #[test]
    fn live_tmux_moves_preserve_process_history_and_work_metadata() -> Result<()> {
        exercise_live_moves(HostKind::Tmux)
    }
    #[test]
    fn live_rmux_moves_preserve_process_history_and_work_metadata() -> Result<()> {
        exercise_live_moves(HostKind::Rmux)
    }

    #[test]
    fn menu_search_naming_cancel_and_follow_share_one_contract() -> Result<()> {
        if !available(HostKind::Tmux) {
            return Ok(());
        }
        let server = Server::start(HostKind::Tmux)?;
        let source = inventory(&server.control())?[0].key(&server.endpoint);
        let mut menu = MoveMenu::open(source)?;
        menu.paste("new session");
        assert_eq!(menu.visible().len(), 1);
        assert!(matches!(
            menu.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            MenuEvent::None
        ));
        assert!(menu.naming);
        menu.paste("separate");
        menu.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        let MenuEvent::Submit(request) =
            menu.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("submit expected")
        };
        assert!(!request.follow);
        assert!(
            matches!(request.destination, Destination::NewSession(ref name) if name == "separate")
        );
        menu.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!menu.naming);
        assert!(matches!(
            menu.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            MenuEvent::Cancel
        ));
        for (width, height) in [(80, 24), (30, 8), (8, 3)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height))?;
            terminal.draw(|frame| menu.render(frame, frame.area()))?;
        }
        Ok(())
    }

    fn topology_menu() -> MoveMenu {
        let source =
            Location::parse("$1\tsource\t@1\tmain\t%1\t101\t/tmp\tworker\t\t\t\t\t\t\t").unwrap();
        let mut review = source.clone();
        review.session_id = "$2".into();
        review.session_name = "project".into();
        review.window_id = "@2".into();
        review.window_name = "review".into();
        review.pane = "%2".into();
        let mut review_peer = review.clone();
        review_peer.pane = "%3".into();
        let mut build = review.clone();
        build.window_id = "@3".into();
        build.window_name = "build".into();
        build.pane = "%4".into();
        let choices = destination_choices(
            &[review, source.clone(), build, review_peer],
            &source,
            false,
        );
        MoveMenu {
            source: source.key(&BackendEndpoint {
                host: HostKind::Tmux,
                socket: "/tmp/test-server".into(),
            }),
            pid: source.pid.clone(),
            title: "Move worker".into(),
            choices,
            rows: vec![source.clone()],
            scope: MoveScope::Pane,
            collapsed: std::collections::BTreeSet::new(),
            selected: 2,
            query: String::new(),
            naming: false,
            follow: true,
            direction: MoveDirection::Right,
            error: None,
        }
    }

    #[test]
    fn destination_tree_keeps_session_ancestry_when_searching_windows() -> Result<()> {
        let mut menu = topology_menu();
        let mut terminal = Terminal::new(TestBackend::new(100, 24))?;
        terminal.draw(|frame| menu.render(frame, frame.area()))?;
        let display = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect::<String>();
        assert!(display.contains("▾ SESSION source · current"));
        assert!(display.contains("├─ + New window"));
        assert!(display.contains("└─ WINDOW main · 1 pane · source"));
        assert!(display.contains("├─ WINDOW review · 2 panes"));
        assert!(display.contains("└─ WINDOW build · 1 pane"));
        menu.paste("review");
        let visible = menu.visible();
        let labels: Vec<_> = visible
            .iter()
            .map(|&i| menu.row_label(i, &visible))
            .collect();
        assert_eq!(
            labels,
            ["▾ SESSION project", "  └─ WINDOW review · 2 panes"]
        );
        menu.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let MenuEvent::Submit(request) =
            menu.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("window submits a move")
        };
        assert!(
            matches!(request.destination, Destination::Window { session, window } if session == "$2" && window == "@2")
        );
        Ok(())
    }

    #[test]
    fn destination_tree_folds_and_expands_without_changing_split_direction() {
        let mut menu = topology_menu();
        menu.on_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        let visible = menu.visible();
        assert_eq!(
            menu.row_label(visible[menu.selected], &visible),
            "▸ SESSION source · current"
        );
        assert_eq!(menu.direction, MoveDirection::Right);
        assert!(!visible
            .iter()
            .any(|&i| menu.choices[i].label.contains("WINDOW main")));
        assert!(matches!(
            menu.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            MenuEvent::None
        ));
        assert!(menu
            .visible()
            .iter()
            .any(|&i| menu.choices[i].label.contains("WINDOW main")));
        menu.on_key(KeyEvent::new(KeyCode::Right, KeyModifiers::CONTROL));
        assert_eq!(menu.direction, MoveDirection::Down);
        menu.paste("project");
        assert_eq!(
            menu.visible().len(),
            4,
            "session search includes its new-window action and both windows"
        );
    }

    #[allow(clippy::too_many_lines)] // one contract verifies both native hosts, including grouped sessions
    fn exercise_whole_window(host: HostKind) -> Result<()> {
        if !available(host) {
            return Ok(());
        }
        let server = Server::start(host)?;
        let control = server.control();
        let source = inventory(&control)?[0].clone();
        control.run(&[
            "split-window",
            "-d",
            "-h",
            "-t",
            &source.target(),
            "sleep 300",
        ])?;
        control.run(&[
            "split-window",
            "-d",
            "-v",
            "-t",
            &source.target(),
            "sleep 300",
        ])?;
        control.run(&[
            "new-session",
            "-d",
            "-t",
            "source",
            "-s",
            "source~view~test",
        ])?;
        set_option(
            &control,
            "",
            &source.session_id,
            "@muxa_workspace_id",
            "source",
        )?;
        set_option(
            &control,
            "",
            &source.session_id,
            "@muxa_managed_workspace",
            "1",
        )?;
        set_option(
            &control,
            "-w",
            &source.target(),
            "@muxa_work_id",
            "TICKET-42",
        )?;
        set_option(&control, "-w", &source.target(), "@muxa_managed_work", "1")?;
        for row in inventory(&control)?
            .iter()
            .filter(|row| row.session_id == source.session_id && row.window_id == source.window_id)
        {
            for (option, value) in [
                ("@muxa_agent_workspace_id", "source"),
                ("@muxa_agent_work_id", "TICKET-42"),
                ("@muxa_pipeline_generation", "7"),
                ("@muxa_agent_role", "implementer"),
            ] {
                set_option(&control, "-p", &row.target(), option, value)?;
            }
        }
        let key = source.key(&server.endpoint);
        let layout = control.output(&[
            "display-message",
            "-p",
            "-t",
            &source.target(),
            "#{window_layout}",
        ])?;
        let members = window_members(&inventory(&control)?, &key);
        let mut request = server.request(&key, Destination::NewSession("detached".into()));
        request.scope = MoveScope::Window;
        let detached = execute(request)?;
        assert_eq!(detached.pane.window.window_id, source.window_id);
        assert_eq!(
            window_members(&inventory(&control)?, &detached.pane),
            members
        );
        assert_eq!(
            control.output(&[
                "display-message",
                "-p",
                "-t",
                &server.row(&detached.pane).target(),
                "#{window_layout}"
            ])?,
            layout
        );
        assert!(!inventory(&control)?.iter().any(|row| row.session_id
            != detached.pane.window.session.session_id
            && row.window_id == source.window_id));
        for row in inventory(&control)?.iter().filter(|row| {
            row.session_id == detached.pane.window.session.session_id
                && row.window_id == source.window_id
        }) {
            assert_eq!(row.pane_workspace, "detached");
            assert_eq!(row.pane_work, "TICKET-42");
            assert!(row.generation.is_empty());
            assert_eq!(
                control
                    .output(&[
                        "show-options",
                        "-p",
                        "-v",
                        "-t",
                        &row.target(),
                        "@muxa_agent_role"
                    ])?
                    .trim(),
                "implementer"
            );
        }
        let mut request = server.request(&detached.pane, Destination::Undo);
        request.scope = MoveScope::Window;
        let restored = execute(request)?;
        assert_eq!(restored.pane, key);
        assert_eq!(server.row(&key).generation, "7");
        let destination = new_session(&control, "destination", &source.cwd)?;
        control.run(&["rename-window", "-t", &destination.target(), "main"])?;
        set_option(
            &control,
            "-w",
            &destination.target(),
            "@muxa_work_id",
            "TICKET-42",
        )?;
        let mut conflicting = server.request(
            &key,
            Destination::NewWindow {
                session: destination.session_id.clone(),
                name: None,
            },
        );
        conflicting.scope = MoveScope::Window;
        assert!(execute(conflicting)
            .unwrap_err()
            .to_string()
            .contains("Work"));
        assert_eq!(window_members(&inventory(&control)?, &key), members);
        assert_eq!(server.row(&key).generation, "7");
        set_option(&control, "-w", &destination.target(), "@muxa_work_id", "")?;
        let before = inventory(&control)?
            .into_iter()
            .filter(|row| row.session_id == destination.session_id)
            .map(|row| (row.window_id, row.pane, row.pid))
            .collect::<Vec<_>>();
        let mut request = server.request(
            &key,
            Destination::NewWindow {
                session: destination.session_id.clone(),
                name: None,
            },
        );
        request.scope = MoveScope::Window;
        let moved = execute(request)?;
        assert_eq!(moved.pane.window.window_id, source.window_id);
        assert_eq!(server.row(&moved.pane).window_name, "main-2");
        assert_eq!(window_members(&inventory(&control)?, &moved.pane), members);
        for (window, pane, pid) in before {
            assert!(inventory(&control)?.iter().any(|row| row.session_id
                == destination.session_id
                && row.window_id == window
                && row.pane == pane
                && row.pid == pid));
        }
        let mut request = server.request(&moved.pane, Destination::Undo);
        request.scope = MoveScope::Window;
        let restored = execute(request)?;
        assert_eq!(restored.pane, key);
        assert_eq!(server.row(&key).window_name, "main");
        let menu = MoveMenu::open_scope(key.clone(), MoveScope::Window)?;
        assert!(menu.title.contains("whole WINDOW"));
        assert!(!menu
            .choices
            .iter()
            .any(|choice| matches!(choice.destination, Some(Destination::Window { .. }))));
        let mut request = server.request(&key, Destination::NewSession("stale-menu".into()));
        request.scope = MoveScope::Window;
        control.run(&["split-window", "-d", "-t", &source.target(), "sleep 300"])?;
        assert!(execute(request)
            .unwrap_err()
            .to_string()
            .contains("changed"));
        assert!(!inventory(&control)?
            .iter()
            .any(|row| row.session_name == "stale-menu"));
        Ok(())
    }

    #[test]
    fn whole_window_tmux_preserves_all_processes_layout_and_window_id() -> Result<()> {
        exercise_whole_window(HostKind::Tmux)
    }
    #[test]
    fn whole_window_rmux_preserves_all_processes_layout_and_window_id() -> Result<()> {
        exercise_whole_window(HostKind::Rmux)
    }

    #[test]
    fn popup_scope_switch_offers_session_actions_for_whole_windows() -> Result<()> {
        if !available(HostKind::Tmux) {
            return Ok(());
        }
        let server = Server::start(HostKind::Tmux)?;
        let control = server.control();
        let source = inventory(&control)?[0].clone();
        control.run(&["split-window", "-d", "-t", &source.target(), "sleep 300"])?;
        new_session(&control, "destination", &source.cwd)?;
        let mut menu = MoveMenu::open(source.key(&server.endpoint))?;
        menu.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(menu.scope, MoveScope::Window);
        assert!(!menu
            .choices
            .iter()
            .any(|choice| matches!(choice.destination, Some(Destination::Window { .. }))));
        menu.paste("destination");
        menu.on_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let MenuEvent::Submit(request) =
            menu.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("session action submits")
        };
        assert_eq!(request.scope, MoveScope::Window);
        assert_eq!(request.window_panes.len(), 2);
        menu.on_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(menu.scope, MoveScope::Pane);
        Ok(())
    }

    #[test]
    fn cli_move_keeps_daemon_and_terminal_sockets_distinct() {
        use clap::Parser;
        let args = crate::Args::try_parse_from([
            "muxa",
            "--socket",
            "/tmp/daemon",
            "agent",
            "move",
            "--server-socket",
            "/tmp/terminal",
            "--host",
            "tmux",
            "--pane",
            "%1",
            "--session",
            "$2",
            "--new-window",
            "review",
            "--stay",
        ])
        .unwrap();
        assert_eq!(args.socket.unwrap().to_str(), Some("/tmp/daemon"));
        let crate::Cmd::Agent {
            action: crate::AgentCmd::Move(args),
        } = args.cmd
        else {
            panic!("move expected")
        };
        assert_eq!(args.server_socket.as_deref(), Some("/tmp/terminal"));
        assert!(args.stay);
    }

    #[test]
    fn cli_whole_window_move_parses_an_exact_source_window() {
        use clap::Parser;
        let args = crate::Args::try_parse_from([
            "muxa",
            "window",
            "move",
            "--source-window",
            "@13",
            "--session",
            "dev",
            "--stay",
        ])
        .unwrap();
        let crate::Cmd::Window {
            action: crate::WindowCmd::Move(args),
        } = args.cmd
        else {
            panic!("window move expected")
        };
        assert_eq!(args.source_window.as_deref(), Some("@13"));
        assert_eq!(args.session.as_deref(), Some("dev"));
    }

    #[test]
    fn final_pane_keeps_its_source_session_and_can_undo() -> Result<()> {
        for host in [HostKind::Tmux, HostKind::Rmux] {
            if !available(host) {
                continue;
            }
            let server = Server::start(host)?;
            let source = inventory(&server.control())?[0].clone();
            let key = source.key(&server.endpoint);
            let outcome =
                execute(server.request(&key, Destination::NewSession("separate".into())))?;
            assert!(inventory(&server.control())?
                .iter()
                .any(|p| p.session_id == source.session_id && p.pane != source.pane));
            let restored = execute(server.request(&outcome.pane, Destination::Undo))?;
            assert_eq!(restored.pane, key);
            assert_eq!(server.row(&restored.pane).pid, source.pid);
        }
        Ok(())
    }

    #[test]
    fn failed_transfer_restores_metadata_and_removes_created_window() -> Result<()> {
        if !available(HostKind::Tmux) {
            return Ok(());
        }
        let server = Server::start(HostKind::Tmux)?;
        let control = server.control();
        let source = inventory(&control)?[0].clone();
        control.run(&["split-window", "-d", "-t", &source.target(), "sleep 300"])?;
        set_option(
            &control,
            "-p",
            &source.target(),
            "@muxa_agent_work_id",
            "ORIGINAL",
        )?;
        control.run(&["set-option", "-g", "default-size", "2x2"])?;
        let rows_before = inventory(&control)?.len();
        let key = source.key(&server.endpoint);
        let error = execute(server.request(
            &key,
            Destination::NewWindow {
                session: source.session_id,
                name: Some("too-small".into()),
            },
        ))
        .unwrap_err();
        assert!(
            error.to_string().contains("space") || error.to_string().contains("small"),
            "{error}"
        );
        assert_eq!(inventory(&control)?.len(), rows_before);
        assert_eq!(server.row(&key).pane_work, "ORIGINAL");
        assert_eq!(server.row(&key).pid, source.pid);
        Ok(())
    }
}
