//! Host inventory and non-interactive Muxa Fleet commands.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use comfy_table::presets::UTF8_BORDERS_ONLY;
use comfy_table::{Cell, ColumnConstraint, ContentArrangement, Table, Width};
use muxa::config::{FleetConnectPolicy, FleetHostConfig};
use muxa::fleet::{
    drain_bounded, read_bounded_line, sanitize_terminal_text, validate_label_key,
    validate_label_value, FleetHostSnapshot, FleetHostState, FleetOperation, GlobalPaneRef,
    HostAccessMode, LabelSelector, RelayFrame, FLEET_MAX_DIAGNOSTIC_BYTES, FLEET_MAX_FRAME_BYTES,
    FLEET_PROTOCOL_VERSION, LOCAL_HOST_ALIAS, LOCAL_MANAGED_LABELS,
};
use muxa::ipc::Client;
use muxa::{Config, PaneKey};
use tokio::io::BufReader;
use tokio::process::Command;
use unicode_width::UnicodeWidthStr;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
enum HostOutputArg {
    #[default]
    Table,
    Wide,
    Json,
}

#[derive(Debug, Args)]
struct HostTableArgs {
    #[arg(short = 'l', long = "selector")]
    selector: Option<String>,
    /// Output format: table (default), wide, or json.
    #[arg(short = 'o', long = "output", value_enum, value_name = "FORMAT")]
    output: Option<HostOutputArg>,
    /// Backwards-compatible alias for `-o json`.
    #[arg(long, conflicts_with = "output")]
    json: bool,
    /// Append all Kubernetes-style labels to the human table.
    #[arg(long, conflicts_with = "json")]
    show_labels: bool,
    /// Append selected label values as columns, for example `-L environment,region`.
    #[arg(
        short = 'L',
        long = "label-columns",
        value_delimiter = ',',
        value_name = "KEYS",
        conflicts_with = "json"
    )]
    label_columns: Vec<String>,
}

impl HostTableArgs {
    fn output(&self) -> HostOutputArg {
        if self.json {
            HostOutputArg::Json
        } else {
            self.output.unwrap_or_default()
        }
    }
}

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
        #[command(flatten)]
        table: HostTableArgs,
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
        #[command(flatten)]
        table: HostTableArgs,
    },
    /// Open the central host/session/window/pane TUI.
    Watch {
        #[arg(short = 'l', long = "selector")]
        selector: Option<String>,
        /// Show agents without an attached multiplexer pane.
        #[arg(long)]
        include_paneless: bool,
        /// Default hierarchy depth, shared with `muxa watch`.
        #[arg(long, value_enum)]
        view: Option<crate::WatchViewArg>,
        /// Tree or swarm presentation, shared with `muxa watch`.
        #[arg(long, value_enum)]
        layout: Option<crate::WatchLayoutArg>,
        /// One-shot sibling ordering, shared with `muxa watch`.
        #[arg(long, value_enum)]
        sort: Option<crate::WatchSortArg>,
        /// One-shot visual theme override.
        #[arg(long, value_enum)]
        theme: Option<crate::theme::ThemeArg>,
    },
    /// Connect an on-demand host.
    Connect { host: String },
    /// Close the persistent relay for one host.
    Disconnect { host: String },
    /// Force a fresh full snapshot.
    Refresh { host: String },
    /// List panes on a local or remote Fleet host with collision-free keys.
    Panes {
        host: String,
        #[arg(long)]
        json: bool,
    },
    /// Capture one exact pane on a local or remote Fleet host.
    Capture { host: String, pane: String },
    /// Send text to one exact agent pane on a named Fleet host.
    Send {
        host: String,
        pane: String,
        text: String,
        #[arg(long, default_value_t = true)]
        submit: bool,
    },
    /// Attach to one exact pane directly when local, or through a separate SSH TTY.
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
            if alias == LOCAL_HOST_ALIAS {
                bail!(
                    "host alias '{LOCAL_HOST_ALIAS}' is reserved for this node; use `muxa host label local` or `muxa host annotate local`"
                );
            }
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
        HostCommand::List { table } => {
            let selector = parse_selector(table.selector.as_deref())?;
            let live = client.fleet_snapshot(selector.as_deref()).await.ok();
            let hosts = inventory_with_live(cfg, live.as_ref(), selector.as_deref())?;
            match table.output() {
                HostOutputArg::Json => println!("{}", serde_json::to_string_pretty(&hosts)?),
                output => print_hosts(&hosts, output, &table.label_columns, table.show_labels),
            }
            Ok(())
        }
        HostCommand::Show { alias } => {
            if alias == LOCAL_HOST_ALIAS {
                let live = client.fleet_snapshot(None).await.ok().and_then(|snapshot| {
                    snapshot
                        .hosts
                        .into_iter()
                        .find(|item| item.alias == LOCAL_HOST_ALIAS)
                });
                println!("alias:       {LOCAL_HOST_ALIAS}");
                println!("transport:   local (in-process)");
                println!("mode:        Control");
                println!("connect:     Always");
                println!("enabled:     true");
                println!("labels:      {}", format_map(&cfg.fleet.local.labels));
                println!("annotations: {}", format_map(&cfg.fleet.local.annotations));
                if let Some(live) = live {
                    println!("effective:   {}", format_map(&live.labels));
                    print_live_host(&live);
                } else {
                    println!("state:       Offline (muxad Fleet IPC unavailable)");
                }
                return Ok(());
            }
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
                print_live_host(&live);
            }
            Ok(())
        }
        HostCommand::Remove { alias } => {
            reject_local_inventory_mutation(&alias, "removed")?;
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
            reject_local_inventory_mutation(&alias, "enabled or disabled")?;
            set_host_enabled(config_path, cfg, client, &alias, true).await
        }
        HostCommand::Disable { alias } => {
            reject_local_inventory_mutation(&alias, "enabled or disabled")?;
            set_host_enabled(config_path, cfg, client, &alias, false).await
        }
        HostCommand::Doctor {
            alias,
            timeout_secs,
        } => doctor(client, cfg, &alias, Duration::from_secs(timeout_secs)).await,
    }
}

