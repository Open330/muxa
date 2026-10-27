//! `muxa workers` — the worker agents muxa launched for peer work, host-wide.
//!
//! Read-only. A worker is listed only when muxa positively knows it is one:
//! its session consumed a launch grant, or its launch is verified and still
//! registering. Unmarked agents are never guessed into this list.

use std::fmt::Write as _;

use anyhow::{Context, Result};
use muxa::collaboration::{
    WorkerEntry, WorkerFlag, WorkerInventory, WorkerSpawnPhase, WorkerSpawnView,
};
use muxa::ipc::Client;
use serde::Serialize;

#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    action: Option<Action>,
    /// Hours of observed idle time after which a worker is flagged.
    #[arg(long, default_value_t = 24)]
    idle_hours: u64,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, clap::Subcommand)]
enum Action {
    /// Give up the capacity an unfinished worker launch still holds, so the
    /// room can launch a worker again. muxad refuses while a live process
    /// still matches the launch; it never stops a process. Verify the old
    /// launcher has stopped first. Audited.
    Release {
        /// The launch id `muxa workers` lists under launches.
        launch_id: String,
        /// Why it is safe to release, recorded in the audit log.
        #[arg(long)]
        reason: String,
    },
}

/// What `muxa workers --json` prints: the inventory, plus launches that
/// still hold capacity without a registered worker. `launches` is `null`
/// when the daemon predates launch coordination, never an empty list
/// standing in for "could not tell".
#[derive(Serialize)]
struct Listing<'a> {
    #[serde(flatten)]
    inventory: &'a WorkerInventory,
    launches: Option<Vec<&'a WorkerSpawnView>>,
}

pub(crate) async fn run(args: Args, client: &Client) -> Result<()> {
    if let Some(Action::Release { launch_id, reason }) = args.action {
        let origin = crate::watch::watch_collaboration_origin(std::env::var("TMUX_PANE").ok());
        client
            .collaboration_worker_spawn_release(&origin, &launch_id, &reason)
            .await
            .with_context(|| format!("releasing worker launch {launch_id}"))?;
        println!("released {launch_id}");
        return Ok(());
    }
    let inventory = client
        .collaboration_workers(args.idle_hours.saturating_mul(3600))
        .await
        .context("reading the worker inventory from muxad (an older daemon lacks it)")?;
    // Launch records come from the spawn coordinator. Only a daemon that
    // lacks it may show none; any other failure is reported, because an empty
    // list would hide the very launch that blocks a room.
    let supported = client
        .hello(std::time::Duration::from_secs(3))
        .await
        .context("asking muxad what it supports")?
        .capabilities
        .iter()
        .any(|cap| cap == muxa::collaboration::WORKER_SPAWN_RELEASE_CAPABILITY);
    let spawns = if supported {
        Some(
            client
                .collaboration_worker_spawns()
                .await
                .context("reading worker launch records from muxad")?,
        )
    } else {
        None
    };
    let launches = spawns
        .as_deref()
        .map(|spawns| unfinished(spawns, &inventory));
    if args.json {
        println!(
            "{}",
            serde_json::to_string(&Listing {
                inventory: &inventory,
                launches,
            })?
        );
    } else {
        print!("{}", render(&inventory));
        match &launches {
            Some(launches) => print!("{}", render_launches(launches)),
            None => println!(
                "\nLaunch records: not available from this muxad (it predates launch \
                 coordination); upgrade and restart it."
            ),
        }
    }
    Ok(())
}

/// Launches still holding capacity that the worker list does not already
/// account for. A bound launch is hidden only when its worker is listed: a
/// bound worker that started a new (protected) session still holds the
/// launch's capacity, and that launch is what a caller's `launch_pending`
/// points at.
fn unfinished<'a>(
    spawns: &'a [WorkerSpawnView],
    inventory: &WorkerInventory,
) -> Vec<&'a WorkerSpawnView> {
    let listed = |spawn: &WorkerSpawnView| {
        inventory.workers.iter().any(|worker| {
            spawn.pane.as_deref() == Some(worker.pane.as_str()) && spawn.room == worker.room
        })
    };
    spawns
        .iter()
        .filter(|spawn| {
            spawn.closed_at.is_none()
                && spawn.phase != WorkerSpawnPhase::Failed
                && !(spawn.phase == WorkerSpawnPhase::SessionBound && listed(spawn))
        })
        .collect()
}

