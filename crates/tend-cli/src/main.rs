//! tend CLI — user-facing entry point.

mod account_module;
mod activity_query;
mod agent_find;
mod agent_launch;
mod agent_move;
mod agent_records;
mod ask_progress;
mod attend;
mod automation;
mod board_screen;
mod collab_screen;
mod collaboration_output;
mod config_cmd;
mod config_sync_cmd;
mod daemon;
mod dashboard_cmd;
mod decision_record;
mod doctor;
mod doctor_report;
mod fleet_cli;
mod fleet_sync;
mod fleet_watch;
mod help_groups;
mod init;
mod interactive_child;
mod logs;
mod mcp;
mod mcp_output;
mod mcp_reload;
mod mcp_revision;
mod mcp_surface;
mod message_skill;
mod mux_control;
mod onboarding;
mod orchestration;
mod owner_only;
mod pane_identity;
mod peek;
mod peer_cli;
mod relay;
mod reload;
mod review_cmd;
mod review_evidence;
mod servers_cmd;
mod sidebar;
mod spawn_provenance;
mod stale_agents;
mod start;
mod stats;
mod swap;
mod theme;
mod time_range;
mod timeline;
mod tmux_work;
mod upgrade;
mod watch;
#[cfg(windows)]
mod windows_detach;
mod work_backlog;
mod work_compose;
mod work_init;
mod work_options;
mod work_pipeline;
mod work_up;
mod worker_cmd;
mod worker_startup;
mod worktrees_cmd;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use comfy_table::presets::UTF8_BORDERS_ONLY;
use comfy_table::{Cell, ColumnConstraint, ContentArrangement, Table, Width};
use owo_colors::Style;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tend::adapters::{claude, run_hook, AntigravityAdapter, ClaudeAdapter, CodexAdapter};
use tend::collaboration::{
    CollaborationClientKind, CollaborationOrigin, CollaborationOriginMatch, MailboxScope,
    NewRequest, Participant, RequestKind, RequestMailbox, RequestStatus, WorkMode,
};
use tend::config::{IconSet, WatchConfig, WatchSortKey, WatchTheme};
use tend::ipc::Client;
use tend::state::Agent;
use tend::{
    discovery, paths, tmux, ActivityEntry, AgentKind, AgentState, Config, HumanInteractionEntry,
    HumanInteractionInput, HumanInteractionKind,
};
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
#[command(name = "tend", version, about = "tend CLI")]
struct Args {
    #[arg(long, env = "TEND_SOCKET", global = true)]
    socket: Option<PathBuf>,

    /// Config file path. Defaults to `$XDG_CONFIG_HOME/tend/config.toml`.
    #[arg(long, env = "TEND_CONFIG", global = true)]
    config: Option<PathBuf>,

    /// Check startup configuration without starting services (upgrade preflight).
    #[arg(long, global = true, hide = true)]
    check_config: bool,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Debug, clap::Args)]
struct AgentsArgs {
    #[arg(long, required = true)]
    json: bool,
}

