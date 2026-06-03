//! muxa CLI — user-facing entry point.

mod activity_query;
mod attend;
mod doctor;
mod init;
mod logs;
mod stats;
mod theme;
mod time_range;
mod upgrade;
mod watch;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand, ValueEnum};
use comfy_table::presets::UTF8_BORDERS_ONLY;
use comfy_table::{Cell, ColumnConstraint, ContentArrangement, Table, Width};
use muxa::adapters::{claude, run_hook, ClaudeAdapter, CodexAdapter, GeminiAdapter};
use muxa::config::{WatchConfig, WatchSortKey, WatchTheme};
use muxa::ipc::Client;
use muxa::state::Agent;
use muxa::{
    discovery, paths, tmux, ActivityEntry, AgentState, Config, HumanInteractionEntry,
    HumanInteractionInput, HumanInteractionKind,
};
use owo_colors::Style;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use theme::{CliTheme, TableTone, ThemeArg};
use time::OffsetDateTime;
use unicode_width::UnicodeWidthChar;

const DEFAULT_TERMINAL_WIDTH: usize = 120;
const FULL_STATUS_TABLE_WIDTH: usize = 120;
const COMPACT_STATUS_TABLE_WIDTH: usize = 76;
const MIN_STATUS_PROMPT_WIDTH: usize = 8;
const MAX_STATUS_PROMPT_WIDTH: usize = 60;

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
        #[arg(long, value_enum)]
        theme: Option<ThemeArg>,
    },
    /// Print a one-liner status suitable for tmux `status-right`.
    StatusLine {
        #[arg(long)]
        pane: Option<String>,
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
    /// Summarize retained prompt history, live agents, and session duration.
    Stats(stats::Args),
    /// Generate a Markdown activity report from the retained stats.
    Report(stats::ReportArgs),
    /// Query raw activity ledger intervals.
    Activity(activity_query::Args),
    /// Hook adapter entrypoints invoked by the agent CLIs themselves.
    Hook {
        #[command(subcommand)]
        which: HookCmd,
    },
    /// Debug: print tmux pane inventory.
    Panes,
    /// Fullscreen TUI dashboard of all tracked agents.
    Watch {
        /// Show agents that have no tmux pane attached. Default behavior
        /// (governed by `[watch] hide_paneless = true`) hides them
        /// because Enter on the picker can't attach to them anyway —
        /// the footer surfaces a count instead. This flag flips the
        /// filter off for one invocation, e.g. when debugging a
        /// detached SDK session.
        #[arg(long)]
        include_paneless: bool,
        /// Row granularity: tmux session (default) or pane.
        #[arg(long, value_enum)]
        view: Option<WatchViewArg>,
        /// One-shot sort override: session, latest/activity/act, dur/duration, st/state, pane, pane-id.
        #[arg(long, value_enum)]
        sort: Option<WatchSortArg>,
        /// One-shot visual theme override.
        #[arg(long, value_enum)]
        theme: Option<ThemeArg>,
    },
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
    /// Tail muxad's stdout/stderr logs without remembering paths.
    /// Falls back to `journalctl --user -u muxad` on Linux when the
    /// systemd unit is the source of truth.
    Logs(logs::Args),
    /// Update muxa from the source repo: `git pull` → cargo install
    /// `muxad` + `muxa-cli` → restart the daemon → verify the IPC
    /// socket is responsive. One command for the full update flow.
    Upgrade(upgrade::Args),
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
    /// opencode hook handler — not yet implemented.
    ///
    /// Kept visible in `--help` so users who try it get a friendly,
    /// targeted error rather than a generic "unrecognized subcommand".
    /// Dispatched in `handle_hook` to return a non-zero exit with a
    /// message pointing at the tracking issue. See
    /// `crates/muxa/src/adapters/opencode.rs` for the deferred design.
    Opencode {
        #[arg(long)]
        event: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum WatchViewArg {
    Pane,
    Session,
}

impl From<WatchViewArg> for muxa::config::WatchView {
    fn from(value: WatchViewArg) -> Self {
        match value {
            WatchViewArg::Pane => Self::Pane,
            WatchViewArg::Session => Self::Session,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum WatchSortArg {
    Session,
    #[value(alias = "activity", alias = "act")]
    Latest,
    #[value(alias = "dur", alias = "duration")]
    SessionTime,
    #[value(alias = "st")]
    State,
    Pane,
    #[value(alias = "pane_id")]
    PaneId,
}

impl WatchSortArg {
    fn keys(self) -> Vec<WatchSortKey> {
        match self {
            Self::Session => vec![WatchSortKey::Session, WatchSortKey::Activity],
            Self::Latest => vec![WatchSortKey::Activity],
            Self::SessionTime => vec![WatchSortKey::SessionTime],
            Self::State => vec![WatchSortKey::State, WatchSortKey::Activity],
            Self::Pane => vec![WatchSortKey::Session, WatchSortKey::Pane],
            Self::PaneId => vec![WatchSortKey::PaneId],
        }
    }
}

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
    let config_path = args.config.clone().or_else(paths::default_config_file);
    let cfg = Config::load_or_default(config_path.as_deref()).context("loading config")?;
    let socket = args
        .socket
        .or_else(|| cfg.socket.clone())
        .unwrap_or_else(paths::default_socket);
    let client = Client::new(socket.clone());

    match args.cmd {
        Cmd::Status { theme } => cmd_status(&client, &cfg, theme).await,
        Cmd::StatusLine { pane } => cmd_status_line(&client, pane).await,
        Cmd::Recap { pane, limit, all } => cmd_recap(&client, pane, limit, all).await,
        Cmd::Stats(stats_args) => stats::run(&client, &cfg, stats_args).await,
        Cmd::Report(report_args) => stats::run_report(&client, &cfg, report_args).await,
        Cmd::Activity(activity_args) => activity_query::run(&cfg, activity_args).await,
        Cmd::Hook { which } => handle_hook(&client, which).await,
        Cmd::Panes => {
            cmd_panes();
            Ok(())
        }
        Cmd::Watch {
            include_paneless,
            view,
            sort,
            theme,
        } => cmd_watch(&client, cfg, include_paneless, view, sort, theme).await,
        Cmd::Attend(attend_args) => cmd_attend(&client, attend_args).await,
        Cmd::Sync => cmd_sync(&client).await,
        Cmd::Init(init_args) => init::run(init_args, socket).await,
        Cmd::Doctor => doctor::run(socket).await,
        Cmd::Logs(logs_args) => logs::run(logs_args).await,
        Cmd::Upgrade(upgrade_args) => upgrade::run(upgrade_args, socket).await,
    }
}

/// Jump to (or list) the agent that needs you. `attend::run` picks the
/// pane — reusing the same selection that drives `--list` — and we perform
/// the actual focus through `jump_to_pane`, the identical machinery the
/// `muxa watch` Enter action uses, so a jump lands the same way from both.
async fn cmd_attend(client: &Client, args: attend::Args) -> Result<()> {
    let backend = muxa::default_backend();
    if let Some(pane) = attend::run(client, backend.as_ref(), args).await? {
        jump_to_pane(&pane);
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

async fn cmd_watch(
    client: &Client,
    cfg: Config,
    include_paneless: bool,
    view: Option<WatchViewArg>,
    sort: Option<WatchSortArg>,
    theme: Option<ThemeArg>,
) -> Result<()> {
    // watch::run restores the terminal before returning, so by the time we
    // get here it's safe to exec tmux commands that mutate the client's
    // attached session / pane.
    //
    // CLI flag wins over config — one-shot override for the current
    // invocation without touching the user's ~/.config/muxa/config.toml.
    let mut watch_cfg = WatchConfig {
        hide_paneless: cfg.watch.hide_paneless && !include_paneless,
        ..cfg.watch
    };
    if let Some(view) = view {
        watch_cfg.view = view.into();
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
    if let Some(pane_id) = watch::run(
        client,
        watch_cfg,
        session_activity_path,
        activity_path.clone(),
    )
    .await?
    {
        jump_to_pane_logged(&pane_id, activity_path.as_deref()).await;
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
    let backend = muxa::default_backend();
    match backend.kind() {
        muxa::HostKind::Tmux => jump_to_pane_tmux(pane_id),
        muxa::HostKind::Zellij => jump_to_pane_zellij(backend.as_ref(), pane_id),
    }
}

fn jump_to_pane_tmux(pane_id: &str) {
    let Some(info) = tmux::resolve_pane(pane_id) else {
        eprintln!("muxa: pane {pane_id} not found in tmux — it may have closed");
        return;
    };
    let target_window = format!("{}:{}", info.session, info.window_index);

    // Pre-position so whichever path we take below lands on the right pane.
    run_tmux(&["select-window", "-t", &target_window]);
    run_tmux(&["select-pane", "-t", pane_id]);

    if tmux::inside_tmux() {
        // Already attached — just switch this client's session.
        run_tmux(&["switch-client", "-t", &info.session]);
    } else {
        // Bare shell — hand our terminal to a fresh tmux attach-session.
        // `.status()` waits for tmux to exit; on detach the user is back at
        // this shell prompt, which is the least-surprising behaviour.
        match muxa::tmux::tmux_command()
            .args(["attach-session", "-t", &info.session])
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
        }
        HookCmd::Gemini { event } => {
            let ev = run_hook::<GeminiAdapter, _>(&event, &mut std::io::stdin())?;
            best_effort_ingest(client, &ev).await;
        }
        HookCmd::Opencode { event: _ } => {
            // The `opencode` adapter is deferred — see
            // `crates/muxa/src/adapters/opencode.rs` and the README's
            // "Agent support" table. Print a friendly, actionable
            // error and exit non-zero so users hitting this aren't
            // left wondering why nothing happened.
            eprintln!(
                "error: 'opencode' adapter is not yet implemented. \
                 See https://github.com/Open330/muxa/issues for status."
            );
            std::process::exit(2);
        }
    }
    Ok(())
}

async fn cmd_status(client: &Client, cfg: &Config, theme: Option<ThemeArg>) -> Result<()> {
    let agents = client.snapshot().await?;
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

async fn cmd_status_line(client: &Client, pane: Option<String>) -> Result<()> {
    let backend = muxa::default_backend();
    let panes_snapshot = backend.list_panes();
    let pane = pane.or_else(|| backend.current_pane());
    let agents = match &pane {
        Some(p) => client.by_pane(p).await?,
        None => client.snapshot().await?,
    };
    // tmux handles its own color markup, so we never emit ANSI here.
    let parts: Vec<String> = agents
        .iter()
        .map(|a| {
            let icon = state_icon(a.state);
            let kind = a.kind.to_string();
            // Prefer session:window when we can resolve it — makes the
            // status-line read "⚙ main:2 claude_code" instead of a
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

fn cmd_panes() {
    let backend = muxa::default_backend();
    let panes = backend.list_panes();
    if panes.is_empty() {
        // Two ways to get here: the host (tmux/zellij) has no panes,
        // or the backend's `caps()` says metadata is plugin-only and
        // not pushed yet. The hint differentiates so users diagnosing
        // a misconfigured zellij plugin see something useful.
        match backend.kind() {
            muxa::HostKind::Tmux => println!("(no tmux panes — server may be down)"),
            muxa::HostKind::Zellij if !backend.caps().current_command => println!(
                "(zellij CLI baseline: pane inventory is plugin-only — install the muxa zellij plugin to populate)"
            ),
            muxa::HostKind::Zellij => println!("(no zellij panes)"),
        }
        return;
    }
    let terminal_width = terminal_width();
    let mut out = std::io::stdout().lock();
    for p in panes {
        let line = format!(
            "{:<8} {}:{}.{}  tty={}  cmd={}  title={}",
            p.pane_id, p.session, p.window_index, p.pane_index, p.tty, p.current_command, p.title
        );
        if writeln!(out, "{}", truncate_cell(&line, terminal_width)).is_err() {
            return;
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

pub(crate) fn state_icon(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "⚙",
        AgentState::Idle => "·",
        AgentState::WaitingInput => "!",
        AgentState::WaitingChoice => "?",
        AgentState::Error => "✗",
        AgentState::Stopped => "∅",
        AgentState::Starting => "…",
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
        return "-".to_string();
    };
    match panes.iter().find(|p| p.pane_id == raw) {
        Some(p) => format!("{}:{}.{}", p.session, p.window_index, p.pane_index),
        None => raw.to_string(),
    }
}

/// Human-friendly delta between `then` and `now`, rounded to the largest
/// unit. Past timestamps only — future clocks collapse to "0s ago".
fn relative_time(now: OffsetDateTime, then: OffsetDateTime) -> String {
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

    fn agent(session_id: &str, pane: Option<&str>, state: AgentState, prompt: &str) -> Agent {
        Agent {
            kind: AgentKind::ClaudeCode,
            session_id: session_id.into(),
            pane: pane.map(str::to_string),
            cwd: None,
            state,
            last_prompt: Some(prompt.into()),
            last_response: None,
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
            pane_id: id.into(),
            session: session.into(),
            window_index: "12".into(),
            pane_index: "3".into(),
            tty: "/dev/pts/0".into(),
            current_command: "claude".into(),
            title: String::new(),
            pane_pid: 0,
        }
    }

    #[test]
    fn watch_sort_cli_aliases_parse_to_expected_keys() {
        for (raw, expected) in [
            (
                "session",
                vec![WatchSortKey::Session, WatchSortKey::Activity],
            ),
            ("act", vec![WatchSortKey::Activity]),
            ("dur", vec![WatchSortKey::SessionTime]),
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
        for command in ["status", "stats", "activity"] {
            let args = Args::try_parse_from(["muxa", command, "--theme", "high-contrast"])
                .unwrap_or_else(|err| panic!("{command} should accept --theme: {err}"));
            let theme = match args.cmd {
                Cmd::Status { theme: Some(theme) } => theme,
                Cmd::Stats(args) => args.theme().expect("expected stats theme arg"),
                Cmd::Activity(args) => args.theme().expect("expected activity theme arg"),
                _ => panic!("expected {command} theme arg"),
            };
            assert_eq!(WatchTheme::from(theme), WatchTheme::HighContrast);
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