fn render_launches(launches: &[&WorkerSpawnView]) -> String {
    if launches.is_empty() {
        return String::new();
    }
    let mut out = String::from("\nLaunches holding capacity:\n");
    for spawn in launches {
        let phase = wire_name(spawn.phase);
        let _ = writeln!(
            out,
            "  {}  {} {} in {}  {}  pane {}{}",
            spawn.launch_id,
            wire_name(spawn.agent_kind),
            spawn.role,
            spawn.room.window_id,
            phase,
            spawn.pane.as_deref().unwrap_or("-"),
            spawn
                .detail
                .as_deref()
                .map_or_else(String::new, |detail| format!(" — {detail}")),
        );
    }
    if launches
        .iter()
        .any(|spawn| spawn.phase == WorkerSpawnPhase::Uncertain)
    {
        out.push_str(
            "An uncertain launch blocks new workers of that kind in its room. Once \
             you have checked its launcher stopped, free it with \
             `muxa workers release <launch-id> --reason <why>`.\n",
        );
    }
    out
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

/// A kind or state as muxa names it on the wire (`claude_code`, `idle`).
fn wire_name(value: impl Serialize) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// The columns of one worker row, before alignment.
fn cells(worker: &WorkerEntry) -> [String; 7] {
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
        wire_name(worker.state)
    };
    let idle = worker
        .observed_idle
        .as_ref()
        .map_or_else(|| "-".to_owned(), |idle| duration(idle.seconds));
    let flags: Vec<&str> = worker.flags.iter().map(|f| flag_label(*f)).collect();
    [
        worker.pane.clone(),
        wire_name(worker.agent_kind),
        place,
        role,
        state,
        idle,
        format!(
            "q{} c{}  {}",
            worker.queued_requests,
            worker.claimed_requests,
            flags.join(",")
        ),
    ]
}

