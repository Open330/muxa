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
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::mux_control;

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
}

#[derive(Debug, Parser)]
pub struct RestoreArgs {
    /// Snapshot directory. Defaults to the most recent one.
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    version: u32,
    taken_at: String,
    host: String,
    socket: String,
    windows: Vec<WindowShape>,
    panes: Vec<PaneShape>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WindowShape {
    session: String,
    index: String,
    name: String,
    active: bool,
    /// The host's own layout string, replayed verbatim by `select-layout`.
    layout: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PaneShape {
    session: String,
    window_index: String,
    pane_index: String,
    path: String,
    /// What the pane was running, as its own argv. `None` for a bare shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    /// False when the process rewrote its argv into a status line, which
    /// reads like a command and is not one.
    #[serde(default)]
    replayable: bool,
    /// The agent muxa had tracked in this pane, when it knew of one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<AgentShape>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AgentShape {
    kind: String,
    /// The provider's own conversation id, which is what makes a resume
    /// possible at all.
    session_id: String,
}

pub async fn snapshot(client: &Client, args: SnapshotArgs) -> Result<()> {
    let endpoint = resolve_endpoint(client, args.mux_socket.as_deref()).await?;
    let snapshot = capture(client, &endpoint).await?;
    let dir = match args.out {
        Some(dir) => dir,
        None => default_snapshot_dir()?,
    };
    let path = write_snapshot(&snapshot, &dir)?;
    println!("{}", path.display());
    println!("  {}", summary(&snapshot));
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
    println!("{} — {}", path.display(), summary(&snapshot));
    if !args.run {
        print_plan(&snapshot, args.layout_only);
        println!(
            "\ndry run. Re-run with --run; sessions, windows and panes that already exist \
             are kept and only what is missing is created."
        );
        return Ok(());
    }
    apply(&endpoint, &snapshot, args.layout_only)
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
        let snapshot = capture(client, &endpoint).await?;
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

    apply(&endpoint, &snapshot, args.layout_only)?;
    println!(
        "\nagent conversations resume only where a provider id existed; \
         `muxa prune` clears the rows whose panes are gone."
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
    let mut shapes = Vec::new();
    for line in panes.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let [session, window_index, window_id, pane_index, pane_id, shell_pid, path] = f[..] else {
            continue;
        };
        if keep.get(window_id).map(String::as_str) != Some(session) {
            continue;
        }
        let command = shell_pid.parse().ok().and_then(child_command);
        shapes.push(PaneShape {
            session: session.to_owned(),
            window_index: window_index.to_owned(),
            pane_index: pane_index.to_owned(),
            path: path.to_owned(),
            replayable: command.as_deref().is_some_and(is_replayable),
            command,
            agent: agents.get(&namespaced(endpoint.host, pane_id)).cloned(),
        });
    }

    Ok(Snapshot {
        version: SNAPSHOT_VERSION,
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

/// The agents on *this* server, by pane id. Every tmux server numbers its
/// panes from `%0`, so an id alone would attach the default server's agent
/// to a pane of the same number elsewhere — and resume its conversation
/// there. An agent without a recorded server is taken to be on the default
/// one, which is what muxad assumes when it records nothing.
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
                },
            ))
        })
        .collect()
}

/// Rebuild the shape, then put back what the panes were running.
fn apply(endpoint: &BackendEndpoint, snapshot: &Snapshot, layout_only: bool) -> Result<()> {
    rebuild_shape(endpoint, snapshot)?;
    if layout_only {
        return Ok(());
    }
    replay_commands(endpoint, snapshot);
    Ok(())
}