#[allow(clippy::too_many_lines)] // explicit subcommand dispatch remains easiest to audit
pub(crate) async fn run_fleet(
    args: FleetArgs,
    client: &Client,
    cfg: &Config,
    config_path: Option<&Path>,
) -> Result<()> {
    match args.command {
        FleetCommand::Status { table } => {
            let selector = parse_selector(table.selector.as_deref())?;
            let snapshot = client
                .fleet_snapshot(selector.as_deref())
                .await
                .context("reading fleet state from muxad")?;
            match table.output() {
                HostOutputArg::Json => println!("{}", serde_json::to_string_pretty(&snapshot)?),
                output => print_hosts(
                    &snapshot.hosts,
                    output,
                    &table.label_columns,
                    table.show_labels,
                ),
            }
            Ok(())
        }
        FleetCommand::Watch {
            selector,
            include_paneless,
            view,
            layout,
            sort,
            theme,
        } => {
            let selector = parse_selector(selector.as_deref())?;
            crate::cmd_fleet_watch(
                client,
                cfg.clone(),
                config_path.map(Path::to_path_buf),
                selector,
                crate::WatchInvocation {
                    include_paneless,
                    view,
                    layout,
                    sort,
                    theme,
                    caller_pane: None,
                },
            )
            .await
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
    if host == LOCAL_HOST_ALIAS {
        return crate::relay::remote_attach(&token);
    }
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

async fn doctor(client: &Client, cfg: &Config, alias: &str, timeout: Duration) -> Result<()> {
    if alias == LOCAL_HOST_ALIAS {
        return doctor_local(client, timeout).await;
    }
    doctor_remote(cfg, alias, timeout).await
}

async fn doctor_local(client: &Client, timeout: Duration) -> Result<()> {
    println!("1/4 inventory       ok (built-in local node)");
    let hello = tokio::time::timeout(timeout, client.hello(timeout))
        .await
        .map_err(|_| anyhow::anyhow!("local muxad hello timed out after {timeout:?}"))??;
    println!(
        "2/4 local muxad     ok (generation {})",
        hello
            .generation
            .map_or_else(|| "-".into(), |value| value.to_string())
    );
    let snapshot = tokio::time::timeout(timeout, client.fleet_snapshot(None))
        .await
        .map_err(|_| anyhow::anyhow!("local Fleet snapshot timed out after {timeout:?}"))??;
    let local = snapshot
        .hosts
        .into_iter()
        .find(|host| host.local)
        .context("muxad did not publish its local Fleet node")?;
    let node_id = local.node_id.context("local Fleet node has no NodeId")?;
    println!("3/4 local NodeId    ok ({node_id})");
    let remote = local
        .remote
        .context("local Fleet node has no topology snapshot")?;
    println!(
        "4/4 local topology  ok ({} agents · {} panes · revision {})",
        remote.agents.len(),
        remote.panes.len(),
        remote.revision
    );
    Ok(())
}

async fn doctor_remote(cfg: &Config, alias: &str, timeout: Duration) -> Result<()> {
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
    let mut hosts = live.map_or_else(Vec::new, |snapshot| snapshot.hosts.clone());
    if !hosts.iter().any(|host| host.local) {
        let labels = local_inventory_labels(cfg);
        if selector
            .as_ref()
            .is_none_or(|selector| selector.matches(&labels))
        {
            hosts.push(FleetHostSnapshot {
                alias: LOCAL_HOST_ALIAS.into(),
                local: true,
                ssh_target: "local://".into(),
                labels,
                annotations: cfg.fleet.local.annotations.clone(),
                mode: HostAccessMode::Control,
                state: FleetHostState::Offline,
                node_id: None,
                hostname: None,
                os: Some(std::env::consts::OS.into()),
                arch: Some(std::env::consts::ARCH.into()),
                muxa_version: Some(env!("CARGO_PKG_VERSION").into()),
                protocol: Some(FLEET_PROTOCOL_VERSION),
                capabilities: Vec::new(),
                daemon_generation: None,
                boot_id: None,
                latency_ms: Some(0),
                last_seen_at: None,
                received_at: None,
                error: Some("muxad Fleet IPC is unavailable".into()),
                remote: None,
            });
        }
    }
    for (alias, host) in &cfg.fleet.hosts {
        if hosts.iter().any(|item| item.alias == *alias)
            || selector
                .as_ref()
                .is_some_and(|selector| !selector.matches(&host.labels))
        {
            continue;
        }
        hosts.push(FleetHostSnapshot {
            alias: alias.clone(),
            local: false,
            ssh_target: host.ssh.clone(),
            labels: host.labels.clone(),
            annotations: host.annotations.clone(),
            mode: host.mode,
            state: if cfg.fleet.enabled && host.enabled {
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
        });
    }
    hosts.sort_by(|left, right| {
        right
            .local
            .cmp(&left.local)
            .then(left.alias.cmp(&right.alias))
    });
    Ok(hosts)
}

fn local_inventory_labels(cfg: &Config) -> BTreeMap<String, String> {
    let mut labels = cfg.fleet.local.labels.clone();
    labels.insert("muxa.io/local".into(), "true".into());
    labels.insert("muxa.io/transport".into(), "local".into());
    labels.insert("kubernetes.io/os".into(), std::env::consts::OS.into());
    labels.insert("kubernetes.io/arch".into(), std::env::consts::ARCH.into());
    let hostname = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok());
    if let Some(hostname) = hostname.filter(|value| validate_label_value(value).is_ok()) {
        labels.insert("kubernetes.io/hostname".into(), hostname);
    }
    labels
}

fn print_live_host(live: &FleetHostSnapshot) {
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
    if let Some(error) = &live.error {
        println!("error:       {}", sanitize_terminal_text(error));
    }
}

fn reject_local_inventory_mutation(alias: &str, action: &str) -> Result<()> {
    if alias == LOCAL_HOST_ALIAS {
        bail!(
            "the always-present local host cannot be {action}; only its labels and annotations are user-managed"
        );
    }
    Ok(())
}

#[derive(Debug, Clone)]
enum HostColumnKind {
    Host,
    State,
    Mode,
    Inventory,
    Agents,
    Panes,
    Attention,
    Age,
    Hostname,
    Version,
    Latency,
    Label(String),
    Labels,
}

#[derive(Debug, Clone)]
struct HostColumn {
    header: String,
    kind: HostColumnKind,
    width: usize,
    min_width: usize,
}

impl HostColumn {
    fn new(header: impl Into<String>, kind: HostColumnKind, min_width: usize) -> Self {
        let header = header.into();
        let width = UnicodeWidthStr::width(header.as_str());
        Self {
            header,
            kind,
            width,
            min_width,
        }
    }

    fn cap(&self) -> usize {
        match self.kind {
            HostColumnKind::Host | HostColumnKind::Label(_) => 24,
            HostColumnKind::State => 12,
            HostColumnKind::Mode | HostColumnKind::Age | HostColumnKind::Latency => 8,
            HostColumnKind::Inventory => 11,
            HostColumnKind::Agents | HostColumnKind::Panes | HostColumnKind::Attention => 7,
            HostColumnKind::Hostname => 28,
            HostColumnKind::Version => 14,
            HostColumnKind::Labels => 48,
        }
    }
}

fn print_hosts(
    hosts: &[FleetHostSnapshot],
    output: HostOutputArg,
    label_columns: &[String],
    show_labels: bool,
) {
    println!(
        "{}",
        render_hosts(
            hosts,
            output,
            label_columns,
            show_labels,
            crate::terminal_width()
        )
    );
}

fn render_hosts(
    hosts: &[FleetHostSnapshot],
    output: HostOutputArg,
    label_columns: &[String],
    show_labels: bool,
    terminal_width: usize,
) -> String {
    let mut columns = host_columns(output, label_columns, show_labels, terminal_width);
    let now = time::OffsetDateTime::now_utc();
    for column in &mut columns {
        let content_width = hosts
            .iter()
            .map(|host| UnicodeWidthStr::width(host_column_value(host, &column.kind, now).as_str()))
            .max()
            .unwrap_or(0);
        column.width = column.width.max(content_width).min(column.cap());
    }
    fit_host_columns(&mut columns, terminal_width);

    let mut table = Table::new();
    table
        .load_preset(UTF8_BORDERS_ONLY)
        .set_content_arrangement(ContentArrangement::Disabled)
        .set_constraints(columns.iter().map(|column| {
            ColumnConstraint::Absolute(Width::Fixed(
                u16::try_from(column.width).unwrap_or(u16::MAX),
            ))
        }))
        .set_header(
            columns
                .iter()
                .map(|column| Cell::new(crate::truncate_cell(&column.header, column.width))),
        );
    for host in hosts {
        table.add_row(columns.iter().map(|column| {
            Cell::new(crate::truncate_cell(
                &host_column_value(host, &column.kind, now),
                column.width,
            ))
        }));
    }
    format!("{table}")
}

fn host_columns(
    output: HostOutputArg,
    label_columns: &[String],
    show_labels: bool,
    terminal_width: usize,
) -> Vec<HostColumn> {
    let mut columns = if terminal_width < 64 {
        vec![
            HostColumn::new("HOST", HostColumnKind::Host, 8),
            HostColumn::new("STATE", HostColumnKind::State, 6),
            HostColumn::new("A/P", HostColumnKind::Inventory, 5),
            HostColumn::new("ATTN", HostColumnKind::Attention, 4),
            HostColumn::new("AGE", HostColumnKind::Age, 4),
        ]
    } else {
        vec![
            HostColumn::new("HOST", HostColumnKind::Host, 8),
            HostColumn::new("STATE", HostColumnKind::State, 6),
            HostColumn::new("MODE", HostColumnKind::Mode, 4),
            HostColumn::new("AGENTS", HostColumnKind::Agents, 3),
            HostColumn::new("PANES", HostColumnKind::Panes, 3),
            HostColumn::new("ATTN", HostColumnKind::Attention, 4),
            HostColumn::new("AGE", HostColumnKind::Age, 4),
        ]
    };
    if output == HostOutputArg::Wide {
        if terminal_width >= 78 {
            columns.push(HostColumn::new("HOSTNAME", HostColumnKind::Hostname, 8));
        }
        if terminal_width >= 96 {
            columns.push(HostColumn::new("VERSION", HostColumnKind::Version, 6));
        }
        if terminal_width >= 112 {
            columns.push(HostColumn::new("LATENCY", HostColumnKind::Latency, 4));
        }
    }
    columns.extend(label_columns.iter().map(|key| {
        HostColumn::new(
            key.to_ascii_uppercase(),
            HostColumnKind::Label(key.clone()),
            4,
        )
    }));
    if show_labels {
        columns.push(HostColumn::new("LABELS", HostColumnKind::Labels, 8));
    }
    columns
}

fn host_column_value(
    host: &FleetHostSnapshot,
    kind: &HostColumnKind,
    now: time::OffsetDateTime,
) -> String {
    match kind {
        HostColumnKind::Host => sanitize_terminal_text(&host.alias),
        HostColumnKind::State => format!("{:?}", host.state).to_lowercase(),
        HostColumnKind::Mode => format!("{:?}", host.mode).to_lowercase(),
        HostColumnKind::Inventory => format!("{}/{}", host.agent_count(), host.pane_count()),
        HostColumnKind::Agents => host.agent_count().to_string(),
        HostColumnKind::Panes => host.pane_count().to_string(),
        HostColumnKind::Attention => host.needs_attention().to_string(),
        HostColumnKind::Age => host.received_at.map_or_else(
            || "-".into(),
            |seen| {
                let age = crate::relative_time(now, seen);
                let age = age.strip_suffix(" ago").unwrap_or(&age);
                if age == "0s" {
                    "now".into()
                } else {
                    age.into()
                }
            },
        ),
        HostColumnKind::Hostname => sanitize_terminal_text(host.hostname.as_deref().unwrap_or("-")),
        HostColumnKind::Version => {
            sanitize_terminal_text(host.muxa_version.as_deref().unwrap_or("-"))
        }
        HostColumnKind::Latency => host
            .latency_ms
            .map_or_else(|| "-".into(), |latency| format!("{latency}ms")),
        HostColumnKind::Label(key) => host
            .labels
            .get(key)
            .map_or_else(|| "<none>".into(), |value| sanitize_terminal_text(value)),
        HostColumnKind::Labels => format_map(&host.labels),
    }
}

fn fit_host_columns(columns: &mut Vec<HostColumn>, terminal_width: usize) {
    // comfy-table's borders, separators, and one-cell padding consume three
    // characters per column plus the closing border. Drop optional right-most
    // columns only in pathologically narrow terminals where even one visible
    // character per cell would not fit.
    while columns.len() > 1 && columns.len().saturating_mul(4).saturating_add(1) > terminal_width {
        columns.pop();
    }
    let content_budget = terminal_width
        .saturating_sub(columns.len().saturating_mul(3).saturating_add(1))
        .max(columns.len());
    while columns.iter().map(|column| column.width).sum::<usize>() > content_budget {
        let candidate = columns
            .iter()
            .enumerate()
            .filter(|(_, column)| column.width > column.min_width)
            .max_by_key(|(_, column)| column.width - column.min_width)
            .map(|(index, _)| index)
            .or_else(|| {
                columns
                    .iter()
                    .enumerate()
                    .filter(|(_, column)| column.width > 1)
                    .max_by_key(|(_, column)| column.width)
                    .map(|(index, _)| index)
            });
        let Some(index) = candidate else {
            break;
        };
        columns[index].width -= 1;
    }
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
    let current = if alias == LOCAL_HOST_ALIAS {
        if annotation {
            &cfg.fleet.local.annotations
        } else {
            &cfg.fleet.local.labels
        }
    } else {
        let host = cfg
            .fleet
            .hosts
            .get(alias)
            .with_context(|| format!("host '{alias}' is not configured"))?;
        if annotation {
            &host.annotations
        } else {
            &host.labels
        }
    };
    if changes.is_empty() {
        println!("{}", format_map(current));
        return Ok(());
    }
    edit_config(path, |document| {
        let table = metadata_table_mut(document, alias, field)?;
        for change in changes {
            if let Some(key) = change.strip_suffix('-').filter(|key| !key.contains('=')) {
                validate_label_key(key).map_err(anyhow::Error::msg)?;
                if !annotation && alias == LOCAL_HOST_ALIAS && LOCAL_MANAGED_LABELS.contains(&key) {
                    bail!("label key '{key}' is managed by muxad and cannot be changed");
                }
                table.remove(key);
                continue;
            }
            let (key, value) = change
                .split_once('=')
                .with_context(|| format!("metadata '{change}' must be KEY=VALUE or KEY-"))?;
            validate_label_key(key).map_err(anyhow::Error::msg)?;
            if !annotation {
                if alias == LOCAL_HOST_ALIAS && LOCAL_MANAGED_LABELS.contains(&key) {
                    bail!("label key '{key}' is managed by muxad and cannot be changed");
                }
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
    let fleet = fleet_table_mut(document)?;
    fleet.insert("enabled", toml_edit::value(true));
    if fleet.get("hosts").is_none() {
        fleet["hosts"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    fleet["hosts"]
        .as_table_mut()
        .context("[fleet.hosts] is not a table")
}

fn fleet_table_mut(document: &mut toml_edit::DocumentMut) -> Result<&mut toml_edit::Table> {
    if document.get("fleet").is_none() {
        document["fleet"] = toml_edit::Item::Table(toml_edit::Table::new());
    }
    document["fleet"]
        .as_table_mut()
        .context("[fleet] is not a table")
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
    if alias == LOCAL_HOST_ALIAS {
        let fleet = fleet_table_mut(document)?;
        if fleet.get("local").is_none() {
            fleet["local"] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        let local = fleet["local"]
            .as_table_mut()
            .context("[fleet.local] is not a table")?;
        if local.get(field).is_none() {
            local[field] = toml_edit::Item::Table(toml_edit::Table::new());
        }
        return local[field]
            .as_table_mut()
            .with_context(|| format!("[fleet.local.{field}] is not a table"));
    }
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

    fn local_host_for_table() -> FleetHostSnapshot {
        FleetHostSnapshot {
            alias: "local".into(),
            local: true,
            ssh_target: "local://".into(),
            labels: BTreeMap::from([
                ("kubernetes.io/hostname".into(), "june.rtzr.ai".into()),
                ("muxa.io/local".into(), "true".into()),
                ("muxa.io/transport".into(), "local".into()),
            ]),
            annotations: BTreeMap::new(),
            mode: HostAccessMode::Control,
            state: FleetHostState::Online,
            node_id: None,
            hostname: Some("june.rtzr.ai".into()),
            os: Some("linux".into()),
            arch: Some("x86_64".into()),
            muxa_version: Some("0.8.34".into()),
            protocol: Some(FLEET_PROTOCOL_VERSION),
            capabilities: Vec::new(),
            daemon_generation: Some(17),
            boot_id: None,
            latency_ms: Some(0),
            last_seen_at: Some(time::OffsetDateTime::now_utc()),
            received_at: Some(time::OffsetDateTime::now_utc()),
            error: None,
            remote: None,
        }
    }

    #[test]
    fn default_host_table_is_compact_and_hides_labels() {
        let rendered = render_hosts(
            &[local_host_for_table()],
            HostOutputArg::Table,
            &[],
            false,
            80,
        );
        assert!(rendered.contains("HOST"));
        assert!(rendered.contains("AGENTS"));
        assert!(rendered.contains("PANES"));
        assert!(!rendered.contains("muxa.io/local"));
        assert!(
            rendered
                .lines()
                .all(|line| UnicodeWidthStr::width(line) <= 80),
            "{rendered}"
        );
    }

    #[test]
    fn host_table_exposes_wide_and_opt_in_label_views_without_overflow() {
        let host = local_host_for_table();
        let wide = render_hosts(
            std::slice::from_ref(&host),
            HostOutputArg::Wide,
            &[],
            false,
            140,
        );
        assert!(wide.contains("HOSTNAME"));
        assert!(wide.contains("VERSION"));
        assert!(wide.contains("LATENCY"));

        let selected = render_hosts(
            std::slice::from_ref(&host),
            HostOutputArg::Table,
            &["muxa.io/local".into()],
            false,
            92,
        );
        assert!(selected.contains("MUXA.IO/LOCAL"));
        assert!(selected.contains("true"));
        assert!(selected
            .lines()
            .all(|line| UnicodeWidthStr::width(line) <= 92));

        let labels = render_hosts(&[host], HostOutputArg::Table, &[], true, 92);
        assert!(labels.contains("LABELS"));
        assert!(labels
            .lines()
            .all(|line| UnicodeWidthStr::width(line) <= 92));
    }

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

    #[test]
    fn local_inventory_is_visible_without_enabling_remote_fleet() {
        let cfg = Config::default();
        let hosts = inventory_with_live(&cfg, None, None).unwrap();
        assert_eq!(hosts.len(), 1);
        assert!(hosts[0].local);
        assert_eq!(hosts[0].alias, LOCAL_HOST_ALIAS);
        assert_eq!(hosts[0].labels["muxa.io/local"], "true");

        let selected = inventory_with_live(&cfg, None, Some("muxa.io/local=true")).unwrap();
        assert_eq!(selected.len(), 1);
        let excluded = inventory_with_live(&cfg, None, Some("muxa.io/local=false")).unwrap();
        assert!(excluded.is_empty());
    }

    #[test]
    fn editing_local_metadata_does_not_enable_remote_connections() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        edit_config(&path, |document| {
            metadata_table_mut(document, LOCAL_HOST_ALIAS, "labels")?
                .insert("environment", toml_edit::value("development"));
            Ok(())
        })
        .unwrap();
        let updated = Config::load(&path).unwrap();
        assert!(!updated.fleet.enabled);
        assert_eq!(updated.fleet.local.labels["environment"], "development");
    }
}
