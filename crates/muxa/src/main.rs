//! muxa CLI — user-facing entry point.

mod watch;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use comfy_table::presets::UTF8_BORDERS_ONLY;
use comfy_table::{Cell, ContentArrangement, Table};
use muxa_adapters::{claude, run_hook, ClaudeAdapter, CodexAdapter, GeminiAdapter};
use muxa_core::paths;
use muxa_core::state::Agent;
use muxa_core::AgentState;
use muxa_runtime::{ipc::Client, tmux};
use owo_colors::{OwoColorize, Style};
use std::io::IsTerminal;
use std::path::PathBuf;
use time::OffsetDateTime;

#[derive(Debug, Parser)]
#[command(name = "muxa", version, about = "muxa CLI")]
struct Args {
    #[arg(long, env = "MUXA_SOCKET", global = true)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// List active agents as a human-readable table.
    Status,
    /// Print a one-liner status suitable for tmux `status-right`.
    StatusLine {
        #[arg(long)]
        pane: Option<String>,
    },
    /// Show the last prompt for the given pane (default: `$TMUX_PANE`).
    Recap {
        #[arg(long)]
        pane: Option<String>,
    },
    /// Hook adapter entrypoints invoked by the agent CLIs themselves.
    Hook {
        #[command(subcommand)]
        which: HookCmd,
    },
    /// Debug: print tmux pane inventory.
    Panes,
    /// Fullscreen TUI dashboard of all tracked agents.
    Watch,
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
    ClaudeStatusline,
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
    let socket = args.socket.unwrap_or_else(paths::default_socket);
    let client = Client::new(socket);

    match args.cmd {
        Cmd::Status => cmd_status(&client).await,
        Cmd::StatusLine { pane } => cmd_status_line(&client, pane).await,
        Cmd::Recap { pane } => cmd_recap(&client, pane).await,
        Cmd::Hook { which } => handle_hook(&client, which).await,
        Cmd::Panes => cmd_panes(),
        Cmd::Watch => watch::run(&client).await,
    }
}

/// Hook commands are invoked on the agent's critical path (every prompt,
/// every tool call). If the daemon is down we MUST NOT block or fail — a
/// best-effort ingest with a stderr warning keeps the agent healthy.
async fn best_effort_ingest(client: &Client, ev: &muxa_core::event::AgentEvent) {
    if let Err(e) = client.ingest(ev).await {
        tracing::debug!(error = %e, "muxa ingest failed (daemon down?)");
    }
}

async fn handle_hook(client: &Client, cmd: HookCmd) -> Result<()> {
    match cmd {
        HookCmd::Claude { event } => {
            let ev = run_hook::<ClaudeAdapter, _>(&event, &mut std::io::stdin())?;
            best_effort_ingest(client, &ev).await;
        }
        HookCmd::ClaudeStatusline => {
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
    }
    Ok(())
}

async fn cmd_status(client: &Client) -> Result<()> {
    let agents = client.snapshot().await?;
    if agents.is_empty() {
        println!("no active agents");
        return Ok(());
    }
    print_table(&agents, OffsetDateTime::now_utc(), use_colors());
    Ok(())
}

async fn cmd_status_line(client: &Client, pane: Option<String>) -> Result<()> {
    let pane = pane.or_else(tmux::current_pane);
    let agents = match &pane {
        Some(p) => client.by_pane(p).await?,
        None => client.snapshot().await?,
    };
    // tmux handles its own color markup, so we never emit ANSI here.
    let parts: Vec<String> = agents
        .iter()
        .map(|a| {
            let icon = state_icon(a.state);
            let kind = kind_label(a);
            // Prefer session:window when we can resolve it — makes the
            // status-line read "⚙ main:2 claude_code" instead of a
            // context-free glyph.
            let loc = a
                .pane
                .as_deref()
                .and_then(tmux::resolve_pane)
                .map(|p| format!(" {}:{}", p.session, p.window_index));
            match loc {
                Some(l) => format!("{icon}{l} {kind}"),
                None => format!("{icon} {kind}"),
            }
        })
        .collect();
    println!("{}", parts.join(" | "));
    Ok(())
}

async fn cmd_recap(client: &Client, pane: Option<String>) -> Result<()> {
    let pane = pane
        .or_else(tmux::current_pane)
        .context("no pane given and $TMUX_PANE is unset")?;
    let agents = client.by_pane(&pane).await?;
    if agents.is_empty() {
        println!("no agent in pane {pane}");
        return Ok(());
    }
    for a in agents {
        let kind = serde_json::to_string(&a.kind)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
        let state = serde_json::to_string(&a.state)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
        let prompt = a.last_prompt.unwrap_or_else(|| "(none)".into());
        println!("── {kind}  [{state}] ────────────");
        println!("{prompt}");
        println!();
    }
    Ok(())
}

fn cmd_panes() -> Result<()> {
    if !tmux::inside_tmux() {
        println!("not inside tmux");
        return Ok(());
    }
    for p in tmux::list_panes()? {
        println!(
            "{:<8} {}:{}.{}  tty={}  cmd={}  title={}",
            p.pane_id, p.session, p.window_index, p.pane_index, p.tty, p.current_command, p.title
        );
    }
    Ok(())
}

/// Decide whether to emit ANSI color. We check `NO_COLOR` (per the de-facto
/// standard) and require stdout to be a TTY — piping `muxa status | grep`
/// should stay clean.
fn use_colors() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    std::io::stdout().is_terminal()
}

