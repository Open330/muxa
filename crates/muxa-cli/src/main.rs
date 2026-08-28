//! muxa CLI — user-facing entry point.

mod activity_query;
mod agent_launch;
mod attend;
mod collab_screen;
mod daemon;
mod dashboard_tui;
mod doctor;
mod fleet_cli;
mod fleet_watch;
mod init;
mod logs;
mod mcp;
mod message_skill;
mod onboarding;
mod peek;
mod relay;
mod stats;
mod theme;
mod time_range;
mod timeline;
mod tmux_work;
mod upgrade;
mod watch;
mod work_init;
mod work_up;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use comfy_table::presets::UTF8_BORDERS_ONLY;
use comfy_table::{Cell, ColumnConstraint, ContentArrangement, Table, Width};
use muxa::adapters::{
    claude, run_hook, AntigravityAdapter, ClaudeAdapter, CodexAdapter, GeminiAdapter,
    OpencodeAdapter,
};
use muxa::collaboration::{
    AirArtifactReference, CollaborationClientKind, CollaborationOrigin, CollaborationOriginMatch,
    MailboxScope, NewRequest, Participant, RequestKind, RequestMailbox, RequestStatus, WorkMode,
};
use muxa::config::{IconSet, WatchConfig, WatchSortKey, WatchTheme};
use muxa::ipc::Client;
use muxa::state::Agent;
use muxa::{
    discovery, paths, tmux, ActivityEntry, AgentKind, AgentState, Config, HumanInteractionEntry,
    HumanInteractionInput, HumanInteractionKind,
};
use owo_colors::Style;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use theme::{CliTheme, TableTone, ThemeArg};
use time::OffsetDateTime;
use unicode_width::UnicodeWidthChar;

const DEFAULT_TERMINAL_WIDTH: usize = 120;
const FULL_STATUS_TABLE_WIDTH: usize = 120;
const COMPACT_STATUS_TABLE_WIDTH: usize = 76;
const MIN_STATUS_PROMPT_WIDTH: usize = 8;
const MAX_STATUS_PROMPT_WIDTH: usize = 60;
const STATUS_LINE_IPC_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, Parser)]
#[command(name = "muxa", version, about = "muxa CLI")]
struct Args {
    #[arg(long, env = "MUXA_SOCKET", global = true)]
    socket: Option<PathBuf>,

