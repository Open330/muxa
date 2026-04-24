//! muxa CLI — user-facing entry point.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use muxa_adapters::{claude, run_hook, ClaudeAdapter, CodexAdapter, GeminiAdapter};
use muxa_core::paths;
use muxa_core::state::Agent;
use muxa_runtime::{ipc::Client, tmux};
use std::path::PathBuf;

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
    print_table(&agents);
    Ok(())
}

async fn cmd_status_line(client: &Client, pane: Option<String>) -> Result<()> {
    let pane = pane.or_else(tmux::current_pane);
    let agents = match &pane {
        Some(p) => client.by_pane(p).await?,
        None => client.snapshot().await?,
    };
    let parts: Vec<String> = agents
        .iter()
        .map(|a| {
            let icon = match a.state {
                muxa_core::AgentState::Working => "⚙",
                muxa_core::AgentState::Idle => "·",
                muxa_core::AgentState::WaitingInput => "!",
                muxa_core::AgentState::Error => "✗",
                muxa_core::AgentState::Stopped => "∅",
                muxa_core::AgentState::Starting => "…",
            };
            let kind = serde_json::to_string(&a.kind)
                .unwrap_or_default()
                .trim_matches('"')
                .to_string();
            format!("{icon} {kind}")
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

fn print_table(agents: &[Agent]) {
    println!(
        "{:<14} {:<12} {:<14} {:<16} LAST PROMPT",
        "PANE", "KIND", "STATE", "MODEL"
    );
    for a in agents {
        let pane = a.pane.as_deref().unwrap_or("-");
        let kind = serde_json::to_string(&a.kind)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
        let state = serde_json::to_string(&a.state)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
        let model = a.model.as_deref().unwrap_or("-");
        let prompt_raw = a.last_prompt.as_deref().unwrap_or("-");
        let prompt: String = prompt_raw
            .lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(60)
            .collect();
        println!("{pane:<14} {kind:<12} {state:<14} {model:<16} {prompt}");
    }
}
