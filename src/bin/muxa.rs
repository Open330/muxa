//! muxa CLI — user-facing entry point.
//!
//!   muxa status                 # list active agents
//!   muxa status-line            # tmux-status-line-ready one-liner
//!   muxa recap [--pane %12]     # show last prompt for pane (default: current pane)
//!   muxa hook claude --event <e>  # hook adapter invoked by Claude Code
//!   muxa panes                  # debug: print tmux panes
//!
//! Not yet implemented: popup, notify, config reload.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use muxa::adapter::{claude, codex, gemini, opencode};
use muxa::{default_socket_path, ipc, tmux};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "muxa", about = "muxa CLI")]
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
    /// Print a one-line status suitable for tmux status-right.
    StatusLine {
        #[arg(long)]
        pane: Option<String>,
    },
    /// Show last prompt submitted in the given pane (default: $TMUX_PANE).
    Recap {
        #[arg(long)]
        pane: Option<String>,
    },
    /// Hook adapter entry point.
    Hook {
        #[command(subcommand)]
        which: HookCmd,
    },
    /// Debug: print tmux pane inventory.
    Panes,
}

#[derive(Debug, Subcommand)]
enum HookCmd {
    Claude {
        #[arg(long)]
        event: String,
    },
    /// Claude Code status-line feeder. Reads Claude's statusline JSON from
    /// stdin, emits a Heartbeat to muxad, then prints a one-line status back
    /// to stdout (so it can still be used as a status line).
    ClaudeStatusline,
    Codex {
        #[arg(long)]
        event: String,
    },
    Gemini {
        #[arg(long)]
        event: String,
    },
    /// opencode ingest. stdin must be a pre-normalized AgentEvent — emitted
    /// by the TS plugin at examples/opencode-muxa-plugin.ts.
    Opencode,
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
    let socket = args.socket.unwrap_or_else(default_socket_path);

    match args.cmd {
        Cmd::Status => cmd_status(&socket).await,
        Cmd::StatusLine { pane } => cmd_status_line(&socket, pane).await,
        Cmd::Recap { pane } => cmd_recap(&socket, pane).await,
        Cmd::Hook { which } => match which {
            HookCmd::Claude { event } => cmd_hook_claude(&socket, &event).await,
            HookCmd::ClaudeStatusline => cmd_hook_claude_statusline(&socket).await,
            HookCmd::Codex { event } => cmd_hook_codex(&socket, &event).await,
            HookCmd::Gemini { event } => cmd_hook_gemini(&socket, &event).await,
            HookCmd::Opencode => cmd_hook_opencode(&socket).await,
        },
        Cmd::Panes => cmd_panes(),
    }
}

async fn cmd_status(socket: &std::path::Path) -> Result<()> {
    let resp = ipc::query(socket, &serde_json::json!({ "kind": "snapshot" })).await?;
    let agents = resp["agents"].as_array().cloned().unwrap_or_default();
    if agents.is_empty() {
        println!("no active agents");
        return Ok(());
    }
    println!(
        "{:<14} {:<10} {:<10} {:<16} {}",
        "PANE", "KIND", "STATE", "MODEL", "LAST PROMPT"
    );
    for a in agents {
        let pane = a["pane"].as_str().unwrap_or("-");
        let kind = a["kind"].as_str().unwrap_or("-");
        let state = a["state"].as_str().unwrap_or("-");
        let model = a["model"].as_str().unwrap_or("-");
        let prompt = a["last_prompt"].as_str().unwrap_or("-");
        let prompt = prompt.lines().next().unwrap_or("").chars().take(60).collect::<String>();
        println!("{pane:<14} {kind:<10} {state:<10} {model:<16} {prompt}");
    }
    Ok(())
}

async fn cmd_status_line(socket: &std::path::Path, pane: Option<String>) -> Result<()> {
    let pane = pane.or_else(tmux::current_pane);
    let req = match &pane {
        Some(p) => serde_json::json!({ "kind": "by_pane", "pane": p }),
        None => serde_json::json!({ "kind": "snapshot" }),
    };
    let resp = ipc::query(socket, &req).await?;
    let agents = resp["agents"].as_array().cloned().unwrap_or_default();

    // Compact one-liner. Format TBD — for now: "<state-icon> <kind>".
    let parts: Vec<String> = agents
        .iter()
        .map(|a| {
            let state = a["state"].as_str().unwrap_or("?");
            let kind = a["kind"].as_str().unwrap_or("?");
            let icon = match state {
                "working" => "⚙",
                "idle" => "·",
                "waiting_input" => "!",
                "error" => "✗",
                "stopped" => "∅",
                _ => "?",
            };
            format!("{icon} {kind}")
        })
        .collect();
    println!("{}", parts.join(" | "));
    Ok(())
}

async fn cmd_recap(socket: &std::path::Path, pane: Option<String>) -> Result<()> {
    let pane = pane
        .or_else(tmux::current_pane)
        .context("no pane given and $TMUX_PANE is unset")?;
    let resp = ipc::query(socket, &serde_json::json!({ "kind": "by_pane", "pane": pane })).await?;
    let agents = resp["agents"].as_array().cloned().unwrap_or_default();
    if agents.is_empty() {
        println!("no agent in pane {pane}");
        return Ok(());
    }
    for a in agents {
        let kind = a["kind"].as_str().unwrap_or("?");
        let state = a["state"].as_str().unwrap_or("?");
        let prompt = a["last_prompt"].as_str().unwrap_or("(none)");
        println!("── {kind}  [{state}] ────────────");
        println!("{prompt}");
        println!();
    }
    Ok(())
}

async fn cmd_hook_claude(socket: &std::path::Path, event: &str) -> Result<()> {
    let event = claude::HookEvent::from_flag(event)?;
    let input = claude::parse_stdin()?;
    let agent_event = claude::to_event(event, input);
    ipc::send_ingest(socket, &agent_event).await?;
    Ok(())
}

async fn cmd_hook_claude_statusline(socket: &std::path::Path) -> Result<()> {
    let input = claude::parse_statusline_stdin()?;
    // Print a short status line for Claude Code itself (so the user still
    // gets a useful in-session display).
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

    let hb = claude::statusline_to_heartbeat(input);
    let _ = ipc::send_ingest(socket, &hb).await;
    Ok(())
}

async fn cmd_hook_codex(socket: &std::path::Path, event: &str) -> Result<()> {
    let event = codex::HookEvent::from_flag(event)?;
    let input = codex::parse_stdin()?;
    let agent_event = codex::to_event(event, input);
    ipc::send_ingest(socket, &agent_event).await?;
    Ok(())
}

async fn cmd_hook_gemini(socket: &std::path::Path, event: &str) -> Result<()> {
    let event = gemini::HookEvent::from_flag(event)?;
    let input = gemini::parse_stdin()?;
    let agent_event = gemini::to_event(event, input);
    ipc::send_ingest(socket, &agent_event).await?;
    Ok(())
}

async fn cmd_hook_opencode(socket: &std::path::Path) -> Result<()> {
    let ev = opencode::parse_stdin_agent_event()?;
    ipc::send_ingest(socket, &ev).await?;
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