    /// Config file path. Defaults to `$XDG_CONFIG_HOME/muxa/config.toml`.
    #[arg(long, env = "MUXA_CONFIG", global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// List active agents as a human-readable table.
    Status {
        /// One-shot visual theme override.
        #[arg(long, value_enum, conflicts_with = "json")]
        theme: Option<ThemeArg>,
        /// Emit a stable JSON snapshot for desktop widgets and other integrations.
        #[arg(long)]
        json: bool,
    },
    /// Print a one-liner status suitable for tmux `status-right`.
    StatusLine {
        #[arg(long)]
        pane: Option<String>,
        /// Emit a GLOBAL attention summary (`⚠ N need you`) counting every
        /// tracked agent that's blocked on a human, instead of the per-pane
        /// detail. Prints an empty line when nothing is blocked, so the tmux
        /// segment disappears when all-clear. Ignores `--pane`.
        #[arg(long)]
        needs_attention: bool,
    },
    /// Show recent prompts for the given pane (default: `$TMUX_PANE`).
    ///
    /// Combines the live agent record (current `last_prompt`) with the
    /// disk-backed history audit log, so prompts persist across pane
    /// closes, agent restarts, and daemon restarts.
    Recap {
        /// Pane id to recap. Defaults to `$TMUX_PANE` when omitted.
        #[arg(long)]
        pane: Option<String>,
        /// Number of historical prompts to show. Defaults to 10.
        /// Use `--all` to ignore the cap.
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Show every prompt the daemon has on file for this pane.
        /// Overrides `--limit`.
        #[arg(long, conflicts_with = "limit")]
        all: bool,
    },
    /// List collaboration peers in the current tmux window.
    Peers {
        /// Emit the full room context as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Durable request/reply messaging with agents in the current tmux window.
    Msg {
        #[command(subcommand)]
        action: MsgCmd,
    },
    /// Register reusable `/` templates for the interactive message composer.
    Skill(message_skill::Args),
    /// Manage local/SSH host inventory and Kubernetes-style metadata.
    Host(fleet_cli::HostArgs),
    /// Observe and control this node plus SSH-connected Muxa hosts.
    Fleet(fleet_cli::FleetArgs),
    /// Deterministic tmux agent lifecycle operations.
    Agent {
        #[command(subcommand)]
        action: AgentCmd,
    },
    /// Manage tmux windows using their stable native identity.
    Window {
        #[command(subcommand)]
        action: WindowCmd,
    },
    /// Manage Muxa Work and its current tmux Run window.
    Work {
        #[command(subcommand)]
        action: WorkCmd,
    },
    /// Manage workspace/project tmux sessions.
    Workspace {
        #[command(subcommand)]
        action: WorkspaceCmd,
    },
    /// Register a room-local alias and roles for this exact agent session.
    Identity {
        #[command(subcommand)]
        action: IdentityCmd,
    },
    /// Summarize retained prompt history, live agents, and session duration.
    Stats(stats::Args),
    /// Generate a Markdown activity report from the retained stats.
    Report(stats::ReportArgs),
    /// Explore agent work/wait/error intervals as an interactive timeline.
    Timeline(timeline::Args),
    /// Work-board TUI for tracking Work, Runs, external issues, and agents.
    Dashboard(dashboard_tui::Args),
    /// Query raw activity ledger intervals.
    Activity(activity_query::Args),
    /// Hook adapter entrypoints invoked by the agent CLIs themselves.
    Hook {
        #[command(subcommand)]
        which: HookCmd,
    },
    /// Debug: print the active pane-host inventory.
    Panes,
    /// Overlay every pane in the current tmux window with its agent's
    /// state, summary, latest prompt, and latest response — then jump to
    /// one by its `display-panes` digit.
    ///
    /// Designed to be bound to `prefix + Q` via a borderless fullscreen
    /// `display-popup` (see `muxa init --only tmux-peek`).
    Peek(peek::Args),
    /// Fullscreen nested session/window/pane topology of tracked agents.
    Watch {
        /// Show the SSH fleet host/session/window/pane hierarchy instead of
        /// the local topology.
        #[arg(long)]
        fleet: bool,
        /// Kubernetes-style host label selector used with `--fleet`.
        #[arg(short = 'l', long = "selector", requires = "fleet")]
        selector: Option<String>,
        /// Show agents that have no tmux pane attached. Default behavior
        /// (governed by `[watch] hide_paneless = true`) hides them
        /// because Enter on the picker can't attach to them anyway —
        /// the footer surfaces a count instead. This flag flips the
        /// filter off for one invocation, e.g. when debugging a
        /// detached SDK session.
        #[arg(long)]
        include_paneless: bool,
        /// Default expansion depth: session, window (default), or pane.
        #[arg(long, value_enum)]
        view: Option<WatchViewArg>,
        /// Presentation of the same topology: nested tree (default) or swarm.
        #[arg(long, value_enum)]
        layout: Option<WatchLayoutArg>,
        /// Which list to open on: the topology (default), or collaboration
        /// across every room the daemon holds.
        #[arg(long, value_enum)]
        screen: Option<WatchScreenArg>,
        /// One-shot sibling sort: name, latest/activity/act, duration/dur, st/state, pane, or pane-id.
        #[arg(long, value_enum)]
        sort: Option<WatchSortArg>,
        /// One-shot visual theme override.
        #[arg(long, value_enum)]
        theme: Option<ThemeArg>,
        /// tmux client that pressed the key, expanded by the binding
        /// (`#{client_name}`). Inside a `display-popup` every unpinned
        /// tmux query resolves against whichever client was last active —
        /// with two terminals attached, routinely the wrong one — so the
        /// binding passes the answer in at keypress time.
        #[arg(long, value_name = "CLIENT")]
        caller_client: Option<String>,
        /// Pane the key was pressed in, expanded by the binding
        /// (`#{pane_id}`). Seeds the collaboration room and the opening
        /// cursor; same rationale as `--caller-client`.
        #[arg(long, value_name = "PANE")]
        caller_pane: Option<String>,
    },
    /// Run a command in a muxa-owned PTY session.
    Run {
        /// Human-readable session name.
        #[arg(long)]
        name: Option<String>,
        /// Working directory for the child process. Defaults to the current directory.
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Spawn the session and return without attaching.
        #[arg(long)]
        detach: bool,
        /// Command and arguments to run. Use `--` before commands with flags.
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Attach this terminal to a muxa-owned PTY session.
    Attach { session: String },
    /// Mark a muxa-owned PTY session as detached.
    Detach { session: String },
    /// Register an arbitrary background process (shell script, game,
    /// automation loop) so it shows up in `muxa status`/`muxa watch`,
    /// tracked by pid liveness. Defaults `--pid` to the calling shell.
    Register {
        /// Display name shown in the NAME column.
        #[arg(long)]
        name: String,
        /// Process id to track. Defaults to the parent process (the shell
        /// that invoked `muxa register`).
        #[arg(long)]
        pid: Option<u32>,
        /// Working directory to record (informational).
        #[arg(long)]
        cwd: Option<String>,
        /// tmux pane id to associate, when the process lives in one.
        #[arg(long)]
        pane: Option<String>,
    },
    /// Internal bridge used by the zellij WASM plugin.
    #[command(hide = true)]
    ZellijPluginSnapshot {
        #[arg(long)]
        json: String,
    },
    /// Bridge the owner-only local muxad socket over an authenticated SSH
    /// stdio channel for Muxa Fleet. Usually launched by another muxad.
    Relay {
        #[arg(long, default_value_t = true)]
        stdio: bool,
    },
    /// Exact remote pane attach endpoint used by `muxa fleet attach`.
    #[command(hide = true)]
    FleetRemoteAttach { token: String },
    /// Jump to the agent that needs you — focus the pane of whichever
    /// agent has been blocked on input/choice/error longest. `--cycle`
    /// rotates through them (bind it to a tmux key); `--list` prints the
    /// queue without jumping.
    #[command(visible_alias = "go")]
    Attend(attend::Args),
    /// Backfill the registry by scanning tmux panes for agent processes.
    Sync,
    /// Interactive install wizard — wires tmux, agent hooks, systemd,
    /// and the dashboard. Use `--preset standard --yes` for one-shot
    /// non-interactive installs.
    Init(init::Args),
    /// Run end-to-end diagnostics and report any setup issues.
    Doctor,
    /// Start, stop, restart, or inspect the local muxad process.
    Daemon {
        #[command(subcommand)]
        action: daemon::Action,
    },
    /// Learn the work/session, agent/pane policy, normal workflow, and watch shortcuts.
    Onboard(onboarding::Args),
    /// Run a Model Context Protocol (MCP) stdio server so a coding agent
    /// can orchestrate muxa — inspect other agents, send them prompts,
    /// capture panes, and wait for state changes. Wire it into Claude Code
    /// with `claude mcp add --scope user muxa -- muxa mcp`. Refuses to start if the
    /// daemon socket is unreachable. See docs/MCP.md.
    Mcp,
    /// Tail muxad's stdout/stderr logs without remembering paths.
    /// Falls back to `journalctl --user -u muxad` on Linux when the
    /// systemd unit is the source of truth.
    Logs(logs::Args),
    /// Update muxa from the source repo: `git pull` → cargo install
    /// `muxad` + `muxa-cli` → restart the daemon → verify the IPC
    /// socket is responsive. One command for the full update flow.
    Upgrade(upgrade::Args),
    /// Delete accumulated "orphan" agent rows — paneless, surfaceless,
    /// pid-less ghosts left by remote/detached sessions (e.g. codex driven
    /// through a detached `app-server`). Only registry rows are removed;
    /// tmux sessions are never touched. The daemon also ages these out on
    /// its own after `[reconciler] paneless_stale_timeout_secs` (24h default).
    Prune {
        /// Only prune rows idle at least this long (e.g. `30m`, `1h`, `24h`).
        /// Spares recently-active sessions. Ignored when `--all` is set.
        #[arg(long, default_value = "1h")]
        older_than: String,
        /// Prune every orphan row regardless of age.
        #[arg(long)]
        all: bool,
        /// Skip the confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum MsgCmd {
    /// Send to `peer`, `%N`, `@alias`, or `role:<role>`.
    Send {
        target: String,
        body: String,
        #[arg(long, default_value = "question")]
        kind: String,
        /// Explicitly authorize edits; the default collaboration contract is read-only.
        #[arg(long)]
        execute: bool,
        /// Advisory writable path scope. Repeat for multiple paths.
        #[arg(long = "path")]
        paths: Vec<String>,
        /// AIR 1 artifact reference as JSON. Repeat for multiple references.
        #[arg(long = "air-ref")]
        air_refs: Vec<String>,
        /// Fire-and-forget message; do not expect a reply.
        #[arg(long)]
        no_reply: bool,
        /// Print the stored request instead of a one-line receipt.
        #[arg(long)]
        json: bool,
    },
    /// Claim and print this agent's pending requests.
    Inbox {
        #[arg(long)]
        json: bool,
    },
    /// List incoming, sent, or all requests without claiming them.
    List {
        #[arg(long, default_value = "all")]
        mailbox: String,
        /// How wide to look: `caller` (this pane's own mailbox), `room` (every
        /// participant in this window), or `all` (every room this daemon
        /// holds). Anything past `caller` speaks as the operator console.
        #[arg(long, default_value = "caller")]
        scope: String,
        #[arg(long)]
        json: bool,
    },
    /// Complete, block, decline, or fail a request.
    Reply {
        request_id: String,
        body: String,
        #[arg(long, default_value = "completed")]
        status: String,
        #[arg(long = "artifact")]
        artifacts: Vec<String>,
        /// AIR 1 artifact reference as JSON. Repeat for multiple references.
        #[arg(long = "air-ref")]
        air_refs: Vec<String>,
        /// Print the stored request instead of a one-line receipt.
        #[arg(long)]
        json: bool,
    },
    /// Wait for the structured reply to a sent request.
    Wait {
        request_id: String,
        /// Maximum wait in seconds (clamped to 600).
        #[arg(long, default_value_t = 300)]
        timeout_secs: u64,
    },
    /// Cancel a request that the recipient has not claimed yet.
    Cancel {
        request_id: String,
        /// Print the stored request instead of a one-line receipt.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum AgentCmd {
    /// Start an allowlisted agent in tmux or a muxa-owned PTY session.
    Start(agent_launch::StartArgs),
    /// Interrupt or terminate one muxa-managed tmux pane or native PTY session.
    Control(tmux_work::AgentControlArgs),
}

#[derive(Debug, Subcommand)]
enum WindowCmd {
    /// Set a stable display name, or restore tmux's automatic process name.
    Rename(tmux_work::WindowRenameArgs),
}

#[derive(Debug, Subcommand)]
enum WorkCmd {
    /// Describe a work pipeline in your own words and let an agent write
    /// the `[ticket]`/`[[route]]`/`[pipeline.*]` config for you. Validated
    /// and shown before anything is written.
    Init(work_init::InitArgs),
    /// Converge a Work's current Run to its pipeline: optionally link an
    /// external issue, route the Work, and create missing agent sessions.
    /// Re-running converges instead of duplicating.
    Up(work_up::UpArgs),
    /// Create a work window with its first agent, or add an agent when it exists.
    Start(agent_launch::WorkStartArgs),
    /// List muxa-managed work windows.
    List(tmux_work::WorkListArgs),
    /// Show one work window and its agent panes.
    Show(tmux_work::WorkShowArgs),
    /// Report that this agent finished its part of the work, which opens
    /// any `after` edge waiting on it. Run it from the agent's own pane.
    Done(tmux_work::WorkDoneArgs),
    /// Internal daemon callback: launch dependency-ready durable aliases.
    #[command(hide = true)]
    Reconcile(work_up::ReconcileArgs),
    /// Close a work window and every agent pane in it.
    Close(tmux_work::WorkCloseArgs),
    /// Counterpart to `up`: close the work window and every agent in it.
    Down(tmux_work::WorkCloseArgs),
}

#[derive(Debug, Subcommand)]
enum WorkspaceCmd {
    /// List muxa-managed workspace sessions.
    List(tmux_work::WorkspaceListArgs),
    /// Show one workspace session with its work windows.
    Show(tmux_work::WorkspaceShowArgs),
    /// Close a workspace session, including every work and agent.
    Close(tmux_work::WorkspaceCloseArgs),
    /// Give this terminal its own view of a workspace, so two terminals on one
    /// session stop following each other's window switches.
    View(tmux_work::WorkspaceViewArgs),
}

#[derive(Debug, Subcommand)]
enum IdentityCmd {
    /// Show this agent's current identity and room peers.
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Replace this agent's alias and role set.
    Set {
        #[arg(long)]
        alias: Option<String>,
        #[arg(long = "role")]
        roles: Vec<String>,
    },
    /// Remove this exact agent session's alias and roles.
    Clear,
}

#[derive(Debug, Subcommand)]
enum HookCmd {
    /// Claude Code hook handler. Reads hook JSON on stdin.
    Claude {
        #[arg(long)]
        event: String,
    },
    /// Claude Code status-line feeder: emit a Heartbeat and print a
    /// one-liner back to stdout (so it remains a valid status line script).
    ///
    /// With `--forward <CMD>`, the captured stdin is tee'd to the given
    /// command (run via `/bin/sh -c`) and its stdout/exit code are passed
    /// through unchanged — useful for layering muxa on top of tools like
    /// `ccstatusline` without giving up their rendering.
    ClaudeStatusline {
        /// Forward stdin to this shell command and pass through its stdout.
        #[arg(long, value_name = "CMD")]
        forward: Option<String>,
    },
    /// Codex hook handler.
    Codex {
        #[arg(long)]
        event: String,
    },
    /// Gemini CLI hook handler.
    Gemini {
        #[arg(long)]
        event: String,
    },
    /// Antigravity CLI (`agy`) hook handler. Reads hook JSON on stdin.
    ///
    /// Writes NOTHING to stdout on any event: agy reads a hook's stdout as a
    /// verdict, and a `PreToolUse` reply without a valid `decision` blocks the
    /// tool call outright.
    Agy {
        #[arg(long)]
        event: String,
    },
    /// opencode hook handler.
    Opencode {
        #[arg(long)]
        event: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum WatchViewArg {
    Session,
    Window,
    Pane,
}

impl From<WatchViewArg> for muxa::config::WatchView {
    fn from(value: WatchViewArg) -> Self {
        match value {
            WatchViewArg::Session => Self::Session,
            WatchViewArg::Window => Self::Window,
            WatchViewArg::Pane => Self::Pane,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum WatchLayoutArg {
    Tree,
    Swarm,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum WatchScreenArg {
    Topology,
    Collab,
}

impl From<WatchScreenArg> for muxa::config::WatchScreen {
    fn from(value: WatchScreenArg) -> Self {
        match value {
            WatchScreenArg::Topology => Self::Topology,
            WatchScreenArg::Collab => Self::Collab,
        }
    }
}

impl From<WatchLayoutArg> for muxa::config::WatchLayout {
    fn from(value: WatchLayoutArg) -> Self {
        match value {
            WatchLayoutArg::Tree => Self::Tree,
            WatchLayoutArg::Swarm => Self::Swarm,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum WatchSortArg {
    Name,
    #[value(alias = "activity", alias = "act")]
    Latest,
    #[value(alias = "dur")]
    Duration,
    #[value(alias = "st")]
    State,
    Pane,
    #[value(alias = "pane_id")]
    PaneId,
}

impl WatchSortArg {
    fn keys(self) -> Vec<WatchSortKey> {
        match self {
            Self::Name => vec![WatchSortKey::Name, WatchSortKey::Activity],
            Self::Latest => vec![WatchSortKey::Activity],
            Self::Duration => vec![WatchSortKey::Duration],
            Self::State => vec![WatchSortKey::State, WatchSortKey::Activity],
            Self::Pane => vec![WatchSortKey::Name, WatchSortKey::Pane],
            Self::PaneId => vec![WatchSortKey::PaneId],
        }
    }
}

async fn run_agent_cmd(action: AgentCmd, client: &Client, socket_path: &Path) -> Result<()> {
    match action {
        AgentCmd::Start(args) => agent_launch::run(args, client, socket_path).await,
        AgentCmd::Control(args) => tmux_work::run_agent_control(args, client).await,
    }
}

fn run_window_cmd(action: WindowCmd) -> Result<()> {
    match action {
        WindowCmd::Rename(args) => tmux_work::run_window_rename(args),
    }
}

async fn run_work_cmd(
    action: WorkCmd,
    cfg: &Config,
    config_path: Option<PathBuf>,
    client: &Client,
) -> Result<()> {
    match action {
        WorkCmd::Init(args) => work_init::run(args, cfg, config_path).await,
        WorkCmd::Up(args) => work_up::run(args, cfg, config_path, Some(client)).await,
        WorkCmd::Start(args) => agent_launch::run_work_start(args),
        WorkCmd::List(args) => tmux_work::run_work_list(args, client).await,
        WorkCmd::Show(args) => tmux_work::run_work_show(args),
        WorkCmd::Done(args) => tmux_work::run_work_done(args, client).await,
        WorkCmd::Reconcile(args) => work_up::run_reconcile(args, client).await,
        WorkCmd::Close(args) | WorkCmd::Down(args) => tmux_work::run_work_close(args),
    }
}

fn run_workspace_cmd(action: WorkspaceCmd) -> Result<()> {
    match action {
        WorkspaceCmd::List(args) => tmux_work::run_workspace_list(args),
        WorkspaceCmd::Show(args) => tmux_work::run_workspace_show(args),
        WorkspaceCmd::Close(args) => tmux_work::run_workspace_close(args),
        WorkspaceCmd::View(args) => tmux_work::run_workspace_view(args),
    }
}

fn collaboration_client_kind(command: &Cmd) -> CollaborationClientKind {
    match command {
        Cmd::Watch { .. } => CollaborationClientKind::Watch,
        Cmd::Mcp => CollaborationClientKind::Mcp,
        Cmd::Dashboard(_) => CollaborationClientKind::Dashboard,
        _ => CollaborationClientKind::Cli,
    }
}

#[allow(clippy::too_many_lines)] // top-level CLI subcommand wiring
#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "muxa=warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();
    // Onboarding must remain available even when config.toml is malformed;
    // it is a recovery/learning surface and does not need daemon state.
    if let Cmd::Onboard(onboard_args) = &args.cmd {
        return onboarding::run(onboard_args.clone());
    }
    let config_path = args.config.clone().or_else(paths::default_config_file);
    let skill_path = config_path.clone();
    let cfg = match Config::load_or_default(config_path.as_deref()) {
        Ok(cfg) => cfg,
        // A hook handler must not hard-fail on an unreadable config. agy
        // reads a non-zero hook exit as its verdict
        // (`tool call denied by pre-tool hook`), so one TOML typo in
        // config.toml would otherwise block every tool call in every agy
        // session — with nothing on screen naming the cause. Degrade to
        // defaults and put the reason on stderr, never stdout (stdout is
        // the verdict channel).
        Err(e) if matches!(args.cmd, Cmd::Hook { .. }) => {
            eprintln!("muxa: config unreadable ({e:#}); this hook is using defaults");
            Config::default()
        }
        Err(e) => return Err(e).context("loading config"),
    };
    set_icon_set(cfg.ui.icons);
    let socket = args
        .socket
        .or_else(|| cfg.socket.clone())
        .unwrap_or_else(paths::default_socket);
    let client = Client::new(socket.clone())
        .with_collaboration_client_kind(collaboration_client_kind(&args.cmd));

    match args.cmd {
        Cmd::Status { theme, json } => cmd_status(&client, &cfg, theme, json).await,
        Cmd::StatusLine {
            pane,
            needs_attention,
        } => cmd_status_line(&client, pane, needs_attention).await,
        Cmd::Recap { pane, limit, all } => cmd_recap(&client, pane, limit, all).await,
        Cmd::Peers { json } => cmd_peers(&client, json).await,
        Cmd::Msg { action } => cmd_msg(&client, action).await,
        Cmd::Skill(a) => message_skill::run(a, &cfg.message, skill_path.as_deref()),
        Cmd::Host(a) => fleet_cli::run_host(a, &client, &cfg, config_path.as_deref()).await,
        Cmd::Fleet(a) => fleet_cli::run_fleet(a, &client, &cfg, config_path.as_deref()).await,
        Cmd::Agent { action } => run_agent_cmd(action, &client, &socket).await,
        Cmd::Window { action } => run_window_cmd(action),
        Cmd::Work { action } => run_work_cmd(action, &cfg, config_path, &client).await,
        Cmd::Workspace { action } => run_workspace_cmd(action),
        Cmd::Identity { action } => cmd_identity(&client, action).await,
        Cmd::Stats(stats_args) => stats::run(&client, &cfg, stats_args).await,
        Cmd::Report(report_args) => stats::run_report(&client, &cfg, report_args).await,
        Cmd::Timeline(timeline_args) => timeline::run(&client, &cfg, timeline_args).await,
        Cmd::Dashboard(dashboard_args) => cmd_dashboard(&client, &cfg, dashboard_args).await,
        Cmd::Activity(activity_args) => activity_query::run(&cfg, activity_args).await,
        Cmd::Hook { which } => handle_hook(&client, which).await,
        Cmd::Panes => cmd_panes(),
        Cmd::Peek(peek_args) => peek::run(&client, peek_args).await,
        Cmd::Watch {
            fleet,
            selector,
            include_paneless,
            view,
            layout,
            screen,
            sort,
            theme,
            caller_client,
            caller_pane,
        } => {
            // Pin focus-moving commands to the requesting client. A value
            // still containing `#{` is a binding that never expanded.
            if let Some(client_name) = caller_client.filter(|c| !c.contains("#{")) {
                let _ = CALLER_CLIENT.set(client_name);
            }
            if fleet {
                return cmd_fleet_watch(
                    &client,
                    cfg,
                    config_path.clone(),
                    selector,
                    WatchInvocation {
                        include_paneless,
                        view,
                        layout,
                        screen,
                        sort,
                        theme,
                        caller_pane: caller_pane.filter(|p| p.starts_with('%')),
                    },
                )
                .await;
            }
            cmd_watch(
                &client,
                cfg,
                config_path.clone(),
                WatchInvocation {
                    include_paneless,
                    view,
                    layout,
                    screen,
                    sort,
                    theme,
                    // Same unexpanded-format guard as the client: a literal
                    // `#{pane_id}` would seed the room and cursor with a
                    // pane that matches nothing.
                    caller_pane: caller_pane.filter(|p| p.starts_with('%')),
                },
            )
            .await
        }
        Cmd::Run {
            name,
            cwd,
            detach,
            command,
        } => cmd_run(&client, &socket, name, cwd, detach, command).await,
        Cmd::Attach { session } => cmd_attach(&client, &session).await,
        Cmd::Detach { session } => cmd_detach(&client, &session).await,
        Cmd::Register {
            name,
            pid,
            cwd,
            pane,
        } => cmd_register(&client, name, pid, cwd, pane).await,
        Cmd::ZellijPluginSnapshot { json } => cmd_zellij_plugin_snapshot(&client, &json).await,
        Cmd::Relay { stdio } => {
            if !stdio {
                anyhow::bail!("only --stdio relay transport is supported");
            }
            relay::run(client).await
        }
        Cmd::FleetRemoteAttach { token } => relay::remote_attach(&token),
        Cmd::Attend(attend_args) => cmd_attend(&client, attend_args).await,
        Cmd::Sync => cmd_sync(&client).await,
        Cmd::Init(init_args) => init::run(init_args, socket, config_path).await,
        Cmd::Doctor => doctor::run(socket).await,
        Cmd::Daemon { action } => {
            daemon::run(action, &client, &socket, config_path.as_deref()).await
        }
        Cmd::Onboard(onboard_args) => onboarding::run(onboard_args),
        Cmd::Mcp => mcp::run(client, cfg).await,
        Cmd::Logs(logs_args) => logs::run(logs_args).await,
        Cmd::Upgrade(upgrade_args) => upgrade::run(upgrade_args, socket).await,
        Cmd::Prune {
            older_than,
            all,
            yes,
        } => cmd_prune(&client, &older_than, all, yes).await,
    }
}

/// Parse a short human duration (`45s`, `30m`, `1h`, `2d`, or a bare
/// seconds count) into whole seconds. Used by `muxa prune --older-than`.
fn parse_duration_secs(s: &str) -> Result<u64> {
    let s = s.trim();
    anyhow::ensure!(!s.is_empty(), "empty duration");
    let (num, mult) = match s.chars().last() {
        Some('d') => (&s[..s.len() - 1], 86_400),
        Some('h') => (&s[..s.len() - 1], 3_600),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('s') => (&s[..s.len() - 1], 1),
        _ => (s, 1),
    };
    let val: u64 = num
        .trim()
        .parse()
        .with_context(|| format!("invalid duration: {s:?} (try 30m, 1h, 24h)"))?;
    Ok(val * mult)
}

/// Delete orphan agent rows (no pane, surface, or pid) via the daemon.
/// Previews the count from a snapshot, confirms (unless `--yes`), then asks
/// the daemon to prune. tmux sessions are never affected — only registry rows.
async fn cmd_prune(client: &Client, older_than: &str, all: bool, yes: bool) -> Result<()> {
    let max_age_secs = if all {
        0
    } else {
        parse_duration_secs(older_than)?
    };
    let now = OffsetDateTime::now_utc();
    let agents = client.snapshot().await.unwrap_or_default();
    let is_orphan = |a: &Agent| {
        a.kind != AgentKind::Task && a.pane.is_none() && a.surface.is_none() && a.pid.is_none()
    };
    let candidates = agents
        .iter()
        .filter(|a| {
            is_orphan(a)
                && (now - a.last_activity_at).whole_seconds()
                    >= i64::try_from(max_age_secs).unwrap_or(i64::MAX)
        })
        .count();
    if candidates == 0 {
        println!("No orphan agent rows to prune.");
        return Ok(());
    }
    if !yes {
        let scope = if all {
            "all ages".to_string()
        } else {
            format!("idle ≥ {older_than}")
        };
        let proceed = cliclack::confirm(format!(
            "Prune {candidates} orphan agent row(s) ({scope})? tmux sessions are not affected."
        ))
        .initial_value(false)
        .interact()
        .unwrap_or(false);
        if !proceed {
            println!("Aborted.");
            return Ok(());
        }
    }
    let pruned = client.prune(Duration::from_secs(max_age_secs)).await?;
    println!("Pruned {pruned} orphan agent row(s).");
    Ok(())
}

fn collaboration_origin() -> Result<CollaborationOrigin> {
    let pane = std::env::var("TMUX_PANE")
        .context("collaboration commands must run inside a tmux pane (TMUX_PANE is unset)")?;
    let socket = std::env::var("TMUX").ok().and_then(|value| {
        let path = value.split(',').next()?.trim();
        Path::new(path)
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
    });
    // `muxa msg` speaks for the agent whose pane it runs in — same reasoning as
    // the MCP origin.
    Ok(CollaborationOrigin {
        pane,
        socket,
        console: false,
    })
}

fn collaboration_request_kind(value: &str) -> Result<RequestKind> {
    match value {
        "question" => Ok(RequestKind::Question),
        "review" => Ok(RequestKind::Review),
        "task" => Ok(RequestKind::Task),
        "notice" => Ok(RequestKind::Notice),
        _ => anyhow::bail!("kind must be question, review, task, or notice"),
    }
}

fn collaboration_reply_status(value: &str) -> Result<RequestStatus> {
    match value {
        "completed" => Ok(RequestStatus::Completed),
        "blocked" => Ok(RequestStatus::Blocked),
        "declined" => Ok(RequestStatus::Declined),
        "failed" => Ok(RequestStatus::Failed),
        _ => anyhow::bail!("status must be completed, blocked, declined, or failed"),
    }
}

fn collaboration_air_references(values: &[String]) -> Result<Vec<AirArtifactReference>> {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            serde_json::from_str(value)
                .with_context(|| format!("invalid --air-ref #{} JSON", index + 1))
        })
        .collect()
}

fn collaboration_list_scope(value: &str) -> Result<MailboxScope> {
    match value {
        "caller" | "self" | "mine" => Ok(MailboxScope::Caller),
        "room" | "window" => Ok(MailboxScope::Room),
        "all" | "fleet" => Ok(MailboxScope::All),
        _ => anyhow::bail!("scope must be caller, room, or all"),
    }
}

fn collaboration_mailbox(value: &str) -> Result<RequestMailbox> {
    match value {
        "incoming" | "inbox" => Ok(RequestMailbox::Incoming),
        "sent" | "outgoing" => Ok(RequestMailbox::Sent),
        "all" => Ok(RequestMailbox::All),
        _ => anyhow::bail!("mailbox must be incoming, sent, or all"),
    }
}

async fn cmd_peers(client: &Client, json: bool) -> Result<()> {
    let room = client
        .collaboration_context(&collaboration_origin()?)
        .await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&room)?);
        return Ok(());
    }
    println!(
        "room {} · self {} · {} unread · {} replies",
        room.current.room.window_id,
        room.current.label(),
        room.unread,
        room.unread_replies,
    );
    if room.peers.is_empty() {
        println!("No collaboration peers in this window.");
    } else {
        for peer in room.peers {
            println!(
                "{}  {:<16}  {:<12}  {}",
                peer.pane,
                peer.label(),
                peer.state,
                display_roles(&peer.roles),
            );
        }
    }
    Ok(())
}

async fn cmd_identity(client: &Client, action: IdentityCmd) -> Result<()> {
    let origin = collaboration_origin()?;
    let (room, json) = match action {
        IdentityCmd::Show { json } => (client.collaboration_context(&origin).await?, json),
        IdentityCmd::Set { alias, roles } => (
            client
                .collaboration_set_identity(&origin, alias.as_deref(), &roles)
                .await?,
            false,
        ),
        IdentityCmd::Clear => (
            client
                .collaboration_set_identity(&origin, None, &[])
                .await?,
            false,
        ),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&room)?);
    } else {
        println!(
            "self {}  roles: {}  room: {}",
            room.current.label(),
            display_roles(&room.current.roles),
            room.current.room.window_id,
        );
        for peer in room.peers {
            println!(
                "peer {}  roles: {}  state: {}",
                peer.label(),
                display_roles(&peer.roles),
                peer.state,
            );
        }
    }
    Ok(())
}

fn display_roles(roles: &[String]) -> String {
    if roles.is_empty() {
        "-".into()
    } else {
        roles.join(",")
    }
}

async fn cmd_msg(client: &Client, action: MsgCmd) -> Result<()> {
    let origin = collaboration_origin()?;
    match action {
        MsgCmd::Send {
            target,
            body,
            kind,
            execute,
            paths,
            air_refs,
            no_reply,
            json,
        } => {
            let kind = collaboration_request_kind(&kind)?;
            let air_artifacts = collaboration_air_references(&air_refs)?;
            let request = client
                .collaboration_send(
                    &origin,
                    &target,
                    &NewRequest {
                        kind,
                        body,
                        expects_reply: !no_reply && kind != RequestKind::Notice,
                        work_mode: if execute {
                            WorkMode::Execute
                        } else {
                            WorkMode::ReadOnly
                        },
                        paths,
                        air_artifacts,
                    },
                )
                .await?;
            print_collaboration_receipt(&request, json)?;
        }
        MsgCmd::Inbox { json } => {
            let requests = client.collaboration_inbox(&origin).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&requests)?);
            } else if requests.is_empty() {
                println!("Inbox is empty.");
            } else {
                for request in requests {
                    println!(
                        "{}  {:?} from {}  {:?}\n  {}",
                        request.id,
                        request.kind,
                        request.from.label(),
                        request.work_mode,
                        request.body,
                    );
                }
            }
        }
        MsgCmd::List {
            mailbox,
            scope,
            json,
        } => cmd_msg_list(client, &origin, &mailbox, &scope, json).await?,
        MsgCmd::Reply {
            request_id,
            body,
            status,
            artifacts,
            air_refs,
            json,
        } => {
            let air_artifacts = collaboration_air_references(&air_refs)?;
            let request = client
                .collaboration_reply(
                    &origin,
                    &request_id,
                    collaboration_reply_status(&status)?,
                    &body,
                    &artifacts,
                    &air_artifacts,
                )
                .await?;
            print_collaboration_receipt(&request, json)?;
        }
        MsgCmd::Wait {
            request_id,
            timeout_secs,
        } => cmd_msg_wait(client, &origin, &request_id, timeout_secs).await?,
        MsgCmd::Cancel { request_id, json } => {
            let request = client.collaboration_cancel(&origin, &request_id).await?;
            print_collaboration_receipt(&request, json)?;
        }
    }
    Ok(())
}

async fn cmd_msg_list(
    client: &Client,
    origin: &CollaborationOrigin,
    mailbox: &str,
    scope: &str,
    json: bool,
) -> Result<()> {
    let scope = collaboration_list_scope(scope)?;
    // A widened listing is the operator asking what the fleet is saying, not
    // the pane agent asking after its own inbox — so it goes out under the
    // console identity the daemon requires for it.
    let listing_origin = CollaborationOrigin {
        console: !matches!(scope, MailboxScope::Caller),
        ..origin.clone()
    };
    let requests = client
        .collaboration_list_scoped(&listing_origin, collaboration_mailbox(mailbox)?, scope)
        .await?;
    print_collaboration_messages(&requests, json, origin, scope)
}

async fn cmd_msg_wait(
    client: &Client,
    origin: &CollaborationOrigin,
    request_id: &str,
    timeout_secs: u64,
) -> Result<()> {
    let request = client
        .collaboration_wait(origin, request_id, timeout_secs)
        .await?;
    if request.status.is_terminal() {
        println!("{}", serde_json::to_string_pretty(&request)?);
        Ok(())
    } else {
        anyhow::bail!("timed out waiting for {request_id}")
    }
}

/// What `send`, `reply`, and `cancel` say when they worked.
///
/// They used to print the whole stored request — every timestamp, every
/// `null` — which buries the two facts the caller wants (it went through, and
/// whether an answer is coming) under thirty lines of JSON. `--json` still
/// gives the record for anything that wants to parse it.
fn print_collaboration_receipt(
    request: &muxa::collaboration::CollaborationRequest,
    json: bool,
) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(request)?);
        return Ok(());
    }
    match request.status {
        RequestStatus::Cancelled => println!("cancelled  {}", request.id),
        // A reply on the request means this was `msg reply`, not `msg send`.
        _ if request.reply.is_some() => {
            println!("replied  {}  to {}", request.id, request.from.label());
        }
        _ => {
            let awaiting = if request.expects_reply {
                "awaiting reply"
            } else {
                "no reply expected"
            };
            println!(
                "sent  {}  to {}  ({awaiting})",
                request.id,
                request.to.label()
            );
        }
    }
    Ok(())
}

