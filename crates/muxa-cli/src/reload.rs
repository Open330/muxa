//! `muxa snapshot` / `muxa restore` / `muxa reload` — take the multiplexer
//! down and put the workspace back.
//!
//! Restarting tmux or rmux is the only way to pick up a new server build, and
//! it discards every pane process and every agent conversation. What can be
//! rebuilt is the *shape* — sessions, windows, panes, geometry, working
//! directories — plus what each pane was running, which is the one thing that
//! has to be read before the restart because it lives in the process table.
//!
//! Everything here goes through the host's own control commands rather than
//! the pane backend, for two reasons: `muxa reload` has to run *outside* the
//! server it is about to kill, where none of the host environment variables
//! are set, and passing arguments to a process avoids the quoting traps a
//! generated shell script walks into (a window layout carries `{...}`, which
//! a shell would brace-expand).

use anyhow::{bail, Context, Result};
use clap::Parser;
use muxa::ipc::Client;
use muxa::{AgentKind, BackendEndpoint, HostKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::mux_control;

mod plan;
mod store;

use plan::RestoreReport;

/// Bumped when a field stops being additive, so an old snapshot is rejected
/// with a clear message instead of restoring a half-understood workspace.
const SNAPSHOT_VERSION: u32 = 1;

/// How long to wait for a freshly created pane's shell to reach a prompt
/// before giving up on setting its directory. A zsh with a plugin manager
/// takes several seconds; a bare `sh` is ready almost immediately, and the
/// poll exits as soon as it is.
const SHELL_READY_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Parser)]
pub struct SnapshotArgs {
    /// Directory to write the snapshot into. Defaults to a timestamped
    /// directory under `$XDG_DATA_HOME/muxa/snapshots`.
    #[arg(long)]
    out: Option<PathBuf>,

    /// Control socket of the multiplexer to snapshot — not muxad's own
    /// `--socket`. Defaults to the one muxa's tracked agents are on.
    #[arg(long)]
    mux_socket: Option<String>,

    /// Print the result as JSON.
    #[arg(long)]
    json: bool,

    /// List the snapshots under `$XDG_DATA_HOME/muxa/snapshots`, newest
    /// first, instead of taking one.
    #[arg(long, conflicts_with_all = ["out", "mux_socket", "delete", "auto"])]
    list: bool,

    /// Delete one snapshot, named by its directory under the snapshots dir.
    #[arg(long, value_name = "ID", conflicts_with_all = ["out", "mux_socket", "auto"])]
    delete: Option<String>,

    /// Take an automatic snapshot, as muxad does on its interval: skipped
    /// when nothing changed since the newest snapshot of the same server,
    /// and older automatic ones beyond `--keep-auto` are removed.
    #[arg(long, hide = true, conflicts_with = "out")]
    auto: bool,

    /// How many automatic snapshots `--auto` keeps.
    #[arg(long, hide = true, default_value_t = 10, requires = "auto")]
    keep_auto: usize,
}

#[derive(Debug, Parser)]
#[allow(clippy::struct_excessive_bools)] // independent CLI switches
pub struct RestoreArgs {
    /// Snapshot directory. Defaults to the most recent one, automatic or
    /// manual.
    snapshot: Option<PathBuf>,

    /// Actually run the restore. Without it the plan is printed and nothing
    /// is created, because a restore into a server that still has these
    /// sessions adds panes rather than replacing them.
    #[arg(long)]
    run: bool,

    /// Rebuild the shape but leave the panes as empty shells.
    #[arg(long)]
    layout_only: bool,

    /// Control socket to restore into. Defaults to the snapshot's own.
    #[arg(long)]
    mux_socket: Option<String>,

    /// Recreate only the sessions the server does not have, and leave every
    /// existing session untouched — no panes added, nothing typed into it.
    #[arg(long)]
    only_missing: bool,

    /// Print the plan — and with `--run` the per-pane results — as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Parser)]
pub struct ReloadArgs {
    /// Restore from this snapshot instead of taking a fresh one. Use it to
    /// retry a restore that was interrupted.
    #[arg(long)]
    snapshot: Option<PathBuf>,

    /// Skip the confirmation prompt.
    #[arg(long)]
    yes: bool,

    /// Rebuild the shape but do not relaunch what the panes were running.
    #[arg(long)]
    layout_only: bool,

