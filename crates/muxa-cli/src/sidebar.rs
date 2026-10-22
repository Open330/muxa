//! Persistent native side pane with an optional, reversible focus layout.
use crate::agent_move::HostArg;
use crate::mux_control::Control;
use anyhow::{ensure, Context, Result};
use clap::{Args as ClapArgs, Subcommand, ValueEnum};
use muxa::{BackendEndpoint, HostKind};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

mod layout;
const POLICY: &str = "@muxa_sidebar_config";
const SAVED_POLICY: &str = "@muxa_sidebar_saved_config";
const PANE: &str = "@muxa_sidebar_pane";
const MARK: &str = "@muxa_sidebar";
const ZOOM: &str = "@muxa_sidebar_focus";
const LAST: &str = "@muxa_sidebar_last_pane";
const FORMAT: &str = "#{session_id}\t#{window_id}\t#{pane_id}\t#{window_width}\t#{window_height}\t#{window_layout}\t#{window_zoomed_flag}";

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Side {
    #[default]
    Left,
    Right,
}

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(subcommand)]
    action: Option<Action>,
    #[arg(long, global = true, value_enum)]
    host: Option<HostArg>,
    /// Pin the multiplexer server; this is not muxad's --socket.
    #[arg(long, global = true)]
    mux_socket: Option<String>,
    #[arg(long, global = true)]
    pane: Option<String>,
    #[arg(long, global = true)]
    window: Option<String>,
    #[arg(long, global = true, value_enum, default_value = "left")]
    side: Side,
    #[arg(long, global = true, default_value_t = 36, value_parser = clap::value_parser!(u16).range(24..=80))]
    width: u16,
}
#[derive(Debug, Subcommand)]
enum Action {
    /// Enable the sidebar for this session and open it in this window.
    Show,
    /// Disable this session's sidebars and remove only their marked panes.
    Hide,
    Toggle,
    /// Expand the selected pane while keeping the sidebar visible; repeat to restore.
    Zoom,
    Status,
    #[command(hide = true)]
    Ensure,
    #[command(hide = true)]
    Run,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Policy {
    side: Side,
    width: u16,
}
#[derive(Serialize, Deserialize)]
struct Focus {
    layout: String,
    panes: Vec<String>,
}
#[derive(Clone)]
pub(crate) struct UiLaunch {
    pub(crate) daemon: PathBuf,
    pub(crate) config: Option<PathBuf>,
}
impl UiLaunch {
    pub(crate) fn words(&self) -> Result<Vec<String>> {
        let mut words = vec![
            std::env::current_exe()?.to_string_lossy().into_owned(),
            "--socket".into(),
            self.daemon.to_string_lossy().into_owned(),
        ];
        if let Some(config) = &self.config {
            words.extend(["--config".into(), config.to_string_lossy().into_owned()]);
        }
        Ok(words)
    }
}
struct Location {
    session: String,
    window: String,
    pane: String,
    width: u16,
    height: u16,
    layout: String,
    zoomed: bool,
}
impl Location {
    fn read(control: &Control, target: &str) -> Result<Self> {
        let output = control.output(&["display-message", "-p", "-t", target, FORMAT])?;
        let fields: Vec<_> = output.trim_end().split('\t').collect();
        ensure!(
            fields.len() == 7 && fields[0].starts_with('$') && fields[1].starts_with('@'),
            "could not resolve sidebar target"
        );
        Ok(Self {
            session: fields[0].into(),
            window: fields[1].into(),
            pane: fields[2].into(),
            width: fields[3].parse()?,
            height: fields[4].parse()?,
            layout: fields[5].into(),
            zoomed: fields[6] == "1",
        })
    }
}
fn option(control: &Control, scope: &str, target: &str, key: &str) -> Result<String> {
    let mut args = vec!["show-options"];
    if !scope.is_empty() {
        args.push(scope);
    }
    args.extend(["-q", "-v", "-t", target, key]);
    control.output(&args).map(|value| value.trim().into())
}
fn set(control: &Control, scope: &str, target: &str, key: &str, value: &str) -> Result<()> {
    let mut args = vec!["set-option"];
    if !scope.is_empty() {
        args.push(scope);
    }
    args.extend(["-t", target, key, value]);
    control.run(&args)
}
fn native(pane: &str) -> &str {
    pane.strip_prefix("rmux:").unwrap_or(pane)
}
fn canonical(endpoint: &BackendEndpoint, pane: &str) -> String {
    if endpoint.host == HostKind::Rmux {
        format!("rmux:{}", native(pane))
    } else {
        pane.into()
    }
}
fn endpoint(args: &Args) -> Result<BackendEndpoint> {
    let host = match args.host {
        Some(HostArg::Rmux) => HostKind::Rmux,
        None if args
            .pane
            .as_deref()
            .is_some_and(|pane| pane.starts_with("rmux:"))
            || muxa::backend::rmux::endpoint_from_env().is_some() =>
        {
            HostKind::Rmux
        }
        Some(HostArg::Tmux) | None => HostKind::Tmux,
    };
    let socket = args
        .mux_socket
        .clone()
        .or_else(|| {
            if host == HostKind::Rmux {
                muxa::backend::rmux::endpoint_from_env()
            } else {
                std::env::var("TMUX")
                    .ok()
                    .and_then(|value| value.split(',').next().map(str::to_string))
            }
        })
        .context("sidebar needs a tmux/rmux server; specify --host and --mux-socket")?;
    Ok(BackendEndpoint { host, socket })
}
fn lock(endpoint: &BackendEndpoint) -> Result<File> {
    use std::hash::{Hash, Hasher};
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    endpoint.host.hash(&mut hash);
    let address = if endpoint.host == HostKind::Rmux {
        "rmux:%0"
    } else {
        "%0"
    };
    muxa::backend::pane_endpoint_identity(Some(address), &endpoint.socket).hash(&mut hash);
    let directory = muxa::paths::data_dir()
        .context("no data directory")?
        .join("muxa/sidebar-locks");
    std::fs::create_dir_all(&directory)?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join(format!("{:x}.lock", hash.finish())))?;
    file.lock()?; // OS lock releases even if the CLI is interrupted.
    Ok(file)
}
fn pane_ids(control: &Control, window: &str) -> Result<Vec<String>> {
    Ok(control
        .output(&["list-panes", "-t", window, "-F", "#{pane_id}"])?
        .lines()
        .map(str::to_string)
        .collect())
}
fn policy(control: &Control, session: &str) -> Result<Option<Policy>> {
    let raw = option(control, "", session, POLICY)?;
    if raw.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        serde_json::from_str(&raw).context("invalid sidebar session policy")?,
    ))
}
fn owned_pane(control: &Control, window: &str) -> Result<Option<String>> {
    let stored = option(control, "-w", window, PANE)?;
    let rows = control.output(&[
        "list-panes",
        "-t",
        window,
        "-F",
        "#{pane_id}\t#{@muxa_sidebar}",
    ])?;
    let marked: Vec<_> = rows
        .lines()
        .filter_map(|line| {
            let (pane, mark) = line.split_once('\t')?;
            (mark == "1").then(|| pane.to_string())
        })
        .collect();
    // Recover an interrupted creation without spawning a second explorer.
    Ok(marked
        .iter()
        .find(|pane| **pane == stored)
        .cloned()
        .or_else(|| marked.first().cloned()))
}