/// Where a participant sits, for listings that span more than one room.
///
/// A room id alone (`@7` on some socket) tells an operator nothing; the tmux
/// names they navigate by do. Fall back to the ids only when the scan that
/// would have supplied the names has not run yet.
fn participant_location(participant: &Participant) -> String {
    if participant.console {
        return "console".to_string();
    }
    let session = participant
        .tmux_session_name
        .as_deref()
        .or(participant.tmux_session_id.as_deref())
        .unwrap_or("?");
    let window = participant
        .window_name
        .as_deref()
        .unwrap_or(&participant.room.window_id);
    format!("{session}:{window}")
}

fn print_collaboration_messages(
    requests: &[muxa::collaboration::CollaborationRequest],
    json: bool,
    origin: &CollaborationOrigin,
    scope: MailboxScope,
) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string_pretty(requests)?);
    } else if requests.is_empty() {
        println!("No collaboration messages.");
    } else {
        for request in requests {
            // Past the caller's own mailbox there is no "the other end" to
            // name: the operator is reading other agents' traffic, so both
            // ends and the window each sits in have to be on the line.
            let direction = if matches!(scope, MailboxScope::Caller) {
                if request.from.pane == origin.pane
                    && origin
                        .socket
                        .as_deref()
                        .is_none_or(|socket| request.from.socket.as_deref() == Some(socket))
                {
                    format!("to {}", request.to.label())
                } else {
                    format!("from {}", request.from.label())
                }
            } else {
                format!(
                    "{} [{}] -> {} [{}]",
                    request.from.label(),
                    participant_location(&request.from),
                    request.to.label(),
                    participant_location(&request.to),
                )
            };
            // Only when it is worth knowing. `matched` is every ordinary
            // request, and printing the caller's pid and pane on each of them
            // turns a mailbox into a debug log — which is what a first look at
            // `muxa msg list` used to be.
            let provenance = request
                .provenance
                .as_ref()
                .filter(|p| p.origin_match != CollaborationOriginMatch::Matched)
                .map_or_else(String::new, |p| {
                    let caller = p
                        .observed_pane
                        .as_deref()
                        .map_or_else(|| "pane=?".into(), |pane| format!("pane={pane}"));
                    let pid = p
                        .caller_pid
                        .map_or_else(String::new, |pid| format!(" pid={pid}"));
                    format!("  [via {} {caller}{pid} {}]", p.client_kind, p.origin_match)
                });
            println!(
                "{}  {:?}  {:?}  {}{}\n  {}",
                request.id, request.status, request.kind, direction, provenance, request.body,
            );
            // Without this the sender sees their own question and a status, and
            // the answer they were told to come here for is nowhere on screen.
            if let Some(reply) = &request.reply {
                println!("  <- {:?}: {}", reply.status, reply.body);
                for artifact in &reply.artifacts {
                    println!("     {artifact}");
                }
            }
        }
    }
    Ok(())
}

async fn cmd_run(
    client: &Client,
    socket_path: &Path,
    name: Option<String>,
    cwd: Option<PathBuf>,
    detach: bool,
    command: Vec<String>,
) -> Result<()> {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    let Some((program, args)) = command.split_first() else {
        anyhow::bail!("missing command");
    };
    let session = client
        .spawn_session(muxa::SpawnSession {
            command: program.clone(),
            args: args.to_vec(),
            env: caller_env(socket_path),
            cwd: Some(cwd.unwrap_or(std::env::current_dir()?)),
            name,
            cols: Some(cols),
            rows: Some(rows),
        })
        .await
        .context("spawning muxa session")?;
    println!(
        "muxa: started {} ({})",
        session
            .display_name
            .as_deref()
            .unwrap_or(session.id.as_str()),
        session.id
    );
    if !detach {
        attach_session(client, &session.id).await?;
    }
    Ok(())
}

pub(crate) fn caller_env(socket_path: &Path) -> Vec<(String, String)> {
    let mut env = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect::<Vec<_>>();
    match env.iter_mut().find(|(key, _)| key == "MUXA_SOCKET") {
        Some((_, value)) if value.is_empty() => {
            *value = socket_path.display().to_string();
        }
        Some(_) => {}
        None => env.push(("MUXA_SOCKET".into(), socket_path.display().to_string())),
    }
    env
}

async fn cmd_attach(client: &Client, session: &str) -> Result<()> {
    let sessions = client.list_sessions().await?;
    let id = sessions
        .iter()
        .find(|s| s.id == session || s.display_name.as_deref() == Some(session))
        .map_or_else(|| session.to_string(), |s| s.id.clone());
    attach_session(client, &id).await
}

async fn cmd_detach(client: &Client, session: &str) -> Result<()> {
    client.set_session_attached(session, false).await?;
    println!("muxa: detached {session}");
    Ok(())
}

async fn cmd_register(
    client: &Client,
    name: String,
    pid: Option<u32>,
    cwd: Option<String>,
    pane: Option<String>,
) -> Result<()> {
    // Default to the calling shell so `muxa register --name X` from a pane
    // tracks that shell without the user hunting for a pid.
    let pid = pid.or_else(|| Some(std::os::unix::process::parent_id()));
    client
        .register(&name, pid, cwd.as_deref(), pane.as_deref(), None)
        .await?;
    match pid {
        Some(p) => println!("muxa: registered {name} (pid {p})"),
        None => println!("muxa: registered {name}"),
    }
    Ok(())
}

async fn cmd_zellij_plugin_snapshot(client: &Client, json: &str) -> Result<()> {
    let panes: Vec<muxa::tmux::PaneInfo> =
        serde_json::from_str(json).context("parsing zellij pane snapshot")?;
    client
        .push_pane_snapshot(&panes)
        .await
        .context("pushing zellij pane snapshot")?;
    Ok(())
}

struct RawModeGuard;

impl RawModeGuard {
    fn enter() -> Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        if let Err(error) =
            crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste)
        {
            // `execute!` may have written the enable sequence before a later
            // flush error. Best-effort reversal keeps a failed attach from
            // leaving the parent terminal in paste mode without a guard.
            let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
            let _ = crossterm::terminal::disable_raw_mode();
            return Err(error.into());
        }
        Ok(Self)
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

async fn attach_session(client: &Client, session_id: &str) -> Result<()> {
    let _guard = RawModeGuard::enter()?;
    client.set_session_attached(session_id, true).await?;
    let result = attach_session_loop(client, session_id).await;
    let detach_result = client.set_session_attached(session_id, false).await;
    match (result, detach_result) {
        (Err(e), _) => Err(e),
        (Ok(()), Err(e)) => Err(e).context("detaching muxa session"),
        (Ok(()), Ok(())) => Ok(()),
    }
}

async fn attach_session_loop(client: &Client, session_id: &str) -> Result<()> {
    use crossterm::event::{Event, KeyCode, KeyEventKind};

    let mut stdout = std::io::stdout().lock();
    let mut offset = 0_u64;
    let mut detach_armed = false;

    loop {
        let output = client.read_session(session_id, offset).await?;
        if !output.data.is_empty() {
            stdout.write_all(output.data.as_bytes())?;
            stdout.flush()?;
            offset = output.next_offset;
        }
        if output.exited {
            break;
        }

        while crossterm::event::poll(Duration::ZERO)? {
            match crossterm::event::read()? {
                Event::Paste(text) => {
                    // A detach prefix applies only to the immediately
                    // following key, never across a whole paste event.
                    detach_armed = false;
                    client
                        .write_session(session_id, &bracketed_paste_input(&text))
                        .await?;
                }
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if detach_armed {
                        detach_armed = false;
                        if matches!(key.code, KeyCode::Char('d' | 'D')) {
                            return Ok(());
                        }
                        client.write_session(session_id, "\u{1d}").await?;
                    }
                    if is_detach_prefix(key) {
                        detach_armed = true;
                    } else if let Some(input) = key_to_pty_input(key) {
                        client.write_session(session_id, &input).await?;
                    }
                }
                Event::Resize(cols, rows) => {
                    let _ = client.resize_session(session_id, cols, rows).await;
                }
                _ => {}
            }
        }

        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    Ok(())
}

/// Recreate the framing that crossterm strips from an `Event::Paste` before
/// relaying it to the child PTY. Keeping the payload bracketed prevents a
/// multiline paste from being executed one line at a time by an interactive
/// shell. The parent terminal reports line feeds; PTY input expects carriage
/// returns, with CRLF normalized first so it does not become a doubled CR.
fn bracketed_paste_input(text: &str) -> String {
    const START: &str = "\x1b[200~";
    const END: &str = "\x1b[201~";

    // Clipboard text can contain terminal controls. If an end marker reaches
    // the child unchanged, readline leaves paste mode early and processes the
    // remainder as ordinary keystrokes, including carriage returns that execute
    // commands. Remove markers while streaming so overlapping inputs cannot
    // reveal a fresh marker after an earlier one is deleted.
    let mut defanged = Vec::with_capacity(text.len());
    for byte in text.bytes() {
        defanged.push(byte);
        if defanged.ends_with(START.as_bytes()) || defanged.ends_with(END.as_bytes()) {
            defanged.truncate(defanged.len() - START.len());
        }
    }
    let defanged = String::from_utf8(defanged).expect("removing ASCII preserves UTF-8");
    let normalized = defanged.replace("\r\n", "\n").replace('\n', "\r");
    format!("{START}{normalized}{END}")
}

fn is_detach_prefix(key: crossterm::event::KeyEvent) -> bool {
    key.modifiers
        .contains(crossterm::event::KeyModifiers::CONTROL)
        && matches!(key.code, crossterm::event::KeyCode::Char(']'))
}

fn key_to_pty_input(key: crossterm::event::KeyEvent) -> Option<String> {
    use crossterm::event::{KeyCode, KeyModifiers};

    // The parent terminal owns platform shortcuts. If one still reaches
    // crossterm, dropping it is safer than turning Cmd+V into a literal `v`.
    if key.modifiers.contains(KeyModifiers::SUPER) {
        return None;
    }

    Some(match key.code {
        KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            let lower = c.to_ascii_lowercase();
            if lower.is_ascii_lowercase() {
                char::from((lower as u8) - b'a' + 1).to_string()
            } else {
                return None;
            }
        }
        KeyCode::Char(c) => c.to_string(),
        KeyCode::Enter => "\r".into(),
        KeyCode::Backspace => "\u{7f}".into(),
        KeyCode::Tab => "\t".into(),
        KeyCode::Esc => "\u{1b}".into(),
        KeyCode::Left => "\u{1b}[D".into(),
        KeyCode::Right => "\u{1b}[C".into(),
        KeyCode::Up => "\u{1b}[A".into(),
        KeyCode::Down => "\u{1b}[B".into(),
        KeyCode::Home => "\u{1b}[H".into(),
        KeyCode::End => "\u{1b}[F".into(),
        KeyCode::Delete => "\u{1b}[3~".into(),
        _ => return None,
    })
}

