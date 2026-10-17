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

mod agent;
mod launch;
mod plan;
#[cfg(not(target_os = "linux"))]
mod process;
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

/// How long a pane may report a program other than its shell before it counts
/// as busy rather than as a shell still starting up.
const BUSY_GRACE: Duration = Duration::from_secs(2);

/// How long a resumed agent gets to draw its screen before the follow-up.
const CONTINUE_READY_TIMEOUT: Duration = Duration::from_secs(30);
/// The screen must hold still this long to count as drawn.
const CONTINUE_READY_QUIET: Duration = Duration::from_millis(1500);
/// Between typing the follow-up and pressing Enter, so a TUI that pastes in
/// bursts has the whole line before it is submitted.
const CONTINUE_SUBMIT_GRACE: Duration = Duration::from_millis(300);

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
    /// is created. What already exists is kept: only missing sessions,
    /// windows and panes are added, so a retry is safe.
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

    /// After the relaunch, type this (default `continue`) into every agent
    /// that was mid-turn when the snapshot was taken. Off by default: an old
    /// snapshot's turn may no longer be wanted. Name the snapshot first —
    /// `muxa restore DIR --run --continue` — or the directory is read as the
    /// text.
    #[arg(long = "continue", value_name = "TEXT", num_args = 0..=1, default_missing_value = DEFAULT_CONTINUE)]
    continue_text: Option<String>,

    /// Print the plan — and with `--run` the per-pane results — as JSON.
    #[arg(long)]
    json: bool,

    /// What a login hook runs (`muxa init --component restore-at-login`):
    /// rebuild the snapshot only when its server has no sessions — as after a
    /// reboot — starting the server if it is down, and change nothing when it
    /// already has sessions. Implies `--run`; no snapshot is not an error.
    #[arg(long, conflicts_with_all = ["only_missing", "json"])]
    boot: bool,
}

#[derive(Debug, Parser)]
#[command(args_conflicts_with_subcommands = true)]
pub struct ReloadArgs {
    #[command(subcommand)]
    target: Option<ReloadTarget>,
    #[command(flatten)]
    multiplexer: MultiplexerReloadArgs,
}

#[derive(Debug, clap::Subcommand)]
enum ReloadTarget {
    /// Refresh existing Codex MCP connections; does not restart the app-server or panes
    Mcp(crate::mcp_reload::McpReloadArgs),
    /// Snapshot, restart the multiplexer, and restore the workspace (same as bare reload)
    Mux(MultiplexerReloadArgs),
}

#[derive(Debug, clap::Args)]
struct MultiplexerReloadArgs {
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

    /// What to type into each agent the reload cut off mid-turn, once it
    /// has resumed.
    #[arg(long = "continue", value_name = "TEXT", default_value = DEFAULT_CONTINUE)]
    continue_text: String,

    /// Resume interrupted agents without telling them to carry on.
    #[arg(long)]
    no_continue: bool,
}

/// The follow-up a reload sends to agents it interrupted.
const DEFAULT_CONTINUE: &str = "continue";

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
    /// Mid-turn when the snapshot was taken. A reload cuts that turn off, so
    /// the resumed agent is told to carry on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    working: bool,
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

