//! `muxa workers` — the worker agents muxa launched for peer work, host-wide.
//!
//! Read-only. A worker is listed only when muxa positively knows it is one:
//! its session consumed a launch grant, or its launch is verified and still
//! registering. Unmarked agents are never guessed into this list.

use std::fmt::Write as _;

use anyhow::{Context, Result};
use muxa::collaboration::{WorkerEntry, WorkerFlag, WorkerInventory};
use muxa::ipc::Client;

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Hours of observed idle time after which a worker is flagged.
    #[arg(long, default_value_t = 24)]
    idle_hours: u64,
    #[arg(long)]
    json: bool,
}

pub(crate) async fn run(args: Args, client: &Client) -> Result<()> {
    let inventory = client
        .collaboration_workers(args.idle_hours.saturating_mul(3600))
        .await
        .context("reading the worker inventory from muxad (an older daemon lacks it)")?;
    if args.json {
        println!("{}", serde_json::to_string(&inventory)?);
    } else {
        print!("{}", render(&inventory));
    }
    Ok(())
}

fn duration(seconds: u64) -> String {
    match seconds {
        s if s >= 86_400 => format!("{}d{}h", s / 86_400, (s % 86_400) / 3600),
        s if s >= 3600 => format!("{}h", s / 3600),
        s => format!("{}m", s / 60),
    }
}

fn flag_label(flag: WorkerFlag) -> &'static str {
    match flag {
        WorkerFlag::Error => "error",
        WorkerFlag::LongIdle => "long-idle",
        WorkerFlag::PossibleOverlap => "possible-overlap",
    }
}

fn row(worker: &WorkerEntry) -> String {
    let place = match (&worker.tmux_session_name, &worker.window_name) {
        (Some(session), Some(window)) => format!("{session}:{window}"),
        _ => worker.room.window_id.clone(),
    };
    let role = if worker.roles.is_empty() {
        "-".to_owned()
    } else {
        worker.roles.join(",")
    };
    let state = if worker.pending {
        "registering".to_owned()
    } else {
        format!("{:?}", worker.state).to_lowercase()
    };
    let idle = worker
        .observed_idle
        .as_ref()
        .map_or_else(|| "-".to_owned(), |idle| duration(idle.seconds));
    let flags: Vec<&str> = worker.flags.iter().map(|f| flag_label(*f)).collect();
    format!(
        "{:<12} {:<11} {:<24} {:<12} {:<12} {:>6}  q{} c{}  {}",
        worker.pane,
        format!("{:?}", worker.agent_kind).to_lowercase(),
        place,
        role,
        state,
        idle,
        worker.queued_requests,
        worker.claimed_requests,
        flags.join(",")
    )
}

fn render(inventory: &WorkerInventory) -> String {
    let mut out = String::new();
    if inventory.workers.is_empty() {
        out.push_str("No live workers.\n");
    } else {
        let _ = writeln!(
            out,
            "{:<12} {:<11} {:<24} {:<12} {:<12} {:>6}  OPEN  FLAGS",
            "PANE", "AGENT", "ROOM", "ROLE", "STATE", "IDLE"
        );
        for worker in &inventory.workers {
            out.push_str(&row(worker));
            out.push('\n');
        }
    }
    let error = inventory.flagged(WorkerFlag::Error);
    let idle = inventory.flagged(WorkerFlag::LongIdle);
    let overlap = inventory.flagged(WorkerFlag::PossibleOverlap);
    let _ = write!(
        out,
        "\n{} live worker(s); {} in error (not reused; a replacement may be spawned), \
         {} observed idle over {}, {} possibly overlapping.\n",
        inventory.workers.len(),
        error,
        idle,
        duration(inventory.idle_threshold_secs),
        overlap,
    );
    let _ = writeln!(
        out,
        "{} launch grant(s) on record, including finished sessions.",
        inventory.recorded_grants
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::collaboration::RoomId;
    use muxa::event::{AgentKind, AgentState};

    fn worker(pane: &str, flags: Vec<WorkerFlag>) -> WorkerEntry {
        WorkerEntry {
            pane: pane.into(),
            agent_kind: AgentKind::Codex,
            agent_session_id: "s".into(),
            room: RoomId {
                host: "rmux".into(),
                socket: None,
                window_id: "@1".into(),
            },
            tmux_session_name: Some("muxa".into()),
            window_name: Some("main".into()),
            roles: vec!["reviewer".into()],
            alias: None,
            state: AgentState::Error,
            pending: false,
            observed_idle: None,
            queued_requests: 1,
            claimed_requests: 0,
            flags,
        }
    }

    #[test]
    fn render_names_each_finding_without_calling_it_a_failure() {
        let inventory = WorkerInventory {
            workers: vec![worker("rmux:%304", vec![WorkerFlag::Error])],
            idle_threshold_secs: 86_400,
            recorded_grants: 96,
        };
        let text = render(&inventory);
        assert!(text.contains("rmux:%304"), "{text}");
        assert!(text.contains("muxa:main"), "{text}");
        assert!(text.contains("q1 c0"), "{text}");
        assert!(text.contains("1 in error (not reused; a replacement may be spawned)"));
        assert!(text.contains("96 launch grant(s) on record, including finished sessions"));
        assert!(render(&WorkerInventory {
            workers: vec![],
            idle_threshold_secs: 3600,
            recorded_grants: 0
        })
        .starts_with("No live workers."));
    }

    #[test]
    fn durations_read_in_days_hours_or_minutes() {
        assert_eq!(duration(2 * 86_400 + 3 * 3600), "2d3h");
        assert_eq!(duration(5 * 3600 + 59), "5h");
        assert_eq!(duration(600), "10m");
    }
}