/// Jump to (or list) the agent that needs you. `attend::run` picks the
/// pane and endpoint — reusing the same selection that drives `--list` — and we perform
/// the actual focus through `jump_to_pane`, the identical machinery the
/// `muxa watch` Enter action uses, so a jump lands the same way from both.
async fn cmd_attend(client: &Client, args: attend::Args) -> Result<()> {
    // Enumerate panes across every active host so a herdr agent that needs
    // a human is jumpable from a tmux-primary shell (and vice versa). The
    // jump itself dispatches per-row in `jump_to_pane`.
    let panes = all_panes().await;
    if let Some(target) = attend::run(client, panes, args).await? {
        if muxa::backend::pane_id_host_kind(&target.pane) == Some(muxa::HostKind::Cmux) {
            let backend = target.endpoint.map_or_else(
                muxa::backend::cmux::CmuxBackend::new,
                muxa::backend::cmux::CmuxBackend::with_endpoint,
            );
            jump_to_pane_cmux(&backend, &target.pane);
        } else {
            jump_to_pane(&target.pane);
        }
    }
    Ok(())
}

/// Backfill the daemon's registry from host panes. Idempotent.
async fn cmd_sync(client: &Client) -> Result<()> {
    use std::fmt::Write as _;

    // Each CLI invocation builds its own backend so `MUXA_HOST` etc.
    // resolve at the user's environment rather than the daemon's
    // (which may have been started under a different shell).
    let backend = muxa::default_backend();
    let report = discovery::run_discovery(client, backend.as_ref())
        .await
        .context("running discovery")?;

    // Build the kind breakdown only for non-zero counts so the line stays
    // readable when only one agent kind is present.
    let mut parts: Vec<String> = Vec::new();
    if report.claude_code > 0 {
        parts.push(format!("{} claude_code", report.claude_code));
    }
    if report.codex > 0 {
        parts.push(format!("{} codex", report.codex));
    }
    if report.gemini_cli > 0 {
        parts.push(format!("{} gemini_cli", report.gemini_cli));
    }
    if report.antigravity > 0 {
        parts.push(format!("{} antigravity", report.antigravity));
    }

    let total = report.total_ingested();
    if total == 0 && report.skipped_known == 0 && report.failed == 0 {
        println!("no agent panes discovered");
        return Ok(());
    }

    let mut line = format!("discovered {total} agent");
    if total != 1 {
        line.push('s');
    }
    if !parts.is_empty() {
        line.push_str(": ");
        line.push_str(&parts.join(", "));
    }
    if report.skipped_known > 0 {
        // `write!` to a String never fails — unwrap is fine and avoids the
        // intermediate allocation that `push_str(&format!(..))` would do.
        let _ = write!(line, " (skipped {} already-known)", report.skipped_known);
    }
    if report.failed > 0 {
        let _ = write!(line, " — {} failed", report.failed);
    }
    println!("{line}");
    Ok(())
}

/// One-shot per-invocation overrides for `muxa watch`, bundled so the
/// argument list stays a description of *this run* rather than a flat
/// parade of options.
#[derive(Debug, Clone)]
pub(crate) struct WatchInvocation {
    pub(crate) include_paneless: bool,
    pub(crate) view: Option<WatchViewArg>,
    pub(crate) layout: Option<WatchLayoutArg>,
    pub(crate) screen: Option<WatchScreenArg>,
    pub(crate) sort: Option<WatchSortArg>,
    pub(crate) theme: Option<ThemeArg>,
    pub(crate) caller_pane: Option<String>,
}

pub(crate) async fn cmd_fleet_watch(
    client: &Client,
    cfg: Config,
    config_path: Option<PathBuf>,
    selector: Option<String>,
    invocation: WatchInvocation,
) -> Result<()> {
    selector
        .as_deref()
        .map(str::parse::<muxa::LabelSelector>)
        .transpose()
        .map_err(anyhow::Error::msg)
        .context("invalid fleet label selector")?;
    let initial = client
        .fleet_snapshot(selector.as_deref())
        .await
        .context("reading initial fleet state from muxad")?;
    // A lone local controller is the overwhelmingly common case and should
    // feel exactly like `muxa watch`: adding the Fleet feature must not insert
    // a redundant host row or discard the mature inspector and collaboration
    // surface. Host hierarchy becomes visible only when it carries information.
    if fleet_watch::uses_native_local_watch(&initial) {
        return cmd_watch(client, cfg, config_path, invocation.clone()).await;
    }
    fleet_watch::run(
        client.clone(),
        &cfg,
        selector,
        initial,
        invocation,
        config_path,
    )
    .await
}

pub(crate) async fn cmd_watch(
    client: &Client,
    cfg: Config,
    config_path: Option<PathBuf>,
    invocation: WatchInvocation,
) -> Result<()> {
    let WatchInvocation {
        include_paneless,
        view,
        layout,
        screen,
        sort,
        theme,
        caller_pane,
    } = invocation;
    // watch::run restores the terminal before returning, so by the time we
    // get here it's safe to exec tmux commands that mutate the client's
    // attached session / pane.
    //
    let message_skills = cfg.message.skills.clone();
    // CLI flag wins over config — one-shot override for the current
    // invocation without touching the user's ~/.config/muxa/config.toml.
    let mut watch_cfg = WatchConfig {
        hide_paneless: cfg.watch.hide_paneless && !include_paneless,
        ..cfg.watch
    };
    if let Some(view) = view {
        watch_cfg.view = view.into();
    }
    if let Some(screen) = screen {
        watch_cfg.screen = screen.into();
    }
    if let Some(layout) = layout {
        watch_cfg.layout = layout.into();
    }
    if let Some(sort) = sort {
        watch_cfg.sort = sort.keys();
    }
    watch_cfg.theme = Some(
        theme
            .map(WatchTheme::from)
            .or(watch_cfg.theme)
            .unwrap_or(cfg.ui.theme),
    );
    let activity_path = cfg
        .activity
        .enabled
        .then(|| {
            cfg.activity
                .path
                .clone()
                .or_else(paths::default_activity_file)
        })
        .flatten();
    let session_activity_path = cfg
        .session_activity
        .enabled
        .then(|| {
            cfg.session_activity
                .path
                .clone()
                .or_else(paths::default_session_activity_file)
        })
        .flatten();
    let message_composer = watch::MessageComposerConfig {
        skills: message_skills,
        collaboration_scope: cfg.collaboration.scope,
    };
    if let Some(target) = watch::run(
        client,
        watch_cfg,
        message_composer,
        session_activity_path,
        activity_path.clone(),
        config_path,
        caller_pane,
    )
    .await?
    {
        match target {
            watch::WatchOpenTarget::TopologyPane(key) => {
                jump_to_topology_pane_logged(&key, activity_path.as_deref()).await;
            }
            watch::WatchOpenTarget::LegacyPane(pane_id) => {
                jump_to_pane_logged(&pane_id, activity_path.as_deref()).await;
            }
        }
    }
    Ok(())
}

async fn cmd_dashboard(client: &Client, cfg: &Config, args: dashboard_tui::Args) -> Result<()> {
    let activity_path = cfg
        .activity
        .enabled
        .then(|| {
            cfg.activity
                .path
                .clone()
                .or_else(paths::default_activity_file)
        })
        .flatten();
    match dashboard_tui::run(client, cfg, args).await? {
        Some(dashboard_tui::OpenTarget::TopologyPane(key)) => {
            jump_to_topology_pane_logged(&key, activity_path.as_deref()).await;
        }
        Some(dashboard_tui::OpenTarget::Pane(pane_id)) => {
            jump_to_pane_logged(&pane_id, activity_path.as_deref()).await;
        }
        Some(dashboard_tui::OpenTarget::PtySession(session_id)) => {
            attach_session(client, &session_id).await?;
        }
        None => {}
    }
    Ok(())
}

async fn jump_to_pane_logged(pane_id: &str, activity_path: Option<&Path>) {
    let should_log_attach = muxa::default_backend().kind() == muxa::HostKind::Tmux
        && !tmux::inside_tmux()
        && activity_path.is_some();
    let target = should_log_attach
        .then(|| tmux_interaction_target(pane_id))
        .flatten();
    let started_at = OffsetDateTime::now_utc();
    jump_to_pane(pane_id);
    let ended_at = OffsetDateTime::now_utc();

    let (Some(path), Some((session_id, session_name))) = (activity_path, target) else {
        return;
    };
    if (ended_at - started_at).whole_seconds() <= 0 {
        return;
    }
    let entry =
        ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
            kind: HumanInteractionKind::TmuxAttach,
            pane: Some(pane_id.to_string()),
            session_id,
            session_name,
            started_at,
            ended_at,
        }));
    if let Err(e) = muxa::activity::append_entry(path, &entry).await {
        tracing::warn!(error = %e, path = %path.display(), "could not append tmux attach interval");
    }
}

async fn jump_to_topology_pane_logged(key: &muxa::PaneKey, activity_path: Option<&Path>) {
    let endpoint = &key.window.session.endpoint;
    let should_log_attach =
        endpoint.host == muxa::HostKind::Tmux && !tmux::inside_tmux() && activity_path.is_some();
    let started_at = OffsetDateTime::now_utc();
    jump_to_topology_pane(key);
    let ended_at = OffsetDateTime::now_utc();
    if !should_log_attach || (ended_at - started_at).whole_seconds() <= 0 {
        return;
    }
    let Some(path) = activity_path else {
        return;
    };
    let entry =
        ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
            kind: HumanInteractionKind::TmuxAttach,
            pane: Some(key.pane_id.clone()),
            session_id: Some(key.window.session.session_id.clone()),
            session_name: None,
            started_at,
            ended_at,
        }));
    if let Err(error) = muxa::activity::append_entry(path, &entry).await {
        tracing::warn!(error = %error, path = %path.display(), "could not append tmux attach interval");
    }
}

fn tmux_interaction_target(pane_id: &str) -> Option<(Option<String>, Option<String>)> {
    let pane = tmux::resolve_pane(pane_id)?;
    let session_id = tmux::list_sessions().ok().and_then(|sessions| {
        sessions
            .into_iter()
            .find(|session| session.name == pane.session)
            .map(|session| session.session_id)
    });
    Some((session_id, Some(pane.session)))
}

/// Attach the user to `pane_id`. Handles two cases:
///
/// * **Inside tmux** (`$TMUX` set): the user is already attached to some
///   client. Run `switch-client` to move that client to the target session,
///   with `select-window` + `select-pane` pre-positioning so the attach
///   lands on the exact pane.
///
/// * **Bare shell** (`$TMUX` unset): the user ran `muxa watch` from a
///   terminal that isn't inside tmux. There's no client to "switch" — we
///   have to hand this terminal over to a new `tmux attach-session`, with
///   the target window+pane already selected so the attach lands there.
///
/// In both cases we pre-select the window and pane *before* attaching/
/// switching — `select-window`/`select-pane` are plain control messages to
/// the tmux server and don't need an attached client.
fn jump_to_pane(pane_id: &str) {
    // Dispatch on the pane id's namespace FIRST: a `herdr:` row must jump
    // via a herdr backend even when the process-global detected host is
    // tmux (and vice versa) — the multi-host unified console lists rows
    // from every host, so the row's own id is the source of truth. Only
    // when the namespace is unrecognized (legacy/synthetic ids) do we fall
    // back to the process-global backend's kind.
    let fallback = muxa::default_backend();
    let kind = dispatch_kind(pane_id, fallback.kind());
    match kind {
        // tmux jumps go straight through `tmux::` helpers and need no backend.
        muxa::HostKind::Tmux => jump_to_pane_tmux(pane_id),
        muxa::HostKind::Cmux => {
            let backend = backend_for_dispatch(kind, &fallback);
            jump_to_pane_cmux(backend.as_ref(), pane_id);
        }
        muxa::HostKind::Rmux => {
            let backend = backend_for_dispatch(kind, &fallback);
            jump_to_pane_rmux(backend.as_ref(), pane_id);
        }
        muxa::HostKind::Zellij => {
            let backend = backend_for_dispatch(kind, &fallback);
            jump_to_pane_zellij(backend.as_ref(), pane_id);
        }
        muxa::HostKind::Herdr => {
            let backend = backend_for_dispatch(kind, &fallback);
            jump_to_pane_herdr(backend.as_ref(), pane_id);
        }
    }
}

fn jump_to_topology_pane(key: &muxa::PaneKey) {
    match key.window.session.endpoint.host {
        muxa::HostKind::Tmux => jump_to_pane_tmux_key(key),
        muxa::HostKind::Cmux => {
            let backend = muxa::backend::cmux::CmuxBackend::with_endpoint(
                key.window.session.endpoint.socket.clone(),
            );
            jump_to_pane_cmux(&backend, &key.pane_id);
        }
        muxa::HostKind::Rmux | muxa::HostKind::Zellij | muxa::HostKind::Herdr => {
            jump_to_pane(&key.pane_id);
        }
    }
}

/// Address for the *window* half of a jump.
///
/// `switch-client -t %pane` names a pane but not a session, and tmux fills the
/// gap from recent client activity. That is unambiguous only while every
/// window belongs to exactly one session. A **session group** — created by
/// `tmux new-session -t <session>`, the supported way to keep two terminals on
/// two different windows of one workspace — links the same window into every
/// session in the group, and there the guess is routinely wrong. Measured on
/// tmux 3.4 with two attached clients: jumping client A pulled it out of its
/// own session into the grouped sibling and dragged client B's view along with
/// it, re-coupling the two terminals the group exists to separate.
///
/// `<session_id>:<window_id>` closes the gap. Both are backend-native ids, so
/// neither can prefix-match a neighbouring object the way session *names* do
/// (`callabo` against `callabo-set`), and together they name exactly one
/// window in exactly one session. Falls back to the bare pane id when no
/// session id is known, which is the behaviour every jump had before.
fn window_target(session_id: Option<&str>, window_id: &str, pane_id: &str) -> String {
    match session_id {
        Some(session) if !session.is_empty() && !window_id.is_empty() => {
            format!("{session}:{window_id}")
        }
        _ => pane_id.to_string(),
    }
}

/// Session target for `attach-session`: the stable `$N` id when the scan
/// carried one, else the session name.
///
/// Ids are exact. Names match by prefix unless anchored with `=`, and real
/// session sets collide — `callabo` also matches `callabo-set`, so a
/// name-targeted attach can hand the terminal to the wrong workspace. The
/// name stays as a fallback only because a row parsed from an older
/// `PANE_FMT` has no session id to use.
fn session_target<'a>(session_id: &'a str, session_name: &'a str) -> &'a str {
    if session_id.is_empty() {
        session_name
    } else {
        session_id
    }
}

/// Choose which session a jump should address, given the asking client's
/// current session and the session recorded for the target pane.
///
/// Prefers the client's own session whenever the target window is linked into
/// it: inside a session group that makes the jump a pure window change, so no
/// sibling session — and therefore no other terminal — moves. Otherwise this
/// is a genuine cross-session jump, and `pane_session` names a definite
/// destination where tmux would otherwise pick one from client activity.
///
/// `window_linked` is the membership probe, taken as a closure so the decision
/// is testable without a tmux server, and called *only* when it can change the
/// answer. Identical session ids already imply membership — the ordinary
/// same-session jump — so that case is answered without a round trip.
///
/// `None` only when neither session is known, leaving [`window_target`] to
/// fall back to the bare pane id.
fn resolve_jump_session(
    client_session: Option<String>,
    pane_session: &str,
    window_linked: impl FnOnce(&str) -> bool,
) -> Option<String> {
    let fallback = || (!pane_session.is_empty()).then(|| pane_session.to_string());
    let Some(client_session) = client_session.filter(|session| !session.is_empty()) else {
        return fallback();
    };
    if client_session == pane_session || window_linked(&client_session) {
        return Some(client_session);
    }
    fallback()
}

/// [`resolve_jump_session`] wired to tmux on the server named by `socket`.
fn jump_session_id(
    socket: Option<&str>,
    client: Option<&str>,
    window_id: &str,
    pane_session: &str,
) -> Option<String> {
    resolve_jump_session(
        client.and_then(|client| muxa::tmux::client_session_id_on(socket, client)),
        pane_session,
        |session| muxa::tmux::window_in_session_on(socket, session, window_id),
    )
}

