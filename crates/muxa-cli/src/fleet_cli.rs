//! Host inventory and non-interactive Muxa Fleet commands.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use comfy_table::presets::UTF8_BORDERS_ONLY;
use comfy_table::{Cell, Table};
use muxa::config::{FleetConnectPolicy, FleetHostConfig};
use muxa::fleet::{
    drain_bounded, read_bounded_line, sanitize_terminal_text, validate_label_key,
    validate_label_value, FleetHostSnapshot, FleetHostState, FleetOperation, GlobalPaneRef,
    HostAccessMode, LabelSelector, RelayFrame, FLEET_MAX_DIAGNOSTIC_BYTES, FLEET_MAX_FRAME_BYTES,
    FLEET_PROTOCOL_VERSION,
};
use muxa::ipc::Client;
use muxa::{Config, PaneKey};
use tokio::io::BufReader;
use tokio::process::Command;

#[derive(Debug, Args)]
pub(crate) struct HostArgs {
    #[command(subcommand)]
    command: HostCommand,
}

#[derive(Debug, Subcommand)]
enum HostCommand {
    /// Register or update one OpenSSH Host alias.
    Add {
        alias: String,
        ssh: String,
        #[arg(long = "label", value_name = "KEY=VALUE")]
        labels: Vec<String>,
        #[arg(long = "annotation", value_name = "KEY=VALUE")]
        annotations: Vec<String>,
        #[arg(long, value_enum, default_value_t = AccessArg::Observe)]
        mode: AccessArg,
        #[arg(long, value_enum, default_value_t = ConnectArg::Auto)]
        connect: ConnectArg,
        #[arg(long, default_value = "muxa")]
        muxa_path: String,
        #[arg(long)]
        remote_socket: Option<PathBuf>,
        /// Replace an existing host entry.
        #[arg(long)]
        overwrite: bool,
    },
    /// List inventory and live connection state.
    List {
        #[arg(short = 'l', long = "selector")]
        selector: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show one host's full configuration and live metadata.
    Show { alias: String },
    /// Remove a host from the central inventory.
    Remove { alias: String },
    /// Add, replace, or remove labels (`key=value`; trailing `key-` removes).
    #[command(visible_alias = "tag")]
    Label {
        alias: String,
        changes: Vec<String>,
        #[arg(long)]
        overwrite: bool,
    },
    /// Add, replace, or remove annotations (`key=value`; trailing `key-` removes).
    Annotate {
        alias: String,
        changes: Vec<String>,
        #[arg(long)]
        overwrite: bool,
    },
    /// Enable a configured host.
    Enable { alias: String },
    /// Disable a configured host without removing its metadata.
    Disable { alias: String },
    /// Verify OpenSSH trust/auth, remote muxa, daemon IPC, and protocol.
    Doctor {
        alias: String,
        #[arg(long, default_value_t = 10)]
        timeout_secs: u64,
    },
}

#[derive(Debug, Args)]
pub(crate) struct FleetArgs {
    #[command(subcommand)]
    command: FleetCommand,
}

#[derive(Debug, Subcommand)]
enum FleetCommand {
    /// Print the aggregated host/session/window/pane state.
    Status {
        #[arg(short = 'l', long = "selector")]
        selector: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Open the central host/session/window/pane TUI.
    Watch {
        #[arg(short = 'l', long = "selector")]
        selector: Option<String>,
    },
    /// Connect an on-demand host.
    Connect { host: String },
    /// Close the persistent relay for one host.
    Disconnect { host: String },
    /// Force a fresh full snapshot.
    Refresh { host: String },
    /// List remote panes and their complete collision-free keys.
    Panes {
        host: String,
        #[arg(long)]
        json: bool,
    },
    /// Capture one exact remote pane selected by native pane id.
    Capture { host: String, pane: String },
    /// Send text to one exact remote agent pane.
    Send {
        host: String,
        pane: String,
        text: String,
        #[arg(long, default_value_t = true)]
        submit: bool,
    },
    /// Attach this terminal to one exact remote pane over a separate SSH TTY.
    Attach { host: String, pane: String },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum AccessArg {
    Observe,
    Control,
}

impl From<AccessArg> for HostAccessMode {
    fn from(value: AccessArg) -> Self {
        match value {
            AccessArg::Observe => Self::Observe,
            AccessArg::Control => Self::Control,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ConnectArg {
    Auto,
    OnDemand,
}

impl From<ConnectArg> for FleetConnectPolicy {
    fn from(value: ConnectArg) -> Self {
        match value {
            ConnectArg::Auto => Self::Auto,
            ConnectArg::OnDemand => Self::OnDemand,
        }
    }
}

#[allow(clippy::too_many_lines)] // explicit CLI subcommand dispatch table
pub(crate) async fn run_host(
    args: HostArgs,
    client: &Client,
    cfg: &Config,
    config_path: Option<&Path>,
) -> Result<()> {
    match args.command {
        HostCommand::Add {
            alias,
            ssh,
            labels,
            annotations,
            mode,
            connect,
            muxa_path,
            remote_socket,
            overwrite,
        } => {
            let path = config_path.context("no config directory is available on this system")?;
            validate_label_value(&alias).map_err(anyhow::Error::msg)?;
            if cfg.fleet.hosts.contains_key(&alias) && !overwrite {
                bail!("host '{alias}' already exists; pass --overwrite to replace it");
            }
            let host = FleetHostConfig {
                ssh,
                muxa_path,
                remote_socket,
                enabled: true,
                connect: connect.into(),
                mode: mode.into(),
                labels: parse_pairs(&labels, false)?,
                annotations: parse_pairs(&annotations, true)?,
            };
            edit_config(path, |document| upsert_host(document, &alias, &host))?;
            println!(
                "{} host {alias} in {}",
                if overwrite { "updated" } else { "added" },
                path.display()
            );
            request_reload(client).await;
            Ok(())
        }
        HostCommand::List { selector, json } => {
            let selector = parse_selector(selector.as_deref())?;
            let live = client.fleet_snapshot(selector.as_deref()).await.ok();
            let hosts = inventory_with_live(cfg, live.as_ref(), selector.as_deref())?;
            if json {
                println!("{}", serde_json::to_string_pretty(&hosts)?);
            } else {
                print_hosts(&hosts);
            }
            Ok(())
        }
        HostCommand::Show { alias } => {
            let host = cfg
                .fleet
                .hosts
                .get(&alias)
                .with_context(|| format!("host '{alias}' is not configured"))?;
            let live =
                client.fleet_snapshot(None).await.ok().and_then(|snapshot| {
                    snapshot.hosts.into_iter().find(|item| item.alias == alias)
                });
            println!("alias:       {alias}");
            println!("ssh:         {}", host.ssh);
            println!("mode:        {:?}", host.mode);
            println!("connect:     {:?}", host.connect);
            println!("enabled:     {}", host.enabled);
            println!("muxa path:   {}", host.muxa_path);
            println!("labels:      {}", format_map(&host.labels));
            println!("annotations: {}", format_map(&host.annotations));
            if let Some(live) = live {
                println!("state:       {:?}", live.state);
                println!(
                    "node id:     {}",
                    live.node_id.as_ref().map_or("-", muxa::NodeId::as_str)
                );
                println!(
                    "hostname:    {}",
                    sanitize_terminal_text(live.hostname.as_deref().unwrap_or("-"))
                );
                println!(
                    "version:     {}",
                    sanitize_terminal_text(live.muxa_version.as_deref().unwrap_or("-"))
                );
                println!("agents:      {}", live.agent_count());
                if let Some(error) = live.error {
                    println!("error:       {}", sanitize_terminal_text(&error));
                }
            }
            Ok(())
        }
        HostCommand::Remove { alias } => {
            let path = config_path.context("no config directory is available on this system")?;
            if !cfg.fleet.hosts.contains_key(&alias) {
                bail!("host '{alias}' is not configured");
            }
            edit_config(path, |document| remove_host(document, &alias))?;
            println!("removed host {alias} from {}", path.display());
            request_reload(client).await;
            Ok(())
        }
        HostCommand::Label {
            alias,
            changes,
            overwrite,
        } => {
            edit_metadata(
                config_path,
                cfg,
                client,
                &alias,
                "labels",
                &changes,
                overwrite,
                false,
            )
            .await
        }
        HostCommand::Annotate {
            alias,
            changes,
            overwrite,
        } => {
            edit_metadata(
                config_path,
                cfg,
                client,
                &alias,
                "annotations",
                &changes,
                overwrite,
                true,
            )
            .await
        }
        HostCommand::Enable { alias } => {
            set_host_enabled(config_path, cfg, client, &alias, true).await
        }
        HostCommand::Disable { alias } => {
            set_host_enabled(config_path, cfg, client, &alias, false).await
        }
        HostCommand::Doctor {
            alias,
            timeout_secs,
        } => doctor(cfg, &alias, Duration::from_secs(timeout_secs)).await,
    }
}

pub(crate) async fn run_fleet(
    args: FleetArgs,
    client: &Client,
    cfg: &Config,
    _config_path: Option<&Path>,
) -> Result<()> {
    match args.command {
        FleetCommand::Status { selector, json } => {
            let selector = parse_selector(selector.as_deref())?;
            let snapshot = client
                .fleet_snapshot(selector.as_deref())
                .await
                .context("reading fleet state from muxad")?;
            if json {
                println!("{}", serde_json::to_string_pretty(&snapshot)?);
            } else {
                print_hosts(&snapshot.hosts);
            }
            Ok(())
        }
        FleetCommand::Watch { selector } => {
            let selector = parse_selector(selector.as_deref())?;
            crate::fleet_watch::run(client.clone(), cfg, selector).await
        }
        FleetCommand::Connect { host } => {
            execute_simple(client, &host, FleetOperation::Connect).await
        }
        FleetCommand::Disconnect { host } => {
            execute_simple(client, &host, FleetOperation::Disconnect).await
        }
        FleetCommand::Refresh { host } => {
            execute_simple(client, &host, FleetOperation::Refresh).await
        }
        FleetCommand::Panes { host, json } => {
            let records = pane_records(client, &host).await?;
            if json {
                let records = records
                    .into_iter()
                    .map(|(pane, key, display)| {
                        serde_json::json!({
                            "display": display,
                            "key": key,
                            "command": pane.current_command,
                            "cwd": pane.current_path,
                        })
                    })
                    .collect::<Vec<_>>();
                println!("{}", serde_json::to_string_pretty(&records)?);
            } else {
                let mut table = Table::new();
                table.load_preset(UTF8_BORDERS_ONLY);
                table.set_header(["PANE", "PATH", "BACKEND", "ENDPOINT", "COMMAND"]);
                for (pane, key, display) in records {
                    table.add_row([
                        Cell::new(sanitize_terminal_text(&pane.pane_id)),
                        Cell::new(sanitize_terminal_text(&display)),
                        Cell::new(key.window.session.endpoint.host),
                        Cell::new(sanitize_terminal_text(&key.window.session.endpoint.socket)),
                        Cell::new(sanitize_terminal_text(&pane.current_command)),
                    ]);
                }
                println!("{table}");
                println!(
                    "Use --json and pass an exact `key` object as PANE when a display path is ambiguous."
                );
            }
            Ok(())
        }
        FleetCommand::Capture { host, pane } => {
            let target = resolve_pane(client, &host, &pane).await?;
            let result = client
                .fleet_execute(&host, &FleetOperation::Capture { pane: target })
                .await?;
            if let Some(capture) = result.capture {
                print!("{capture}");
                io::stdout().flush()?;
            }
            Ok(())
        }
        FleetCommand::Send {
            host,
            pane,
            text,
            submit,
        } => {
            let target = resolve_pane(client, &host, &pane).await?;
            let result = client
                .fleet_execute(
                    &host,
                    &FleetOperation::SendPrompt {
                        pane: target,
                        text,
                        submit,
                    },
                )
                .await?;
            if let Some(send) = result.send {
                println!("sent={} submitted={}", send.sent, send.submitted);
            }
            Ok(())
        }
        FleetCommand::Attach { host, pane } => attach(client, cfg, &host, &pane).await,
    }
}

async fn execute_simple(client: &Client, host: &str, operation: FleetOperation) -> Result<()> {
    let result = client.fleet_execute(host, &operation).await?;
    println!("{}", result.message.unwrap_or_else(|| "accepted".into()));
    Ok(())
}

pub(crate) async fn resolve_pane(client: &Client, host: &str, query: &str) -> Result<PaneKey> {
    let records = pane_records(client, host).await?;
    if query.trim_start().starts_with('{') {
        let exact: PaneKey = serde_json::from_str(query).context("decoding exact PaneKey JSON")?;
        return records
            .iter()
            .any(|(_, key, _)| key == &exact)
            .then_some(exact)
            .with_context(|| format!("exact pane key is stale or absent on host '{host}'"));
    }
    let mut matches = records
        .into_iter()
        .filter_map(|(pane, key, display)| {
            (pane.pane_id == query || display == query).then_some(key)
        })
        .collect::<Vec<_>>();
    match matches.len() {
        0 => bail!("pane '{query}' was not found on host '{host}'"),
        1 => Ok(matches.remove(0)),
        count => bail!(
            "pane '{query}' is ambiguous across {count} backend endpoints on host '{host}'; run `muxa fleet panes {host} --json` and pass an exact key object",
        ),
    }
}

async fn pane_records(
    client: &Client,
    host: &str,
) -> Result<Vec<(muxa::tmux::PaneInfo, PaneKey, String)>> {
    let snapshot = client.fleet_snapshot(None).await?;
    let host_snapshot = snapshot
        .hosts
        .into_iter()
        .find(|candidate| candidate.alias == host)
        .with_context(|| format!("fleet host '{host}' is not known"))?;
    let remote = host_snapshot
        .remote
        .with_context(|| format!("fleet host '{}' has no snapshot", host_snapshot.alias))?;
    let fallback_kind = remote.backends.first().map(|backend| backend.kind);
    Ok(remote
        .panes
        .into_iter()
        .filter_map(|pane| {
            let kind = muxa::backend::pane_id_host_kind(&pane.pane_id).or(fallback_kind)?;
            let key = PaneKey::from_pane(kind, &pane);
            let display = format!(
                "{}/{}/{}",
                pane.session,
                if pane.window_name.is_empty() {
                    &pane.window_index
                } else {
                    &pane.window_name
                },
                pane.pane_id
            );
            Some((pane, key, display))
        })
        .collect())
}

async fn attach(client: &Client, cfg: &Config, host: &str, pane: &str) -> Result<()> {
    let key = resolve_pane(client, host, pane).await?;
    let snapshot = client.fleet_snapshot(None).await?;
    let live = snapshot
        .hosts
        .into_iter()
        .find(|candidate| candidate.alias == host)
        .context("host disappeared from fleet snapshot")?;
    let node_id = live
        .node_id
        .context("host has not completed its relay handshake")?;
    let target = GlobalPaneRef {
        node_id,
        pane: key,
        agent_session_id: None,
    };
    attach_exact(cfg, host, &target)
}

pub(crate) fn attach_exact(cfg: &Config, host: &str, target: &GlobalPaneRef) -> Result<()> {
    let token = crate::relay::encode_attach_token(target)?;
    let configured = cfg
        .fleet
        .hosts
        .get(host)
        .with_context(|| format!("host '{host}' is not configured locally"))?;
    let mut command = std::process::Command::new("ssh");
    command.args([
        "-t",
        "-o",
        "BatchMode=yes",
        "-o",
        "ClearAllForwardings=yes",
        "--",
        &configured.ssh,
        &configured.muxa_path,
    ]);
    if let Some(socket) = &configured.remote_socket {
        command.arg("--socket").arg(socket);
    }
    let status = command.args(["fleet-remote-attach", &token]).status()?;
    if !status.success() {
        bail!("remote attach exited with {status}");
    }
    Ok(())
}

async fn doctor(cfg: &Config, alias: &str, timeout: Duration) -> Result<()> {
    let host = cfg
        .fleet
        .hosts
        .get(alias)
        .with_context(|| format!("host '{alias}' is not configured"))?;
    println!("1/4 inventory       ok ({})", host.ssh);
    let mut command = Command::new("ssh");
    command
        .kill_on_drop(true)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ClearAllForwardings=yes",
            "--",
            &host.ssh,
            &host.muxa_path,
        ]);
    if let Some(socket) = &host.remote_socket {
        command.arg("--socket").arg(socket);
    }
    command.args(["relay", "--stdio"]);
    let mut child = command.spawn().context("starting OpenSSH")?;
    let stdout = child
        .stdout
        .take()
        .context("OpenSSH stdout is unavailable")?;
    let mut stderr = child
        .stderr
        .take()
        .context("OpenSSH stderr is unavailable")?;
    let stderr_task = tokio::spawn(async move {
        let bytes = drain_bounded(&mut stderr, FLEET_MAX_DIAGNOSTIC_BYTES)
            .await
            .unwrap_or_default();
        sanitize_terminal_text(String::from_utf8_lossy(&bytes).trim())
    });
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let read = tokio::time::timeout(
        timeout,
        read_bounded_line(&mut reader, &mut line, FLEET_MAX_FRAME_BYTES),
    )
    .await
    .map_err(|_| anyhow::anyhow!("SSH/relay handshake timed out after {timeout:?}"))??;
    if read == 0 {
        let _ = child.wait().await;
        let error = stderr_task.await.unwrap_or_default();
        bail!("SSH relay exited before hello: {error}");
    }
    let frame: RelayFrame = serde_json::from_str(line.trim()).context("decoding relay hello")?;
    let RelayFrame::Hello { hello } = frame else {
        bail!("remote did not send a relay hello frame");
    };
    println!("2/4 ssh + host key  ok");
    println!(
        "3/4 remote muxad    ok (muxa {})",
        sanitize_terminal_text(&hello.muxa_version)
    );
    if hello.min_fleet_protocol > FLEET_PROTOCOL_VERSION
        || hello.fleet_protocol < FLEET_PROTOCOL_VERSION
    {
        bail!(
            "fleet protocol mismatch: local={FLEET_PROTOCOL_VERSION}, remote=[{},{}]",
            hello.min_fleet_protocol,
            hello.fleet_protocol
        );
    }
    println!(
        "4/4 fleet protocol ok (node {} · {} · {}/{})",
        hello.node_id,
        sanitize_terminal_text(&hello.hostname),
        sanitize_terminal_text(&hello.os),
        sanitize_terminal_text(&hello.arch)
    );
    let _ = child.kill().await;
    let _ = child.wait().await;
    let _ = stderr_task.await;
    Ok(())
}

fn parse_selector(selector: Option<&str>) -> Result<Option<String>> {
    selector
        .map(|selector| {
            selector
                .parse::<LabelSelector>()
                .map_err(anyhow::Error::msg)?;
            Ok(selector.to_string())
        })
        .transpose()
}

fn inventory_with_live(
    cfg: &Config,
    live: Option<&muxa::FleetSnapshot>,
    selector: Option<&str>,
) -> Result<Vec<FleetHostSnapshot>> {
    let selector = selector
        .map(str::parse::<LabelSelector>)
        .transpose()
        .map_err(anyhow::Error::msg)?;
    Ok(cfg
        .fleet
        .hosts
        .iter()
        .filter(|(_, host)| {
            selector
                .as_ref()
                .is_none_or(|selector| selector.matches(&host.labels))
        })
        .map(|(alias, host)| {
            live.and_then(|snapshot| snapshot.hosts.iter().find(|item| item.alias == *alias))
                .cloned()
                .unwrap_or_else(|| FleetHostSnapshot {
                    alias: alias.clone(),
                    ssh_target: host.ssh.clone(),
                    labels: host.labels.clone(),
                    annotations: host.annotations.clone(),
                    mode: host.mode,
                    state: if host.enabled {
                        FleetHostState::Offline
                    } else {
                        FleetHostState::Disabled
                    },
                    node_id: None,
                    hostname: None,
                    os: None,
                    arch: None,
                    muxa_version: None,
                    protocol: None,
                    capabilities: Vec::new(),
                    daemon_generation: None,
                    boot_id: None,
                    latency_ms: None,
                    last_seen_at: None,
                    received_at: None,
                    error: Some("muxad has not loaded this inventory entry".into()),
                    remote: None,
                })
        })
        .collect())
}

fn print_hosts(hosts: &[FleetHostSnapshot]) {
    let mut table = Table::new();
    table.load_preset(UTF8_BORDERS_ONLY);
    table.set_header([
        "HOST",
        "STATE",
        "MODE",
        "LABELS",
        "AGENTS",
        "ATTN",
        "LAST/ERROR",
    ]);
    for host in hosts {
        let last = host.error.clone().unwrap_or_else(|| {
            host.received_at.map_or_else(
                || "-".into(),
                |time| format!("{}", time.replace_nanosecond(0).unwrap_or(time)),
            )
        });
        table.add_row([
            Cell::new(&host.alias),
            Cell::new(format!("{:?}", host.state).to_lowercase()),
            Cell::new(format!("{:?}", host.mode).to_lowercase()),
            Cell::new(format_map(&host.labels)),
            Cell::new(host.agent_count()),
            Cell::new(host.needs_attention()),
            Cell::new(sanitize_terminal_text(&last)),
        ]);
    }
    println!("{table}");
}

fn format_map(values: &BTreeMap<String, String>) -> String {
    if values.is_empty() {
        "-".into()
    } else {
        values
            .iter()
            .map(|(key, value)| {
                format!(
                    "{}={}",
                    sanitize_terminal_text(key),
                    sanitize_terminal_text(value)
                )
            })
            .collect::<Vec<_>>()
            .join(",")
    }
}

fn parse_pairs(values: &[String], annotation: bool) -> Result<BTreeMap<String, String>> {
    values
        .iter()
        .map(|value| {
            let (key, value) = value
                .split_once('=')
                .with_context(|| format!("metadata '{value}' must be KEY=VALUE"))?;
            validate_label_key(key).map_err(anyhow::Error::msg)?;
            if !annotation {
                validate_label_value(value).map_err(anyhow::Error::msg)?;
            }
            Ok((key.to_string(), value.to_string()))
        })
        .collect()
}

#[allow(clippy::too_many_arguments)] // keeps the shared atomic edit path explicit at both call sites
async fn edit_metadata(
    config_path: Option<&Path>,
    cfg: &Config,
    client: &Client,
    alias: &str,
    field: &str,
    changes: &[String],
    overwrite: bool,
    annotation: bool,
) -> Result<()> {
    let path = config_path.context("no config directory is available on this system")?;
    let current = cfg
        .fleet
        .hosts
        .get(alias)
        .with_context(|| format!("host '{alias}' is not configured"))?;
    if changes.is_empty() {
        let values = if annotation {
            &current.annotations
        } else {
            &current.labels
        };
        println!("{}", format_map(values));
        return Ok(());
    }
    edit_config(path, |document| {
        let table = metadata_table_mut(document, alias, field)?;
        for change in changes {
            if let Some(key) = change.strip_suffix('-').filter(|key| !key.contains('=')) {
                validate_label_key(key).map_err(anyhow::Error::msg)?;
                table.remove(key);
                continue;
            }
            let (key, value) = change
                .split_once('=')
                .with_context(|| format!("metadata '{change}' must be KEY=VALUE or KEY-"))?;
            validate_label_key(key).map_err(anyhow::Error::msg)?;
            if !annotation {
                validate_label_value(value).map_err(anyhow::Error::msg)?;
            }
            if table.contains_key(key) && !overwrite {
                bail!("{field} key '{key}' already exists; pass --overwrite to replace it");
            }
            table.insert(key, toml_edit::value(value));
        }
        Ok(())
    })?;
    println!("updated {field} for host {alias}");
    request_reload(client).await;
    Ok(())
}

async fn set_host_enabled(
    config_path: Option<&Path>,
    cfg: &Config,
    client: &Client,
    alias: &str,
    enabled: bool,
) -> Result<()> {
    let path = config_path.context("no config directory is available on this system")?;
    if !cfg.fleet.hosts.contains_key(alias) {
        bail!("host '{alias}' is not configured");
    }
    edit_config(path, |document| {
        host_table_mut(document, alias)?.insert("enabled", toml_edit::value(enabled));
        Ok(())
    })?;
    println!(
        "{} host {alias}",
        if enabled { "enabled" } else { "disabled" }
    );
    request_reload(client).await;
    Ok(())
}

fn load_document(path: &Path) -> Result<toml_edit::DocumentMut> {
    match std::fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => text
            .parse::<toml_edit::DocumentMut>()
            .with_context(|| format!("parsing {}", path.display())),
        Ok(_) => Ok(toml_edit::DocumentMut::new()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(toml_edit::DocumentMut::new()),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

fn edit_config(
    path: &Path,
    edit: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<()>,
) -> Result<()> {
    let mut document = load_document(path)?;
    edit(&mut document)?;
    let text = document.to_string();
    let parsed: Config = toml::from_str(&text).context("validating updated muxa config")?;
    parsed
        .validate()
        .context("validating updated muxa config")?;
    write_document(path, &text)
}

fn write_document(path: &Path, text: &str) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    let temporary = path.with_file_name(format!(".{file_name}.muxa-{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("creating {}", temporary.display()))?;
    if let Err(error) = (|| -> Result<()> {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })() {
        let _ = std::fs::remove_file(&temporary);
        return Err(error).with_context(|| format!("writing {}", path.display()));
    }
    Ok(())
}

fn fleet_hosts_table_mut(document: &mut toml_edit::DocumentMut) -> Result<&mut toml_edit::Table> {
    if document.get("fleet").is_none() {
        document["fleet"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    let fleet = document["fleet"]
        .as_table_mut()
        .context("[fleet] is not a table")?;
    fleet.insert("enabled", toml_edit::value(true));
    if fleet.get("hosts").is_none() {
        fleet["hosts"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    fleet["hosts"]
        .as_table_mut()
        .context("[fleet.hosts] is not a table")
}

fn host_table_mut<'a>(
    document: &'a mut toml_edit::DocumentMut,
    alias: &str,
) -> Result<&'a mut toml_edit::Table> {
    fleet_hosts_table_mut(document)?
        .get_mut(alias)
        .and_then(toml_edit::Item::as_table_mut)
        .with_context(|| format!("[fleet.hosts.{alias}] is missing or is not a table"))
}

fn metadata_table_mut<'a>(
    document: &'a mut toml_edit::DocumentMut,
    alias: &str,
    field: &str,
) -> Result<&'a mut toml_edit::Table> {
    let host = host_table_mut(document, alias)?;
    if host.get(field).is_none() {
        host[field] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    host[field]
        .as_table_mut()
        .with_context(|| format!("[fleet.hosts.{alias}.{field}] is not a table"))
}

fn upsert_host(
    document: &mut toml_edit::DocumentMut,
    alias: &str,
    host: &FleetHostConfig,
) -> Result<()> {
    let mut table = toml_edit::Table::new();
    table.insert("ssh", toml_edit::value(&host.ssh));
    table.insert("muxa_path", toml_edit::value(&host.muxa_path));
    table.insert("enabled", toml_edit::value(host.enabled));
    table.insert(
        "connect",
        toml_edit::value(match host.connect {
            FleetConnectPolicy::Auto => "auto",
            FleetConnectPolicy::OnDemand => "on_demand",
        }),
    );
    table.insert(
        "mode",
        toml_edit::value(match host.mode {
            HostAccessMode::Observe => "observe",
            HostAccessMode::Control => "control",
        }),
    );
    if let Some(socket) = &host.remote_socket {
        table.insert(
            "remote_socket",
            toml_edit::value(socket.to_string_lossy().as_ref()),
        );
    }
    let mut labels = toml_edit::Table::new();
    for (key, value) in &host.labels {
        labels.insert(key, toml_edit::value(value));
    }
    table.insert("labels", toml_edit::Item::Table(labels));
    let mut annotations = toml_edit::Table::new();
    for (key, value) in &host.annotations {
        annotations.insert(key, toml_edit::value(value));
    }
    table.insert("annotations", toml_edit::Item::Table(annotations));
    fleet_hosts_table_mut(document)?.insert(alias, toml_edit::Item::Table(table));
    Ok(())
}

fn remove_host(document: &mut toml_edit::DocumentMut, alias: &str) -> Result<()> {
    fleet_hosts_table_mut(document)?.remove(alias);
    Ok(())
}

async fn request_reload(client: &Client) {
    match client.restart(Duration::from_secs(2)).await {
        Ok(()) => println!("requested muxad reload"),
        Err(error) => eprintln!(
            "warning: config was saved but muxad could not reload itself ({error}); restart muxad manually"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn inventory_editor_preserves_unrelated_config_and_validates_labels() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "[watch]\nspinner = false\n").unwrap();
        let host = FleetHostConfig {
            ssh: "devbox".into(),
            mode: HostAccessMode::Control,
            labels: BTreeMap::from([("environment".into(), "dev".into())]),
            ..FleetHostConfig::default()
        };
        edit_config(&path, |document| upsert_host(document, "dev", &host)).unwrap();
        let updated = Config::load(&path).unwrap();
        assert!(!updated.watch.spinner);
        assert!(updated.fleet.enabled);
        assert_eq!(updated.fleet.hosts["dev"].labels["environment"], "dev");
    }

    #[test]
    fn pair_parser_accepts_annotation_urls_but_not_label_urls() {
        assert!(parse_pairs(&["docs=https://example.com/a".into()], true).is_ok());
        assert!(parse_pairs(&["docs=https://example.com/a".into()], false).is_err());
    }
}