pub async fn run(
    client: &muxa::ipc::Client,
    config: &muxa::config::Config,
    args: Args,
    config_path: Option<&Path>,
) -> Result<()> {
    let launch = UiLaunch {
        daemon: client.socket().to_path_buf(),
        config: config_path.map(Path::to_path_buf),
    };
    let endpoint = endpoint(&args)?;
    if matches!(args.action, Some(Action::Run)) {
        let pane = args
            .pane
            .or_else(|| muxa::default_backend().current_pane())
            .context("sidebar could not identify its own pane")?;
        let pane = canonical(&endpoint, &pane);
        return crate::watch::sidebar::run(client, config, endpoint, pane, launch).await;
    }
    let control = Control::Endpoint(endpoint.clone());
    let target = args
        .window
        .as_deref()
        .or(args.pane.as_deref())
        .map_or("", native);
    let _lock = lock(&endpoint)?;
    let location = Location::read(&control, target)?;
    match args.action.unwrap_or(Action::Toggle) {
        Action::Show => show(
            &control,
            &endpoint,
            &location,
            Policy {
                side: args.side,
                width: args.width,
            },
            &launch,
        )?,
        Action::Toggle => {
            if policy(&control, &location.session)?.is_some() {
                hide(&control, &location)?;
            } else {
                let saved = option(&control, "", &location.session, SAVED_POLICY)?;
                let preferences = if saved.is_empty() {
                    Policy {
                        side: args.side,
                        width: args.width,
                    }
                } else {
                    serde_json::from_str(&saved).context("invalid saved sidebar policy")?
                };
                show(&control, &endpoint, &location, preferences, &launch)?;
            }
        }
        Action::Hide => hide(&control, &location)?,
        Action::Ensure => {
            if let Some(policy) = policy(&control, &location.session)? {
                ensure_pane(&control, &endpoint, &location, &policy, &launch)?;
            }
        }
        Action::Zoom => zoom(&control, &location)?,
        Action::Status => println!(
            "{}",
            serde_json::json!({"enabled":policy(&control,&location.session)?.is_some(),"pane":owned_pane(&control,&location.window)?,"focus":!option(&control,"-w",&location.window,ZOOM)?.is_empty()})
        ),
        Action::Run => unreachable!(),
    }
    Ok(())
}
fn show(
    control: &Control,
    endpoint: &BackendEndpoint,
    location: &Location,
    policy: Policy,
    launch: &UiLaunch,
) -> Result<()> {
    // Validate before enabling hooks or changing session preferences.
    ensure!(
        location.width >= policy.width + 21,
        "window needs at least {} columns for this sidebar",
        policy.width + 21
    );
    if let Some(pane) = owned_pane(control, &location.window)? {
        if self::policy(control, &location.session)?.as_ref() != Some(&policy) {
            restore(control, location)?;
            ensure!(
                pane_ids(control, &location.window)?.len() > 1,
                "cannot move a sidebar without a content pane"
            );
            let current = Location::read(control, &location.window)?;
            let base = layout::undock(&current.layout, current.width, current.height, &pane)?;
            control.run(&["kill-pane", "-t", &pane])?;
            control.run(&["select-layout", "-t", &location.window, &base])?;
            set(control, "-w", &location.window, PANE, "")?;
        }
    }
    let current = Location::read(control, &location.window)?;
    ensure_pane(control, endpoint, &current, &policy, launch)?;
    set(
        control,
        "",
        &location.session,
        POLICY,
        &serde_json::to_string(&policy)?,
    )?;
    install_bindings(control, endpoint, launch)?;
    if option(control, "-w", &location.window, PANE)? != location.pane
        && pane_ids(control, &location.window)?.contains(&location.pane)
    {
        set(control, "-w", &location.window, LAST, &location.pane)?;
    }
    println!("Sidebar enabled; prefix + e toggles, prefix + s opens watch, prefix + z focuses (custom bindings preserved)");
    Ok(())
}
fn ensure_pane(
    control: &Control,
    endpoint: &BackendEndpoint,
    location: &Location,
    policy: &Policy,
    launch: &UiLaunch,
) -> Result<()> {
    if let Some(pane) = owned_pane(control, &location.window)? {
        if option(control, "-w", &location.window, PANE)? != pane {
            set(control, "-w", &location.window, PANE, &pane)?;
        }
        if location.zoomed {
            zoom(control, location)?;
        }
        return Ok(());
    }
    ensure!(
        location.width >= policy.width + 21,
        "window is too narrow for the sidebar"
    );
    if location.zoomed {
        control.run(&["resize-pane", "-Z", "-t", &location.pane])?;
    }
    // Detached split preserves the human's active pane and its subtree.
    let mut words = launch.words()?;
    words.extend([
        "sidebar".into(),
        "run".into(),
        "--host".into(),
        endpoint.host.to_string(),
        "--mux-socket".into(),
        endpoint.socket.clone(),
    ]);
    let shell = muxa::shell::Dialect::native();
    let command = shell.exec_line(&words);
    let mut split = vec![
        "split-window",
        "-d",
        "-f",
        "-h",
        "-P",
        "-F",
        "#{pane_id}",
        "-t",
        &location.window,
        "-l",
    ];
    let width = policy.width.to_string();
    split.push(&width);
    if matches!(policy.side, Side::Left) {
        split.push("-b");
    }
    // The UI resolves its own pane from the multiplexer, not an agent env.
    split.push(&command);
    let pane = control.output(&split)?.trim().to_string();
    ensure!(pane.starts_with('%'), "sidebar split returned no pane ID");
    let marked = (|| {
        let docked = layout::dock(
            &location.layout,
            location.width,
            location.height,
            policy.width,
            policy.side,
            &pane,
            None,
        )?;
        control.run(&["select-layout", "-t", &location.window, &docked])?;
        set(control, "-p", &pane, MARK, "1")?;
        set(control, "-w", &location.window, PANE, &pane)?;
        set(control, "-w", &location.window, LAST, &location.pane)?;
        control.run(&["select-pane", "-t", &pane, "-T", "Muxa agents"])?;
        control.run(&["select-pane", "-t", &location.pane])
    })();
    if let Err(error) = marked {
        let _ = control.run(&["kill-pane", "-t", &pane]);
        let _ = set(control, "-w", &location.window, PANE, "");
        let _ = control.run(&["select-layout", "-t", &location.window, &location.layout]);
        return Err(error);
    }
    Ok(())
}
fn restore(control: &Control, location: &Location) -> Result<()> {
    let raw = option(control, "-w", &location.window, ZOOM)?;
    if raw.is_empty() {
        return Ok(());
    }
    let saved: Focus = serde_json::from_str(&raw)?;
    let mut actual = pane_ids(control, &location.window)?;
    actual.sort();
    let mut expected = saved.panes;
    expected.sort();
    if actual == expected {
        control.run(&["select-layout", "-t", &location.window, &saved.layout])?;
    }
    // A user's split/close during focus is authoritative; never recreate panes.
    set(control, "-w", &location.window, ZOOM, "")
}
fn zoom(control: &Control, location: &Location) -> Result<()> {
    let Some(sidebar) = owned_pane(control, &location.window)? else {
        return control.run(&["resize-pane", "-Z", "-t", &location.pane]);
    };
    if location.zoomed {
        control.run(&["resize-pane", "-Z", "-t", &location.pane])?;
    }
    if !option(control, "-w", &location.window, ZOOM)?.is_empty() {
        return restore(control, location);
    }
    let Some(policy) = policy(control, &location.session)? else {
        return Ok(());
    };
    let panes = pane_ids(control, &location.window)?;
    let selected = if location.pane == sidebar {
        option(control, "-w", &location.window, LAST)?
    } else {
        location.pane.clone()
    };
    ensure!(
        selected != sidebar && panes.contains(&selected),
        "select an agent pane before sidebar zoom"
    );
    let others: Vec<_> = panes
        .iter()
        .filter(|pane| **pane != sidebar && **pane != selected)
        .cloned()
        .collect();
    let layout = layout::focus(
        location.width,
        location.height,
        policy.width,
        policy.side,
        &sidebar,
        &selected,
        &others,
    )?;
    let saved = serde_json::to_string(&Focus {
        layout: location.layout.clone(),
        panes,
    })?;
    set(control, "-w", &location.window, ZOOM, &saved)?;
    if let Err(error) = control.run(&["select-layout", "-t", &location.window, &layout]) {
        let _ = set(control, "-w", &location.window, ZOOM, "");
        return Err(error);
    }
    control.run(&["select-pane", "-t", &selected])
}
fn hide(control: &Control, location: &Location) -> Result<()> {
    let preferences = option(control, "", &location.session, POLICY)?;
    if !preferences.is_empty() {
        set(control, "", &location.session, SAVED_POLICY, &preferences)?;
    }
    set(control, "", &location.session, POLICY, "")?;
    // Only this session's marked sidebar panes are eligible for removal.
    let windows = control.output(&[
        "list-windows",
        "-t",
        &location.session,
        "-F",
        "#{window_id}",
    ])?;
    for window in windows.lines() {
        if let Some(pane) = owned_pane(control, window)? {
            let current = Location::read(control, window)?;
            restore(control, &current)?;
            let current = Location::read(control, window)?;
            if pane_ids(control, window)?.len() == 1 {
                set(control, "-p", &pane, MARK, "")?;
                let shell = control.output(&[
                    "show-options",
                    "-v",
                    "-t",
                    &location.session,
                    "default-shell",
                ])?;
                control.run(&["respawn-pane", "-k", "-t", &pane, shell.trim()])?;
            } else {
                let base = layout::undock(&current.layout, current.width, current.height, &pane)?;
                control.run(&["kill-pane", "-t", &pane])?;
                control.run(&["select-layout", "-t", window, &base])?;
            }
        }
        set(control, "-w", window, PANE, "")?;
        set(control, "-w", window, ZOOM, "")?;
    }
    Ok(())
}
fn install_bindings(
    control: &Control,
    endpoint: &BackendEndpoint,
    launch: &UiLaunch,
) -> Result<()> {
    let shell = muxa::shell::Dialect::native();
    let mut words = launch.words()?;
    words.extend([
        "sidebar".into(),
        "ensure".into(),
        "--host".into(),
        endpoint.host.to_string(),
        "--mux-socket".into(),
        endpoint.socket.clone(),
        "--window".into(),
        "#{window_id}".into(),
    ]);
    let command = shell.command_line(&words);
    let guarded = format!("run-shell -b {}", shell.quote(&command));
    let hook = format!("if-shell -F '#{{{POLICY}}}' {}", shell.quote(&guarded));
    for name in [
        "after-select-window[9105]",
        "after-new-window[9105]",
        "client-session-changed[9105]",
    ] {
        control.run(&["set-hook", "-g", name, &hook])?;
    }
    let focus_hook = format!(
        "if-shell -F '#{{&&:#{{{POLICY}}},#{{!=:#{{{MARK}}},1}}}}' \"set-option -w {LAST} '#{{pane_id}}'\""
    );
    control.run(&["set-hook", "-g", "after-select-pane[9105]", &focus_hook])?;
    install_toggle(control, endpoint, launch)?;
    let zoom_hook = format!(
        "if-shell -F '#{{&&:#{{window_zoomed_flag}},#{{{PANE}}}}}' {}",
        shell.quote(&guarded)
    );
    control.run(&["set-hook", "-g", "after-resize-pane[9105]", &zoom_hook])?;
    // rmux currently returns an empty result for positional one-key queries.
    // Read just the prefix table and select the exact key locally.
    let bindings = control.output(&["list-keys", "-T", "prefix"])?;
    let binding = bindings
        .lines()
        .find(|line| {
            let words: Vec<_> = line.split_whitespace().collect();
            words.windows(3).any(|words| words == ["-T", "prefix", "z"])
        })
        .unwrap_or("");
    if !binding.trim_end().ends_with("resize-pane -Z") && !binding.contains("sidebar zoom") {
        eprintln!(
            "Custom prefix + z preserved; use `muxa sidebar zoom` to keep the sidebar visible"
        );
        return Ok(());
    }
    let mut words = launch.words()?;
    words.extend([
        "sidebar".into(),
        "zoom".into(),
        "--host".into(),
        endpoint.host.to_string(),
        "--mux-socket".into(),
        endpoint.socket.clone(),
        "--pane".into(),
        "#{pane_id}".into(),
    ]);
    let command = shell.command_line(&words);
    // Outside an enabled window, preserve tmux's native zoom behaviour.
    control.run(&[
        "bind-key",
        "-T",
        "prefix",
        "z",
        "if-shell",
        "-F",
        "#{@muxa_sidebar_pane}",
        &format!("run-shell -b {}", shell.quote(&command)),
        "resize-pane -Z",
    ])
}