fn jump_to_pane_tmux_key(key: &muxa::PaneKey) {
    let socket = Some(key.window.session.endpoint.socket.as_str());
    let pane = key.pane_id.as_str();
    let run = |args: &[&str]| {
        if let Err(error) = muxa::tmux::run_control_on(socket, args) {
            eprintln!("muxa: tmux {} failed: {error}", args.join(" "));
        }
    };
    if tmux::inside_tmux() {
        let pinned = CALLER_CLIENT.get().cloned().or_else(tmux::current_client);
        // Address the window by session, not just by pane — see `window_target`.
        let session = jump_session_id(
            socket,
            pinned.as_deref(),
            &key.window.window_id,
            &key.window.session.session_id,
        );
        let target = window_target(session.as_deref(), &key.window.window_id, pane);
        if let Some(client) = pinned.as_deref() {
            run(&["switch-client", "-c", client, "-t", &target]);
        } else {
            run(&["switch-client", "-t", &target]);
        }
        run(&["select-window", "-t", &target]);
        run(&["select-pane", "-t", pane]);
        return;
    }

    // Pre-position the session we are about to attach to. Its id is already
    // in the key, so qualify the window with it rather than letting a bare
    // pane id send `select-window` at a grouped sibling — that would move a
    // window in a session some *other* terminal is looking at, before this
    // terminal has even attached.
    let target = window_target(
        Some(key.window.session.session_id.as_str()),
        &key.window.window_id,
        pane,
    );
    run(&["select-window", "-t", &target]);
    run(&["select-pane", "-t", pane]);
    match muxa::tmux::tmux_command_on(socket)
        .args([
            "attach-session",
            "-t",
            key.window.session.session_id.as_str(),
        ])
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!(
            "muxa: tmux attach-session exited with {}",
            status
                .code()
                .map_or_else(|| "signal".into(), |code| code.to_string())
        ),
        Err(error) => eprintln!("muxa: failed to spawn tmux attach-session: {error}"),
    }
}

/// Effective host for a per-row action: the pane id's namespace when
/// recognized, else the process-global `fallback`. Pure so the dispatch
/// table is unit-testable without constructing backends.
fn dispatch_kind(pane_id: &str, fallback: muxa::HostKind) -> muxa::HostKind {
    muxa::backend::pane_id_host_kind(pane_id).unwrap_or(fallback)
}

/// Reuse the already-built `fallback` backend when its kind matches, else
/// construct a fresh one for `kind` (cheap — the constructors are a unit
/// struct for tmux/zellij and a socket path for herdr).
fn backend_for_dispatch(
    kind: muxa::HostKind,
    fallback: &muxa::SharedBackend,
) -> muxa::SharedBackend {
    if fallback.kind() == kind {
        fallback.clone()
    } else {
        backend_for_kind(kind)
    }
}

/// Build one backend of the given kind. The CLI-side analog of the
/// (private) `muxa::backend::backend_of`, used for per-row host dispatch
/// where the row's host differs from the process-global one.
pub(crate) fn backend_for_kind(kind: muxa::HostKind) -> muxa::SharedBackend {
    match kind {
        muxa::HostKind::Tmux => std::sync::Arc::new(muxa::TmuxBackend::new()),
        muxa::HostKind::Cmux => std::sync::Arc::new(muxa::backend::cmux::CmuxBackend::new()),
        muxa::HostKind::Rmux => std::sync::Arc::new(muxa::RmuxBackend::new()),
        muxa::HostKind::Zellij => std::sync::Arc::new(muxa::ZellijBackend::new()),
        muxa::HostKind::Herdr => std::sync::Arc::new(muxa::backend::herdr::HerdrBackend::new()),
    }
}

/// Resolve the backend that owns `pane_id` by its namespace, falling back
/// to the process-global backend for unrecognized ids. Used by the live
/// pane-capture paths (`watch` preview, `dashboard` capture) so a capture
/// hits the host the pane actually lives on.
pub(crate) fn backend_for_pane(pane_id: &str) -> muxa::SharedBackend {
    match muxa::backend::pane_id_host_kind(pane_id) {
        Some(kind) => backend_for_kind(kind),
        None => muxa::default_backend(),
    }
}

/// Aggregate pane inventories across every active backend — the multi-host
/// enumeration used by `muxa panes`, `stats`, `timeline`, and `attend`.
/// Rows already carry their host namespace in `pane_id`, so a plain concat
/// keeps them distinct.
///
/// Each backend's `list_panes` blocks (tmux shells out; herdr hits a socket),
/// so we fan the calls out onto the blocking pool up front and join them —
/// the tick budget is one host's latency, not the sum, and a slow/failing
/// host can't stall the runtime or the others (a join error contributes an
/// empty list). Mirrors the concurrent fan-out `watch::compute_refresh` uses.
pub(crate) async fn all_panes() -> Vec<muxa::tmux::PaneInfo> {
    let tasks: Vec<_> = muxa::active_backends()
        .into_iter()
        .map(|backend| tokio::task::spawn_blocking(move || backend.list_panes()))
        .collect();
    let mut panes = Vec::new();
    for task in tasks {
        match task.await {
            Ok(list) => panes.extend(list),
            Err(e) => tracing::debug!(error = %e, "pane enumeration task failed"),
        }
    }
    panes
}

/// The tmux client that invoked this process, as expanded by the key
/// binding (`#{client_name}`) at the moment the key was pressed.
///
/// This is the only trustworthy client identity a popup-launched process
/// has: any `display-message` it runs resolves "current client" from
/// recent activity, which with two terminals attached is routinely the
/// other one — measured live, a popup opened from `/dev/pts/67` answered
/// `/dev/pts/87`. Empty when launched outside a managed binding.
static CALLER_CLIENT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

fn jump_to_pane_tmux(pane_id: &str) {
    let Some(info) = tmux::resolve_pane(pane_id) else {
        eprintln!("muxa: pane {pane_id} not found in tmux — it may have closed");
        return;
    };
    if tmux::inside_tmux() {
        // Switch after pre-positioning, addressed by stable ids, pinned to
        // the asking client.
        //
        // Each of those three matters, and the version before them had
        // none. It pre-positioned with `select-window -t "<name>:<idx>"`
        // *before* switching anyone: that mutates the target session's
        // current window immediately, so any other terminal already
        // attached to that session jumped on the spot — a window the user
        // never asked to move, in a terminal they were not even looking at.
        // The name-based target is its own hazard, because tmux matches
        // session names by prefix unless anchored with `=`, and real
        // session sets collide (`callabo` against `callabo-set`). And
        // without `-c`, tmux picks "the current client" from recent
        // activity, which with two terminals attached is routinely the
        // other one.
        //
        // A pane id alone is *not* the unambiguous identifier it looks
        // like. It names one pane, but `switch-client` needs a session,
        // and a window can be linked into more than one — that is exactly
        // what a session group is. `window_target` explains what tmux does
        // with the ambiguity and why the answer is wrong often enough to
        // matter. Address the window by `<session_id>:<window_id>` instead.
        // Prefer the binding-expanded client: it names who pressed the key.
        // `current_client()` is an activity-based guess and only acceptable
        // when nothing better exists (an old binding without the flag);
        // unpinned is last, safe only when a single client is attached.
        let pinned = CALLER_CLIENT.get().cloned().or_else(tmux::current_client);
        let session = jump_session_id(None, pinned.as_deref(), &info.window_id, &info.session_id);
        let target = window_target(session.as_deref(), &info.window_id, pane_id);
        if let Some(client) = pinned {
            run_tmux(&["switch-client", "-c", &client, "-t", &target]);
        } else {
            run_tmux(&["switch-client", "-t", &target]);
        }
        // Selecting the window *after* the switch confines the shared-state
        // mutation to the session we are entering — clients attached to *it*
        // follow, which is tmux's model for a session's current window, but
        // no bystander session is touched the way the old pre-switch
        // select-window did. With the session-qualified target above, a
        // grouped sibling is a bystander too: it keeps its own current
        // window, so the other terminal stays where the user left it.
        run_tmux(&["select-window", "-t", &target]);
        run_tmux(&["select-pane", "-t", pane_id]);
    } else {
        // Pre-position for the fresh attach below; there is no client of
        // ours yet to pin, and the session is about to become ours. Qualify
        // the window with that session all the same: a bare pane id lets
        // `select-window` land on a grouped sibling and move a window some
        // other terminal is looking at, before we have attached anywhere.
        let target = window_target(Some(&info.session_id), &info.window_id, pane_id);
        run_tmux(&["select-window", "-t", &target]);
        run_tmux(&["select-pane", "-t", pane_id]);
        // Bare shell — hand our terminal to a fresh tmux attach-session.
        // Target the session by id: names match by prefix unless anchored,
        // and real session sets collide (`callabo` against `callabo-set`).
        // `.status()` waits for tmux to exit; on detach the user is back at
        // this shell prompt, which is the least-surprising behaviour.
        match muxa::tmux::tmux_command()
            .args([
                "attach-session",
                "-t",
                session_target(&info.session_id, &info.session),
            ])
            .status()
        {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!(
                "muxa: tmux attach-session exited with {}",
                s.code().map_or_else(|| "signal".into(), |c| c.to_string())
            ),
            Err(e) => eprintln!("muxa: failed to spawn tmux attach-session: {e}"),
        }
    }
}

/// Jump on zellij: a single `zellij action focus-pane-with-id <id>`
/// covers the whole story. There's no session/window analog to
/// pre-select, and no "outside zellij" attach equivalent — running
/// `muxa watch` from a bare shell on a zellij host would land here
/// only if the user explicitly set `MUXA_HOST=zellij`, in which case
/// failing to focus is just a "couldn't reach the zellij server"
/// stderr warning.
fn jump_to_pane_zellij(backend: &dyn muxa::PaneBackend, pane_id: &str) {
    if !backend.focus_pane(pane_id) {
        eprintln!("muxa: zellij focus-pane-with-id {pane_id} failed — pane may have closed");
    }
}

fn jump_to_pane_cmux(backend: &dyn muxa::PaneBackend, pane_id: &str) {
    if !backend.focus_pane(pane_id) {
        eprintln!(
            "muxa: cmux surface.focus {pane_id} failed — surface may have closed or socket access may be disabled"
        );
    }
}

/// Jump within the rmux client associated with the native `$RMUX` endpoint.
/// Full bare-terminal attach is intentionally outside the initial backend
/// slice; focus still covers watch/dashboard usage launched from rmux itself.
fn jump_to_pane_rmux(backend: &dyn muxa::PaneBackend, pane_id: &str) {
    if !backend.focus_pane(pane_id) {
        eprintln!("muxa: rmux select-pane {pane_id} failed — pane may have closed");
    }
}

/// Jump on herdr: `pane.focus` over the herdr socket is the whole story,
/// same single-call shape as zellij. Focus moves the herdr UI wherever a
/// client is attached; there is no bare-shell attach handover analog.
fn jump_to_pane_herdr(backend: &dyn muxa::PaneBackend, pane_id: &str) {
    if !backend.focus_pane(pane_id) {
        eprintln!(
            "muxa: herdr pane.focus {pane_id} failed — pane may have closed or the herdr server is down"
        );
    }
}

fn run_tmux(args: &[&str]) {
    match muxa::tmux::tmux_command().args(args).status() {
        Ok(s) if s.success() => {}
        Ok(s) => eprintln!(
            "muxa: `tmux {}` exited with {}",
            args.join(" "),
            s.code().map_or_else(|| "signal".into(), |c| c.to_string())
        ),
        Err(e) => eprintln!("muxa: failed to spawn `tmux {}`: {e}", args.join(" ")),
    }
}

/// Hook commands are invoked on the agent's critical path (every prompt,
/// every tool call). If the daemon is down we MUST NOT block or fail — a
/// best-effort ingest with a stderr warning keeps the agent healthy.
async fn best_effort_ingest(client: &Client, ev: &muxa::event::AgentEvent) {
    if let Err(e) = client.ingest(ev).await {
        tracing::debug!(error = %e, "muxa ingest failed (daemon down?)");
    }
}

/// Name the agent's pane the first time a session starts in it, so it is
/// addressable as `@claude` rather than only as `%1242`.
///
/// Gated on `Started` — the one hook event that fires once per session —
/// because everything else on this path fires per tool call, and a tmux
/// round-trip per tool call is a tax on the agent's critical path for a
/// fact that cannot have changed.
///
/// Best-effort like the ingest above, and for a stronger reason: this runs
/// inside the agent's own hook, where a non-zero exit is the agent's
/// problem. A pane stuck with `%1242` as its only handle is a worse
/// interface, not a broken agent.
async fn best_effort_default_alias(client: &Client, ev: &muxa::event::AgentEvent) {
    let Some((pane, base)) = alias_target(ev) else {
        return;
    };
    // The answer for every session start after the first, and one tmux call
    // rather than an IPC round-trip on the agent's critical path.
    if tmux_work::pane_is_named(&pane).unwrap_or(true) {
        return;
    }
    let request = muxa::collaboration::HandleRequest::Mint {
        base: base.to_string(),
    };
    let issued = client
        .collaboration_issue_handle(&pane, None, &request, HANDLE_IPC_TIMEOUT)
        .await;
    match issued {
        Ok(Some(handle)) => match tmux_work::claim_alias(&pane, &handle) {
            Ok(Some(alias)) => tracing::debug!(pane, alias, "named pane"),
            Ok(None) => tracing::debug!(pane, handle, "pane was named first"),
            Err(e) => tracing::debug!(error = %e, pane, "could not name pane"),
        },
        // No free name, no room for the pane, or no daemon to referee. Naming
        // a pane without the arbiter is exactly what this path stopped doing,
        // so the pane keeps `%1242` until its next session start.
        Ok(None) => {}
        Err(e) => tracing::debug!(error = %e, pane, "handle refused"),
    }
}

/// Budget for the one namespace round-trip. This runs inside an agent's
/// session-start hook, so a wedged daemon must cost the pane its name rather
/// than the agent its startup.
const HANDLE_IPC_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(500);

/// The pane this event should name and the runtime to name it after, or
/// `None` when the event names nothing.
fn alias_target(ev: &muxa::event::AgentEvent) -> Option<(String, &'static str)> {
    let muxa::event::AgentEvent::Started { id, .. } = ev else {
        return None;
    };
    // `Task` rows are pid-tracked subagents rather than panes, and an
    // unrecognised runtime has no name worth minting from.
    if matches!(id.kind, AgentKind::Task | AgentKind::Unknown) {
        return None;
    }
    // No second-guessing the hook layer's pane resolution. `run_hook` has
    // already tried the host env vars and walked the parent-pid chain, and
    // it deliberately reports `None` for a muxa-owned PTY surface: that
    // agent's shell inherits `$TMUX_PANE` from the terminal that *requested*
    // the PTY, which owns nothing. Reading the environment again here would
    // name that outer pane after the runtime running inside the PTY.
    let pane = id.pane.clone()?;
    Some((pane, watch::agent_kind_short(id.kind)))
}

/// Spawn `cmd` via `/bin/sh -c`, feed it `stdin_bytes`, stream its stdout to
/// our stdout, and return its exit code (128 + signal if killed by signal).
fn run_forward(cmd: &str, stdin_bytes: &[u8]) -> Result<i32> {
    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(cmd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        // stderr is inherited so the forwarded tool can report errors.
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("failed to spawn forward command: {cmd}"))?;

    // Write the captured stdin in a scope so the pipe closes and the child
    // sees EOF — otherwise `npx`/`ccstatusline` would hang.
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(stdin_bytes)
            .with_context(|| "failed to write stdin to forward command")?;
    }

    if let Some(mut stdout) = child.stdout.take() {
        let mut out = std::io::stdout().lock();
        std::io::copy(&mut stdout, &mut out)
            .with_context(|| "failed to relay forward command stdout")?;
    }

    let status = child
        .wait()
        .with_context(|| "failed to wait on forward command")?;
    Ok(status.code().unwrap_or_else(|| {
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            status.signal().map_or(1, |s| 128 + s)
        }
        #[cfg(not(unix))]
        {
            1
        }
    }))
}