    /// Control socket of the multiplexer to reload — not muxad's own
    /// `--socket`.
    #[arg(long)]
    mux_socket: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    version: u32,
    taken_at: String,
    /// Why it was taken. Automatic snapshots are the only ones muxad prunes;
    /// a snapshot from before this field existed reads as manual.
    #[serde(default)]
    origin: SnapshotOrigin,
    host: String,
    socket: String,
    windows: Vec<WindowShape>,
    panes: Vec<PaneShape>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SnapshotOrigin {
    /// `muxa snapshot`, or Save Snapshot in Muxa.app.
    #[default]
    Manual,
    /// muxad's periodic snapshot.
    Auto,
    /// Taken by `muxa reload` just before it restarted the server.
    Reload,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct WindowShape {
    session: String,
    index: String,
    name: String,
    active: bool,
    /// The host's own layout string, replayed verbatim by `select-layout`.
    layout: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct PaneShape {
    session: String,
    window_index: String,
    pane_index: String,
    path: String,
    /// What the pane was running, as its own argv. `None` for a bare shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    /// The same command as separate arguments, where the platform exposes
    /// them (Linux `/proc/<pid>/cmdline`). Replaying the joined `command`
    /// would re-split `--title "My App"` on its space; each argument is
    /// quoted on its own instead. `None` on macOS, whose `ps` reports only the
    /// joined line, and in snapshots taken before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    argv: Option<Vec<String>>,
    /// False when the process rewrote its argv into a status line, which
    /// reads like a command and is not one.
    #[serde(default)]
    replayable: bool,
    /// The agent muxa had tracked in this pane, when it knew of one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<AgentShape>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct AgentShape {
    kind: String,
    /// The provider's own conversation id, which is what makes a resume
    /// possible at all.
    session_id: String,
}

pub async fn snapshot(client: &Client, args: SnapshotArgs) -> Result<()> {
    if args.list {
        return list(args.json);
    }
    if let Some(id) = args.delete.as_deref() {
        let dir = store::delete(&store::root()?, id)?;
        if args.json {
            print_json(&serde_json::json!({ "deleted": id, "dir": dir }))?;
        } else {
            println!("deleted {}", dir.display());
        }
        return Ok(());
    }
    if args.auto {
        let outcome = auto_snapshot(client, args.keep_auto).await?;
        if args.json {
            print_json(&outcome)?;
        } else if let Some(reason) = outcome.reason {
            println!("skipped: {reason}");
        } else if let (Some(path), Some(summary)) = (&outcome.path, &outcome.summary_line) {
            println!("{path}");
            println!("  {summary}");
        }
        return Ok(());
    }
    let endpoint = resolve_endpoint(client, args.mux_socket.as_deref()).await?;
    let snapshot = capture(client, &endpoint).await?;
    let dir = match args.out {
        Some(dir) => dir,
        None => default_snapshot_dir()?,
    };
    let path = write_snapshot(&snapshot, &dir)?;
    if args.json {
        print_json(&SaveOutcome::saved(&snapshot, &dir, &path, Vec::new()))?;
        return Ok(());
    }
    println!("{}", path.display());
    println!("  {}", summary(&snapshot));
    Ok(())
}

/// What `muxa snapshot --json` (and `--auto --json`) reports.
#[derive(Debug, Serialize)]
struct SaveOutcome {
    skipped: bool,
    /// Why an automatic snapshot was not written: `no_server`,
    /// `ambiguous_server` or `unchanged`.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    /// The snapshot an unchanged workspace still matches.
    #[serde(skip_serializing_if = "Option::is_none")]
    matches: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<plan::Summary>,
    #[serde(skip)]
    summary_line: Option<String>,
    /// Automatic snapshots removed to stay within `--keep-auto`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pruned: Vec<String>,
}

impl SaveOutcome {
    fn saved(snapshot: &Snapshot, dir: &Path, path: &Path, pruned: Vec<String>) -> Self {
        Self {
            skipped: false,
            reason: None,
            matches: None,
            message: None,
            id: dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned()),
            dir: Some(dir.display().to_string()),
            path: Some(path.display().to_string()),
            summary: Some(plan::summarize(snapshot)),
            summary_line: Some(summary(snapshot)),
            pruned,
        }
    }

    fn skipped(reason: &'static str, message: Option<String>, matches: Option<String>) -> Self {
        Self {
            skipped: true,
            reason: Some(reason),
            matches,
            message,
            id: None,
            dir: None,
            path: None,
            summary: None,
            summary_line: None,
            pruned: Vec::new(),
        }
    }
}

/// muxad's periodic snapshot. A workspace nobody changed since the last
/// snapshot of its server is not written again, and only the newest
/// `keep` automatic snapshots survive, so the directory stays a short,
/// useful history rather than a pile of identical copies.
async fn auto_snapshot(client: &Client, keep: usize) -> Result<SaveOutcome> {
    let servers = tracked_servers(client).await?;
    let endpoint = match servers.len() {
        0 => return Ok(SaveOutcome::skipped("no_server", None, None)),
        1 => {
            let (socket, host) = servers.into_iter().next().expect("one entry");
            endpoint_for(&host, socket)?
        }
        _ => {
            let message = format!(
                "agents span several servers ({}); automatic snapshots need one",
                servers.keys().cloned().collect::<Vec<_>>().join(", ")
            );
            return Ok(SaveOutcome::skipped(
                "ambiguous_server",
                Some(message),
                None,
            ));
        }
    };
    let mut snapshot = capture(client, &endpoint).await?;
    if snapshot.panes.is_empty() {
        return Ok(SaveOutcome::skipped("no_server", None, None));
    }
    snapshot.origin = SnapshotOrigin::Auto;
    let root = store::root()?;
    let existing = store::entries(&root);
    if let Some(newest) = store::newest_of_server(&existing, &snapshot) {
        if newest
            .snapshot
            .as_ref()
            .is_ok_and(|previous| store::same_topology(previous, &snapshot))
        {
            return Ok(SaveOutcome::skipped(
                "unchanged",
                None,
                Some(newest.id.clone()),
            ));
        }
    }
    let dir = store::unique_dir(&root, time::OffsetDateTime::now_utc().unix_timestamp());
    let path = write_snapshot(&snapshot, &dir)?;
    let mut pruned = Vec::new();
    for entry in store::prunable(&store::entries(&root), keep) {
        if std::fs::remove_dir_all(&entry.dir).is_ok() {
            pruned.push(entry.id.clone());
        }
    }
    Ok(SaveOutcome::saved(&snapshot, &dir, &path, pruned))
}

fn list(json: bool) -> Result<()> {
    let root = store::root()?;
    let entries = store::entries(&root);
    if json {
        let rows: Vec<serde_json::Value> = entries
            .iter()
            .map(|entry| match &entry.snapshot {
                Ok(snapshot) => serde_json::json!({
                    "id": entry.id,
                    "dir": entry.dir,
                    "summary": plan::summarize(snapshot),
                }),
                Err(error) => serde_json::json!({
                    "id": entry.id,
                    "dir": entry.dir,
                    "error": error,
                }),
            })
            .collect();
        return print_json(&serde_json::json!({ "root": root, "snapshots": rows }));
    }
    if entries.is_empty() {
        println!("no snapshots under {}", root.display());
    }
    for entry in &entries {
        match &entry.snapshot {
            Ok(snapshot) => println!(
                "{}  {:<6}  {}",
                entry.id,
                match snapshot.origin {
                    SnapshotOrigin::Manual => "manual",
                    SnapshotOrigin::Auto => "auto",
                    SnapshotOrigin::Reload => "reload",
                },
                summary(snapshot)
            ),
            Err(error) => println!("{}  unreadable: {error}", entry.id),
        }
    }
    Ok(())
}

fn print_json(value: &impl Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

pub fn restore(args: RestoreArgs) -> Result<()> {
    let path = match args.snapshot {
        Some(path) => path,
        None => newest_snapshot()?,
    };
    let snapshot = read_snapshot(&path)?;
    let endpoint = match args.mux_socket {
        Some(socket) => endpoint_for(&snapshot.host, socket)?,
        None => endpoint_for(&snapshot.host, snapshot.socket.clone())?,
    };
    let endpoint = control_endpoint(&endpoint);
    let live = live_sessions(&endpoint);
    let mut sessions = plan::plan(
        &snapshot,
        live.as_ref().unwrap_or(&BTreeSet::new()),
        args.only_missing,
        args.layout_only,
    );
    let document = |sessions: Vec<plan::SessionPlan>, totals: Option<plan::Totals>| {
        let dir = if path.is_dir() {
            path.clone()
        } else {
            path.parent().map(Path::to_path_buf).unwrap_or_default()
        };
        plan::RestoreDocument {
            id: dir
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
            dir: dir.display().to_string(),
            summary: plan::summarize(&snapshot),
            socket: snapshot.socket.clone(),
            only_missing: args.only_missing,
            layout_only: args.layout_only,
            server_reachable: live.is_some(),
            run: args.run,
            sessions,
            totals,
        }
    };
    if !args.json {
        println!("{} — {}", path.display(), summary(&snapshot));
    }
    if !args.run {
        if args.json {
            return print_json(&document(sessions, None));
        }
        plan::print_plan(&sessions, args.layout_only);
        if args.only_missing {
            println!("\ndry run. Re-run with --run to create the missing sessions.");
        } else {
            println!(
                "\ndry run. Re-run with --run against a server that does not have these sessions."
            );
        }
        return Ok(());
    }

    let target = plan::without_skipped(&snapshot, &sessions);
    let skipped: Vec<&str> = sessions
        .iter()
        .filter(|session| session.action == plan::SessionAction::Skip)
        .map(|session| session.name.as_str())
        .collect();
    if !args.json && !skipped.is_empty() {
        println!(
            "leaving {} existing session(s) untouched: {}",
            skipped.len(),
            skipped.join(", ")
        );
    }
    let mut report = RestoreReport::new(!args.json);
    let outcome = if target.windows.is_empty() {
        report.say("nothing to restore: every session in the snapshot already exists");
        Ok(())
    } else {
        apply(&endpoint, &target, args.layout_only, &mut report)
    };
    if args.json {
        plan::attach_results(&mut sessions, &report);
        let totals = plan::totals(&sessions);
        print_json(&document(sessions, Some(totals)))?;
    }
    outcome
}

/// The sessions the server has right now; `None` when it does not answer,
/// which for a restore means every recorded session is missing.
fn live_sessions(endpoint: &BackendEndpoint) -> Option<BTreeSet<String>> {
    mux_control::capture(endpoint, &["list-sessions", "-F", "#{session_name}"])
        .ok()
        .map(|listing| listing.lines().map(str::to_owned).collect())
}

pub async fn reload(client: &Client, args: ReloadArgs) -> Result<()> {
    if inside_a_multiplexer() {
        bail!(
            "refusing to reload from inside a multiplexer: this kills the server \
             this shell lives in, so the restore would never run. Detach and try \
             again from a plain shell."
        );
    }
    let endpoint = resolve_endpoint(client, args.mux_socket.as_deref()).await?;

    let snapshot = if let Some(path) = args.snapshot.as_ref() {
        let snapshot = read_snapshot(path)?;
        println!("reusing {} — {}", path.display(), summary(&snapshot));
        snapshot
    } else {
        let mut snapshot = capture(client, &endpoint).await?;
        snapshot.origin = SnapshotOrigin::Reload;
        let path = write_snapshot(&snapshot, &default_snapshot_dir()?)?;
        println!("snapshot {} — {}", path.display(), summary(&snapshot));
        snapshot
    };

    if !args.yes && !confirm(&endpoint) {
        println!("aborted; nothing was touched.");
        return Ok(());
    }

    // Resolved while the server still answers, so the restore below targets
    // the socket the old server actually used rather than a guess.
    let control = control_endpoint(&endpoint);
    println!("stopping {}", endpoint.socket);
    let _ = mux_control::run(&control, &["kill-server"]);
    wait_for_server_to_stop(&control)?;

    apply(
        &control,
        &snapshot,
        args.layout_only,
        &mut RestoreReport::new(true),
    )?;
    println!(
        "\nagent conversations resume only where a provider id existed; \
         `muxa prune` clears the rows whose panes are gone."
    );
    Ok(())
}

/// Read the whole workspace shape off the host, plus what each pane runs.
async fn capture(client: &Client, endpoint: &BackendEndpoint) -> Result<Snapshot> {
    let control = control_endpoint(endpoint);
    let panes = mux_control::capture(
        &control,
        &[
            "list-panes",
            "-a",
            "-F",
            "#{session_name}\t#{window_index}\t#{window_id}\t#{pane_index}\t#{pane_id}\t#{pane_pid}\t#{pane_current_path}",
        ],
    )
    .map_err(anyhow::Error::msg)
    .context("listing panes")?;
    let windows = mux_control::capture(
        &control,
        &[
            "list-windows",
            "-a",
            "-F",
            "#{session_name}\t#{window_index}\t#{window_name}\t#{window_active}\t#{window_id}\t#{window_layout}",
        ],
    )
    .map_err(anyhow::Error::msg)
    .context("listing windows")?;

    let agents = client
        .snapshot()
        .await
        .map(|agents| agents_by_pane(&agents))
        .unwrap_or_default();

    let (windows, keep) = fold_window_groups(&windows);
    let mut shapes = Vec::new();
    for line in panes.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let [session, window_index, window_id, pane_index, pane_id, shell_pid, path] = f[..] else {
            continue;
        };
        if keep.get(window_id).map(String::as_str) != Some(session) {
            continue;
        }
        let (command, argv) = match shell_pid.parse().ok().and_then(child_command) {
            Some(child) => (Some(child.command), child.argv),
            None => (None, None),
        };
        shapes.push(PaneShape {
            session: session.to_owned(),
            window_index: window_index.to_owned(),
            pane_index: pane_index.to_owned(),
            path: path.to_owned(),
            replayable: command.as_deref().is_some_and(is_replayable),
            command,
            argv,
            agent: agents.get(&namespaced(endpoint.host, pane_id)).cloned(),
        });
    }

    Ok(Snapshot {
        version: SNAPSHOT_VERSION,
        origin: SnapshotOrigin::Manual,
        taken_at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        host: endpoint.host.to_string(),
        socket: endpoint.socket.clone(),
        windows,
        panes: shapes,
    })
}

/// One window is listed once per session that shows it, because a session
/// group shares its window list — which is exactly what `muxa workspace view`
/// builds so two terminals can sit on two windows of one workspace. Keeping
/// every listing would duplicate every pane, so each window id is attributed
/// to a single session, chosen deterministically.
fn fold_window_groups(listing: &str) -> (Vec<WindowShape>, BTreeMap<String, String>) {
    let mut owner: BTreeMap<String, String> = BTreeMap::new();
    let mut rows: Vec<(String, WindowShape)> = Vec::new();
    for line in listing.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let [session, index, name, active, window_id, layout] = f[..] else {
            continue;
        };
        let entry = owner
            .entry(window_id.to_owned())
            .or_insert_with(|| session.to_owned());
        if entry.as_str() > session {
            session.clone_into(entry);
        }
        rows.push((
            window_id.to_owned(),
            WindowShape {
                session: session.to_owned(),
                index: index.to_owned(),
                name: name.to_owned(),
                active: active == "1",
                layout: layout.to_owned(),
            },
        ));
    }
    let windows = rows
        .into_iter()
        .filter(|(window_id, window)| owner.get(window_id) == Some(&window.session))
        .map(|(_, window)| window)
        .collect();
    (windows, owner)
}

/// The registry names a pane by its host — `rmux:%12` — while the host's own
/// control commands report the bare `%12` they mint. Without the namespace the
/// two never meet, and no pane would ever be recognised as an agent's.
fn namespaced(host: HostKind, pane_id: &str) -> String {
    let prefix = match host {
        HostKind::Rmux => muxa::backend::rmux::PANE_ID_PREFIX,
        HostKind::Cmux => muxa::backend::cmux::PANE_ID_PREFIX,
        HostKind::Herdr => muxa::backend::herdr::PANE_ID_PREFIX,
        HostKind::Tmux | HostKind::Zellij => return pane_id.to_owned(),
    };
    if pane_id.starts_with(prefix) {
        pane_id.to_owned()
    } else {
        format!("{prefix}{pane_id}")
    }
}

/// The endpoint to send control commands to. A tmux server is named by a
/// short socket name (`default`), and the lookup that turns a name into a
/// socket path only finds sockets that exist — which, right after
/// `kill-server`, this one does not, so every restore command used to land on
/// `/dev/null`. Pinning the full path keeps the commands on the server being
/// recreated. rmux endpoints are already paths.
fn control_endpoint(endpoint: &BackendEndpoint) -> BackendEndpoint {
    match endpoint.host {
        HostKind::Tmux => BackendEndpoint {
            host: endpoint.host,
            socket: muxa::tmux::socket_path_or_default(&endpoint.socket)
                .to_string_lossy()
                .into_owned(),
        },
        _ => endpoint.clone(),
    }
}

fn agents_by_pane(agents: &[muxa::Agent]) -> HashMap<String, AgentShape> {
    agents
        .iter()
        .filter_map(|agent| {
            let pane = agent.pane.clone()?;
            let kind = match agent.kind {
                AgentKind::ClaudeCode => "claude_code",
                AgentKind::Codex => "codex",
                _ => "other",
            };
            Some((
                pane,
                AgentShape {
                    kind: kind.to_owned(),
                    session_id: agent.session_id.clone(),
                },
            ))
        })
        .collect()
}

/// Rebuild the shape, then put back what the panes were running.
fn apply(
    endpoint: &BackendEndpoint,
    snapshot: &Snapshot,
    layout_only: bool,
    report: &mut RestoreReport,
) -> Result<()> {
    rebuild_shape(endpoint, snapshot, report)?;
    if layout_only {
        return Ok(());
    }
    replay_commands(endpoint, snapshot, report);
    Ok(())
}

/// Sessions, windows, panes, geometry, working directories.
fn rebuild_shape(
    endpoint: &BackendEndpoint,
    snapshot: &Snapshot,
    report: &mut RestoreReport,
) -> Result<()> {
    let mut created = 0_usize;
    for window in &snapshot.windows {
        match ensure_window(endpoint, snapshot, window) {
            Ok(count) => {
                created += count;
                report.reached.insert(window.session.clone());
            }
            Err(error) => {
                report.failed = Some((window.session.clone(), format!("{error:#}")));
                return Err(error);
            }
        }
    }

    // Every window starts with one pane, so the first pane recorded for each
    // window is already there — whatever its index, since `pane-base-index`
    // may start the numbering at 1.
    let mut split_failures = Vec::new();
    for pane in panes_after_the_first(&snapshot.panes) {
        let target = format!("={}:{}", pane.session, pane.window_index);
        // `split-window -d` keeps splitting the same pane, halving it each
        // time until there is no room left. Re-tiling first hands the split
        // an evenly sized pane; the recorded layout replaces the tiling below.
        let _ = mux_control::run(endpoint, &["select-layout", "-t", &target, "tiled"]);
        if let Err(error) = mux_control::run(
            endpoint,
            &["split-window", "-d", "-t", &target, "-c", &pane.path],
        ) {
            report.pane_failed(pane_target(pane), format!("could not split — {error}"));
            split_failures.push((pane_target(pane), error));
        }
    }

    // Geometry last, once every pane exists.
    for window in &snapshot.windows {
        let target = format!("={}:{}", window.session, window.index);
        let _ = mux_control::run(endpoint, &["select-layout", "-t", &target, &window.layout]);
    }

    // `select-layout` renumbers panes by geometry, which is not the order the
    // splits above created them in, so directories are set per *final* index
    // rather than inherited from `split-window -c`.
    for pane in &snapshot.panes {
        let target = pane_target(pane);
        wait_for_shell(endpoint, &target);
        if let Err(error) = mux_control::run(
            endpoint,
            &[
                "send-keys",
                "-t",
                &target,
                &format!("cd {}", shell_quote(&pane.path)),
                "Enter",
            ],
        ) {
            report.pane_failed(target, error);
        }
    }

    report.say(&plan::rebuilt_line(created, snapshot.panes.len()));
    for (target, error) in split_failures {
        report.say(&plan::split_failure_line(&target, &error));
    }
    Ok(())
}

/// Every pane but the first recorded one of each window, in snapshot order.
fn panes_after_the_first(panes: &[PaneShape]) -> Vec<&PaneShape> {
    let mut seen = std::collections::BTreeSet::new();
    panes
        .iter()
        .filter(|pane| !seen.insert((pane.session.as_str(), pane.window_index.as_str())))
        .collect()
}

/// The `columns x rows` a window had, from its layout string
/// (`b25d,238x61,0,0{…}`).
fn layout_size(layout: &str) -> Option<(u32, u32)> {
    let size = layout.split(',').nth(1)?;
    let (columns, rows) = size.split_once('x')?;
    Some((columns.parse().ok()?, rows.parse().ok()?))
}

/// Create `window` — and its session, if that is missing too — at the size
/// it was recorded with. Returns how many sessions/windows it created.
fn ensure_window(
    endpoint: &BackendEndpoint,
    snapshot: &Snapshot,
    window: &WindowShape,
) -> Result<usize> {
    let mut created = 0_usize;
    let first = snapshot
        .panes
        .iter()
        .find(|pane| pane.session == window.session && pane.window_index == window.index);
    let path = first.map_or("", |pane| pane.path.as_str());
    // A detached session is 80x24 unless told otherwise, and halving that
    // runs out of room after a handful of splits. The layout string
    // carries the size the window had, so the window starts at it.
    let size = layout_size(&window.layout);
    let session_target = format!("={}", window.session);
    if mux_control::run(endpoint, &["has-session", "-t", &session_target]).is_err() {
        let mut new_session = vec![
            "new-session".to_owned(),
            "-d".to_owned(),
            "-s".to_owned(),
            window.session.clone(),
            "-n".to_owned(),
            window.name.clone(),
            "-c".to_owned(),
            path.to_owned(),
        ];
        if let Some((columns, rows)) = size {
            new_session.extend([
                "-x".to_owned(),
                columns.to_string(),
                "-y".to_owned(),
                rows.to_string(),
            ]);
        }
        let new_session: Vec<&str> = new_session.iter().map(String::as_str).collect();
        mux_control::run(endpoint, &new_session)
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("creating session {}", window.session))?;
        created += 1;
    }
    // `has-session` resolves only the session half of a target, so it
    // answers yes for a window that does not exist. The window list is the
    // only honest answer — and the session's own first window does not
    // necessarily land on the index this one wants.
    let window_target = format!("={}:{}", window.session, window.index);
    if !window_indexes(endpoint, &session_target).contains(&window.index) {
        mux_control::run(
            endpoint,
            &[
                "new-window",
                "-d",
                "-t",
                &window_target,
                "-n",
                &window.name,
                "-c",
                path,
            ],
        )
        .map_err(anyhow::Error::msg)
        .with_context(|| format!("creating window {window_target}"))?;
        created += 1;
    }
    if let Some((columns, rows)) = size {
        let _ = mux_control::run(
            endpoint,
            &[
                "resize-window",
                "-t",
                &window_target,
                "-x",
                &columns.to_string(),
                "-y",
                &rows.to_string(),
            ],
        );
    }
    Ok(created)
}

/// The window indexes a session currently has.
fn window_indexes(endpoint: &BackendEndpoint, session_target: &str) -> Vec<String> {
    mux_control::capture(
        endpoint,
        &[
            "list-windows",
            "-t",
            session_target,
            "-F",
            "#{window_index}",
        ],
    )
    .map(|listing| listing.lines().map(str::to_owned).collect())
    .unwrap_or_default()
}

/// Type each pane's own command line back into it.
fn replay_commands(endpoint: &BackendEndpoint, snapshot: &Snapshot, report: &mut RestoreReport) {
    let mut replayed = 0_usize;
    let mut manual = Vec::new();
    for pane in &snapshot.panes {
        let Some(command) = relaunch_command(pane) else {
            continue;
        };
        if !pane.replayable {
            manual.push(pane_target(pane));
            continue;
        }
        let target = pane_target(pane);
        match mux_control::run(endpoint, &["send-keys", "-t", &target, &command, "Enter"]) {
            Ok(()) => replayed += 1,
            Err(error) => report.pane_failed(target, error),
        }
    }
    report.say(&plan::relaunched_line(replayed));
    for target in manual {
        report.say(&plan::manual_line(&target));
    }
}

/// What to type back into a pane: its own command line, with the provider's
/// conversation id spliced in when muxa knows one and the command does not
/// already carry it.
fn relaunch_command(pane: &PaneShape) -> Option<String> {
    let command = match pane.argv.as_deref() {
        Some(argv) if !argv.is_empty() => argv
            .iter()
            .map(|argument| shell_word(argument))
            .collect::<Vec<_>>()
            .join(" "),
        _ => pane.command.clone()?,
    };
    let Some(agent) = pane.agent.as_ref() else {
        return Some(command);
    };
    if agent.session_id.is_empty() || agent.session_id.starts_with("synthetic") {
        return Some(command);
    }
    match agent.kind.as_str() {
        "claude_code" if !command.contains("--resume") => {
            Some(format!("{command} --resume {}", agent.session_id))
        }
        "codex" if !command.contains(" resume ") => {
            Some(format!("{command} resume {}", agent.session_id))
        }
        _ => Some(command),
    }
}

/// False for argv a process rewrote into a status line. Puma and friends
/// replace their own argv with something like
/// `puma 5.6.8 (tcp://0.0.0.0:5072) [admin]`, which reads like a command and
/// is not one; the original invocation is simply not in the process table.
fn is_replayable(command: &str) -> bool {
    !command.is_empty() && !command.contains(['(', ')', '[', ']'])
}

fn pane_target(pane: &PaneShape) -> String {
    format!(
        "={}:{}.{}",
        pane.session, pane.window_index, pane.pane_index
    )
}

/// A pane created a moment ago is still starting its shell, and keys sent
/// before the prompt appears are swallowed. Poll for the shell instead of
/// sleeping a guessed interval.
fn wait_for_shell(endpoint: &BackendEndpoint, target: &str) {
    let deadline = Instant::now() + SHELL_READY_TIMEOUT;
    while Instant::now() < deadline {
        let idle = mux_control::capture(
            endpoint,
            &[
                "display-message",
                "-p",
                "-t",
                target,
                "#{pane_current_command}",
            ],
        )
        .is_ok_and(|command| is_shell(command.trim()));
        if idle {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// One argument as a shell reads it back: bare when it is plainly a word,
/// quoted otherwise, so the relaunch hands the program the same argv.
fn shell_word(value: &str) -> String {
    let plain = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,".contains(c));
    if plain {
        value.to_owned()
    } else {
        shell_quote(value)
    }
}

fn summary(snapshot: &Snapshot) -> String {
    let sessions: std::collections::BTreeSet<_> = snapshot
        .windows
        .iter()
        .map(|window| &window.session)
        .collect();
    format!(
        "{} pane(s), {} window(s), {} session(s) on {} {}",
        snapshot.panes.len(),
        snapshot.windows.len(),
        sessions.len(),
        snapshot.host,
        snapshot.socket,
    )
}

/// Which multiplexer to act on. The agents muxa already tracks name their own
/// server, so the common case needs no flag; two servers in play is the case
/// that genuinely cannot be guessed.
async fn resolve_endpoint(client: &Client, socket: Option<&str>) -> Result<BackendEndpoint> {
    if let Some(socket) = socket {
        // An explicit socket still needs a host to speak the right dialect.
        // The shell's own host is the best available answer; outside a
        // multiplexer the socket path is all there is to go on.
        let host = muxa::backend::detect_host_env()
            .or_else(|| host_from_socket_path(socket))
            .unwrap_or(HostKind::Tmux);
        return endpoint_for(&host.to_string(), socket.to_owned());
    }
    let sockets = tracked_servers(client).await?;
    match sockets.len() {
        1 => {
            let (socket, host) = sockets.into_iter().next().expect("one entry");
            endpoint_for(&host, socket)
        }
        0 => bail!(
            "muxad tracks no tmux/rmux agents, so there is nothing to work out the \
             target server from; pass --mux-socket"
        ),
        _ => bail!(
            "agents span several servers ({}); pass --mux-socket to name one",
            sockets.keys().cloned().collect::<Vec<_>>().join(", ")
        ),
    }
}

/// The control sockets muxa's tracked agents live on, each with its host.
async fn tracked_servers(client: &Client) -> Result<BTreeMap<String, String>> {
    let agents = client
        .snapshot()
        .await
        .context("asking muxad which multiplexer its agents are on")?;
    let mut sockets: BTreeMap<String, String> = BTreeMap::new();
    for agent in &agents {
        let (Some(pane), Some(socket)) = (agent.pane.as_ref(), agent.tmux_socket.as_ref()) else {
            continue;
        };
        let Some(host) = muxa::backend::pane_id_host_kind(pane) else {
            continue;
        };
        if mux_control::supported(host) {
            sockets.insert(socket.clone(), host.to_string());
        }
    }
    Ok(sockets)
}

fn endpoint_for(host: &str, socket: String) -> Result<BackendEndpoint> {
    let host = match host {
        "tmux" => HostKind::Tmux,
        "rmux" => HostKind::Rmux,
        "cmux" => HostKind::Cmux,
        other => bail!("{other} has no control interface to snapshot through"),
    };
    if !mux_control::supported(host) {
        bail!("{host} has no control interface to snapshot through");
    }
    Ok(BackendEndpoint { host, socket })
}

/// rmux and cmux keep their sockets in their own directories, which is the
/// only hint left once the shell is outside every multiplexer.
fn host_from_socket_path(socket: &str) -> Option<HostKind> {
    if socket.contains("/rmux-") {
        Some(HostKind::Rmux)
    } else if socket.contains("/cmux-") {
        Some(HostKind::Cmux)
    } else if socket.contains("/tmux-") {
        Some(HostKind::Tmux)
    } else {
        None
    }
}

fn inside_a_multiplexer() -> bool {
    ["TMUX", "RMUX", "CMUX_SURFACE_ID"]
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|value| !value.is_empty()))
}

fn wait_for_server_to_stop(endpoint: &BackendEndpoint) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if mux_control::capture(endpoint, &["list-sessions"]).is_err() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    bail!(
        "{} still answers after kill-server; stopping here so the snapshot stays usable",
        endpoint.socket
    )
}

fn confirm(endpoint: &BackendEndpoint) -> bool {
    println!(
        "\nThis kills every pane and every running agent on {}.",
        endpoint.socket
    );
    let answer: String = cliclack::input("Type 'reload' to continue")
        .default_input("")
        .interact()
        .unwrap_or_default();
    answer.trim() == "reload"
}

fn default_snapshot_dir() -> Result<PathBuf> {
    let root =
        muxa::paths::default_snapshot_dir().context("no data directory to write snapshots into")?;
    let stamp = time::OffsetDateTime::now_utc().unix_timestamp();
    Ok(store::unique_dir(&root, stamp))
}

fn newest_snapshot() -> Result<PathBuf> {
    let root =
        muxa::paths::default_snapshot_dir().context("no data directory to read snapshots from")?;
    let mut entries: Vec<_> = std::fs::read_dir(&root)
        .with_context(|| format!("reading {}", root.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.join("snapshot.json").is_file())
        .collect();
    entries.sort();
    entries
        .pop()
        .with_context(|| format!("no snapshots under {}", root.display()))
}

fn write_snapshot(snapshot: &Snapshot, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join("snapshot.json");
    std::fs::write(&path, serde_json::to_vec_pretty(snapshot)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn read_snapshot(path: &Path) -> Result<Snapshot> {
    let path = if path.is_dir() {
        path.join("snapshot.json")
    } else {
        path.to_path_buf()
    };
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let snapshot: Snapshot =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    if snapshot.version > SNAPSHOT_VERSION {
        bail!(
            "{} was written by a newer muxa (format {} > {SNAPSHOT_VERSION})",
            path.display(),
            snapshot.version
        );
    }
    Ok(snapshot)
}

/// A pane's foreground command as captured from the process table.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ChildCommand {
    /// The command line, joined.
    command: String,
    /// Its separate arguments, where the platform exposes them.
    argv: Option<Vec<String>>,
}

/// What a pane is running: the pane's own process is the shell, and the
/// command is its child. Only readable while that process lives, which is why
/// it is captured before the restart rather than reconstructed after one.
#[cfg(target_os = "linux")]
fn child_command(pane_pid: u32) -> Option<ChildCommand> {
    let children =
        std::fs::read_to_string(format!("/proc/{pane_pid}/task/{pane_pid}/children")).ok()?;
    children.split_whitespace().find_map(|child| {
        let raw = std::fs::read(format!("/proc/{child}/cmdline")).ok()?;
        let argv: Vec<String> = String::from_utf8_lossy(&raw)
            .split('\0')
            .filter(|argument| !argument.is_empty())
            .map(str::to_owned)
            .collect();
        let command = argv.join(" ").trim().to_owned();
        (!command.is_empty() && !is_shell(&command)).then_some(ChildCommand {
            command,
            argv: Some(argv),
        })
    })
}

/// BSD `ps` has no `--ppid`, so the children come from `pgrep -P` and each
/// one's command line from `ps -p`. macOS only reports that line joined; the
/// original argument boundaries are not recoverable without `sysctl`
/// (`KERN_PROCARGS2`), which needs unsafe code this workspace forbids, so an
/// argument that contained a space replays split on it.
#[cfg(not(target_os = "linux"))]
fn child_command(pane_pid: u32) -> Option<ChildCommand> {
    child_command_with(
        pane_pid,
        |pid| {
            std::process::Command::new("pgrep")
                .args(["-P", &pid.to_string()])
                .output()
                .map(|output| parse_pids(&String::from_utf8_lossy(&output.stdout)))
                .unwrap_or_default()
        },
        |pid| {
            std::process::Command::new("ps")
                .args(["-o", "command=", "-p", &pid.to_string()])
                .output()
                .ok()
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        },
    )
}

/// The first child of `pane_pid` that is not itself a shell, with the process
/// table reached through `children` and `command_of` so the choice can be
/// tested without one.
#[cfg(any(not(target_os = "linux"), test))]
fn child_command_with(
    pane_pid: u32,
    children: impl Fn(u32) -> Vec<u32>,
    command_of: impl Fn(u32) -> Option<String>,
) -> Option<ChildCommand> {
    children(pane_pid).into_iter().find_map(|child| {
        let command = command_of(child)?;
        (!command.is_empty() && !is_shell(&command)).then_some(ChildCommand {
            command,
            argv: None,
        })
    })
}

#[cfg(any(not(target_os = "linux"), test))]
fn parse_pids(listing: &str) -> Vec<u32> {
    listing
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

fn is_shell(command: &str) -> bool {
    matches!(
        command.trim_start_matches('-'),
        "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(session: &str, window: &str, index: &str, command: Option<&str>) -> PaneShape {
        PaneShape {
            session: session.into(),
            window_index: window.into(),
            pane_index: index.into(),
            path: "/tmp".into(),
            replayable: command.is_some_and(is_replayable),
            command: command.map(str::to_owned),
            argv: None,
            agent: None,
        }
    }

    #[test]
    fn a_pane_is_captured_from_its_child_not_its_shell() {
        // macOS: `pgrep -P` lists the shell's children and `ps -p` names
        // each; a nested shell is skipped in favour of the real command.
        let children = |pid: u32| if pid == 10 { vec![11, 12] } else { vec![] };
        let command_of = |pid: u32| match pid {
            11 => Some("-zsh".to_owned()),
            12 => Some("npm run dev".to_owned()),
            _ => None,
        };
        assert_eq!(
            child_command_with(10, children, command_of).map(|child| child.command),
            Some("npm run dev".to_owned())
        );
        assert_eq!(child_command_with(99, children, command_of), None);
        assert_eq!(parse_pids("123\n 456\n\nnot-a-pid\n"), vec![123, 456]);
    }

    #[test]
    fn the_first_recorded_pane_of_each_window_is_not_split_again() {
        // With `pane-base-index 1` there is no pane "0"; the window's own
        // first pane must still be the one that is not created by a split.
        let panes = [
            pane("work", "1", "1", None),
            pane("work", "1", "2", None),
            pane("work", "2", "1", None),
            pane("side", "1", "1", None),
            pane("side", "1", "2", None),
        ];
        let split: Vec<String> = panes_after_the_first(&panes)
            .into_iter()
            .map(pane_target)
            .collect();
        assert_eq!(split, ["=work:1.2", "=side:1.2"]);
    }

    #[test]
    fn a_window_starts_at_its_recorded_size() {
        assert_eq!(
            layout_size("b25d,238x61,0,0{119x61,0,0,1,118x61,120,0,2}"),
            Some((238, 61))
        );
        assert_eq!(layout_size("c3a1,80x24,0,0,5"), Some((80, 24)));
        assert_eq!(layout_size("garbage"), None);
    }

    #[test]
    fn captured_arguments_replay_with_their_boundaries() {
        let mut app = pane("work", "0", "0", Some("app --title My App"));
        app.argv = Some(vec!["app".into(), "--title".into(), "My App".into()]);
        assert_eq!(
            relaunch_command(&app).as_deref(),
            Some("app --title 'My App'")
        );
        let mut script = pane("work", "0", "0", Some("bash -c make && ./run"));
        script.argv = Some(vec!["bash".into(), "-c".into(), "make && ./run".into()]);
        assert_eq!(
            relaunch_command(&script).as_deref(),
            Some("bash -c 'make && ./run'")
        );
    }

    #[test]
    fn a_tmux_socket_name_is_pinned_to_a_path_even_when_the_server_is_down() {
        let control = control_endpoint(&BackendEndpoint {
            host: HostKind::Tmux,
            socket: "__muxa_reload_test_gone__".into(),
        });
        assert!(control.socket.contains('/'), "{}", control.socket);
        assert!(control.socket.ends_with("/__muxa_reload_test_gone__"));
        let explicit = control_endpoint(&BackendEndpoint {
            host: HostKind::Tmux,
            socket: "/tmp/tmux-501/work".into(),
        });
        assert_eq!(explicit.socket, "/tmp/tmux-501/work");
    }

    #[test]
    fn grouped_windows_are_attributed_to_one_session() {
        // `muxa workspace view` puts two terminals on one workspace by adding
        // session-group members, and every window is then listed once per
        // member. Both listings describe the same window.
        let listing = "work\t0\tmain\t1\t@7\tlayout-a\n\
                       work~view~9\t0\tmain\t1\t@7\tlayout-a\n\
                       work\t1\tside\t0\t@8\tlayout-b\n";
        let (windows, owner) = fold_window_groups(listing);
        assert_eq!(windows.len(), 2, "one row per window, not per member");
        assert!(windows.iter().all(|window| window.session == "work"));
        assert_eq!(owner.get("@7").map(String::as_str), Some("work"));
    }

    #[test]
    fn a_rewritten_argv_is_not_replayable() {
        assert!(is_replayable("python -m uvicorn asgi:app --port 6002"));
        assert!(!is_replayable("puma 5.6.8 (tcp://0.0.0.0:5072) [admin]"));
    }

    #[test]
    fn a_tracked_claude_pane_comes_back_on_its_own_conversation() {
        let mut claude = pane("work", "0", "0", Some("aas e june@claude"));
        claude.agent = Some(AgentShape {
            kind: "claude_code".into(),
            session_id: "abc-123".into(),
        });
        assert_eq!(
            relaunch_command(&claude).as_deref(),
            Some("aas e june@claude --resume abc-123"),
        );
    }

    #[test]
    fn a_command_that_already_resumes_is_left_alone() {
        let mut claude = pane("work", "0", "0", Some("claude --resume kept-id"));
        claude.agent = Some(AgentShape {
            kind: "claude_code".into(),
            session_id: "other-id".into(),
        });
        assert_eq!(
            relaunch_command(&claude).as_deref(),
            Some("claude --resume kept-id"),
        );
    }

    #[test]
    fn a_synthetic_id_is_not_a_conversation_to_resume() {
        // Panes muxa only discovered by scanning carry a synthetic id, which
        // no provider would recognise.
        let mut codex = pane("work", "0", "0", Some("codex --yolo"));
        codex.agent = Some(AgentShape {
            kind: "codex".into(),
            session_id: "synthetic-7".into(),
        });
        assert_eq!(relaunch_command(&codex).as_deref(), Some("codex --yolo"));
    }

    #[test]
    fn a_pane_id_is_looked_up_under_its_host_namespace() {
        // The registry stores `rmux:%12`; the control command reports `%12`.
        assert_eq!(namespaced(HostKind::Rmux, "%12"), "rmux:%12");
        assert_eq!(namespaced(HostKind::Rmux, "rmux:%12"), "rmux:%12");
        assert_eq!(namespaced(HostKind::Tmux, "%12"), "%12");
    }

    #[test]
    fn shell_quoting_survives_an_apostrophe() {
        assert_eq!(shell_quote("/tmp/june's dir"), r"'/tmp/june'\''s dir'");
    }
}
