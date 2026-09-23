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
            "\ndry run. Re-run with --run against a server that does not have these sessions."
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

    if !args.yes && !confirm(&endpoint) {
        println!("aborted; nothing was touched.");
        return Ok(());
    }

    println!("stopping {}", endpoint.socket);
    let _ = mux_control::run(&endpoint, &["kill-server"]);
    wait_for_server_to_stop(&endpoint)?;

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
        let command = shell_pid.parse().ok().and_then(child_command);
        shapes.push(PaneShape {
            session: session.to_owned(),
            window_index: window_index.to_owned(),
            pane_index: pane_index.to_owned(),
            path: path.to_owned(),
            replayable: command.as_deref().is_some_and(is_replayable),
            command,
            agent: agents.get(pane_id).cloned(),
        });
    }

    Ok(Snapshot {
        version: SNAPSHOT_VERSION,
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
    }

    for pane in &snapshot.panes {
        if pane.pane_index == "0" {
            continue;
        }
        let target = format!("={}:{}", pane.session, pane.window_index);
        let _ = mux_control::run(
            endpoint,
            &["split-window", "-d", "-t", &target, "-c", &pane.path],
        );
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
        "rebuilt {created} session(s)/window(s), {} pane(s)",
        snapshot.panes.len()
    );
    Ok(())
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
        let Some(command) = relaunch_command(pane) else {
            continue;
        };
        if !pane.replayable {
            manual.push(pane_target(pane));
            continue;
        }
        let target = pane_target(pane);
        let _ = mux_control::run(endpoint, &["send-keys", "-t", &target, &command, "Enter"]);
        replayed += 1;
    }
    println!("relaunched {replayed} pane(s)");
    for target in manual {
        println!("  {target}: argv was a process title, not a command — start it by hand");
    }
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
             target server from; pass --socket"
        ),
        _ => bail!(
            "agents span several servers ({}); pass --socket to name one",
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
    let output = std::process::Command::new("ps")
        .args(["-o", "command=", "--ppid", &pane_pid.to_string()])
        .output()
        .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .find(|command| !command.is_empty() && !is_shell(command))
        .map(str::to_owned)
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
    fn shell_quoting_survives_an_apostrophe() {
        assert_eq!(shell_quote("/tmp/june's dir"), r"'/tmp/june'\''s dir'");
    }
}