async fn handle_hook(client: &Client, cmd: HookCmd) -> Result<()> {
    match cmd {
        HookCmd::Claude { event } => {
            let ev = run_hook::<ClaudeAdapter, _>(&event, &mut std::io::stdin())?;
            best_effort_ingest(client, &ev).await;
            best_effort_default_alias(client, &ev).await;
        }
        HookCmd::ClaudeStatusline { forward } => {
            if let Some(cmd) = forward {
                // Forward mode: capture stdin, fire Heartbeat best-effort,
                // then tee stdin to the forwarded command and pass its
                // stdout + exit code back to our parent unchanged.
                let mut buf = Vec::new();
                std::io::stdin().read_to_end(&mut buf)?;

                // Best-effort Heartbeat: parse errors on stdin must never
                // cause the status line to fail, so log and move on.
                if let Ok(input) = serde_json::from_slice::<claude::StatusLineInput>(&buf) {
                    let pane = std::env::var("TMUX_PANE").ok();
                    let ev = claude::statusline_heartbeat(input, pane);
                    best_effort_ingest(client, &ev).await;
                } else {
                    tracing::debug!("claude-statusline: stdin was not valid statusline JSON");
                }

                let code = run_forward(&cmd, &buf)?;
                std::process::exit(code);
            }

            let input = claude::parse_statusline(&mut std::io::stdin())?;
            let label = input
                .model
                .as_ref()
                .and_then(|m| m.display_name.clone())
                .unwrap_or_else(|| "claude".into());
            let ctx = input
                .context_window
                .as_ref()
                .and_then(|c| c.used_percentage)
                .map(|p| format!(" · ctx {p:.0}%"))
                .unwrap_or_default();
            let cost = input
                .cost
                .as_ref()
                .and_then(|c| c.total_cost_usd)
                .map(|u| format!(" · ${u:.2}"))
                .unwrap_or_default();
            println!("{label}{ctx}{cost}");
            let pane = std::env::var("TMUX_PANE").ok();
            let ev = claude::statusline_heartbeat(input, pane);
            best_effort_ingest(client, &ev).await;
        }
        HookCmd::Codex { event } => {
            let ev = run_hook::<CodexAdapter, _>(&event, &mut std::io::stdin())?;
            best_effort_ingest(client, &ev).await;
            best_effort_default_alias(client, &ev).await;
        }
        HookCmd::Gemini { event } => {
            let ev = run_hook::<GeminiAdapter, _>(&event, &mut std::io::stdin())?;
            best_effort_ingest(client, &ev).await;
            best_effort_default_alias(client, &ev).await;
        }
        HookCmd::Agy { event } => {
            // FAIL-OPEN, and deliberately unlike the other hook arms.
            //
            // agy treats a non-zero hook exit as a verdict: a `PreToolUse`
            // handler that dies takes the tool call down with it
            // ("tool call denied by pre-tool hook"). Observability must never
            // be able to block the agent, so an unparseable payload — a shape
            // change in a future agy, a truncated stdin — is logged and
            // swallowed rather than propagated to a non-zero exit.
            match run_hook::<AntigravityAdapter, _>(&event, &mut std::io::stdin()) {
                Ok(ev) => {
                    best_effort_ingest(client, &ev).await;
                    best_effort_default_alias(client, &ev).await;
                }
                Err(e) => tracing::debug!(error = %e, event, "agy hook payload ignored"),
            }
        }
        HookCmd::Opencode { event } => {
            let ev = run_hook::<OpencodeAdapter, _>(&event, &mut std::io::stdin())?;
            best_effort_ingest(client, &ev).await;
            best_effort_default_alias(client, &ev).await;
        }
    }
    Ok(())
}

async fn cmd_status(
    client: &Client,
    cfg: &Config,
    theme: Option<ThemeArg>,
    json: bool,
) -> Result<()> {
    let agents = client.snapshot().await?;
    if json {
        let inputs = muxa::active_backends()
            .into_iter()
            .map(|backend| muxa::TopologyInput::new(backend.kind(), backend.list_panes()))
            .collect();
        return print_status_json(&agents, inputs, OffsetDateTime::now_utc());
    }
    if agents.is_empty() {
        println!("no active agents");
        return Ok(());
    }
    // Resolve the full pane inventory once via the active backend so
    // `pane_display` does N lookups against a slice instead of N
    // backend calls. Empty on backends without pane metadata (zellij
    // CLI without the WASM plugin) — table still renders, just with
    // raw pane ids in the location column.
    let backend = muxa::default_backend();
    let panes = backend.list_panes();
    let theme = theme::for_config(cfg, theme, use_colors());
    print_table(&agents, &panes, OffsetDateTime::now_utc(), theme);
    Ok(())
}

fn status_json(
    agents: &[Agent],
    inputs: Vec<muxa::TopologyInput>,
    generated_at: OffsetDateTime,
) -> muxa::TopologySnapshot {
    muxa::TopologySnapshot::build(generated_at, inputs, agents.to_vec())
}

fn print_status_json(
    agents: &[Agent],
    inputs: Vec<muxa::TopologyInput>,
    generated_at: OffsetDateTime,
) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    serde_json::to_writer(&mut out, &status_json(agents, inputs, generated_at))
        .context("serializing status JSON")?;
    writeln!(out)?;
    Ok(())
}

/// Format the global attention segment for tmux `status-right`. Empty when
/// nothing is blocked so the segment vanishes when all-clear; otherwise
/// tmux-style color markup — never raw ANSI, since tmux renders `#[...]`
/// itself exactly like the per-pane path relies on.
fn attention_segment(count: usize) -> String {
    if count == 0 {
        String::new()
    } else {
        let verb = if count == 1 { "needs" } else { "need" };
        format!("#[fg=red]⚠ {count} {verb} you#[default]")
    }
}

async fn cmd_status_line(
    client: &Client,
    pane: Option<String>,
    needs_attention: bool,
) -> Result<()> {
    // Global attention summary: a blocked agent in ANY pane surfaces
    // passively, even while you're focused on a different pane. Keep the
    // tight IPC deadline + empty-on-timeout contract of the status-line
    // path so a slow daemon can never stall the tmux status bar.
    if needs_attention {
        let agents = client
            .snapshot_with_timeout(STATUS_LINE_IPC_TIMEOUT)
            .await
            .unwrap_or_default();
        let count = agents
            .iter()
            .filter(|a| attend::needs_attention(a.state))
            .count();
        println!("{}", attention_segment(count));
        return Ok(());
    }

    let backend = muxa::default_backend();
    let pane = pane.or_else(|| backend.current_pane());
    let agents = match &pane {
        Some(p) => client
            .by_pane_with_timeout(p, STATUS_LINE_IPC_TIMEOUT)
            .await
            .unwrap_or_default(),
        None => client
            .snapshot_with_timeout(STATUS_LINE_IPC_TIMEOUT)
            .await
            .unwrap_or_default(),
    };
    if agents.is_empty() {
        println!();
        return Ok(());
    }

    let panes_snapshot = backend.list_panes();
    // tmux handles its own color markup, so we never emit ANSI here.
    let parts: Vec<String> = agents
        .iter()
        .map(|a| {
            let icon = state_icon(a.state);
            let kind = a.kind.to_string();
            // Prefer session:window when we can resolve it — makes the
            // status-line read "● main:2 claude_code" instead of a
            // context-free glyph.
            // Resolve against the snapshot fetched once above instead
            // of shelling out per agent.
            let loc = a.pane.as_deref().and_then(|raw| {
                panes_snapshot
                    .iter()
                    .find(|p| p.pane_id == raw)
                    .map(|p| format!(" {}:{}", p.session, p.window_index))
            });
            match loc {
                Some(l) => format!("{icon}{l} {kind}"),
                None => format!("{icon} {kind}"),
            }
        })
        .collect();
    println!("{}", parts.join(" | "));
    Ok(())
}

async fn cmd_recap(client: &Client, pane: Option<String>, limit: usize, all: bool) -> Result<()> {
    let pane = pane
        .or_else(|| muxa::default_backend().current_pane())
        .context("no pane given and could not determine current pane (set $TMUX_PANE / $ZELLIJ_PANE_ID, or pass --pane)")?;

    let agents = client.by_pane(&pane).await?;
    let history_limit = if all { Some(0) } else { Some(limit) };
    let history = client
        .recent_prompts(Some(&pane), history_limit)
        .await
        .unwrap_or_default();

    if agents.is_empty() && history.is_empty() {
        println!("no agent or recorded prompts in pane {pane}");
        return Ok(());
    }

    for a in agents {
        let kind = a.kind.to_string();
        let state = a.state.to_string();
        let prompt = a.last_prompt.unwrap_or_else(|| "(none)".into());
        println!("── {kind}  [{state}] ────────────");
        println!("{prompt}");
        println!();
    }

    if !history.is_empty() {
        let count = history.len();
        let header = if all {
            format!("── recent prompts ({count} total) ────────────")
        } else {
            format!("── recent prompts (showing up to {limit}) ────────────")
        };
        println!("{header}");
        for entry in history {
            // Concise, scannable row: "<rfc3339-short>  <kind>  <prompt-first-line>"
            let stamp = entry
                .at
                .format(time::macros::format_description!(
                    "[year]-[month]-[day] [hour]:[minute]"
                ))
                .unwrap_or_else(|_| entry.at.to_string());
            let first_line = entry.prompt.lines().next().unwrap_or("");
            println!("{stamp}  {}  {first_line}", entry.kind);
        }
    }
    Ok(())
}

fn cmd_panes() -> Result<()> {
    // Aggregate across every active host — the cross-multiplexer pane
    // inventory. Rows carry their namespace in `pane_id`, so a concat keeps
    // tmux `%N` and herdr `herdr:…` rows distinct. Per-host hints still fire
    // for any host in the set that contributed zero panes, so a single-host
    // user sees the same diagnostic as before while a multi-host user learns
    // which side is empty.
    let backends = muxa::active_backends();
    let mut all: Vec<muxa::tmux::PaneInfo> = Vec::new();
    let mut empty_hosts: Vec<&muxa::SharedBackend> = Vec::new();
    for backend in &backends {
        let panes = backend.list_panes();
        if panes.is_empty() {
            empty_hosts.push(backend);
        }
        all.extend(panes);
    }

    let terminal_width = terminal_width();
    let mut out = std::io::stdout().lock();
    for p in &all {
        let line = format!(
            "{:<8} {}:{}.{}  tty={}  cmd={}  title={}",
            p.pane_id, p.session, p.window_index, p.pane_index, p.tty, p.current_command, p.title
        );
        if let Err(error) = writeln!(out, "{}", truncate_cell(&line, terminal_width)) {
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error).context("write pane inventory");
        }
    }

    // A host contributed zero: print its diagnostic. When the set is a
    // single host and it's empty this reproduces the pre-multi-host hint;
    // when other hosts have panes it tells the user which side came up dry.
    for backend in empty_hosts {
        let hint = empty_pane_hint(backend.as_ref());
        if let Err(error) = writeln!(out, "{hint}") {
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error).context("write pane inventory hint");
        }
    }
    Ok(())
}

/// The empty-state diagnostic for a host that reported no panes. Two ways
/// to get here: the host really has no panes, or the backend's `caps()`
/// says metadata is plugin-only and not pushed yet — the zellij branch
/// differentiates so a misconfigured plugin is diagnosable.
fn empty_pane_hint(backend: &dyn muxa::PaneBackend) -> &'static str {
    match backend.kind() {
        muxa::HostKind::Tmux => "(no tmux panes — server may be down)",
        muxa::HostKind::Cmux => {
            "(cmux first slice: only the current env surface is visible; full inventory is pending)"
        }
        muxa::HostKind::Rmux => "(no rmux panes — server may be down or endpoint unreachable)",
        muxa::HostKind::Zellij if !backend.caps().current_command => {
            "(zellij CLI baseline: pane inventory is plugin-only — install the muxa zellij plugin to populate)"
        }
        muxa::HostKind::Zellij => "(no zellij panes)",
        muxa::HostKind::Herdr => {
            "(no herdr panes — server may be down or socket unreachable)"
        }
    }
}

/// Decide whether to emit ANSI color. We check `NO_COLOR` (per the de-facto
/// standard) and require stdout to be a TTY — piping `muxa status | grep`
/// should stay clean.
pub(crate) fn use_colors() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

pub(crate) fn terminal_width() -> usize {
    crossterm::terminal::size().map_or(DEFAULT_TERMINAL_WIDTH, |(width, _)| usize::from(width))
}

pub(crate) fn truncate_cell(value: &str, max_chars: usize) -> String {
    if value
        .chars()
        .map(|ch| UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum::<usize>()
        <= max_chars
    {
        return value.to_string();
    }

    if max_chars <= 3 {
        let mut used = 0;
        let mut out = String::new();
        for ch in value.chars() {
            let width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + width > max_chars {
                break;
            }
            used += width;
            out.push(ch);
        }
        return out;
    }

    let content_width = max_chars - 3;
    let mut used = 0;
    let mut out = String::new();
    for ch in value.chars() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + width > content_width {
            break;
        }
        used += width;
        out.push(ch);
    }
    out.push_str("...");
    out
}

/// Process-wide glyph set, recorded once at startup from `[ui] icons`.
///
/// Mirrors `use_colors()` as a global display predicate so the pure
/// `state_icon` / `state_marker` helpers don't need a config parameter
/// threaded through every call site. Unset (e.g. in unit tests) defaults
/// to `Unicode`, preserving prior behavior.
static ICON_SET: std::sync::OnceLock<IconSet> = std::sync::OnceLock::new();

pub(crate) fn set_icon_set(set: IconSet) {
    let _ = ICON_SET.set(set);
}

pub(crate) fn icon_set() -> IconSet {
    ICON_SET.get().copied().unwrap_or_default()
}

pub(crate) fn state_icon(state: AgentState) -> &'static str {
    match icon_set() {
        IconSet::Unicode => state_icon_unicode(state),
        IconSet::Ascii => state_icon_ascii(state),
    }
}

fn state_icon_unicode(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "●",
        AgentState::Idle => "○",
        AgentState::WaitingInput => "▶",
        AgentState::WaitingChoice => "◆",
        AgentState::Error => "■",
        AgentState::Stopped => "×",
        AgentState::Starting => "◌",
    }
}

fn state_icon_ascii(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "*",
        AgentState::Idle => "o",
        AgentState::WaitingInput => ">",
        AgentState::WaitingChoice => "?",
        AgentState::Error => "!",
        AgentState::Stopped => "x",
        AgentState::Starting => "~",
    }
}

pub(crate) fn state_style(state: AgentState) -> Style {
    match state {
        AgentState::Working => Style::new().green(),
        AgentState::WaitingInput => Style::new().yellow(),
        AgentState::WaitingChoice => Style::new().yellow().bold(),
        AgentState::Idle => Style::new().dimmed(),
        AgentState::Error => Style::new().red(),
        AgentState::Stopped => Style::new().dimmed().strikethrough(),
        AgentState::Starting => Style::new().cyan(),
    }
}

/// Pretty-print `Agent.pane` as `session:window.pane` by lookup against
/// a pre-fetched pane list, or fall back to the raw pane id (or `-` if
/// unknown).
///
/// **Why a slice and not a tmux shell-out**: this used to call
/// `tmux::resolve_pane` which itself shells out to `tmux list-panes
/// -a` per call. With 30+ agents that meant 30+ subprocess invocations
/// for one `muxa status` run. Callers now fetch the pane list once and
/// pass it down.
pub(crate) fn pane_display(a: &Agent, panes: &[muxa::tmux::PaneInfo]) -> String {
    let Some(raw) = a.pane.as_deref() else {
        // Paneless background tasks have no tmux location — show their
        // registered name instead of a context-free "-".
        if a.kind == AgentKind::Task {
            return a.session_id.clone();
        }
        return "-".to_string();
    };
    match panes.iter().find(|p| p.pane_id == raw) {
        Some(p) => format!("{}:{}.{}", p.session, p.window_index, p.pane_index),
        None => raw.to_string(),
    }
}

/// Human-friendly delta between `then` and `now`, rounded to the largest
/// unit. Past timestamps only — future clocks collapse to "0s ago".
pub(crate) fn relative_time(now: OffsetDateTime, then: OffsetDateTime) -> String {
    let delta = now - then;
    let secs = delta.whole_seconds().max(0);
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{mins}m ago");
    }
    let hours = mins / 60;
    if hours < 24 {
        return format!("{hours}h ago");
    }
    let days = hours / 24;
    format!("{days}d ago")
}