/// Focus a selected node without exiting the sidebar or writing to its agent.
pub(crate) fn select(
    endpoint: &BackendEndpoint,
    pane: &muxa::PaneKey,
    source: &str,
    launch: &UiLaunch,
) -> Result<()> {
    ensure!(
        pane.window.session.endpoint.host == endpoint.host
            && muxa::backend::pane_endpoints_match(
                Some(&pane.pane_id),
                &pane.window.session.endpoint.socket,
                &endpoint.socket
            ),
        "sidebar cannot select a pane on another server"
    );
    let _lock = lock(endpoint)?;
    let control = Control::Endpoint(endpoint.clone());
    let id = native(&pane.pane_id);
    let destination = Location::read(&control, id)?;
    ensure!(
        destination.window == pane.window.window_id
            && destination.session == pane.window.session.session_id,
        "pane moved; refresh the sidebar before selecting it"
    );
    let origin = Location::read(&control, native(source))?;
    let client = if origin.session == destination.session {
        None
    } else {
        Some(focused_client(&control, source)?)
    };
    // Carry the explorer into a destination workspace only after an explicit
    // user selection, without stealing another client's session/window.
    if client.is_some() && policy(&control, &destination.session)?.is_none() {
        if let Some(preferences) = policy(&control, &origin.session)? {
            ensure_pane(&control, endpoint, &destination, &preferences, launch)?;
            set(
                &control,
                "",
                &destination.session,
                POLICY,
                &serde_json::to_string(&preferences)?,
            )?;
        }
    }
    let target = format!(
        "{}:{}",
        pane.window.session.session_id, pane.window.window_id
    );
    control.run(&["select-window", "-t", &target])?;
    if let Some(client) = client {
        control.run(&["switch-client", "-c", &client, "-t", &destination.session])?;
    }
    set(&control, "-w", &pane.window.window_id, LAST, id)?;
    control.run(&["select-pane", "-t", id])
}

