//! `tend servers` — TCP listeners on this host associated with tend: under
//! a pane where tend tracks an agent, or inside a worktree tend created.
//! Read-only; see `tend::listener_inventory` for what each status means and
//! what it does not.

use std::fmt::Write as _;

use anyhow::Result;
use tend::ipc::Client;
use tend::listener_inventory::{Association, Bind, Coverage, ListenerInventory, ListenerStatus};

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run(args: Args, client: &Client) -> Result<()> {
    // Without the daemon, rows lean on worktrees and the output says
    // attribution was partial.
    let agents = client.snapshot().await.map_err(|error| error.to_string());
    let inventory =
        tokio::task::spawn_blocking(move || tend::listener_inventory::inventory(&agents, None))
            .await?;
    if args.json {
        println!("{}", serde_json::to_string_pretty(&inventory)?);
    } else {
        print!("{}", render(&inventory));
    }
    Ok(())
}

fn coverage(label: &str, coverage: &Coverage) -> Option<String> {
    match coverage {
        Coverage::Complete => None,
        Coverage::Partial { reason } => Some(format!("{label} partial: {reason}")),
        Coverage::Unavailable { reason } => Some(format!("{label} unavailable: {reason}")),
    }
}

fn status_text(status: ListenerStatus, secs: Option<i64>) -> String {
    let ago = secs
        .map(|s| format!(" for {}", duration(s)))
        .unwrap_or_default();
    match status {
        ListenerStatus::WorkCompleted => "its Work completed".into(),
        ListenerStatus::AgentStopped => format!("agent stopped{ago}"),
        ListenerStatus::AgentIdle => format!("agent idle{ago}"),
        ListenerStatus::AgentError => format!("agent in error{ago}"),
        ListenerStatus::AgentNeedsInput => "agent needs input".into(),
        ListenerStatus::AgentWorking => "agent working".into(),
        ListenerStatus::Unknown => "status unknown".into(),
    }
}

fn duration(secs: i64) -> String {
    match secs {
        s if s < 120 => format!("{s}s"),
        s if s < 7200 => format!("{}m", s / 60),
        s if s < 172_800 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}

fn render(inventory: &ListenerInventory) -> String {
    let mut out = String::new();
    if let Some(line) = coverage("Listing", &inventory.collection) {
        let _ = writeln!(out, "{line}");
        if matches!(inventory.collection, Coverage::Unavailable { .. }) {
            return out;
        }
    }
    if let Some(line) = coverage("Association", &inventory.attribution) {
        let _ = writeln!(out, "{line}");
    }
    if inventory.rows.is_empty() {
        let _ = writeln!(
            out,
            "No listening servers associated with tend on this host."
        );
        return out;
    }
    let _ = writeln!(
        out,
        "{} listening process(es) associated with tend. Association is not proof an agent started it.",
        inventory.total
    );
    for row in &inventory.rows {
        let ports: Vec<String> = row
            .endpoints
            .iter()
            .map(|e| {
                let note = match e.bind {
                    Bind::Loopback => "",
                    Bind::Wildcard => " (all interfaces)",
                    Bind::Interface => " (non-loopback)",
                };
                format!("{}:{}{note}", e.address, e.port)
            })
            .collect();
        let _ = writeln!(
            out,
            "{}  pid {}  {}  — {}",
            row.process_name,
            row.pid,
            ports.join(", "),
            status_text(row.status, row.state_for_secs)
        );
        let via = match &row.association {
            Association::Pane {
                endpoint,
                pane_id,
                agent_kind,
                ..
            } => format!("under {agent_kind} pane {pane_id} ({endpoint})"),
            Association::Worktree { path, origin, .. } => {
                format!("in worktree {} ({origin})", path.display())
            }
        };
        let _ = writeln!(out, "    {via}");
        if let Some(cwd) = &row.cwd {
            let _ = writeln!(out, "    cwd {cwd}");
        }
    }
    out
}