/// Sessions, windows, panes, geometry, working directories.
fn rebuild_shape(endpoint: &BackendEndpoint, snapshot: &Snapshot) -> Result<()> {
    let mut created = 0_usize;
    let mut fresh: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for window in &snapshot.windows {
        let first = snapshot
            .panes
            .iter()
            .find(|pane| pane.session == window.session && pane.window_index == window.index);
        let path = first.map_or("", |pane| pane.path.as_str());
        let session_target = format!("={}", window.session);
        if mux_control::run(endpoint, &["has-session", "-t", &session_target]).is_err() {
            mux_control::run(
                endpoint,
                &[
                    "new-session",
                    "-d",
                    "-s",
                    &window.session,
                    "-n",
                    &window.name,
                    "-c",
                    path,
                ],
            )
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("creating session {}", window.session))?;
            created += 1;
            fresh.insert(format!("{}:{}", window.session, window.index));
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
            fresh.insert(format!("{}:{}", window.session, window.index));
        }
    }

    // A window that already has panes — a retried restore, or a plugin that
    // restored its own save on server start — gets only the ones it lacks.
    // Splitting unconditionally is how a restore doubles a workspace.
    let mut kept = 0_usize;
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
            println!("  {target}: could not list its panes; left as is");
            continue;
        };
        let have = have.max(1);
        if !fresh.contains(&format!("{}:{}", window.session, window.index)) {
            kept += have.min(wanted.len());
        }
        for pane in wanted.iter().skip(have) {
            let _ = mux_control::run(
                endpoint,
                &["split-window", "-d", "-t", &target, "-c", &pane.path],
            );
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
        if !wait_for_shell(endpoint, &target) {
            // Something is already running there; typing into it would be
            // input to that program, not a directory change.
            continue;
        }
        let _ = mux_control::run(
            endpoint,
            &[
                "send-keys",
                "-t",
                &target,
                &format!("cd {}", shell_quote(&pane.path)),
                "Enter",
            ],
        );
    }

    println!(
        "rebuilt {created} session(s)/window(s), {} pane(s) ({kept} already there)",
        snapshot.panes.len()
    );
    Ok(())
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
fn replay_commands(endpoint: &BackendEndpoint, snapshot: &Snapshot) {
    let mut replayed = 0_usize;
    let mut manual = Vec::new();
    for pane in &snapshot.panes {
        let target = pane_target(pane);
        let Some(command) = relaunch_command(pane) else {
            if let Some(hint) = resume_hint(pane) {
                manual.push(format!("{target}: {hint}"));
            }
            continue;
        };
        if !pane.replayable {
            manual.push(format!(
                "{target}: argv was a process title, not a command — start it by hand"
            ));
            continue;
        }
        if !wait_for_shell(endpoint, &target) {
            manual.push(format!("{target}: already running something; left alone"));
            continue;
        }
        let _ = mux_control::run(endpoint, &["send-keys", "-t", &target, &command, "Enter"]);
        replayed += 1;
    }
    println!("relaunched {replayed} pane(s)");
    for line in manual {
        println!("  {line}");
    }
}