/// Rows aligned to their widest cell, so long room names keep columns.
fn table(rows: &[[String; 7]]) -> String {
    let mut widths = [0usize; 7];
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut out = String::new();
    for row in rows {
        let mut line = String::new();
        for (i, (cell, width)) in row.iter().zip(widths).enumerate() {
            if i + 1 == row.len() {
                line.push_str(cell);
            } else {
                let _ = write!(line, "{cell:<width$}  ");
            }
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

fn render(inventory: &WorkerInventory) -> String {
    let mut out = String::new();
    if inventory.workers.is_empty() {
        out.push_str("No live workers.\n");
    } else {
        let header = [
            "PANE",
            "AGENT",
            "ROOM",
            "ROLE",
            "STATE",
            "IDLE",
            "OPEN  FLAGS",
        ]
        .map(str::to_owned);
        let rows: Vec<[String; 7]> = std::iter::once(header)
            .chain(inventory.workers.iter().map(cells))
            .collect();
        out.push_str(&table(&rows));
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
            draining: false,
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
        assert!(text.contains("codex"), "{text}");
        assert!(text.contains("error"), "{text}");
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

    fn spawn(id: &str, phase: WorkerSpawnPhase, closed: bool) -> WorkerSpawnView {
        let at = time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        WorkerSpawnView {
            launch_id: id.into(),
            room: RoomId {
                host: "rmux".into(),
                socket: None,
                window_id: "@34".into(),
            },
            agent_kind: AgentKind::Codex,
            role: "reviewer".into(),
            phase,
            pane: None,
            socket: None,
            detail: Some("create result unknown".into()),
            lease_until: at,
            closed_at: closed.then_some(at),
        }
    }

    #[test]
    fn only_launches_still_holding_capacity_are_shown_with_how_to_free_them() {
        let mut bound = spawn("bound", WorkerSpawnPhase::SessionBound, false);
        bound.pane = Some("rmux:%5".into());
        // Bound, but its worker started a new protected session and is no
        // longer listed: it still holds capacity, so it must show.
        let mut orphaned = spawn("orphaned", WorkerSpawnPhase::SessionBound, false);
        orphaned.pane = Some("rmux:%6".into());
        let spawns = [
            bound,
            orphaned,
            spawn("failed", WorkerSpawnPhase::Failed, false),
            spawn("closed", WorkerSpawnPhase::Uncertain, true),
            spawn("stuck", WorkerSpawnPhase::Uncertain, false),
            spawn("starting", WorkerSpawnPhase::Launching, false),
        ];
        let mut listed = worker("rmux:%5", vec![]);
        listed.room.window_id = "@34".into();
        let inventory = WorkerInventory {
            workers: vec![listed],
            idle_threshold_secs: 3600,
            recorded_grants: 0,
        };
        let open = unfinished(&spawns, &inventory);
        assert_eq!(
            open.iter()
                .map(|s| s.launch_id.as_str())
                .collect::<Vec<_>>(),
            ["orphaned", "stuck", "starting"]
        );
        let text = render_launches(&open);
        assert!(
            text.contains("stuck  codex reviewer in @34  uncertain"),
            "{text}"
        );
        assert!(text.contains("create result unknown"), "{text}");
        assert!(
            text.contains("muxa workers release <launch-id> --reason"),
            "{text}"
        );
        // Nothing uncertain, no release advice.
        assert!(!render_launches(&open[2..]).contains("release"));
        assert!(render_launches(&[]).is_empty());
    }

    #[test]
    fn columns_stay_aligned_past_a_long_room_name() {
        let mut long = worker("rmux:%1", vec![]);
        long.window_name = Some("RELEASE-1025-STREAMING".into());
        long.agent_kind = AgentKind::ClaudeCode;
        let text = render(&WorkerInventory {
            workers: vec![long, worker("rmux:%22", vec![WorkerFlag::LongIdle])],
            idle_threshold_secs: 3600,
            recorded_grants: 0,
        });
        let lines: Vec<&str> = text.lines().take(3).collect();
        let column = |line: &str| line.find("reviewer").unwrap();
        assert_eq!(column(lines[1]), column(lines[2]), "{text}");
        assert!(text.contains("claude_code"), "{text}");
    }

    /// A daemon that answers `collaboration_workers` and fails
    /// `collaboration_worker_spawns`, advertising `capabilities`.
    #[cfg(unix)]
    fn half_working_daemon(dir: &std::path::Path, capabilities: Vec<String>) -> Client {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
        let socket = dir.join("muxad.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let capabilities = capabilities.clone();
                tokio::spawn(async move {
                    let (reader, mut writer) = stream.into_split();
                    let mut lines = BufReader::new(reader).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                        let answer = match request["kind"].as_str().unwrap_or_default() {
                            "hello" => serde_json::json!({
                                "ok": true,
                                "capabilities": capabilities,
                                "protocol": muxa::event::PROTOCOL_VERSION,
                            }),
                            "collaboration_workers" => serde_json::json!({
                                "ok": true,
                                "worker_inventory": {
                                    "workers": [],
                                    "idle_threshold_secs": 3600,
                                    "recorded_grants": 0,
                                },
                            }),
                            _ => serde_json::json!({"ok": false, "error": "coordinator busy"}),
                        };
                        let mut bytes = serde_json::to_vec(&answer).unwrap();
                        bytes.push(b'\n');
                        if writer.write_all(&bytes).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Client::new(socket)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_failed_launch_lookup_is_an_error_not_an_empty_list() {
        let list = || Args {
            action: None,
            idle_hours: 1,
            json: true,
        };
        let dir = tempfile::tempdir().unwrap();
        let client = half_working_daemon(
            dir.path(),
            vec![muxa::collaboration::WORKER_SPAWN_RELEASE_CAPABILITY.into()],
        );
        let error = run(list(), &client).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("reading worker launch records"),
            "{error:#}"
        );
        // A daemon without launch coordination is not an error: it has none.
        let older = tempfile::tempdir().unwrap();
        let client = half_working_daemon(older.path(), Vec::new());
        run(list(), &client).await.unwrap();
    }

    #[test]
    fn durations_read_in_days_hours_or_minutes() {
        assert_eq!(duration(2 * 86_400 + 3 * 3600), "2d3h");
        assert_eq!(duration(5 * 3600 + 59), "5h");
        assert_eq!(duration(600), "10m");
    }
}