// Keep this optional view's command builder out of the large root parser frame.
#[derive(Debug, Subcommand)]
enum AgentViewCmd {
    /// List agents with display-only launch provenance as JSON.
    Agents(AgentsArgs),
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Open Tend: find running agents, start one, or try the short demo.
    Start,
    #[command(flatten)]
    AgentView(AgentViewCmd),
    /// List active agents as a human-readable table.
    Status {
        /// One-shot visual theme override.
        #[arg(long, value_enum, conflicts_with = "json")]
        theme: Option<ThemeArg>,
        /// One row per agent instead of the session / window / pane tree.
        #[arg(long, conflicts_with = "json")]
        flat: bool,
        /// Emit a stable JSON snapshot for desktop widgets and other integrations.
        #[arg(long)]
        json: bool,
    },
    /// Print a one-liner status suitable for tmux `status-right`.
    #[command(hide = true)]
    StatusLine {
        #[arg(long)]
        pane: Option<String>,
        /// Emit a GLOBAL attention summary (`⚠ N need you`) counting every
        /// tracked agent that's blocked on a human, instead of the per-pane
        /// detail, followed by open review items (`◇ N to review · age`).
        /// Prints an empty line when nothing is blocked or awaiting review,
        /// so the tmux segment disappears when all-clear. Ignores `--pane`.
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
    /// Send a peer request through dedicated workers, creating one when needed.
    Peer(peer_cli::Args),
    /// Durable request/reply messaging with agents in the current tmux window.
    Msg {
        #[command(subcommand)]
        action: MsgCmd,
    },
    /// Ask a headless provider and print the durable reply. The built-ins
    /// are claude, codex, gemini, anthropic, and openai — plus apple on a
    /// Mac; add your own with `tend ask provider add`, and `tend ask
    /// providers` lists every one the daemon can drive.
    #[command(args_conflicts_with_subcommands = true)]
    Ask {
        #[command(subcommand)]
        action: Option<AskCmd>,
        /// Question or task for the selected headless provider.
        prompt: Option<String>,
        /// Override the daemon's selected provider for this and later
        /// asks. Any provider id from `tend ask providers`.
        #[arg(long, value_name = "ID")]
        agent: Option<String>,
        /// Read one API key from piped stdin and use it only for this turn.
        /// The key is never stored in config, history, logs, or argv.
        #[arg(long)]
        api_key_stdin: bool,
        /// Queue the Ask and return immediately instead of waiting for its answer.
        #[arg(long)]
        detach: bool,
        /// Start a fresh conversation with the provider instead of resuming
        /// the current one. The old conversation stays in history.
        #[arg(long)]
        new: bool,
        /// Print the stored Ask entry as JSON.
        #[arg(long)]
        json: bool,
        /// Maximum time this CLI waits for the durable answer.
        #[arg(long, default_value_t = 1800)]
        timeout_secs: u64,
    },
    /// Register reusable `/` templates for the interactive message composer.
    Skill(message_skill::Args),
    /// Rules that watch agent state and act on it — resuming a session
    /// after a usage cap resets, for one. See docs/AUTOMATION.md.
    Automation(automation::Args),
    /// Read and replace the daemon's config.toml, the same way Tend.app's
    /// Advanced settings does.
    Config(config_cmd::Args),
    /// Worker agents tend launched for peer work, across every room: state,
    /// observed idle time, open requests, and what is left behind.
    Workers(worker_cmd::Args),
    /// Results someone asked to have reviewed, kept visible until accepted,
    /// returned or withdrawn.
    Review(Box<review_cmd::Args>),
    /// Git worktrees tend created on this host, whether each could be
    /// cleaned up, and the cleanup: set aside, restore, delete. Lists only
    /// worktrees tend holds a receipt for; changes need --yes.
    Worktrees(worktrees_cmd::Args),
    /// TCP servers still listening on this host that tend's agents or
    /// worktrees are associated with. Read-only.
    Servers(servers_cmd::Args),
    /// Former name of `tend fleet host`. Tend.app still invokes it, and an
    /// app may run against an older or newer CLI, so keep this alias.
    #[command(hide = true)]
    Host(fleet_cli::HostArgs),
    /// Observe and control this node plus SSH-connected Tend hosts.
    Fleet(fleet_cli::FleetArgs),
    /// Agent lifecycle and searchable conversation/command evidence.
    Agent {
        #[command(subcommand)]
        action: AgentCmd,
    },
    /// Manage tmux windows using their stable native identity.
    Window {
        #[command(subcommand)]
        action: WindowCmd,
    },
    /// Manage Tend Work and its current tmux Run window.
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
    Stats(stats::StatsCommand),
    /// Former name of `tend stats report`.
    #[command(hide = true)]
    Report(stats::ReportInvocation),
    /// Explore agent work/wait/error intervals as an interactive timeline.
    Timeline(timeline::Args),
    /// The Work board: `tend watch` opened on its board screen, tracking Work,
    /// Runs, external issues, agents and their collaboration.
    Dashboard(dashboard_cmd::Args),
    /// Former name of `tend stats activity`.
    #[command(hide = true)]
    Activity(activity_query::ActivityInvocation),
    /// Hook adapter entrypoints invoked by the agent CLIs themselves.
    #[command(hide = true)]
    Hook {
        #[command(subcommand)]
        which: HookCmd,
    },
    /// Former name of `tend registry panes`.
    #[command(hide = true)]
    Panes,
    /// Inspect and repair the agent registry: backfill from panes, drop
    /// orphan rows, list pane hosts.
    Registry {
        #[command(subcommand)]
        action: RegistryCmd,
    },
    /// Overlay every pane in the current tmux window with its agent's
    /// state, summary, latest prompt, and latest response — then jump to
    /// one by its `display-panes` digit.
    ///
    /// Press `|` for a scrollable reader, or start with `--expanded`.
    /// Designed to be bound to `prefix + q` via a borderless fullscreen
    /// `display-popup` (see `tend init --only tmux-peek`).
    Peek(peek::Args),
    /// Persistent left/right agent tree, with sidebar-aware focus zoom.
    Sidebar(sidebar::Args),
    /// Fullscreen nested session/window/pane topology of tracked agents.
    Watch {
        /// Open watch in a popup on the current tmux or rmux server.
        #[arg(long)]
        popup: bool,
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
        /// Presentation of the same topology: nested tree (default), swarm, or
        /// a flat one-row-per-Work table.
        #[arg(long, value_enum)]
        layout: Option<WatchLayoutArg>,
        /// Collaboration history presentation: table (default) or sequence.
        /// This is independent from the topology `--layout`.
        #[arg(long, value_parser = parse_watch_collab_layout)]
        collab_layout: Option<watch::CollabLayout>,
        /// Which list to open on: the topology (default), collaboration
        /// across every room the daemon holds, or the Work board.
        #[arg(long, value_enum)]
        screen: Option<WatchScreenArg>,
        /// Work board order: attention (default), activity, busy, or name.
        #[arg(long, value_parser = parse_board_sort)]
        board_sort: Option<tend::config::WatchBoardSort>,
        /// Work board ACT/WACT window, as `tend stats --since` takes it.
        #[arg(long, value_name = "SINCE")]
        since: Option<String>,
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
    /// Run, attach to, or register processes tend tracks without agent hooks.
    Proc {
        #[command(subcommand)]
        action: ProcCmd,
    },
    /// Former name of `tend proc run`.
    #[command(hide = true)]
    Run(RunArgs),
    /// Former name of `tend proc attach`.
    #[command(hide = true)]
    Attach { session: String },
    /// Former name of `tend proc detach`.
    #[command(hide = true)]
    Detach { session: String },
    /// Former name of `tend proc register`.
    #[command(hide = true)]
    Register(RegisterArgs),
    /// Internal bridge used by the zellij WASM plugin.
    #[command(hide = true)]
    ZellijPluginSnapshot {
        #[arg(long)]
        json: String,
    },
    /// Bridge the owner-only local tendd socket over an authenticated SSH
    /// stdio channel for Tend Fleet. Usually launched by another tendd.
    #[command(hide = true)]
    Relay {
        #[arg(long, default_value_t = true)]
        stdio: bool,
    },
    /// Exact remote pane attach endpoint used by `tend fleet attach`.
    #[command(hide = true)]
    FleetRemoteAttach {
        token: String,
        #[arg(long)]
        fit: bool,
    },
    /// Jump to the agent that needs you — focus the pane of whichever
    /// agent has been blocked on input/choice/error longest. `--cycle`
    /// rotates through them (bind it to a tmux key); `--list` prints the
    /// queue without jumping.
    #[command(visible_alias = "go")]
    Attend(attend::Args),
    /// Former name of `tend registry sync`.
    #[command(hide = true)]
    Sync,
    /// Interactive install wizard — wires tmux, agent hooks, systemd,
    /// and the dashboard. Use `--preset standard --yes` for one-shot
    /// non-interactive installs.
    Init(init::Args),
    /// Run end-to-end diagnostics and report any setup issues.
    Doctor {
        /// List every check, including the ones that pass.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Start, stop, restart, or inspect the local tendd process.
    Daemon {
        #[command(subcommand)]
        action: daemon::Action,
    },
    /// Learn the work/session, agent/pane policy, normal workflow, and watch shortcuts.
    Onboard(onboarding::Args),
    /// Run a Model Context Protocol (MCP) stdio server so a coding agent
    /// can orchestrate tend — inspect other agents, send them prompts,
    /// capture panes, and wait for state changes. Wire it into Claude Code
    /// with `claude mcp add --scope user tend -- tend mcp`. Refuses to start if the
    /// daemon socket is unreachable. See docs/MCP.md.
    Mcp,
    /// Former name of `tend daemon logs`, kept for scripts and muscle memory.
    #[command(hide = true)]
    Logs(logs::Args),
    /// Capture this multiplexer's workspace — sessions, windows, panes,
    /// geometry, and what each pane is running — so a restart can be undone
    Snapshot(reload::SnapshotArgs),

    /// Rebuild a workspace from a snapshot. Prints the plan unless `--run`
    Restore(reload::RestoreArgs),

    /// Reload MCP connections or snapshot/restart/restore the multiplexer
    Reload(reload::ReloadArgs),

    /// Move agents onto another account (`aas` is optional) and resume them in place
    Swap(swap::Args),

    /// Update tend from the source repo: `git pull` → cargo install
    /// `tendd` + `tend-cli` → restart the daemon → verify the IPC
    /// socket is responsive. One command for the full update flow.
    Upgrade(upgrade::Args),
    /// Former name of `tend registry prune`.
    #[command(hide = true)]
    Prune(PruneArgs),
}

#[derive(Debug, Subcommand)]
enum ProcCmd {
    /// Run a command in a tend-owned PTY session.
    Run(RunArgs),
    /// Attach this terminal to a tend-owned PTY session.
    Attach { session: String },
    /// Mark a tend-owned PTY session as detached.
    Detach { session: String },
    /// Show any background process in `tend status`/`tend watch`.
    ///
    /// For a shell script, game, or automation loop. Tracked by pid
    /// liveness. Defaults `--pid` to the calling shell.
    Register(RegisterArgs),
}

#[derive(Debug, clap::Args)]
struct RunArgs {
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
}

#[derive(Debug, clap::Args)]
struct RegisterArgs {
    /// Display name shown in the NAME column.
    #[arg(long)]
    name: String,
    /// Process id to track. Defaults to the parent process (the shell
    /// that invoked `tend proc register`).
    #[arg(long)]
    pid: Option<u32>,
    /// Working directory to record (informational).
    #[arg(long)]
    cwd: Option<String>,
    /// tmux pane id to associate, when the process lives in one.
    #[arg(long)]
    pane: Option<String>,
}

#[derive(Debug, Subcommand)]
enum RegistryCmd {
    /// Backfill the registry by scanning tmux panes for agent processes.
    Sync,
    /// Delete accumulated "orphan" agent rows.
    ///
    /// Orphans are paneless, surfaceless,
    /// pid-less ghosts left by remote/detached sessions (e.g. codex driven
    /// through a detached `app-server`), including references to missing
    /// native PTY sessions. Only registry rows are removed;
    /// tmux sessions are never touched. The daemon also ages these out on
    /// its own after `[reconciler] paneless_stale_timeout_secs` (24h default).
    Prune(PruneArgs),
    /// Forget one agent registry row by its exact session id.
    ///
    /// Use `tend status --json` to find `agent_session_id`. Useful for stale
    /// cmux records that prune cannot remove. Leaves the process and pane
    /// running; a later hook or scan can register the agent again.
    Forget {
        session_id: String,
        /// Skip the confirmation prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Debug: print the active pane-host inventory.
    Panes,
}

#[derive(Debug, clap::Args)]
struct PruneArgs {
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
}

#[derive(Debug, Subcommand)]
enum MsgCmd {
    /// Explicitly accept one peer request on this session, including a protected primary.
    Accept {
        request_id: String,
        #[arg(long)]
        json: bool,
    },
    /// Ask for required human attention; shown in connected Fleet operator inboxes.
    Attention {
        body: String,
        #[arg(long, default_value = "information", value_parser = ["approval", "choice", "information"])]
        action: String,
        #[arg(long)]
        json: bool,
        /// Return the legacy full request, including bodies and audit data.
        #[arg(long, requires = "json")]
        full: bool,
    },
    /// Send to `peer`, `%N`, `@alias`, `role:<role>`, or a
    /// `workspace/work/role[@alias]` path.
    Send {
        target: String,
        body: String,
        /// Typed v1 task contract JSON file (requires a supporting daemon).
        #[arg(long)]
        task_contract: Option<PathBuf>,
        /// Replace the relevance of your read-only task; retain late replies as history.
        #[arg(long, requires = "task_contract")]
        supersedes: Option<String>,
        /// Decision required from the operator; use with target `human`.
        #[arg(long, value_parser = ["approval", "choice", "information"])]
        human_action: Option<String>,
        #[arg(long, default_value = "question")]
        kind: String,
        /// Stable conversation id used to group related requests and replies.
        #[arg(long)]
        thread: Option<String>,
        /// Request id this message follows. Its thread is inherited when
        /// `--thread` is omitted.
        #[arg(long)]
        parent: Option<String>,
        /// Stable Work id associated with this message (for example ENG-7345).
        #[arg(long)]
        work: Option<String>,
        /// Workspace containing `--work`.
        #[arg(long)]
        workspace: Option<String>,
        /// Stable Run id associated with this message.
        #[arg(long)]
        run: Option<String>,
        /// Explicitly authorize edits; the default collaboration contract is read-only.
        #[arg(long)]
        execute: bool,
        /// Advisory writable path scope. Repeat for multiple paths.
        #[arg(long = "path")]
        paths: Vec<String>,
        /// Generic artifact produced or consumed by this request. Repeatable.
        #[arg(long = "artifact")]
        artifacts: Vec<String>,
        /// Related URL or external resource. Repeatable.
        #[arg(long = "link")]
        links: Vec<String>,
        /// Fire-and-forget message; do not expect a reply.
        #[arg(long)]
        no_reply: bool,
        /// Override recipient wake delivery (true for urgent notices, false for quiet records).
        /// Bare `--notify` means true; `--notify false` keeps it quiet.
        /// Defaults to quiet for notice, enabled for other kinds; still respects idle-only wake.
        #[arg(long, action = clap::ArgAction::Set, num_args = 0..=1, default_missing_value = "true")]
        notify: Option<bool>,
        /// Your earlier request this one replaces: its late reply will not
        /// wake you. Its work and reply are unchanged. Repeatable.
        #[arg(long = "withdraw", value_name = "REQUEST")]
        withdraw: Vec<String>,
        /// The user told you to deliver this: a protected primary (a human's
        /// own session) takes it at its next turn boundary, through its
        /// provider hook, instead of holding it for `msg accept`.
        /// Use only on the user's explicit instruction; it is recorded on
        /// the request.
        #[arg(long)]
        user_authorized: bool,
        /// Print a compact acknowledgement without repeating request/reply bodies.
        #[arg(long)]
        json: bool,
        /// Return the legacy full request, including bodies and audit data.
        #[arg(long, requires = "json")]
        full: bool,
    },
    /// Append quiet guidance/progress to an existing open request, preserving its scope.
    Update {
        request_id: String,
        /// Guidance/progress within the existing scope, at most 8192 UTF-8 bytes.
        body: String,
        /// Print a compact acknowledgement; read the full record with `msg list --request-id`.
        #[arg(long)]
        json: bool,
    },
    /// Claim and print this agent's pending requests.
    Inbox {
        /// Compact instructions and routing, matching the MCP inbox view.
        #[arg(long)]
        json: bool,
        /// Include legacy full audit records instead of the compact view.
        #[arg(long, requires = "json")]
        full: bool,
    },
    /// List incoming, sent, or all requests without claiming them.
    List {
        #[arg(long, default_value = "all")]
        mailbox: String,
        /// `auto`: own mailbox for a tracked agent, current window for an
        /// ordinary terminal. `caller`: own mailbox; `room`: current window;
        /// `all`: every room. Room/all reads speak as the operator console.
        #[arg(long, default_value = "auto")]
        scope: String,
        /// Only messages in this time range. Accepts durations such as `2h`
        /// and `7d`, dates, RFC 3339 timestamps, and calendar keywords.
        #[arg(long)]
        since: Option<String>,
        /// Only messages associated with this Work id.
        #[arg(long)]
        work: Option<String>,
        /// Only messages associated with this workspace. Combines with
        /// `--work` when both are supplied.
        #[arg(long)]
        workspace: Option<String>,
        /// Only messages in this conversation thread.
        #[arg(long)]
        thread: Option<String>,
        /// Only messages of this kind: question, review, task, or notice.
        #[arg(long)]
        kind: Option<String>,
        /// Only messages with this status: queued, claimed, completed,
        /// blocked, declined, failed, expired, or cancelled.
        #[arg(long)]
        status: Option<String>,
        /// Only messages whose sender or recipient is in this window. Matches
        /// either the human window name or stable tmux window id.
        #[arg(long, visible_alias = "room")]
        window: Option<String>,
        /// Maximum number of matching messages to print (default 20; 0 for all).
        #[arg(long, default_value = "20")]
        limit: Option<usize>,
        /// Read one exact request instead of retrieving mailbox history.
        #[arg(long, conflicts_with_all = ["mailbox", "since", "work", "workspace", "thread", "kind", "status", "window", "limit", "offset"])]
        request_id: Option<String>,
        /// Read only updates newer than this sequence, without the original task body.
        #[arg(long, requires = "request_id", conflicts_with = "full")]
        after_update: Option<u64>,
        /// Include full original requests and replies; default is metadata only.
        #[arg(long)]
        full: bool,
        /// Number of matching messages to skip before applying `--limit`.
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long)]
        json: bool,
    },
    /// Complete, block, decline, or fail a request.
    Reply {
        request_id: String,
        /// Typed task report JSON file, bound to the request attempt and revision.
        #[arg(long)]
        report: Option<PathBuf>,
        body: String,
        #[arg(long, default_value = "completed")]
        status: String,
        #[arg(long = "artifact")]
        artifacts: Vec<String>,
        /// Print a compact acknowledgement without repeating request/reply bodies.
        #[arg(long)]
        json: bool,
        /// Return the legacy full request, including bodies and audit data.
        #[arg(long, requires = "json")]
        full: bool,
    },
    /// Wait for the structured reply to a sent request.
    Wait {
        request_id: String,
        /// Maximum wait in seconds (clamped to 600).
        #[arg(long, visible_alias = "timeout", default_value_t = 300)]
        timeout_secs: u64,
        /// Print only the final reply body, without shell-side JSON parsing.
        #[arg(long, conflicts_with = "full")]
        body: bool,
        /// Return the legacy full request JSON instead of a compact reply receipt.
        #[arg(long)]
        full: bool,
        /// Write the result directly to a UTF-8 file instead of stdout.
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Cancel a request that the recipient has not claimed yet.
    Cancel {
        request_id: String,
        /// Print a compact acknowledgement without repeating request/reply bodies.
        #[arg(long)]
        json: bool,
        /// Return the legacy full request, including bodies and audit data.
        #[arg(long, requires = "json")]
        full: bool,
    },
    /// Retire one reviewed interrupted attempt; no process stop or success report.
    Retire {
        request_id: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        evidence: String,
        /// Commit the retirement; without this flag only preview it.
        #[arg(long)]
        execute: bool,
        #[arg(long)]
        json: bool,
    },
    /// Stop being woken for replies to your own requests you no longer need,
    /// for example ones you gave up on or re-sent. Their work, replies and
    /// `msg wait` results are unchanged; use `cancel` to stop queued work.
    Withdraw {
        #[arg(required = true, value_name = "REQUEST")]
        request_ids: Vec<String>,
        /// Print the affected requests as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Operator cleanup: expire every request nobody claimed within the
    /// given age, whoever sent it, including senders that no longer exist.
    /// Human decision requests, including recovery questions, are protected.
    Expire {
        /// Age past which an unclaimed request expires (e.g. `24h`, `7d`).
        #[arg(long, value_name = "AGE")]
        older_than: String,
        /// Preview the count and sampled requests without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Print the total, up to 100 small request previews, and truncation status as JSON.
        #[arg(long)]
        json: bool,
        /// Only quiet one-way notices with no delivery or claim evidence.
        #[arg(long)]
        quiet_only: bool,
        /// Limit cleanup to one exact request, rechecking all other filters.
        #[arg(long)]
        request_id: Option<String>,
    },
}

/// The closed set of engines an instance can be built on. Adding a
/// provider is config; adding an engine is code, which is why this is a
/// `ValueEnum` and provider ids are free strings.
#[derive(Debug, Clone, Copy, ValueEnum)]
enum AskEngineArg {
    Claude,
    Codex,
    Gemini,
    Anthropic,
    #[value(name = "openai")]
    OpenAi,
    /// Apple's on-device and Private Cloud Compute models (macOS 26+).
    Apple,
}

impl AskEngineArg {
    fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Anthropic => "anthropic",
            Self::OpenAi => "openai",
            Self::Apple => "apple",
        }
    }
}

#[derive(Debug, Subcommand)]
enum AskCmd {
    /// List every provider the daemon can ask: engine, kind, model,
    /// credential status, and which one is selected.
    Providers {
        #[arg(long)]
        json: bool,
    },
    /// Add, remove, or configure an `[ask.providers.<id>]` instance.
    Provider {
        #[command(subcommand)]
        action: AskProviderCmd,
    },
}

#[derive(Debug, Subcommand)]
enum AskProviderCmd {
    /// Add a provider instance under an id of your choosing. Several
    /// instances may share an engine, so a work and a personal account of
    /// the same API can sit side by side, each with its own key.
    Add {
        /// New provider id. A TOML bare key: letters, digits, `-`, `_`.
        id: String,
        /// Engine that drives it.
        #[arg(long, value_enum)]
        engine: AskEngineArg,
        /// Display name. Defaults to a humanized id.
        #[arg(long)]
        title: Option<String>,
        /// Model name for this instance.
        #[arg(long)]
        model: Option<String>,
        /// Name of an environment variable holding the API key. The key
        /// itself is never written to config.
        #[arg(long, value_name = "VAR")]
        api_key_env: Option<String>,
        /// Binary a CLI engine spawns for this instance.
        #[arg(long, value_name = "PATH")]
        executable: Option<String>,
        /// Print the updated provider list as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Make a provider the default: questions go to it from now on, and
    /// `[ask] agent` is rewritten so a daemon restart keeps the choice.
    /// `tend ask --agent` switches only until the next restart.
    Use {
        /// Provider id from `tend ask providers`.
        id: String,
        /// Print the updated provider list as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Remove a provider instance. A built-in id keeps its row and only
    /// loses the settings you gave it.
    Remove {
        /// Provider id to remove.
        id: String,
        /// Print the updated provider list as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Set or clear a provider's title, model, key variable, and binary.
    /// Flags you omit leave that key unchanged. The engine is fixed at
    /// `add` time; to change it, remove the instance and add it again.
    Set {
        /// Provider id from `tend ask providers`.
        id: String,
        /// Display name for this provider.
        #[arg(long, conflicts_with = "clear_title")]
        title: Option<String>,
        /// Model name for this provider.
        #[arg(long, conflicts_with = "clear_model")]
        model: Option<String>,
        /// Name of an environment variable holding the API key. The key
        /// itself is never written to config.
        #[arg(long, value_name = "VAR", conflicts_with = "clear_api_key_env")]
        api_key_env: Option<String>,
        /// Binary a CLI engine spawns for this provider.
        #[arg(long, value_name = "PATH", conflicts_with = "clear_executable")]
        executable: Option<String>,
        /// Remove `title` so the default name applies.
        #[arg(long)]
        clear_title: bool,
        /// Remove `model` so the provider's default applies.
        #[arg(long)]
        clear_model: bool,
        /// Remove `api_key_env`.
        #[arg(long)]
        clear_api_key_env: bool,
        /// Remove `executable` so the engine's own binary applies.
        #[arg(long)]
        clear_executable: bool,
        /// Print the updated provider list as JSON.
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Subcommand)]
enum AgentCmd {
    /// Find current agent locations by task keywords, with bounded evidence.
    Find(agent_find::FindArgs),
    /// Search historical Claude/Codex records with a natural-language question.
    Search(agent_records::SearchArgs),
    /// Read surrounding evidence and paired tool results from a search hit.
    Read(agent_records::ReadArgs),
    /// Start an allowlisted agent in tmux or a tend-owned PTY session.
    Start(agent_launch::StartArgs),
    /// Mechanically prepare an exact worker launch and verify its input prompt.
    Prepare(worker_startup::Args),
    /// Interrupt or terminate one tend-managed tmux pane or native PTY session.
    Control(tmux_work::AgentControlArgs),
    /// Move a running agent without restarting its conversation.
    Move(agent_move::MoveArgs),
}

#[derive(Debug, Subcommand)]
enum WindowCmd {
    /// Move a whole live window, including all agents and its layout.
    Move(agent_move::MoveArgs),
    /// Set a stable display name, or restore tmux's automatic process name.
    Rename(tmux_work::WindowRenameArgs),
}

#[derive(Debug, Subcommand)]
enum WorkCmd {
    #[command(hide = true)]
    FleetAsk,
    /// Place a Work using shared Fleet policy; accepts a bounded JSON request.
    Dispatch(orchestration::DispatchArgs),
    /// Read a durable dispatch, including after a lost reply.
    DispatchStatus(orchestration::StatusArgs),
    /// Read registered workspaces from the shared coordinator.
    DispatchOptions {
        #[arg(long, hide = true)]
        at_coordinator: bool,
    },
    #[command(hide = true)]
    DispatchExecute(orchestration::ExecuteArgs),
    #[command(hide = true)]
    DispatchWorkerStatus { dispatch_id: String },
    /// Describe a work pipeline in your own words and let an agent write
    /// the `[ticket]`/`[[route]]`/`[pipeline.*]` config for you. Validated
    /// and shown before anything is written.
    Init(work_init::InitArgs),
    /// Draft one pipeline from a description as the JSON `work pipeline
    /// set` reads, with a read-only agent turn. Prints it; writes nothing.
    Compose(work_compose::ComposeArgs),
    /// Converge a Work's current Run to its pipeline: optionally link an
    /// external issue, route the Work, and create missing agent sessions.
    /// Re-running converges instead of duplicating.
    Up(work_up::UpArgs),
    /// Add or edit a planned Work in the backlog without launching it.
    /// `tend work up WORK` later starts it from the plan.
    Plan(work_backlog::PlanArgs),
    /// Create a work window with its first agent, or add an agent when it exists.
    Start(agent_launch::WorkStartArgs),
    /// List tend-managed work windows.
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
    /// Print the routes, pipelines, message skills, and built-in presets a
    /// Work launcher can offer, so a GUI never has to parse config.toml.
    Options(work_options::OptionsArgs),
    /// Built-in pipeline presets: list them, or write one into config.toml
    /// as `[pipeline.<name>]` without spending an agent turn.
    Preset(work_options::PresetArgs),
    /// Write, replace, or remove a `[pipeline.<name>]` from the JSON shape
    /// `work options` prints, so an editor never parses config.toml.
    Pipeline(work_pipeline::PipelineArgs),
    /// Add, update, or remove one `[[route]]` by its `match` regex.
    Route(work_pipeline::RouteArgs),
}

#[derive(Debug, Subcommand)]
enum WorkspaceCmd {
    /// List tend-managed workspace sessions.
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
    /// Set this exact session to primary (protected) or worker.
    ///
    /// With `--session`, set exact live agent sessions instead, for example
    /// tend-launched workers that predate purposes. Audited with the caller.
    Purpose {
        purpose: tend::collaboration::LaunchPurpose,
        /// Agent session id to set (operator only). Repeatable.
        #[arg(long = "session", value_name = "SESSION_ID")]
        sessions: Vec<String>,
    },
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
    /// through unchanged — useful for layering tend on top of tools like
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
    /// Retired Gemini CLI hook handler, kept as a silent no-op.
    ///
    /// An install that predates the removal still has `tend hook gemini`
    /// wired in `~/.gemini/settings.json` until `tend init` cleans it up.
    /// Exiting 0 without output keeps those leftover hooks from erroring on
    /// every Gemini CLI turn.
    #[command(hide = true)]
    Gemini {
        #[arg(long)]
        event: Option<String>,
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

impl From<WatchViewArg> for tend::config::WatchView {
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
    Work,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum WatchScreenArg {
    Topology,
    Collab,
    Board,
}

impl From<WatchScreenArg> for tend::config::WatchScreen {
    fn from(value: WatchScreenArg) -> Self {
        match value {
            WatchScreenArg::Topology => Self::Topology,
            WatchScreenArg::Collab => Self::Collab,
            WatchScreenArg::Board => Self::Board,
        }
    }
}

impl From<WatchLayoutArg> for tend::config::WatchLayout {
    fn from(value: WatchLayoutArg) -> Self {
        match value {
            WatchLayoutArg::Tree => Self::Tree,
            WatchLayoutArg::Swarm => Self::Swarm,
            WatchLayoutArg::Work => Self::Work,
        }
    }
}

fn parse_board_sort(value: &str) -> std::result::Result<tend::config::WatchBoardSort, String> {
    board_screen::parse_sort(value)
        .ok_or_else(|| format!("unknown board sort `{value}` (attention, activity, busy, name)"))
}

fn parse_watch_collab_layout(value: &str) -> std::result::Result<watch::CollabLayout, String> {
    match value {
        "table" | "sequence" => watch::parse_collab_layout(value)
            .ok_or_else(|| format!("unsupported collaboration layout {value:?}")),
        _ => Err("collaboration layout must be table or sequence".into()),
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

fn collaboration_client_kind(command: &Cmd) -> CollaborationClientKind {
    match command {
        Cmd::Start | Cmd::Watch { .. } => CollaborationClientKind::Watch,
        Cmd::Mcp => CollaborationClientKind::Mcp,
        Cmd::Dashboard(_) => CollaborationClientKind::Dashboard,
        _ => CollaborationClientKind::Cli,
    }
}

/// Parse argv with the grouped top-level help (see `help_groups`).
fn parse_args() -> Args {
    use clap::{CommandFactory, FromArgMatches};
    let command = Args::command();
    let template = format!(
        "{{before-help}}{{about-with-newline}}\n{{usage-heading}} {{usage}}\n\n{}\
         Options:\n{{options}}{{after-help}}",
        help_groups::grouped_commands(&command)
    );
    let mut command = command.help_template(template);
    let matches = command.clone().get_matches();
    stats::check_flag_placement(&mut command, &matches).unwrap_or_else(|error| error.exit());
    Args::from_arg_matches(&matches).unwrap_or_else(|error| error.exit())
}
#[allow(clippy::large_futures)] // delegates the existing command future without changing its allocation
#[tokio::main]
async fn main() -> Result<()> {
    command_dispatch::main().await
}

#[cfg(test)]
#[path = "cli_tests.rs"]
mod tests;
mod work_handlers;
use work_handlers::{run_agent_cmd, run_window_cmd, run_work_cmd, run_workspace_cmd};

mod ask_handlers;
use ask_handlers::{cmd_ask, cmd_ask_admin, require_capability, AskOptions};

mod command_dispatch;

mod registry_handlers;
use registry_handlers::{cmd_forget_agent, cmd_prune, parse_duration_secs};

mod mailbox_handlers;
use mailbox_handlers::{cmd_identity, cmd_msg, cmd_peers};

mod session_handlers;
use session_handlers::{
    caller_env, cmd_attach, cmd_detach, cmd_register, cmd_run, cmd_zellij_plugin_snapshot,
};

mod watch_handlers;
use watch_handlers::{
    cmd_attend, cmd_dashboard, cmd_fleet_watch, cmd_sync, cmd_watch, WatchInvocation,
};

mod navigation;
use navigation::{
    all_panes, backend_for_kind, jump_to_pane, jump_to_pane_cmux, jump_to_pane_logged,
    jump_to_topology_pane, jump_to_topology_pane_logged, CALLER_CLIENT,
};

mod hook_handlers;
use hook_handlers::handle_hook;

mod status_handlers;
use status_handlers::{
    cmd_panes, cmd_recap, cmd_status, cmd_status_line, icon_set, pane_display, relative_time,
    set_icon_set, state_icon, state_style, terminal_width, truncate_cell, use_colors,
};