fn print_table(
    agents: &[Agent],
    panes: &[muxa::tmux::PaneInfo],
    now: OffsetDateTime,
    theme: CliTheme,
) {
    println!(
        "{}",
        render_status_table(agents, panes, now, theme, terminal_width())
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusTableLayout {
    Full,
    Compact,
    Minimal,
}

fn render_status_table(
    agents: &[Agent],
    panes: &[muxa::tmux::PaneInfo],
    now: OffsetDateTime,
    theme: CliTheme,
    terminal_width: usize,
) -> String {
    let layout = status_table_layout(terminal_width);
    let prompt_width = status_prompt_width(terminal_width, layout);
    let mut table = Table::new();
    table
        .load_preset(UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_constraints(status_table_constraints(layout, prompt_width))
        .set_header(status_table_header(layout, prompt_width, theme));

    for a in agents {
        let pane = pane_display(a, panes);
        let state_txt = status_state_label(a.state, layout).to_string();
        let last_activity = relative_time(now, a.last_activity_at);
        let prompt_raw = a.last_prompt.as_deref().unwrap_or("-");
        let prompt = prompt_raw.lines().next().unwrap_or("");

        let state_cell = status_state_cell(&state_txt, a.state, theme);
        let row = match layout {
            StatusTableLayout::Full => vec![
                Cell::new(truncate_cell(&pane, 24)),
                state_cell,
                theme.right_cell(truncate_cell(&last_activity, 7), TableTone::Dim),
                Cell::new(truncate_cell(prompt, prompt_width)),
            ],
            StatusTableLayout::Compact => vec![
                Cell::new(truncate_cell(&pane, 18)),
                state_cell,
                theme.right_cell(truncate_cell(&last_activity, 7), TableTone::Dim),
                Cell::new(truncate_cell(prompt, prompt_width)),
            ],
            StatusTableLayout::Minimal => vec![
                Cell::new(truncate_cell(&pane, 14)),
                state_cell,
                theme.right_cell(truncate_cell(&last_activity, 7), TableTone::Dim),
                Cell::new(truncate_cell(prompt, prompt_width)),
            ],
        };
        table.add_row(row);
    }

    format!("{table}")
}

fn status_table_layout(terminal_width: usize) -> StatusTableLayout {
    if terminal_width >= FULL_STATUS_TABLE_WIDTH {
        StatusTableLayout::Full
    } else if terminal_width >= COMPACT_STATUS_TABLE_WIDTH {
        StatusTableLayout::Compact
    } else {
        StatusTableLayout::Minimal
    }
}

fn status_table_constraints(
    layout: StatusTableLayout,
    prompt_width: usize,
) -> Vec<ColumnConstraint> {
    // Column order matches `status_table_header`: NAME, ST, ACT, LAST PROMPT.
    // Per-layout widths are the NAME / ST / ACT cells; the prompt cell
    // soaks up whatever's left and is provided by the caller.
    let widths: &[usize] = match layout {
        StatusTableLayout::Full => &[24, 14, 7],
        StatusTableLayout::Compact => &[18, 7, 7],
        StatusTableLayout::Minimal => &[14, 6, 7],
    };
    widths
        .iter()
        .copied()
        .chain(std::iter::once(prompt_width))
        .map(|width| {
            ColumnConstraint::Absolute(Width::Fixed(u16::try_from(width).unwrap_or(u16::MAX)))
        })
        .collect()
}

fn status_table_header(
    layout: StatusTableLayout,
    prompt_width: usize,
    theme: CliTheme,
) -> Vec<Cell> {
    // Default columns: NAME / ST / ACT / LAST PROMPT across every layout.
    // KIND and MODEL used to share the row but were demoted to opt-in
    // because the prompt is the highest-value column on every screen
    // size — leading with identity + state + age + content keeps the
    // narrow-terminal path readable without losing parity with wide
    // terminals.
    let prompt_label = if matches!(layout, StatusTableLayout::Full) {
        "LAST PROMPT"
    } else {
        "PROMPT"
    };
    vec![
        theme.cell("NAME", TableTone::Header),
        theme.cell("ST", TableTone::Header),
        theme.right_cell("ACT", TableTone::Header),
        theme.cell(truncate_cell(prompt_label, prompt_width), TableTone::Header),
    ]
}

fn status_prompt_width(terminal_width: usize, layout: StatusTableLayout) -> usize {
    // Sum of the non-prompt column widths declared in
    // `status_table_constraints` — keep these in sync.
    let fixed_width: usize = match layout {
        StatusTableLayout::Full => 24 + 14 + 7,
        StatusTableLayout::Compact => 18 + 7 + 7,
        StatusTableLayout::Minimal => 14 + 6 + 7,
    };
    // Every layout is now 4 columns (NAME / ST / ACT / LAST PROMPT).
    let column_count: usize = 4;
    let border_and_padding_width = column_count + 1 + column_count * 2;
    terminal_width
        .saturating_sub(fixed_width + border_and_padding_width)
        .clamp(MIN_STATUS_PROMPT_WIDTH, MAX_STATUS_PROMPT_WIDTH)
}

fn status_state_label(state: AgentState, layout: StatusTableLayout) -> &'static str {
    if layout == StatusTableLayout::Full {
        return match state {
            AgentState::Working => "working",
            AgentState::Idle => "idle",
            AgentState::WaitingInput => "waiting_input",
            AgentState::WaitingChoice => "waiting_choice",
            AgentState::Error => "error",
            AgentState::Stopped => "stopped",
            AgentState::Starting => "starting",
        };
    }
    match state {
        AgentState::Working => "work",
        AgentState::Idle => "idle",
        AgentState::WaitingInput => "input",
        AgentState::WaitingChoice => "choice",
        AgentState::Error => "error",
        AgentState::Stopped => "stop",
        AgentState::Starting => "start",
    }
}

fn status_state_cell(label: &str, state: AgentState, theme: CliTheme) -> Cell {
    theme.state_cell(label, state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::AgentKind;
    use time::macros::datetime;
    use unicode_width::UnicodeWidthStr;

    fn started(kind: AgentKind, pane: Option<&str>) -> muxa::event::AgentEvent {
        muxa::event::AgentEvent::Started {
            id: muxa::event::AgentId {
                kind,
                session_id: "s".into(),
                surface: None,
                pane: pane.map(ToString::to_string),
                tmux_socket: None,
                cwd: None,
            },
            at: time::OffsetDateTime::now_utc(),
        }
    }

    #[test]
    fn attached_session_paste_is_reframed_and_normalizes_newlines() {
        assert_eq!(
            bracketed_paste_input("first\nsecond\r\nthird"),
            "\x1b[200~first\rsecond\rthird\x1b[201~"
        );
    }

    #[test]
    fn attached_session_paste_cannot_close_its_own_brackets() {
        let framed =
            bracketed_paste_input("safe\x1b[201~\r\nnext\x1b[200~\x1b\x1b[201~[201~\r\ncommand");
        assert!(framed.starts_with("\x1b[200~"));
        assert!(framed.ends_with("\x1b[201~"));
        assert_eq!(framed.matches("\x1b[200~").count(), 1);
        assert_eq!(framed.matches("\x1b[201~").count(), 1);
        assert_eq!(&framed[6..framed.len() - 6], "safe\rnext\rcommand");
    }

    #[test]
    fn attached_session_drops_super_shortcuts() {
        let key = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('v'),
            crossterm::event::KeyModifiers::SUPER,
        );
        assert_eq!(key_to_pty_input(key), None);
    }

    #[test]
    fn a_paneless_start_names_nothing() {
        // `run_hook` reports `pane: None` for a muxa-owned PTY surface on
        // purpose: that agent's shell inherited `$TMUX_PANE` from whatever
        // terminal asked for the PTY, and that pane owns nothing. Reaching
        // for the environment here would name the outer pane after the
        // runtime running inside the PTY.
        assert_eq!(alias_target(&started(AgentKind::ClaudeCode, None)), None);
    }

    #[test]
    fn only_a_session_start_names_a_pane() {
        assert_eq!(
            alias_target(&started(AgentKind::ClaudeCode, Some("%7"))),
            Some(("%7".to_string(), "claude")),
        );
        // Everything else on the hook path fires per tool call.
        let tool = muxa::event::AgentEvent::ToolStarted {
            id: muxa::event::AgentId {
                kind: AgentKind::ClaudeCode,
                session_id: "s".into(),
                surface: None,
                pane: Some("%7".into()),
                tmux_socket: None,
                cwd: None,
            },
            tool: "Bash".into(),
            subagent: None,
            at: time::OffsetDateTime::now_utc(),
        };
        assert_eq!(alias_target(&tool), None);
    }

    #[test]
    fn pid_tracked_and_unknown_rows_name_nothing() {
        // A `Task` row is a subagent under a pane, not a pane of its own.
        assert_eq!(alias_target(&started(AgentKind::Task, Some("%7"))), None);
        assert_eq!(alias_target(&started(AgentKind::Unknown, Some("%7"))), None);
    }

    #[test]
    fn dispatch_kind_prefers_pane_namespace_over_fallback() {
        use muxa::HostKind;
        // Namespaced ids dispatch to their own host regardless of the
        // process-global fallback — a herdr row jumps via herdr even when
        // the shell is tmux-primary, and vice versa.
        assert_eq!(dispatch_kind("%3", HostKind::Herdr), HostKind::Tmux);
        assert_eq!(dispatch_kind("rmux:%3", HostKind::Tmux), HostKind::Rmux);
        assert_eq!(dispatch_kind("herdr:abc", HostKind::Tmux), HostKind::Herdr);
        assert_eq!(dispatch_kind("zellij:7", HostKind::Tmux), HostKind::Zellij);
        // Unrecognized ids fall back to the process-global host.
        assert_eq!(dispatch_kind("legacy-id", HostKind::Tmux), HostKind::Tmux);
        assert_eq!(dispatch_kind("legacy-id", HostKind::Herdr), HostKind::Herdr);
    }

    #[test]
    fn window_target_qualifies_the_window_with_its_session() {
        // The whole point: a window addressed together with its session
        // cannot be resolved into a *different* session of the same group.
        assert_eq!(window_target(Some("$1"), "@4", "%9"), "$1:@4");
    }

    #[test]
    fn window_target_falls_back_to_the_pane_id() {
        // No session, no window, or an empty id from an older PANE_FMT: the
        // pane id is what every jump used before, so degrade to it rather
        // than emit a malformed target like `:@4` that tmux would reject.
        assert_eq!(window_target(None, "@4", "%9"), "%9");
        assert_eq!(window_target(Some("$1"), "", "%9"), "%9");
        assert_eq!(window_target(Some(""), "@4", "%9"), "%9");
    }

    #[test]
    fn jump_stays_in_the_asking_client_session_without_probing() {
        // The ordinary same-session jump: the ids already match, so the
        // membership probe — a tmux round trip on a keypress path — must not
        // run at all.
        let answer = resolve_jump_session(Some("$0".into()), "$0", |_| {
            panic!("probed tmux for a session we already know matches")
        });
        assert_eq!(answer.as_deref(), Some("$0"));
    }

    #[test]
    fn jump_stays_in_the_asking_client_session_when_the_window_is_linked() {
        // The session-group case, and the whole point of the change: the pane
        // is recorded under `$0`, but the asking client sits in the grouped
        // sibling `$1` where that window is linked too. Answering `$1` keeps
        // this terminal in its own session, so `$0` — and whoever is looking
        // at it — never moves.
        let answer = resolve_jump_session(Some("$1".into()), "$0", |session| {
            assert_eq!(session, "$1");
            true
        });
        assert_eq!(answer.as_deref(), Some("$1"));
    }

    #[test]
    fn jump_crosses_to_the_pane_session_when_the_window_is_not_linked() {
        // A genuine cross-session jump. The pane's own session is a definite
        // destination; tmux would otherwise choose one from client activity.
        let answer = resolve_jump_session(Some("$1".into()), "$0", |_| false);
        assert_eq!(answer.as_deref(), Some("$0"));
    }

    #[test]
    fn jump_falls_back_when_the_client_session_is_unknown() {
        // No caller client, or a client that detached mid-call: the pane's
        // session still beats letting tmux guess.
        assert_eq!(
            resolve_jump_session(None, "$0", |_| unreachable!()).as_deref(),
            Some("$0")
        );
        assert_eq!(
            resolve_jump_session(Some(String::new()), "$0", |_| unreachable!()).as_deref(),
            Some("$0")
        );
        // Neither known — `window_target` degrades to the bare pane id.
        assert_eq!(resolve_jump_session(None, "", |_| unreachable!()), None);
    }

    #[test]
    fn session_target_prefers_the_stable_id_over_the_name() {
        // `callabo` matches `callabo-set` by prefix; `$3` matches nothing else.
        assert_eq!(session_target("$3", "callabo"), "$3");
        // Only a row with no id at all falls back to the ambiguous name.
        assert_eq!(session_target("", "callabo"), "callabo");
    }

    fn agent(session_id: &str, pane: Option<&str>, state: AgentState, prompt: &str) -> Agent {
        Agent {
            tmux_socket: None,
            tmux_session: None,
            kind: AgentKind::ClaudeCode,
            session_id: session_id.into(),
            surface: None,
            pane: pane.map(str::to_string),
            pid: None,
            workload: muxa::WorkloadSummary::default(),
            subagents: Vec::new(),
            cwd: None,
            state,
            last_prompt: Some(prompt.into()),
            last_response: None,
            recap: None,
            ai_title: None,
            last_notification: None,
            model: Some("claude-sonnet-very-long-model-name".into()),
            context_used_pct: None,
            cost_usd: None,
            rate_limit_5h_pct: None,
            rate_limit_5h_resets_at: None,
            rate_limit_7d_pct: None,
            rate_limit_7d_resets_at: None,
            rate_limited_until: None,
            rate_limit_scope: None,
            rate_limit_source: None,
            started_at: datetime!(2026-04-24 12:00:00 UTC),
            last_activity_at: datetime!(2026-04-24 11:55:00 UTC),
            state_entered_at: datetime!(2026-04-24 11:55:00 UTC),
        }
    }

    fn pane(id: &str, session: &str) -> muxa::tmux::PaneInfo {
        muxa::tmux::PaneInfo {
            session_group: None,
            agent_role: None,
            agent_alias: None,
            socket: None,
            pane_id: id.into(),
            session_id: String::new(),
            session: session.into(),
            window_id: String::new(),
            window_name: String::new(),
            window_index: "12".into(),
            pane_index: "3".into(),
            tty: "/dev/pts/0".into(),
            current_command: "claude".into(),
            title: String::new(),
            pane_pid: 0,
            current_path: String::new(),
        }
    }

    #[test]
    fn watch_sort_cli_aliases_parse_to_expected_keys() {
        for (raw, expected) in [
            ("name", vec![WatchSortKey::Name, WatchSortKey::Activity]),
            ("act", vec![WatchSortKey::Activity]),
            ("dur", vec![WatchSortKey::Duration]),
            ("st", vec![WatchSortKey::State, WatchSortKey::Activity]),
            ("pane_id", vec![WatchSortKey::PaneId]),
        ] {
            let args = Args::try_parse_from(["muxa", "watch", "--sort", raw]).unwrap();
            let Cmd::Watch {
                sort: Some(sort), ..
            } = args.cmd
            else {
                panic!("expected watch sort arg");
            };
            assert_eq!(sort.keys(), expected);
        }
    }

    #[test]
    fn watch_rejects_legacy_view_and_sort_vocabulary() {
        for args in [
            ["muxa", "watch", "--view", "work"],
            ["muxa", "watch", "--view", "swarm"],
            ["muxa", "watch", "--sort", "workspace"],
            ["muxa", "watch", "--sort", "workspace-time"],
        ] {
            assert!(Args::try_parse_from(args).is_err(), "accepted {args:?}");
        }
        assert!(Args::try_parse_from(["muxa", "watch", "--layout", "swarm"]).is_ok());
    }

    #[test]
    fn message_skill_cli_is_discoverable() {
        let args = Args::try_parse_from([
            "muxa",
            "skill",
            "add",
            "agent-review",
            "ask codex to review our changes",
        ])
        .unwrap();
        assert!(matches!(args.cmd, Cmd::Skill(_)));
        assert!(Args::try_parse_from(["muxa", "skill", "list"]).is_ok());
        assert!(Args::try_parse_from(["muxa", "skill", "remove", "agent-review"]).is_ok());
    }

    #[test]
    fn agent_start_cli_parses_a_structured_pane_launch() {
        let args = Args::try_parse_from([
            "muxa",
            "agent",
            "start",
            "--agent",
            "codex",
            "--target",
            "%42",
            "--cwd",
            "/tmp",
            "--prompt",
            "review the changes",
            "--direction",
            "down",
            "--json",
        ])
        .unwrap();
        let Cmd::Agent {
            action: AgentCmd::Start(start),
        } = args.cmd
        else {
            panic!("expected agent start");
        };
        assert_eq!(start.agent, agent_launch::AgentProgram::Codex);
        assert_eq!(start.host, agent_launch::LaunchHost::Auto);
        assert_eq!(start.placement, agent_launch::Placement::Pane);
        assert_eq!(start.target.as_deref(), Some("%42"));
        assert_eq!(start.direction, agent_launch::SplitDirection::Down);
        assert!(start.json);
    }

    #[test]
    fn work_start_cli_pins_ticket_agent_and_role() {
        let args = Args::try_parse_from([
            "muxa",
            "work",
            "start",
            "cal-7041",
            "--workspace",
            "muxa",
            "--agent",
            "codex",
            "--cwd",
            "/tmp",
            "--role",
            "reviewer",
            "--prompt",
            "review it",
        ])
        .unwrap();
        let Cmd::Work {
            action: WorkCmd::Start(start),
        } = args.cmd
        else {
            panic!("expected work start");
        };
        assert_eq!(start.work, "cal-7041");
        assert_eq!(start.workspace.as_deref(), Some("muxa"));
        assert_eq!(start.agent, agent_launch::AgentProgram::Codex);
        assert_eq!(start.role.as_deref(), Some("reviewer"));
    }

    #[test]
    fn work_up_cli_carries_the_pipeline_overrides() {
        let args = Args::try_parse_from([
            "muxa",
            "work",
            "up",
            "cal-1234",
            "--pipeline",
            "triad",
            "--body",
            "rebase onto main",
            "--skill",
            "/review-plan",
            "--context",
            "tests pass",
            "--no-ticket",
            "--dry-run",
            "--json",
        ])
        .unwrap();
        let Cmd::Work {
            action: WorkCmd::Up(up),
        } = args.cmd
        else {
            panic!("expected work up");
        };
        assert_eq!(up.work, "cal-1234");
        assert_eq!(up.pipeline.as_deref(), Some("triad"));
        assert_eq!(up.body.as_deref(), Some("rebase onto main"));
        assert_eq!(up.skill.as_deref(), Some("/review-plan"));
        assert_eq!(up.context.as_deref(), Some("tests pass"));
        assert!(up.no_ticket);
        assert!(up.dry_run);
        assert!(up.json);
    }

    #[test]
    fn work_up_still_accepts_prompt_as_a_spelling_of_body() {
        let args =
            Args::try_parse_from(["muxa", "work", "up", "cal-1234", "--prompt", "do it"]).unwrap();
        let Cmd::Work {
            action: WorkCmd::Up(up),
        } = args.cmd
        else {
            panic!("expected work up");
        };
        assert_eq!(up.body.as_deref(), Some("do it"));
    }

    #[test]
    fn work_down_is_a_spelling_of_work_close() {
        let args = Args::try_parse_from(["muxa", "work", "down", "cal-1234", "-y"]).unwrap();
        let Cmd::Work {
            action: WorkCmd::Down(down),
        } = args.cmd
        else {
            panic!("expected work down");
        };
        assert_eq!(down.work, "cal-1234");
        assert!(down.yes);
    }

    #[test]
    fn window_rename_cli_supports_explicit_automatic_and_buffered_names() {
        let args = Args::try_parse_from([
            "muxa",
            "window",
            "rename",
            "CAL-7175 auth refactor",
            "--window",
            "@42",
            "--json",
        ])
        .unwrap();
        let Cmd::Window {
            action: WindowCmd::Rename(rename),
        } = args.cmd
        else {
            panic!("expected window rename");
        };
        assert_eq!(rename.name.as_deref(), Some("CAL-7175 auth refactor"));
        assert!(rename.buffer.is_none());
        assert_eq!(rename.window.as_deref(), Some("@42"));
        assert!(!rename.auto);
        assert!(rename.json);

        let args = Args::try_parse_from(["muxa", "window", "rename", "--window", "@42", "--auto"])
            .unwrap();
        assert!(matches!(
            args.cmd,
            Cmd::Window {
                action: WindowCmd::Rename(tmux_work::WindowRenameArgs { auto: true, .. })
            }
        ));
        assert!(Args::try_parse_from([
            "muxa", "window", "rename", "name", "--window", "@42", "--auto"
        ])
        .is_err());

        let args = Args::try_parse_from([
            "muxa",
            "window",
            "rename",
            "--window",
            "@42",
            "--buffer",
            "muxa-window-name-123",
        ])
        .unwrap();
        let Cmd::Window {
            action: WindowCmd::Rename(rename),
        } = args.cmd
        else {
            panic!("expected buffered window rename");
        };
        assert_eq!(rename.buffer.as_deref(), Some("muxa-window-name-123"));
        assert!(rename.name.is_none());
    }

    #[test]
    fn workspace_cli_exposes_session_lifecycle() {
        let args = Args::try_parse_from(["muxa", "workspace", "show", "muxa", "--json"]).unwrap();
        let Cmd::Workspace {
            action: WorkspaceCmd::Show(show),
        } = args.cmd
        else {
            panic!("expected workspace show");
        };
        assert_eq!(show.workspace, "muxa");
        assert!(show.json);
    }

    #[test]
    fn agent_control_and_printable_onboarding_parse() {
        let args = Args::try_parse_from([
            "muxa",
            "agent",
            "control",
            "--pane",
            "%42",
            "--action",
            "interrupt",
        ])
        .unwrap();
        let Cmd::Agent {
            action: AgentCmd::Control(control),
        } = args.cmd
        else {
            panic!("expected agent control");
        };
        assert_eq!(control.pane.as_deref(), Some("%42"));
        assert!(control.session.is_none());

        let args = Args::try_parse_from([
            "muxa",
            "agent",
            "control",
            "--session",
            "pty-7",
            "--action",
            "terminate",
            "--yes",
        ])
        .unwrap();
        let Cmd::Agent {
            action: AgentCmd::Control(control),
        } = args.cmd
        else {
            panic!("expected native agent control");
        };
        assert_eq!(control.session.as_deref(), Some("pty-7"));
        assert!(control.pane.is_none());

        assert!(Args::try_parse_from([
            "muxa",
            "agent",
            "control",
            "--pane",
            "%42",
            "--session",
            "pty-7",
            "--action",
            "interrupt",
        ])
        .is_err());

        let args = Args::try_parse_from(["muxa", "onboard", "--print", "--no-quiz"]).unwrap();
        let Cmd::Onboard(onboard) = args.cmd else {
            panic!("expected onboard");
        };
        assert!(onboard.print);
        assert!(onboard.no_quiz);
    }

    #[test]
    fn watch_theme_cli_aliases_parse() {
        for (raw, expected) in [
            ("oh-my-muxa", WatchTheme::OhMyMuxa),
            ("oh_my_muxa", WatchTheme::OhMyMuxa),
            ("focus", WatchTheme::Focus),
            ("ops", WatchTheme::Ops),
            ("mono", WatchTheme::Mono),
            ("high-contrast", WatchTheme::HighContrast),
            ("high_contrast", WatchTheme::HighContrast),
            ("minimal", WatchTheme::Minimal),
        ] {
            let args = Args::try_parse_from(["muxa", "watch", "--theme", raw]).unwrap();
            let Cmd::Watch {
                theme: Some(theme), ..
            } = args.cmd
            else {
                panic!("expected watch theme arg");
            };
            assert_eq!(WatchTheme::from(theme), expected);
        }
    }

    #[test]
    fn table_theme_cli_aliases_parse() {
        for command in ["status", "stats", "timeline", "activity"] {
            let args = Args::try_parse_from(["muxa", command, "--theme", "high-contrast"])
                .unwrap_or_else(|err| panic!("{command} should accept --theme: {err}"));
            let theme = match args.cmd {
                Cmd::Status {
                    theme: Some(theme), ..
                } => theme,
                Cmd::Stats(args) => args.theme().expect("expected stats theme arg"),
                Cmd::Timeline(args) => args.theme().expect("expected timeline theme arg"),
                Cmd::Activity(args) => args.theme().expect("expected activity theme arg"),
                _ => panic!("expected {command} theme arg"),
            };
            assert_eq!(WatchTheme::from(theme), WatchTheme::HighContrast);
        }
    }

    #[test]
    fn status_json_cli_flag_parses_and_conflicts_with_theme() {
        let args = Args::try_parse_from(["muxa", "status", "--json"]).unwrap();
        assert!(matches!(
            args.cmd,
            Cmd::Status {
                json: true,
                theme: None
            }
        ));
        assert!(Args::try_parse_from(["muxa", "status", "--json", "--theme", "minimal"]).is_err());
    }

    #[test]
    fn status_json_uses_the_nested_topology_contract() {
        let mut tracked = agent(
            "session-1",
            Some("%7"),
            AgentState::WaitingInput,
            "Approve the deployment?\nMore detail",
        );
        tracked.cwd = Some("/tmp/project".into());
        tracked.last_response = Some("private assistant response".into());
        tracked.last_notification = Some("Approval required".into());
        tracked.context_used_pct = Some(42.5);
        tracked.cost_usd = Some(1.25);

        let generated_at = datetime!(2026-04-24 12:01:00 UTC);
        let value = serde_json::to_value(status_json(
            &[tracked],
            vec![muxa::TopologyInput::new(
                muxa::HostKind::Tmux,
                vec![pane("%7", "muxa")],
            )],
            generated_at,
        ))
        .unwrap();

        assert_eq!(value["schema_version"], 1);
        assert_eq!(value["generated_at"], "2026-04-24T12:01:00Z");
        assert_eq!(value["sessions"][0]["name"], "muxa");
        assert_eq!(value["sessions"][0]["windows"][0]["index"], "12");
        let agent = &value["sessions"][0]["windows"][0]["panes"][0]["agent"];
        assert_eq!(agent["kind"], "claude_code");
        assert_eq!(agent["agent_session_id"], "session-1");
        assert!(agent.get("session_id").is_none());
        assert_eq!(agent["state"], "waiting_input");
        assert_eq!(agent["last_prompt"], "Approve the deployment?\nMore detail");
        assert_eq!(agent["context_used_pct"], 42.5);
        assert_eq!(agent["last_response"], "private assistant response");
        assert!(agent.get("rate_limit_5h_pct").is_none());
        assert_eq!(value["sessions"][0]["states"]["waiting_input"], 1);
    }

    #[test]
    fn status_json_exposes_workload_counts_when_present() {
        let generated_at = datetime!(2026-04-24 12:01:00 UTC);
        let mut tracked = agent(
            "session-1",
            Some("%9"),
            AgentState::Working,
            "build the tree",
        );
        tracked.workload = muxa::process_tree::WorkloadSummary {
            primary_pid: Some(4321),
            process_count: 4,
            shell_count: 1,
            subagent_count: 2,
            helper_count: 1,
            preview: Vec::new(),
        };
        tracked.subagents = vec![
            muxa::state::Subagent {
                kind: "Explore".into(),
                description: Some("map the codebase".into()),
                started_at: generated_at,
            },
            muxa::state::Subagent {
                kind: "general-purpose".into(),
                description: None,
                started_at: generated_at,
            },
        ];

        let value = serde_json::to_value(status_json(
            &[tracked],
            vec![muxa::TopologyInput::new(
                muxa::HostKind::Tmux,
                vec![pane("%9", "muxa")],
            )],
            generated_at,
        ))
        .unwrap();

        let agent = &value["sessions"][0]["windows"][0]["panes"][0]["agent"];
        let wl = &agent["workload"];
        assert_eq!(wl["subagent_count"], 2);
        assert_eq!(wl["shell_count"], 1);
        assert_eq!(wl["process_count"], 4);
        assert_eq!(wl["helper_count"], 1);
        assert!(wl.get("preview").is_none());

        // Named, hook-tracked Task subagents ride in a sibling array.
        let subs = &agent["subagents"];
        assert_eq!(subs[0]["kind"], "Explore");
        assert_eq!(subs[0]["description"], "map the codebase");
        assert_eq!(subs[0]["started_at"], "2026-04-24T12:01:00Z");
        assert_eq!(subs[1]["kind"], "general-purpose");
        assert!(subs[1].get("description").is_none());
    }

    const ALL_STATES: [AgentState; 7] = [
        AgentState::Working,
        AgentState::WaitingInput,
        AgentState::WaitingChoice,
        AgentState::Error,
        AgentState::Idle,
        AgentState::Starting,
        AgentState::Stopped,
    ];

    #[test]
    fn status_line_icons_are_single_cell() {
        for state in ALL_STATES {
            assert_eq!(UnicodeWidthStr::width(state_icon(state)), 1);
        }
    }

    #[test]
    fn attention_segment_is_empty_when_all_clear() {
        // Nothing blocked → empty string so the tmux segment disappears.
        assert_eq!(attention_segment(0), "");
    }

    #[test]
    fn attention_segment_renders_count_with_tmux_markup() {
        let seg = attention_segment(2);
        assert_eq!(seg, "#[fg=red]⚠ 2 need you#[default]");
        // tmux styling, never raw ANSI (no ESC).
        assert!(!seg.contains('\u{1b}'));
        // Wraps the visible text in tmux color markup.
        assert!(seg.starts_with("#[fg=red]"));
        assert!(seg.ends_with("#[default]"));
        assert!(seg.contains("2 need you"));

        // Count is faithfully interpolated for other N, with subject-verb
        // agreement (singular "needs", plural "need").
        assert!(attention_segment(1).contains("1 needs you"));
        assert!(attention_segment(7).contains("7 need you"));
    }

    #[test]
    fn icon_sets_are_single_cell_and_distinct() {
        // Both glyph sets must stay one cell wide and unambiguous so the
        // [ui] icons toggle never breaks column alignment or readability.
        for build in [state_icon_unicode, state_icon_ascii] {
            let mut seen = std::collections::HashSet::new();
            for state in ALL_STATES {
                let glyph = build(state);
                assert_eq!(
                    UnicodeWidthStr::width(glyph),
                    1,
                    "{glyph:?} not single-cell"
                );
                assert!(seen.insert(glyph), "duplicate glyph {glyph:?}");
            }
        }
        // The ascii set must be pure ASCII to survive font-less terminals.
        for state in ALL_STATES {
            assert!(state_icon_ascii(state).is_ascii());
        }
    }

    #[test]
    fn relative_time_units() {
        let now = datetime!(2026-04-24 12:00:00 UTC);
        assert_eq!(relative_time(now, now), "0s ago");
        assert_eq!(
            relative_time(now, datetime!(2026-04-24 11:59:30 UTC)),
            "30s ago"
        );
        assert_eq!(
            relative_time(now, datetime!(2026-04-24 11:55:00 UTC)),
            "5m ago"
        );
        assert_eq!(
            relative_time(now, datetime!(2026-04-24 10:00:00 UTC)),
            "2h ago"
        );
        assert_eq!(
            relative_time(now, datetime!(2026-04-22 12:00:00 UTC)),
            "2d ago"
        );
    }

    #[test]
    fn relative_time_future_clamped() {
        let now = datetime!(2026-04-24 12:00:00 UTC);
        // Clock skew or reordering — don't emit negative strings.
        assert_eq!(
            relative_time(now, datetime!(2026-04-24 12:00:30 UTC)),
            "0s ago"
        );
    }

    #[test]
    fn truncate_cell_clips_to_ascii_ellipsis() {
        assert_eq!(truncate_cell("short", 10), "short");
        assert_eq!(truncate_cell("0123456789abc", 8), "01234...");
        assert_eq!(truncate_cell("한글테스트입니다", 6), "한...");
    }

    #[test]
    fn pane_display_shows_task_name_when_paneless() {
        // A paneless background task shows its registered name in NAME...
        let mut task = agent("game", None, AgentState::Working, "");
        task.kind = AgentKind::Task;
        assert_eq!(pane_display(&task, &[]), "game");
        // ...while other paneless agents (e.g. SDK sessions) still show "-".
        let sdk = agent("sdk-1", None, AgentState::Idle, "");
        assert_eq!(pane_display(&sdk, &[]), "-");
    }

    #[test]
    fn status_table_compacts_without_wrapping_at_88_cols() {
        let agents = vec![
            agent(
                "a",
                Some("%1"),
                AgentState::WaitingChoice,
                "please choose one of the available deployment options",
            ),
            agent(
                "b",
                Some("%2"),
                AgentState::Working,
                "this is a very long prompt that should be truncated in compact status output",
            ),
        ];
        let panes = vec![
            pane("%1", "callabo-knowledge-long-session-name"),
            pane("%2", "muxa"),
        ];

        let rendered = render_status_table(
            &agents,
            &panes,
            datetime!(2026-04-24 12:00:00 UTC),
            CliTheme::plain(),
            88,
        );

        assert!(rendered.contains("choice"));
        assert!(!rendered.contains("MODEL"));
        assert!(rendered.contains("callabo-knowled..."));
        for line in rendered.lines() {
            assert!(
                UnicodeWidthStr::width(line) <= 88,
                "line exceeded compact status width: {line:?}"
            );
        }
    }

    #[test]
    fn status_table_minimal_layout_for_very_narrow_width() {
        let agents = vec![agent(
            "a",
            Some("%1"),
            AgentState::WaitingInput,
            "approve permission",
        )];
        let panes = vec![pane("%1", "callabo-set")];

        let rendered = render_status_table(
            &agents,
            &panes,
            datetime!(2026-04-24 12:00:00 UTC),
            CliTheme::plain(),
            60,
        );

        assert!(rendered.contains("ST"));
        assert!(rendered.contains("ACT"));
        assert!(rendered.contains("NAME"));
        assert!(!rendered.contains("KIND"));
        assert!(!rendered.contains("MODEL"));
        for line in rendered.lines() {
            assert!(
                UnicodeWidthStr::width(line) <= 60,
                "line exceeded minimal status width: {line:?}"
            );
        }
    }
}