fn state_icon(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "⚙",
        AgentState::Idle => "·",
        AgentState::WaitingInput => "!",
        AgentState::Error => "✗",
        AgentState::Stopped => "∅",
        AgentState::Starting => "…",
    }
}

fn kind_label(a: &Agent) -> String {
    serde_json::to_string(&a.kind)
        .unwrap_or_default()
        .trim_matches('"')
        .to_string()
}

fn state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Working => "working",
        AgentState::Idle => "idle",
        AgentState::WaitingInput => "waiting_input",
        AgentState::Error => "error",
        AgentState::Stopped => "stopped",
        AgentState::Starting => "starting",
    }
}

fn state_style(state: AgentState) -> Style {
    match state {
        AgentState::Working => Style::new().green(),
        AgentState::WaitingInput => Style::new().yellow(),
        AgentState::Idle => Style::new().dimmed(),
        AgentState::Error => Style::new().red(),
        AgentState::Stopped => Style::new().dimmed().strikethrough(),
        AgentState::Starting => Style::new().cyan(),
    }
}

/// Pretty-print `Agent.pane` as `session:window.pane` when tmux agrees, or
/// fall back to the raw pane id (or `-` if unknown).
fn pane_display(a: &Agent) -> String {
    let Some(raw) = a.pane.as_deref() else {
        return "-".to_string();
    };
    match tmux::resolve_pane(raw) {
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

fn print_table(agents: &[Agent], now: OffsetDateTime, colored: bool) {
    let mut table = Table::new();
    table
        .load_preset(UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Dynamic)
        .set_header(vec![
            "PANE",
            "KIND",
            "STATE",
            "MODEL",
            "LAST ACTIVITY",
            "LAST PROMPT",
        ]);

    for a in agents {
        let pane = pane_display(a);
        let kind = kind_label(a);
        let state_txt = state_label(a.state);
        let state_cell = if colored {
            Cell::new(state_txt.style(state_style(a.state)).to_string())
        } else {
            Cell::new(state_txt)
        };
        let model = a.model.as_deref().unwrap_or("-").to_string();
        let last_activity = relative_time(now, a.last_activity_at);
        let prompt_raw = a.last_prompt.as_deref().unwrap_or("-");
        let prompt: String = prompt_raw
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect();

        table.add_row(vec![
            Cell::new(pane),
            Cell::new(kind),
            state_cell,
            Cell::new(model),
            Cell::new(last_activity),
            Cell::new(prompt),
        ]);
    }

    println!("{table}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

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
}