fn install_toggle(control: &Control, endpoint: &BackendEndpoint, launch: &UiLaunch) -> Result<()> {
    let bindings = control.output(&["list-keys", "-T", "prefix"])?;
    let existing = bindings.lines().find(|line| {
        line.split_whitespace()
            .collect::<Vec<_>>()
            .windows(3)
            .any(|words| words == ["-T", "prefix", "e"])
    });
    if existing.is_some_and(|binding| !binding.contains("sidebar toggle")) {
        eprintln!("Custom prefix + e preserved; use muxa sidebar toggle");
        return Ok(());
    }
    let mut words = launch.words()?;
    words.extend([
        "sidebar".into(),
        "toggle".into(),
        "--host".into(),
        endpoint.host.to_string(),
        "--mux-socket".into(),
        endpoint.socket.clone(),
        "--pane".into(),
        "#{pane_id}".into(),
    ]);
    let command = muxa::shell::Dialect::native().command_line(&words);
    control.run(&["bind-key", "-T", "prefix", "e", "run-shell", "-b", &command])
}

/// A popup opened from the explorer should select its last work pane.
pub(crate) fn watch_source(control: &Control, source: &str) -> Option<String> {
    if option(control, "-p", native(source), MARK).ok()?.as_str() != "1" {
        return None;
    }
    let pane = option(control, "-w", native(source), LAST).ok()?;
    (!pane.is_empty()).then_some(pane)
}

/// rmux does not expose `pane_id` in list-clients; resolve the active session
/// pane separately and refuse to switch an ambiguous group of clients.
pub(crate) fn focused_client(control: &Control, source: &str) -> Result<String> {
    let source = native(source);
    let session = control.output(&["display-message", "-p", "-t", source, "#{session_id}"])?;
    let target = format!("{}:", session.trim());
    let active = control.output(&["display-message", "-p", "-t", &target, "#{pane_id}"])?;
    ensure!(
        active.trim() == source,
        "focus this sidebar before navigating"
    );
    let clients = control.output(&["list-clients", "-F", "#{client_name}\t#{session_id}"])?;
    let clients: Vec<_> = clients
        .lines()
        .filter_map(|line| {
            let (name, id) = line.split_once('\t')?;
            (id == session.trim()).then_some(name)
        })
        .collect();
    ensure!(
        clients.len() == 1,
        "navigation needs one client focused on this sidebar; use prefix + s"
    );
    Ok(clients[0].into())
}