/// Snapshot the tracked workspace before `muxa upgrade` swaps binaries, so
/// `muxa restore` can rebuild it if the agents have to be restarted. Returns
/// the snapshot directory and a one-line summary.
pub async fn snapshot_before_upgrade(client: &Client) -> Result<(PathBuf, String)> {
    let endpoint = resolve_endpoint(client, None).await?;
    let snapshot = capture(client, &endpoint).await?;
    let dir = default_snapshot_dir()?;
    write_snapshot(&snapshot, &dir)?;
    Ok((dir, summary(&snapshot)))
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
    // The snapshot just written is the newest automatic one, so keeping at
    // least one is what keeps it: `keep_auto = 0` would otherwise delete it
    // and report an id that no longer exists.
    for entry in store::prunable(&store::entries(&root), keep.max(1)) {
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
    if args.boot {
        return restore_at_boot(args);
    }
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
    // What the server has now, window by window: a dry run shows it beside
    // the snapshot. Only a server that answered is compared; one that is
    // down has nothing running to compare with.
    let new_sessions = live
        .as_ref()
        .and_then(|_| live_shape(&endpoint))
        .map(|shape| plan::compare(&mut sessions, &shape));
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
            new_sessions: new_sessions.clone(),
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
        for line in plan::new_lines(&sessions, new_sessions.as_deref().unwrap_or_default()) {
            println!("{line}");
        }
        if args.only_missing {
            println!("\ndry run. Re-run with --run to create the missing sessions.");
        } else {
            println!(
                "\ndry run. Re-run with --run; sessions, windows and panes that already exist \
                 are kept and only what is missing is created."
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
        apply(
            &endpoint,
            &target,
            args.layout_only,
            args.continue_text.as_deref(),
            &mut report,
        )
    };
    if args.json {
        plan::attach_results(&mut sessions, &report);
        let totals = plan::totals(&sessions);
        print_json(&document(sessions, Some(totals)))?;
    }
    outcome
}

/// The session `restore --boot` holds a Windows rmux server open with until
/// the snapshot's own sessions exist.
const BOOT_PLACEHOLDER: &str = "__muxa_boot";

/// `restore --boot`. A server that already has sessions — the hook firing a
/// second time, or a user quicker than it — is left exactly as it is, and
/// having no snapshot yet is a fresh install, not a failure.
fn restore_at_boot(mut args: RestoreArgs) -> Result<()> {
    let path = match args.snapshot.take() {
        Some(path) => path,
        None => match newest_snapshot() {
            Ok(path) => path,
            Err(error) => {
                println!("nothing to restore: {error:#}");
                return Ok(());
            }
        },
    };
    let snapshot = read_snapshot(&path)?;
    let socket = args
        .mux_socket
        .clone()
        .unwrap_or_else(|| snapshot.socket.clone());
    let endpoint = control_endpoint(&endpoint_for(&snapshot.host, socket)?);
    if live_sessions(&endpoint).is_some_and(|sessions| !sessions.is_empty()) {
        println!("{} already has sessions; nothing restored", endpoint.socket);
        return Ok(());
    }
    let placeholder = start_boot_server(&endpoint)?;
    args.snapshot = Some(path);
    args.run = true;
    args.boot = false;
    let restored = restore(args);
    if placeholder {
        // Its own `=name` target: an unanchored one could match a
        // restored session whose name merely begins the same way.
        let _ = muxa::RmuxBackend::new().capture_control(&[
            "kill-session",
            "-t",
            &format!("={BOOT_PLACEHOLDER}"),
        ]);
    }
    restored
}

/// Bring up the server a boot restore goes into, when naming it is not
/// enough. A tmux or Unix rmux socket path starts its server on the
/// restore's first `new-session`. A Windows rmux pipe ends in a nonce the
/// server draws anew at every start, so the recorded pipe can no longer
/// start anything: start the default server, held open by a placeholder
/// session, and confirm it is the one the snapshot was taken on — the
/// recorded pipe then resolves to it. `true` when a placeholder was made.
///
/// The server is started outside this process's job: a login task's job
/// forbids the breakaway rmux needs, and rmux will not start a daemon
/// that would die with the task.
#[cfg(windows)]
fn start_boot_server(endpoint: &BackendEndpoint) -> Result<bool> {
    if endpoint.host != HostKind::Rmux {
        return Ok(false);
    }
    let default = muxa::RmuxBackend::new();
    let mut start = default.command(None);
    start.args(["new-session", "-d", "-s", BOOT_PLACEHOLDER]);
    crate::windows_detach::spawn(&start).context("starting the default rmux server")?;
    let placeholder = format!("={BOOT_PLACEHOLDER}");
    let started = Instant::now();
    while default
        .capture_control(&["has-session", "-t", &placeholder])
        .is_err()
    {
        if started.elapsed() > Duration::from_secs(20) {
            bail!("the default rmux server did not start within 20s");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    if live_sessions(endpoint).is_none() {
        let _ = default.capture_control(&["kill-session", "-t", &placeholder]);
        bail!(
            "the snapshot was taken on an rmux server other than the default one ({}); \
             start that server and run `muxa restore --run`",
            endpoint.socket
        );
    }
    Ok(true)
}

#[cfg(not(windows))]
#[allow(clippy::unnecessary_wraps)] // same signature as the Windows arm
fn start_boot_server(_endpoint: &BackendEndpoint) -> Result<bool> {
    Ok(false)
}

/// The sessions the server has right now; `None` when it does not answer,
/// which for a restore means every recorded session is missing.
fn live_sessions(endpoint: &BackendEndpoint) -> Option<BTreeSet<String>> {
    mux_control::capture(endpoint, &["list-sessions", "-F", "#{session_name}"])
        .ok()
        .map(|listing| listing.lines().map(str::to_owned).collect())
}

/// Every session, window and pane the server has right now; `None` when it
/// does not answer.
fn live_shape(endpoint: &BackendEndpoint) -> Option<plan::LiveShape> {
    mux_control::capture(
        endpoint,
        &["list-panes", "-a", "-F", plan::LIVE_PANE_FORMAT],
    )
    .ok()
    .map(|listing| plan::parse_live_shape(&listing))
}

pub async fn reload(client: &Client, args: ReloadArgs) -> Result<()> {
    match args.target {
        Some(ReloadTarget::Mcp(args)) => crate::mcp_reload::run(args).await,
        Some(ReloadTarget::Mux(args)) => reload_multiplexer(client, args).await,
        None => reload_multiplexer(client, args.multiplexer).await,
    }
}

async fn reload_multiplexer(client: &Client, args: MultiplexerReloadArgs) -> Result<()> {
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

    // Only a socket path survives the restart. An explicit --mux-socket names
    // the server, as it does for `restore`; otherwise the snapshot does, and
    // one from before paths were recorded still carries a short name, so it
    // is pinned while the server is up to be asked.
    let endpoint = if args.mux_socket.is_some() {
        pinned(&endpoint)
    } else {
        pinned(&endpoint_for(&snapshot.host, snapshot.socket.clone())?)
    };
    if !Path::new(&endpoint.socket).is_absolute() {
        bail!(
            "{} is not a socket path, and the server it names cannot be asked for one; \
             it could not be brought back after a restart. Pass --mux-socket with the path.",
            endpoint.socket
        );
    }
    let continuum = continuum_restores(&endpoint);

    if !args.yes && !confirm(&endpoint) {
        println!("aborted; nothing was touched.");
        return Ok(());
    }

    println!("stopping {}", endpoint.socket);
    let _ = mux_control::run(&endpoint, &["kill-server"]);
    wait_for_server_to_stop(&endpoint)?;

    if continuum {
        // tmux-continuum rebuilds its own last save the moment the server
        // starts. Racing it means every window gets its panes twice, so let
        // it finish and then fill in only what it left out.
        println!("tmux-continuum restores on start; waiting for it to settle");
        let _ = mux_control::run(&endpoint, &["start-server"]);
        wait_for_sessions_to_settle(&endpoint);
    }

    let follow_up = (!args.no_continue && !args.continue_text.trim().is_empty())
        .then_some(args.continue_text.as_str());
    apply(
        &endpoint,
        &snapshot,
        args.layout_only,
        follow_up,
        &mut RestoreReport::new(true),
    )?;
    println!(
        "\nagent conversations resume only where a provider id existed; \
         `muxa registry prune` clears the rows whose panes are gone."
    );
    Ok(())
}

/// Read the whole workspace shape off the host, plus what each pane runs.
async fn capture(client: &Client, endpoint: &BackendEndpoint) -> Result<Snapshot> {
    let panes = mux_control::capture(
        endpoint,
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
        endpoint,
        &[
            "list-windows",
            "-a",
            "-F",
            "#{session_name}\t#{window_index}\t#{window_name}\t#{window_active}\t#{window_id}\t#{window_layout}",
        ],
    )
    .map_err(anyhow::Error::msg)
    .context("listing windows")?;

    let socket = pinned(endpoint).socket;

    let agents = client
        .snapshot()
        .await
        .map(|agents| agents_by_pane(&agents, &socket))
        .unwrap_or_default();

    let (windows, keep) = fold_window_groups(&windows);
    #[cfg(not(target_os = "linux"))]
    let processes = process::ProcessTable::read();
    #[cfg(not(target_os = "linux"))]
    let child_command = |pid: u32| processes.child_command(pid);
    let mut shapes = Vec::new();
    let mut pane_pids = Vec::new();
    for line in panes.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let [session, window_index, window_id, pane_index, pane_id, shell_pid, path] = f[..] else {
            continue;
        };
        if keep.get(window_id).map(String::as_str) != Some(session) {
            continue;
        }
        let process: Option<u32> = shell_pid.parse().ok();
        let (command, argv) = match process.and_then(child_command) {
            Some(child) => (Some(child.command), child.argv),
            None => (None, None),
        };
        let recorded = agents.get(&namespaced(endpoint.host, pane_id)).cloned();
        shapes.push(PaneShape {
            session: session.to_owned(),
            window_index: window_index.to_owned(),
            pane_index: pane_index.to_owned(),
            path: path.to_owned(),
            replayable: is_replayable_command(command.as_deref(), argv.as_deref()),
            agent: agent::reconcile(recorded, argv.as_deref()),
            command,
            argv,
        });
        pane_pids.push(process);
    }
    #[cfg(not(target_os = "linux"))]
    recover_codex_threads(&mut shapes, &pane_pids, &processes);
    #[cfg(target_os = "linux")]
    let _ = pane_pids;

    Ok(Snapshot {
        version: SNAPSHOT_VERSION,
        origin: SnapshotOrigin::Manual,
        taken_at: time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap_or_default(),
        host: endpoint.host.to_string(),
        socket,
        windows,
        panes: shapes,
    })
}

/// The same server, named by its socket's absolute path. The short name muxad
/// records on a pane (`default`) resolves only against a live server, and a
/// snapshot has to outlive one — so the path is read while the server is
/// still there to ask. A server that cannot be asked keeps its name.
fn pinned(endpoint: &BackendEndpoint) -> BackendEndpoint {
    if endpoint.host != HostKind::Tmux || Path::new(&endpoint.socket).is_absolute() {
        return endpoint.clone();
    }
    let socket = mux_control::capture(endpoint, &["display-message", "-p", "#{socket_path}"])
        .ok()
        .map(|path| path.trim().to_owned())
        .filter(|path| Path::new(path).is_absolute())
        .unwrap_or_else(|| endpoint.socket.clone());
    BackendEndpoint {
        host: endpoint.host,
        socket,
    }
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

/// The endpoint `restore` sends control commands to. Snapshots record the
/// socket's absolute path, which is used as given. One taken before paths
/// were recorded carries a short name (`default`), which resolves only
/// against a live server — so after a reboot it is pinned to the path tmux
/// itself would create for that name rather than to `/dev/null`. rmux
/// endpoints are already paths.
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

/// The agents on *this* server, by pane id. Every tmux server numbers its
/// panes from `%0`, so an id alone would attach the default server's agent
/// to a pane of the same number elsewhere — and resume its conversation
/// there. An agent without a recorded server is taken to be on the default
/// one, which is what muxad assumes when it records nothing.
/// Give each Codex pane muxa holds no thread id for the one thread begun in
/// its directory after its agent started. Ambiguous panes stay unknown, and
/// are left for a person to resume rather than relaunched as a new
/// conversation.
#[cfg(not(target_os = "linux"))]
fn recover_codex_threads(
    shapes: &mut [PaneShape],
    pane_pids: &[Option<u32>],
    processes: &process::ProcessTable,
) {
    let mut orphans = BTreeMap::new();
    for (at, (shape, pane_pid)) in shapes.iter().zip(pane_pids).enumerate() {
        let Some(agent) = &shape.agent else { continue };
        if agent.kind != "codex" || agent::resumable(&agent.session_id) {
            continue;
        }
        let Some(started) = pane_pid.and_then(|pid| processes.agent_started(pid, "codex")) else {
            continue;
        };
        orphans.insert(
            at,
            agent::Orphan {
                cwd: PathBuf::from(&shape.path),
                started: i64::try_from(started).unwrap_or(i64::MAX),
            },
        );
    }
    let (Some(earliest), Some(sessions)) = (
        orphans.values().map(|orphan| orphan.started).min(),
        agent::codex_sessions_dir(),
    ) else {
        return;
    };
    let claimed: BTreeSet<String> = shapes
        .iter()
        .filter_map(|shape| shape.agent.as_ref())
        .filter(|agent| agent.kind == "codex" && agent::resumable(&agent.session_id))
        .map(|agent| agent.session_id.clone())
        .collect();
    let threads = agent::threads_since(&sessions, earliest - 60);
    for (at, id) in agent::match_orphans(&orphans, &threads, &claimed) {
        if let Some(agent) = shapes[at].agent.as_mut() {
            agent.session_id = id;
        }
    }
}

fn agents_by_pane(agents: &[muxa::Agent], socket: &str) -> HashMap<String, AgentShape> {
    agents
        .iter()
        .filter_map(|agent| {
            let pane = agent.pane.clone()?;
            let recorded = agent.tmux_socket.as_deref().unwrap_or("default");
            if !muxa::backend::pane_endpoints_match(Some(&pane), socket, recorded) {
                return None;
            }
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
                    working: agent.state == muxa::event::AgentState::Working,
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
    follow_up: Option<&str>,
    report: &mut RestoreReport,
) -> Result<()> {
    rebuild_shape(endpoint, snapshot, report)?;
    if layout_only {
        return Ok(());
    }
    let relaunched = replay_commands(endpoint, snapshot, report);
    if let Some(text) = follow_up {
        continue_interrupted(endpoint, snapshot, &relaunched, text, report);
    }
    Ok(())
}

/// The panes whose agent was mid-turn, among those that relaunched.
fn interrupted_targets(snapshot: &Snapshot, relaunched: &BTreeSet<String>) -> Vec<String> {
    snapshot
        .panes
        .iter()
        // A conversation that did not come back has no turn to continue: a
        // fresh agent told "continue" acts on nothing it knows.
        .filter(|pane| {
            pane.agent
                .as_ref()
                .is_some_and(|agent| agent.working && agent::resumable(&agent.session_id))
        })
        .map(pane_target)
        .filter(|target| relaunched.contains(target))
        .collect()
}

/// Tell every agent the reload cut off mid-turn to carry on, once its
/// resumed screen has stopped redrawing.
fn continue_interrupted(
    endpoint: &BackendEndpoint,
    snapshot: &Snapshot,
    relaunched: &BTreeSet<String>,
    text: &str,
    report: &mut RestoreReport,
) {
    let targets = interrupted_targets(snapshot, relaunched);
    if targets.is_empty() {
        return;
    }
    let mut continued = 0;
    for target in targets {
        if !screen_settled(endpoint, &target) {
            report.say(&plan::manual_line(
                &target,
                "was mid-turn; resumed screen never settled, so nothing was sent",
            ));
            continue;
        }
        let sent = mux_control::run(endpoint, &["send-keys", "-t", &target, "-l", "--", text])
            .and_then(|()| {
                std::thread::sleep(CONTINUE_SUBMIT_GRACE);
                mux_control::run(endpoint, &["send-keys", "-t", &target, "Enter"])
            });
        match sent {
            Ok(()) => continued += 1,
            Err(error) => report.say(&plan::manual_line(
                &target,
                &format!("was mid-turn; could not send {text:?}: {error}"),
            )),
        }
    }
    if continued > 0 {
        report.say(&format!(
            "sent {text:?} to {continued} agent(s) the reload interrupted mid-turn"
        ));
    }
}

/// Wait for a resumed agent's screen to hold still, so the follow-up lands in
/// its input box rather than in its startup.
fn screen_settled(endpoint: &BackendEndpoint, target: &str) -> bool {
    let started = Instant::now();
    let mut last = String::new();
    let mut quiet_since = Instant::now();
    while started.elapsed() < CONTINUE_READY_TIMEOUT {
        let screen = mux_control::capture(endpoint, &["capture-pane", "-p", "-t", target])
            .unwrap_or_default();
        if screen != last {
            last = screen;
            quiet_since = Instant::now();
        } else if !last.trim().is_empty() && quiet_since.elapsed() >= CONTINUE_READY_QUIET {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

/// Sessions, windows, panes, geometry, working directories.
fn rebuild_shape(
    endpoint: &BackendEndpoint,
    snapshot: &Snapshot,
    report: &mut RestoreReport,
) -> Result<()> {
    let mut created = 0_usize;
    let mut fresh: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for window in &snapshot.windows {
        match ensure_window(endpoint, snapshot, window) {
            Ok(count) => {
                created += count;
                if count > 0 {
                    fresh.insert(format!("{}:{}", window.session, window.index));
                }
                report.reached.insert(window.session.clone());
            }
            Err(error) => {
                report.failed = Some((window.session.clone(), format!("{error:#}")));
                return Err(error);
            }
        }
    }

    // A window that already has panes — a retried restore, or a plugin that
    // restored its own save on server start — gets only the ones it lacks.
    // Splitting unconditionally is how a restore doubles a workspace.
    let mut kept = 0_usize;
    let mut split_failures = Vec::new();
    for window in &snapshot.windows {
        let target = format!("={}:{}", window.session, window.index);
        let wanted: Vec<_> = snapshot
            .panes
            .iter()
            .filter(|pane| pane.session == window.session && pane.window_index == window.index)
            .collect();
        // A window that cannot be listed is not one with a single pane;
        // splitting on a guess is the doubling this guards against.
        let Some(have) = pane_count(endpoint, &target) else {
            report.say(&format!("  {target}: could not list its panes; left as is"));
            continue;
        };
        let have = have.max(1);
        if !fresh.contains(&format!("{}:{}", window.session, window.index)) {
            kept += have.min(wanted.len());
        }
        for pane in wanted.iter().skip(have) {
            // `split-window -d` keeps splitting the same pane, halving it
            // each time until there is no room left. Re-tiling first hands
            // the split an evenly sized pane; the recorded layout replaces
            // the tiling below.
            let _ = mux_control::run(endpoint, &["select-layout", "-t", &target, "tiled"]);
            if let Err(error) = mux_control::run(
                endpoint,
                &["split-window", "-d", "-t", &target, "-c", &pane.path],
            ) {
                report.pane_failed(pane_target(pane), format!("could not split — {error}"));
                split_failures.push((pane_target(pane), error));
            }
        }
    }

    // Geometry last, once every pane exists.
    for window in &snapshot.windows {
        let target = format!("={}:{}", window.session, window.index);
        let _ = mux_control::run(endpoint, &["select-layout", "-t", &target, &window.layout]);
        // The recorded size was only needed to lay the panes out; hand the
        // window back to the attached client's size so it neither clips nor
        // leaves space unused on a different terminal.
        if fresh.contains(&format!("{}:{}", window.session, window.index)) {
            let _ = mux_control::run(
                endpoint,
                &["set-option", "-w", "-u", "-t", &target, "window-size"],
            );
        }
    }

    // `select-layout` renumbers panes by geometry, which is not the order the
    // splits above created them in, so directories are set per *final* index
    // rather than inherited from `split-window -c`.
    for pane in &snapshot.panes {
        let target = pane_target(pane);
        if !wait_for_shell(endpoint, &target) {
            // Something is already running there; typing into it would be
            // input to that program, not a directory change.
            continue;
        }
        if let Err(error) = mux_control::run(
            endpoint,
            &[
                "send-keys",
                "-t",
                &target,
                &muxa::shell::Dialect::native().cd(&pane.path),
                "Enter",
            ],
        ) {
            report.pane_failed(target, error);
        }
    }

    report.say(&plan::rebuilt_line(created, snapshot.panes.len(), kept));
    for (target, error) in split_failures {
        report.say(&plan::split_failure_line(&target, &error));
    }
    Ok(())
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
    // Only a window made here is sized: `resize-window` pins the window's
    // `window-size` to `manual`, which a live window must never get. The
    // pin is lifted again once the layout is applied (`rebuild_shape`).
    if let Some((columns, rows)) = size.filter(|_| created > 0) {
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

/// How many panes a window has right now; `None` when the server could not
/// say, which is not the same as none.
fn pane_count(endpoint: &BackendEndpoint, window_target: &str) -> Option<usize> {
    mux_control::capture(
        endpoint,
        &["list-panes", "-t", window_target, "-F", "#{pane_id}"],
    )
    .ok()
    .map(|listing| {
        listing
            .lines()
            .filter(|line| !line.trim().is_empty())
            .count()
    })
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
/// Returns the panes whose program came back up.
fn replay_commands(
    endpoint: &BackendEndpoint,
    snapshot: &Snapshot,
    report: &mut RestoreReport,
) -> BTreeSet<String> {
    let mut sent = Vec::new();
    let mut manual = Vec::new();
    for pane in &snapshot.panes {
        let target = pane_target(pane);
        let Some(command) = relaunch_command(pane) else {
            if let Some(hint) = resume_hint(pane) {
                manual.push((target, hint));
            }
            continue;
        };
        if !pane.replayable {
            manual.push((
                target,
                "argv was a process title, not a command — start it by hand".to_owned(),
            ));
            continue;
        }
        if !wait_for_shell(endpoint, &target) {
            manual.push((target, "already running something; left alone".to_owned()));
            continue;
        }
        match mux_control::run(endpoint, &["send-keys", "-t", &target, &command, "Enter"]) {
            Ok(()) => sent.push(target),
            Err(error) => report.pane_failed(target, error),
        }
    }
    // Keys sent are not a program started: only a pane whose foreground
    // moved off its shell counts as relaunched.
    let unconfirmed = confirm_launches(endpoint, sent.clone());
    let replayed = sent.len() - unconfirmed.len();
    report.say(&plan::relaunched_line(replayed));
    let relaunched = sent
        .into_iter()
        .filter(|target| !unconfirmed.contains(target))
        .collect();
    for target in unconfirmed {
        report.say(&plan::manual_line(&target, launch::UNCONFIRMED_NOTE));
        report.pane_unconfirmed(target, launch::UNCONFIRMED_NOTE.to_owned());
    }
    for (target, note) in manual {
        report.say(&plan::manual_line(&target, &note));
        report.pane_left(target, note);
    }
    relaunched
}

/// What to tell a human about an agent pane whose command line was not
/// captured: the conversation is still resumable, just not automatically,
/// because the wrapper it was launched through is unknown.
fn resume_hint(pane: &PaneShape) -> Option<String> {
    let agent = pane.agent.as_ref()?;
    if !agent::resumable(&agent.session_id) {
        if !starts_with_instruction(pane) {
            return None;
        }
        let picker = match agent.kind.as_str() {
            "claude_code" => "claude --resume",
            "codex" => "codex resume",
            _ => return None,
        };
        return Some(format!(
            "started with an instruction, and its conversation id is unknown; \
             relaunching would run the instruction again as a new task. Pick the \
             conversation with `{picker}` (through the same wrapper)"
        ));
    }
    let resume = match agent.kind.as_str() {
        "claude_code" => format!("claude --resume {}", agent.session_id),
        "codex" => format!("codex resume {}", agent.session_id),
        _ => return None,
    };
    Some(format!(
        "command line was not captured; resume by hand with `{resume}` \
         (through the same wrapper it was started with)"
    ))
}

/// What to type back into a pane: its own command line, with the provider's
/// conversation id spliced in when muxa knows one and the command does not
/// already carry it.
fn relaunch_command(pane: &PaneShape) -> Option<String> {
    if starts_with_instruction(pane)
        && pane
            .agent
            .as_ref()
            .is_some_and(|agent| !agent::resumable(&agent.session_id))
    {
        return None;
    }
    let resuming = pane.agent.as_ref().and_then(|agent| {
        (!agent.session_id.is_empty() && !agent.session_id.starts_with("synthetic"))
            .then_some(agent.kind.as_str())
    });
    let command = match pane.argv.as_deref() {
        Some(argv) if !argv.is_empty() => {
            let argv = match resuming {
                Some(kind) => without_initial_prompt(argv, kind),
                None => argv.to_vec(),
            };
            muxa::shell::Dialect::native().command_line(&argv)
        }
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

/// An agent pane whose command carries an initial instruction: dropping the
/// prompt changes its argv.
fn starts_with_instruction(pane: &PaneShape) -> bool {
    let (Some(agent), Some(argv)) = (pane.agent.as_ref(), pane.argv.as_deref()) else {
        return false;
    };
    matches!(agent.kind.as_str(), "claude_code" | "codex")
        && without_initial_prompt(argv, &agent.kind).len() != argv.len()
}

/// An agent launched with its first instruction (`claude … 'Read ISSUE.md
/// and …'`) would, resumed with that argv, send the instruction again and
/// restart the task. Drop the prompt argument; keep every flag and the
/// values of the flags known to take one.
///
/// A prompt is a positional argument with whitespace in it, after the
/// agent's own arguments begin (past a wrapper's `--`, or the `claude` /
/// `codex` program, or an `aas` `name@claude` target). A one-word value of an
/// unknown flag is therefore never mistaken for one.
fn without_initial_prompt(argv: &[String], kind: &str) -> Vec<String> {
    let value_flags: &[&str] = match kind {
        "claude_code" => &[
            "--model",
            "--permission-mode",
            "--add-dir",
            "--append-system-prompt",
            "--system-prompt",
            "--allowedTools",
            "--allowed-tools",
            "--disallowedTools",
            "--disallowed-tools",
            "--mcp-config",
            "--settings",
            "--session-id",
            "--resume",
            "-r",
            "--output-format",
            "--input-format",
            "--fallback-model",
            "--agents",
            "--agent",
            "--setting-sources",
            "--plugin-dir",
            "--max-turns",
        ],
        "codex" => &[
            "-m",
            "--model",
            "-c",
            "--config",
            "-p",
            "--profile",
            "-s",
            "--sandbox",
            "-a",
            "--ask-for-approval",
            "-C",
            "--cd",
            "-i",
            "--image",
            "--enable",
            "--disable",
            "--add-dir",
        ],
        _ => return argv.to_vec(),
    };
    let program = |argument: &str| {
        let name = muxa::shell::program_name(argument);
        matches!(name, "claude" | "codex") || name.ends_with("@claude") || name.ends_with("@codex")
    };
    let start = argv
        .iter()
        .rposition(|argument| argument == "--" || program(argument))
        // Unrecognised, argv[0] is still the program — never a prompt, even
        // when its path has a space in it (`C:\Program Files\…`).
        .map_or(usize::from(!argv.is_empty()), |at| at + 1);
    let mut kept = argv[..start].to_vec();
    let mut takes_value = false;
    for argument in &argv[start..] {
        if takes_value {
            takes_value = false;
            kept.push(argument.clone());
            continue;
        }
        if argument.starts_with('-') {
            takes_value = !argument.contains('=') && value_flags.contains(&argument.as_str());
            kept.push(argument.clone());
            continue;
        }
        if argument.chars().any(char::is_whitespace) {
            continue;
        }
        kept.push(argument.clone());
    }
    kept
}

/// False for argv a process rewrote into a status line. Puma and friends
/// replace their own argv with something like
/// `puma 5.6.8 (tcp://0.0.0.0:5072) [admin]`, which reads like a command and
/// is not one; the original invocation is simply not in the process table.
fn is_replayable(command: &str) -> bool {
    !command.is_empty() && !command.contains(['(', ')', '[', ']'])
}

/// [`is_replayable`], judged from the argv when it was captured. A rewritten
/// title lands in the program argument (`argv[0]`, the rest blank), while a
/// real command keeps its arguments apart — so an agent launched with an
/// instruction like `"… (commit, push) …"` is still a command, and is
/// relaunched rather than silently left out of the restore.
fn is_replayable_command(command: Option<&str>, argv: Option<&[String]>) -> bool {
    match argv {
        Some([program, rest @ ..]) if !program.is_empty() => {
            let title_like = program.contains(['(', ')', '[', ']']);
            let rewritten =
                program.contains(char::is_whitespace) && rest.iter().all(String::is_empty);
            !title_like && !rewritten
        }
        _ => command.is_some_and(is_replayable),
    }
}

fn pane_target(pane: &PaneShape) -> String {
    format!(
        "={}:{}.{}",
        pane.session, pane.window_index, pane.pane_index
    )
}

/// A pane created a moment ago is still starting its shell, and keys sent
/// before its line editor is up can be swallowed by that startup. Poll for
/// the shell instead of sleeping a guessed interval. A pane that keeps
/// reporting some other program is not starting, it is busy — a shell's own
/// startup may run a child for a moment, so it gets a short grace, not the
/// full wait — and the answer is `false`: nothing should be typed into it.
/// See [`launch`] for what counts as ready.
fn wait_for_shell(endpoint: &BackendEndpoint, target: &str) -> bool {
    let started = Instant::now();
    let mut watch = launch::ShellWatch::default();
    let tty = pane_tty(endpoint, target);
    while started.elapsed() < SHELL_READY_TIMEOUT {
        let command = foreground(endpoint, target);
        let shell = command.as_deref().is_some_and(|c| is_shell(c.trim()));
        let prompt_drawn = shell && prompt_drawn(endpoint, target);
        let line_editing = if prompt_drawn {
            tty.as_deref().and_then(terminal_line_editing)
        } else {
            None
        };
        let probe = launch::PaneProbe {
            command: command.as_deref(),
            prompt_drawn,
            line_editing,
        };
        match watch.observe(started.elapsed(), probe) {
            launch::Readiness::Ready => return true,
            launch::Readiness::Busy => return false,
            launch::Readiness::Wait => {}
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    watch.at_deadline()
}

/// `#{pane_current_command}`, or `None` when the server did not answer.
///
/// A Windows console has no foreground process group, so rmux there reports
/// the pane's shell for as long as it lives, whatever runs inside it. A
/// program the shell started is read from the process table instead.
fn foreground(endpoint: &BackendEndpoint, target: &str) -> Option<String> {
    let reply = mux_control::capture(
        endpoint,
        &[
            "display-message",
            "-p",
            "-t",
            target,
            "#{pane_current_command}\t#{pane_pid}",
        ],
    )
    .ok()?;
    let reply = reply.trim_end_matches(['\r', '\n']);
    let (command, pane_pid) = reply.split_once('\t').unwrap_or((reply, ""));
    #[cfg(not(windows))]
    let _ = pane_pid;
    #[cfg(windows)]
    if is_shell(command.trim()) {
        if let Ok(pane_pid) = pane_pid.trim().parse::<u32>() {
            if let Some(child) = process::ProcessTable::read().child_command(pane_pid) {
                let program = child
                    .argv
                    .as_ref()
                    .and_then(|argv| argv.first())
                    .map_or(child.command.as_str(), String::as_str);
                return Some(muxa::shell::program_name(program).to_owned());
            }
        }
    }
    Some(command.to_owned())
}

/// The pane's terminal device, for reading its mode.
fn pane_tty(endpoint: &BackendEndpoint, target: &str) -> Option<String> {
    mux_control::capture(
        endpoint,
        &["display-message", "-p", "-t", target, "#{pane_tty}"],
    )
    .ok()
    .map(|tty| tty.trim().to_owned())
    .filter(|tty| tty.starts_with("/dev/"))
}

/// Whether the terminal at `tty` has a line editor reading it (non-canonical
/// mode). Reading the mode opens the device but changes nothing on it.
fn terminal_line_editing(tty: &str) -> Option<bool> {
    let flag = if cfg!(target_os = "linux") {
        "-F"
    } else {
        "-f"
    };
    let output = std::process::Command::new("stty")
        .args([flag, tty, "-a"])
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    launch::line_editing(&String::from_utf8_lossy(&output.stdout))
}

/// Watch relaunched panes until each shows its program, and return the ones
/// that never did. Every pane shares one deadline, so a workspace of many
/// panes waits once, not once per pane.
fn confirm_launches(endpoint: &BackendEndpoint, targets: Vec<String>) -> BTreeSet<String> {
    let started = Instant::now();
    let mut watch = launch::LaunchWatch::new(targets);
    while !watch.is_settled() && started.elapsed() < launch::CONFIRM_TIMEOUT {
        let pending: Vec<String> = watch.pending().cloned().collect();
        for target in pending {
            let command = foreground(endpoint, &target);
            watch.observe(&target, command.as_deref());
        }
        if !watch.is_settled() {
            std::thread::sleep(Duration::from_millis(250));
        }
    }
    watch.unconfirmed()
}

/// Whether anything is on the pane's screen — a prompt, for a shell.
fn prompt_drawn(endpoint: &BackendEndpoint, target: &str) -> bool {
    mux_control::capture(endpoint, &["capture-pane", "-p", "-t", target])
        .is_ok_and(|screen| !screen.trim().is_empty())
}

/// Whether tmux-continuum will rebuild its own save when this server starts.
fn continuum_restores(endpoint: &BackendEndpoint) -> bool {
    mux_control::capture(endpoint, &["show-options", "-gqv", "@continuum-restore"])
        .is_ok_and(|value| value.trim() == "on")
}

/// Wait until the pane listing stops changing: a plugin restoring on server
/// start works asynchronously, and nothing else says when it is done.
fn wait_for_sessions_to_settle(endpoint: &BackendEndpoint) {
    const QUIET: Duration = Duration::from_secs(3);
    /// The restore is launched from the config as the server starts and
    /// takes a moment to make its first session; quiet before then means
    /// nothing.
    const HEAD_START: Duration = Duration::from_secs(5);
    let started = Instant::now();
    let deadline = started + Duration::from_secs(60);
    let listing = || {
        mux_control::capture(
            endpoint,
            &[
                "list-panes",
                "-a",
                "-F",
                "#{session_name}:#{window_index}.#{pane_index}",
            ],
        )
        .unwrap_or_default()
    };
    let mut last = listing();
    let mut quiet_since = Instant::now();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        let now = listing();
        if now != last {
            last = now;
            quiet_since = Instant::now();
        } else if quiet_since.elapsed() >= QUIET && started.elapsed() >= HEAD_START {
            return;
        }
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
        // A socket that is plainly a tmux server wins over the shell's host:
        // tmux inside a cmux terminal is common, and cmux's variables would
        // otherwise send an explicit tmux socket to cmux.
        let host = host_from_socket_path(socket)
            .or_else(|| {
                (!socket.contains('/'))
                    .then(|| muxa::tmux::resolve_socket_path(socket))
                    .flatten()
                    .map(|_| HostKind::Tmux)
            })
            .or_else(muxa::backend::detect_host_env)
            .unwrap_or(HostKind::Tmux);
        return endpoint_for(&host.to_string(), socket.to_owned());
    }
    let sockets = tracked_servers(client).await?;
    match sockets.len() {
        1 => {
            let (socket, host) = sockets.into_iter().next().expect("one entry");
            endpoint_for(&host, socket)
        }
        // No tracked agents — common before any agent hook is installed —
        // leaves the server this shell runs in as the only honest guess.
        0 => match shell_server(|name| std::env::var(name).ok()) {
            Some((socket, host)) => endpoint_for(&host.to_string(), socket),
            None => bail!(
                "muxad tracks no tmux/rmux agents and this shell is not inside one, \
                 so there is nothing to work out the target server from; pass --mux-socket"
            ),
        },
        _ => bail!(
            "agents span several servers ({}); pass --mux-socket to name one",
            sockets.keys().cloned().collect::<Vec<_>>().join(", ")
        ),
    }
}

/// The server the calling shell is inside, read from the variable its host
/// exports (`$RMUX` for rmux, `$TMUX` for tmux): `<socket>,<pid>,<session>`.
/// `read` stands in for `std::env::var` so tests need not mutate the
/// process environment.
fn shell_server(read: impl Fn(&str) -> Option<String>) -> Option<(String, HostKind)> {
    let host = muxa::backend::detect_host_env_with(&read)?;
    let var = match host {
        HostKind::Rmux => "RMUX",
        HostKind::Tmux => "TMUX",
        _ => return None,
    };
    let raw = read(var)?;
    let socket = raw.split(',').next().filter(|path| !path.is_empty())?;
    Some((socket.to_owned(), host))
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
            // An agent last heard from before an rmux restart still names
            // the old Windows pipe; it is the same server as the live one.
            let socket = if host == HostKind::Rmux {
                muxa::backend::rmux::live_endpoint(socket)
            } else {
                socket.clone()
            };
            sockets.insert(socket, host.to_string());
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
    if muxa::backend::rmux::is_endpoint(socket) {
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
    let mut snapshot: Snapshot =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    // Re-judge with the current rule, so snapshots taken before it still
    // relaunch an argv-captured command that merely contains parentheses.
    for pane in &mut snapshot.panes {
        if pane.argv.is_some() {
            pane.replayable = is_replayable_command(pane.command.as_deref(), pane.argv.as_deref());
        }
    }
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
///
/// A pane started *with* a command (`new-window 'aas exec … -- --yolo'`,
/// as `muxa agent start` does) has no shell: its own process is the command.
/// Taking its child there records what the wrapper launched (`node …/codex`)
/// and drops the wrapper — and with it the account profile a resume needs.
#[cfg(target_os = "linux")]
fn child_command(pane_pid: u32) -> Option<ChildCommand> {
    if let Some(own) = std::fs::read(format!("/proc/{pane_pid}/cmdline"))
        .ok()
        .map(|raw| split_cmdline(&raw))
        .filter(|argv| !argv.is_empty())
    {
        let command = own.join(" ").trim().to_owned();
        if !command.is_empty() && !is_shell(&command) {
            return Some(ChildCommand {
                command,
                argv: Some(own),
            });
        }
    }
    let children =
        std::fs::read_to_string(format!("/proc/{pane_pid}/task/{pane_pid}/children")).ok()?;
    children.split_whitespace().find_map(|child| {
        let raw = std::fs::read(format!("/proc/{child}/cmdline")).ok()?;
        let argv = split_cmdline(&raw);
        let command = argv.join(" ").trim().to_owned();
        (!command.is_empty() && !is_shell(&command)).then_some(ChildCommand {
            command,
            argv: Some(argv),
        })
    })
}

/// `/proc/<pid>/cmdline` is NUL-terminated arguments. Only the final NUL is
/// a terminator; an empty string between two NULs is a real argument
/// (`--title ""`) and must survive, or the replay shifts every argument
/// after it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn split_cmdline(raw: &[u8]) -> Vec<String> {
    let raw = raw.strip_suffix(b"\0").unwrap_or(raw);
    if raw.is_empty() {
        return Vec::new();
    }
    String::from_utf8_lossy(raw)
        .split('\0')
        .map(str::to_owned)
        .collect()
}

/// Whether `command` is not what a pane runs: the shell itself, or the
/// shell-rc autostart's `muxa daemon start` caught while a new shell reads
/// its profile. Replaying either would only type a stray command into the
/// restored shell.
fn is_shell(command: &str) -> bool {
    muxa::shell::is_shell_program(command) || is_daemon_autostart(command)
}

fn is_daemon_autostart(command: &str) -> bool {
    let line = command.trim();
    let program = muxa::shell::program_token(line);
    if muxa::shell::program_name(program) != "muxa" {
        return false;
    }
    let rest = line
        .split_once(program)
        .map_or("", |(_, rest)| rest.trim_start_matches('"'));
    rest.split_whitespace().next() == Some("daemon")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A profile's `muxa daemon start`, caught mid-flight, is no pane's
    /// program; `muxa watch` in a pane is one, and restore brings it back.
    #[test]
    fn the_daemon_autostart_is_not_what_a_pane_runs() {
        for line in [
            "muxa daemon start --quiet",
            "/home/u/.cargo/bin/muxa daemon start",
            r"C:\Users\u\.cargo\bin\muxa.exe daemon start --quiet",
            r#""C:\Program Files\muxa\muxa.exe" daemon start"#,
        ] {
            assert!(is_shell(line), "{line}");
        }
        for line in ["muxa watch", "muxad", "vim daemon", "claude --resume x"] {
            assert!(!is_shell(line), "{line}");
        }
    }

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
    fn the_shells_own_server_is_the_fallback_target() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| (*value).to_owned())
            }
        };
        // rmux exports both variables; its own wins.
        assert_eq!(
            shell_server(env(&[
                ("RMUX", r"\\.\pipe\rmux-a,23380,3"),
                ("TMUX", r"\\.\pipe\rmux-a,23380,3"),
            ])),
            Some((r"\\.\pipe\rmux-a".to_owned(), HostKind::Rmux))
        );
        assert_eq!(
            shell_server(env(&[("TMUX", "/tmp/tmux-1000/default,42,0")])),
            Some(("/tmp/tmux-1000/default".to_owned(), HostKind::Tmux))
        );
        assert_eq!(shell_server(env(&[])), None);
        assert_eq!(shell_server(env(&[("ZELLIJ", "0")])), None);
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

    /// The pane a real reload skipped: an agent whose first instruction
    /// contained "(commit, push, PR creation and Linear updates)".
    #[test]
    fn parentheses_in_an_argument_do_not_make_a_command_a_title() {
        let argv = |args: &[&str]| args.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>();
        let agent = argv(&[
            "/home/june/.local/bin/aas",
            "exec",
            "e-ed@claude",
            "--",
            "--dangerously-skip-permissions",
            "Stop before the FINALIZE steps (commit, push, PR creation) [then review]",
        ]);
        let joined = agent.join(" ");
        assert!(is_replayable_command(Some(&joined), Some(&agent)));
        // A rewritten title is one argument, the rest blank.
        let title = argv(&["puma 5.6.8 (tcp://0.0.0.0:5072) [admin]", "", ""]);
        assert!(!is_replayable_command(Some(&title[0]), Some(&title)));
        // Without argv (macOS), the joined line keeps the old rule.
        assert!(!is_replayable_command(Some(&joined), None));
    }

    fn snapshot_of(panes: Vec<PaneShape>) -> Snapshot {
        Snapshot {
            version: SNAPSHOT_VERSION,
            taken_at: String::new(),
            origin: SnapshotOrigin::Reload,
            host: "tmux".into(),
            socket: "/tmp/tmux-501/default".into(),
            windows: Vec::new(),
            panes,
        }
    }

    #[test]
    fn only_relaunched_agents_that_were_mid_turn_are_told_to_continue() {
        let agent = |working| {
            Some(AgentShape {
                kind: "claude_code".into(),
                session_id: "sid".into(),
                working,
            })
        };
        let mut busy = pane("work", "0", "0", Some("claude"));
        busy.agent = agent(true);
        let mut idle = pane("work", "0", "1", Some("claude"));
        idle.agent = agent(false);
        let mut busy_but_failed = pane("work", "1", "0", Some("claude"));
        busy_but_failed.agent = agent(true);
        let shell = pane("work", "1", "1", None);
        let snapshot = snapshot_of(vec![busy, idle, busy_but_failed, shell]);
        let relaunched: BTreeSet<String> = ["=work:0.0", "=work:0.1"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        assert_eq!(
            interrupted_targets(&snapshot, &relaunched),
            vec!["=work:0.0".to_owned()]
        );
    }

    /// The follow-up against a private tmux server: a pane running `cat`
    /// echoes what was typed once Enter commits it.
    #[test]
    #[ignore = "drives a private tmux server; run with --ignored"]
    fn continue_types_the_follow_up_into_a_live_pane() {
        let dir = std::env::temp_dir().join(format!("mrc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s").display().to_string();
        std::process::Command::new("tmux")
            .args([
                "-S",
                &socket,
                "-f",
                "/dev/null",
                "new-session",
                "-d",
                "-s",
                "work",
            ])
            .args(["-x", "120", "-y", "30", "cat"])
            .status()
            .unwrap();
        let endpoint = endpoint_for("tmux", socket.clone()).unwrap();
        let mut agent = pane("work", "0", "0", Some("cat"));
        agent.agent = Some(AgentShape {
            kind: "claude_code".into(),
            session_id: "sid".into(),
            working: true,
        });
        let snapshot = snapshot_of(vec![agent]);
        let target = pane_target(&snapshot.panes[0]);
        // A first line so the screen is not blank.
        mux_control::run(
            &endpoint,
            &["send-keys", "-t", &target, "-l", "--", "ready"],
        )
        .unwrap();
        let relaunched = BTreeSet::from([target.clone()]);
        let mut report = RestoreReport::new(false);
        continue_interrupted(&endpoint, &snapshot, &relaunched, "carry on", &mut report);
        std::thread::sleep(Duration::from_millis(300));
        let screen = mux_control::capture(&endpoint, &["capture-pane", "-p", "-t", &target]);
        let _ = mux_control::run(&endpoint, &["kill-server"]);
        let _ = std::fs::remove_dir_all(&dir);
        let screen = screen.unwrap();
        assert!(
            screen.contains("readycarry on\nreadycarry on"),
            "{screen:?}"
        );
    }

    #[test]
    fn a_snapshot_from_before_the_working_flag_reads_as_idle() {
        let shape: AgentShape =
            serde_json::from_str(r#"{"kind":"codex","session_id":"sid"}"#).unwrap();
        assert!(!shape.working);
        let written = serde_json::to_string(&shape).unwrap();
        assert!(!written.contains("working"), "{written}");
    }

    #[test]
    fn reload_continues_by_default_and_restore_only_when_asked() {
        #[derive(Parser)]
        struct Reload {
            #[command(flatten)]
            args: MultiplexerReloadArgs,
        }
        let parse = |argv: &[&str]| {
            Reload::parse_from(std::iter::once("reload").chain(argv.iter().copied())).args
        };
        let args = parse(&[]);
        assert_eq!(args.continue_text, DEFAULT_CONTINUE);
        assert!(!args.no_continue);
        assert_eq!(
            parse(&["--continue", "keep going"]).continue_text,
            "keep going"
        );
        assert!(parse(&["--no-continue"]).no_continue);

        let restore = |argv: &[&str]| {
            RestoreArgs::parse_from(std::iter::once("restore").chain(argv.iter().copied()))
                .continue_text
        };
        assert_eq!(restore(&[]), None);
        assert_eq!(restore(&["--continue"]).as_deref(), Some(DEFAULT_CONTINUE));
        assert_eq!(restore(&["--continue", "go on"]).as_deref(), Some("go on"));
    }

    #[test]
    fn a_tracked_claude_pane_comes_back_on_its_own_conversation() {
        let mut claude = pane("work", "0", "0", Some("aas e june@claude"));
        claude.agent = Some(AgentShape {
            kind: "claude_code".into(),
            session_id: "abc-123".into(),
            working: false,
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
            working: false,
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
            working: false,
        });
        assert_eq!(relaunch_command(&codex).as_deref(), Some("codex --yolo"));
    }

    #[test]
    fn an_instruction_launch_with_no_known_conversation_is_left_for_a_person() {
        let line = ["codex", "--yolo", "Review the qqg kitchen deck"];
        let mut codex = pane("work", "0", "0", Some(&line.join(" ")));
        codex.argv = Some(line.iter().map(|word| (*word).to_owned()).collect());
        codex.replayable = true;
        codex.agent = Some(AgentShape {
            kind: "codex".into(),
            session_id: "synthetic-7:default:%74".into(),
            working: false,
        });
        assert_eq!(relaunch_command(&codex), None, "would start the task over");
        let hint = resume_hint(&codex).unwrap();
        assert!(hint.contains("codex resume"), "{hint}");

        codex.agent.as_mut().unwrap().session_id = "01a13353-ba8c-70f3-a502-2a33f3521878".into();
        assert_eq!(
            relaunch_command(&codex).as_deref(),
            Some("codex --yolo resume 01a13353-ba8c-70f3-a502-2a33f3521878")
        );
    }

    #[test]
    fn an_agent_that_came_back_as_a_new_conversation_is_not_told_to_continue() {
        let mut fresh = pane("work", "0", "0", Some("codex --yolo"));
        fresh.agent = Some(AgentShape {
            kind: "codex".into(),
            session_id: "synthetic-7:default:%19".into(),
            working: true,
        });
        let snapshot = snapshot_of(vec![fresh]);
        let relaunched = BTreeSet::from(["=work:0.0".to_owned()]);
        assert!(interrupted_targets(&snapshot, &relaunched).is_empty());
    }

    /// The panes found in a real snapshot: a Claude started with its task
    /// instruction, and wrapped Codex panes.
    #[test]
    fn a_resumed_agent_does_not_replay_its_first_instruction() {
        let shape = |argv: &[&str], kind: &str, session: &str| {
            let mut pane = pane("callabo", "1", "0", Some(&argv.join(" ")));
            pane.argv = Some(argv.iter().map(|a| (*a).to_owned()).collect());
            pane.agent = Some(AgentShape {
                kind: kind.into(),
                session_id: session.into(),
                working: false,
            });
            pane
        };
        let claude = shape(
            &[
                "aas",
                "exec",
                "e-ed@claude",
                "--",
                "--dangerously-skip-permissions",
                "--model",
                "opus",
                "--append-system-prompt",
                "be brief and careful",
                "You are already the implementation agent. Read ./ISSUE.md",
            ],
            "claude_code",
            "7961fc4b",
        );
        assert_eq!(
            relaunch_command(&claude).as_deref(),
            Some(
                "aas exec e-ed@claude -- --dangerously-skip-permissions --model opus \
                 --append-system-prompt 'be brief and careful' --resume 7961fc4b"
            )
        );
        let codex = shape(
            &[
                "aas",
                "exec",
                "e-ed@codex",
                "--",
                "--yolo",
                "Review the diff please",
            ],
            "codex",
            "01a12f38",
        );
        assert_eq!(
            relaunch_command(&codex).as_deref(),
            Some("aas exec e-ed@codex -- --yolo resume 01a12f38")
        );
        // Bare program, prompt last.
        let bare = shape(
            &[
                "/home/june/.local/bin/claude",
                "--dangerously-skip-permissions",
                "fix the bug",
            ],
            "claude_code",
            "s1",
        );
        assert_eq!(
            relaunch_command(&bare).as_deref(),
            Some("/home/june/.local/bin/claude --dangerously-skip-permissions --resume s1")
        );
        // Not resuming: the argv replays untouched.
        let mut fresh = bare.clone();
        fresh.agent = None;
        assert!(relaunch_command(&fresh).unwrap().contains("'fix the bug'"));
    }

    #[test]
    fn an_agent_without_a_command_line_gets_a_resume_hint_not_a_relaunch() {
        let mut claude = pane("work", "0", "0", None);
        claude.agent = Some(AgentShape {
            kind: "claude_code".into(),
            session_id: "abc-123".into(),
            working: false,
        });
        assert_eq!(relaunch_command(&claude), None);
        assert!(resume_hint(&claude)
            .unwrap()
            .contains("claude --resume abc-123"));
        let plain = pane("work", "0", "1", None);
        assert_eq!(resume_hint(&plain), None);
    }

    #[test]
    fn a_pane_id_is_looked_up_under_its_host_namespace() {
        // The registry stores `rmux:%12`; the control command reports `%12`.
        assert_eq!(namespaced(HostKind::Rmux, "%12"), "rmux:%12");
        assert_eq!(namespaced(HostKind::Rmux, "rmux:%12"), "rmux:%12");
        assert_eq!(namespaced(HostKind::Tmux, "%12"), "%12");
    }

    #[test]
    fn an_agent_on_another_server_is_not_attached_by_pane_number() {
        let agent = |socket: Option<&str>| {
            let now = time::OffsetDateTime::now_utc();
            muxa::Agent {
                kind: AgentKind::ClaudeCode,
                session_id: "abc-123".into(),
                surface: None,
                pane: Some("%1".into()),
                tmux_socket: socket.map(str::to_owned),
                tmux_session: None,
                cwd: None,
                pid: None,
                workload: muxa::WorkloadSummary::default(),
                subagents: Vec::new(),
                state: muxa::AgentState::Idle,
                last_prompt: None,
                last_prompt_at: None,
                last_response: None,
                recap: None,
                ai_title: None,
                last_notification: None,
                model: None,
                context_used_pct: None,
                cost_usd: None,
                rate_limit_5h_pct: None,
                rate_limit_5h_resets_at: None,
                rate_limit_7d_pct: None,
                rate_limit_7d_resets_at: None,
                rate_limited_until: None,
                rate_limit_scope: None,
                rate_limit_source: None,
                started_at: now,
                last_activity_at: now,
                state_entered_at: now,
            }
        };
        let on_default = [agent(Some("default")), agent(None)];
        let by_pane = agents_by_pane(&on_default, "/tmp/tmux-501/muxa-reload-test");
        assert!(by_pane.is_empty());
        let by_pane = agents_by_pane(&on_default, "/tmp/tmux-501/default");
        assert_eq!(by_pane.len(), 1);
        let by_pane = agents_by_pane(
            &[agent(Some("muxa-reload-test"))],
            "/tmp/tmux-501/muxa-reload-test",
        );
        assert_eq!(by_pane["%1"].session_id, "abc-123");
    }

    #[test]
    fn a_real_empty_argument_survives_the_cmdline_split() {
        assert_eq!(
            split_cmdline(b"app\0--title\0\0--verbose\0"),
            vec!["app", "--title", "", "--verbose"]
        );
        assert_eq!(split_cmdline(b"sleep\x00777\x00"), vec!["sleep", "777"]);
        assert!(split_cmdline(b"").is_empty());
    }
}