/// What to tell a human about an agent pane whose command line was not
/// captured: the conversation is still resumable, just not automatically,
/// because the wrapper it was launched through is unknown.
fn resume_hint(pane: &PaneShape) -> Option<String> {
    let agent = pane.agent.as_ref()?;
    if agent.session_id.is_empty() || agent.session_id.starts_with("synthetic") {
        return None;
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
    let command = pane.command.clone()?;
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
/// sleeping a guessed interval. A pane that keeps reporting some other
/// program is not starting, it is busy — a shell's own startup may run a
/// child for a moment, so it gets a short grace, not the full wait — and
/// the answer is `false`: nothing should be typed into it.
fn wait_for_shell(endpoint: &BackendEndpoint, target: &str) -> bool {
    let deadline = Instant::now() + SHELL_READY_TIMEOUT;
    let mut busy_since: Option<Instant> = None;
    let mut saw_shell = false;
    while Instant::now() < deadline {
        let command = mux_control::capture(
            endpoint,
            &[
                "display-message",
                "-p",
                "-t",
                target,
                "#{pane_current_command}",
            ],
        );
        match command {
            Ok(command) if is_shell(command.trim()) => {
                // The pane runs its shell from the first instant, before the
                // prompt is up and keys are read. The prompt being drawn is
                // the only sign tmux can give that the shell is listening.
                saw_shell = true;
                busy_since = None;
                if prompt_drawn(endpoint, target) {
                    return true;
                }
            }
            Ok(_) => {
                let since = *busy_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= BUSY_GRACE {
                    return false;
                }
            }
            Err(_) => busy_since = None,
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // A shell that never drew anything in all that time is taken at its
    // word; a pane that was never a shell is not typed into.
    saw_shell
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

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

fn print_plan(snapshot: &Snapshot, layout_only: bool) {
    for window in &snapshot.windows {
        let panes = snapshot
            .panes
            .iter()
            .filter(|pane| pane.session == window.session && pane.window_index == window.index)
            .count();
        println!(
            "  {}:{} {} — {panes} pane(s)",
            window.session, window.index, window.name
        );
    }
    if layout_only {
        return;
    }
    for pane in &snapshot.panes {
        let Some(command) = relaunch_command(pane) else {
            if let Some(hint) = resume_hint(pane) {
                println!("  {}\n    {hint}", pane_target(pane));
            }
            continue;
        };
        let note = if pane.replayable {
            ""
        } else {
            "  [process title, not a command]"
        };
        println!("  {}{note}\n    {command}", pane_target(pane));
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
    Ok(root.join(stamp.to_string()))
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

/// What a pane is running: the pane's own process is the shell, and the
/// command is its child. Only readable while that process lives, which is why
/// it is captured before the restart rather than reconstructed after one.
#[cfg(target_os = "linux")]
fn child_command(pane_pid: u32) -> Option<String> {
    let children =
        std::fs::read_to_string(format!("/proc/{pane_pid}/task/{pane_pid}/children")).ok()?;
    children.split_whitespace().find_map(|child| {
        let raw = std::fs::read(format!("/proc/{child}/cmdline")).ok()?;
        let command = String::from_utf8_lossy(&raw)
            .replace('\0', " ")
            .trim()
            .to_owned();
        (!command.is_empty() && !is_shell(&command)).then_some(command)
    })
}

#[cfg(not(target_os = "linux"))]
fn child_command(pane_pid: u32) -> Option<String> {
    // BSD `ps` has no `--ppid`; list every process with its parent and
    // pick out the shell's children here.
    let output = std::process::Command::new("ps")
        .args(["-axo", "ppid=,command="])
        .output()
        .ok()?;
    children_of(&String::from_utf8_lossy(&output.stdout), pane_pid)
}

/// The first non-shell command among a process's children, from a
/// `ps -axo ppid=,command=` listing.
#[cfg(any(not(target_os = "linux"), test))]
fn children_of(listing: &str, parent: u32) -> Option<String> {
    listing.lines().find_map(|line| {
        let (ppid, command) = line.trim_start().split_once(char::is_whitespace)?;
        if ppid.parse::<u32>().ok()? != parent {
            return None;
        }
        let command = command.trim();
        (!command.is_empty() && !is_shell(command)).then(|| command.to_owned())
    })
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
            agent: None,
        }
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
    fn children_are_picked_out_of_a_bsd_ps_listing() {
        let listing = "    1 /sbin/launchd\n\
                       4242 -zsh\n\
                       4242 aas e june@claude\n\
                       4243 codex resume abc\n";
        assert_eq!(
            children_of(listing, 4242).as_deref(),
            Some("aas e june@claude")
        );
        assert_eq!(
            children_of(listing, 4243).as_deref(),
            Some("codex resume abc")
        );
        assert_eq!(children_of(listing, 7), None);
    }

    #[test]
    fn an_agent_without_a_command_line_gets_a_resume_hint_not_a_relaunch() {
        let mut claude = pane("work", "0", "0", None);
        claude.agent = Some(AgentShape {
            kind: "claude_code".into(),
            session_id: "abc-123".into(),
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
    fn shell_quoting_survives_an_apostrophe() {
        assert_eq!(shell_quote("/tmp/june's dir"), r"'/tmp/june'\''s dir'");
    }
}
