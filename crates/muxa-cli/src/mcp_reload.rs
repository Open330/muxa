//! Refresh existing Codex MCP connections through the host API, without restarting it.
use anyhow::{bail, Context, Result};
use clap::Args;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncWrite};
#[cfg(unix)]
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{tungstenite::Message, WebSocketStream};

#[derive(Debug, Args)]
#[allow(clippy::struct_excessive_bools)] // independent CLI switches
pub struct McpReloadArgs {
    /// Codex profile to refresh. Defaults to explicit `CODEX_HOME`; otherwise pass this flag.
    #[arg(long, conflicts_with = "all")]
    codex_home: Option<PathBuf>,
    /// Discover stale owners (all shared owners with --reconnect), refresh their Codex profiles, and explain standalone clients needing reconnect.
    #[arg(long)]
    all: bool,
    /// Rebuild cached Muxa clients even when the binary is unchanged; agent sessions stay running, but in-flight Muxa tool calls can be interrupted.
    #[arg(long)]
    reconnect: bool,
    /// Print the selected scope without sending a reload request.
    #[arg(long)]
    dry_run: bool,
    /// Maximum seconds for each host's complete handshake and reload request.
    #[arg(long, default_value_t = 15, value_parser = clap::value_parser!(u64).range(1..=60))]
    timeout_secs: u64,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Serialize)]
struct HostResult {
    codex_home: PathBuf,
    control_socket: PathBuf,
    state: &'static str,
    detail: String,
    stale_muxa_processes_before: usize,
    stale_muxa_processes_after: Option<usize>,
    binary_revision_update: bool,
    reconnect_requested: bool,
    config_backup: Option<PathBuf>,
    muxa_connections: Option<usize>,
    current_revision_connections: Option<usize>,
    transport_connected: Option<usize>,
}

#[derive(Debug, Serialize)]
struct Report {
    scope: &'static str,
    dry_run: bool,
    hosts: Vec<HostResult>,
    manual_reconnect: Vec<String>,
}

#[derive(Debug)]
struct Rejection(Option<i64>);

impl std::fmt::Display for Rejection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 == Some(-32601) {
            formatter.write_str("Codex does not support this reload API; reconnect in its client")
        } else {
            write!(
                formatter,
                "Codex rejected the request (code {:?}); inspect host diagnostics",
                self.0
            )
        }
    }
}

impl std::error::Error for Rejection {}

fn default_home() -> Result<PathBuf> {
    std::env::var_os("CODEX_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .context("Codex profile is not explicit; pass --codex-home or --all (no ~/.codex guess)")
}

fn control_socket(home: &std::path::Path) -> PathBuf {
    let socket = home.join("app-server-control/app-server-control.sock");
    socket.canonicalize().unwrap_or(socket)
}

fn is_control_socket(path: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        std::fs::metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        false
    }
}

fn stale_count(socket: &std::path::Path, owners: &crate::stale_agents::StaleReport) -> usize {
    owners
        .shared_codex
        .iter()
        .filter(|((_, home), _)| {
            home.as_ref()
                .is_some_and(|home| control_socket(std::path::Path::new(home)) == socket)
        })
        .map(|(_, count)| *count)
        .sum()
}

/// Provider session ids of standalone Codex owners, by owner label. A
/// standalone client is refreshed only by restarting it, and naming its
/// conversation is what makes that a resume rather than a lost session.
type Sessions = std::collections::BTreeMap<String, String>;

async fn standalone_sessions(
    client: &muxa::ipc::Client,
    stale: &crate::stale_agents::StaleReport,
) -> Sessions {
    let mut out = Sessions::new();
    for owner in &stale.standalone_codex {
        // Labels read `<pane> (<session>:<window>.<pane>)`.
        let Some(pane) = owner.split_whitespace().next() else {
            continue;
        };
        let lookup = tokio::time::timeout(Duration::from_secs(2), client.by_pane(pane)).await;
        if let Ok(Ok(agents)) = lookup {
            if let Some(agent) = agents
                .iter()
                .find(|agent| agent.kind == muxa::event::AgentKind::Codex)
            {
                out.insert(owner.clone(), agent.session_id.clone());
            }
        }
    }
    out
}

fn plan(
    args: &McpReloadArgs,
    stale: &crate::stale_agents::StaleReport,
    sessions: &Sessions,
) -> Result<Report> {
    let mut homes = BTreeSet::new();
    let mut manual = Vec::new();
    if args.all {
        for (pid, home) in stale.shared_codex.keys() {
            if let Some(home) = home.as_ref().filter(|home| !home.is_empty()) {
                homes.insert(PathBuf::from(home));
            } else {
                manual.push(format!(
                    "Codex app-server pid {pid}: profile could not be identified; pass --codex-home DIR explicitly"
                ));
            }
        }
        for owner in stale.panes.keys() {
            manual.push(if stale.standalone_codex.contains(owner) {
                // Codex has no in-client MCP reconnect; a standalone CLI picks
                // up the new server only when it starts again.
                match sessions.get(owner) {
                    Some(id) => format!("{owner}: standalone Codex CLI, outside the app-server reload; when it is idle, exit it and start it again the way it was launched with `resume {id}` (e.g. `codex resume {id}`)"),
                    None => format!("{owner}: standalone Codex CLI, outside the app-server reload; when it is idle, exit it and resume its conversation (`codex resume`)"),
                }
            } else { format!("{owner}: reconnect MCP in its agent client") });
        }
        if stale.other > 0 {
            manual.push(format!(
                "{} unidentified MCP owner(s): reconnect in their client",
                stale.other
            ));
        }
    } else {
        homes.insert(args.codex_home.clone().map_or_else(default_home, Ok)?);
    }
    let mut sockets = BTreeSet::new();
    Ok(Report {
        scope: "reload configured MCP state; verify registered Muxa connections on each selected Codex profile",
        dry_run: args.dry_run,
        hosts: homes
            .into_iter()
            .map(|codex_home| {
                let socket = control_socket(&codex_home);
                HostResult {
                    stale_muxa_processes_before: stale_count(&socket, stale),
                    stale_muxa_processes_after: None,
                    binary_revision_update: false,
                    reconnect_requested: args.reconnect,
                    config_backup: None,
                    muxa_connections: None,
                    current_revision_connections: None,
                    transport_connected: None,
                    control_socket: socket,
                    codex_home,
                    state: "planned",
                    detail: "config/mcpServer/reload via the existing app-server control socket"
                        .into(),
                }
            })
            .filter(|host| sockets.insert(host.control_socket.clone()))
            .collect(),
        manual_reconnect: manual,
    })
}

fn plan_inventory(
    args: &McpReloadArgs,
    owners: &crate::stale_agents::McpOwnerInventory,
    sessions: &Sessions,
) -> Result<Report> {
    let mut selected = owners.stale.clone();
    if args.all && args.reconnect {
        // A live executable revision says nothing about a host's cached
        // transport. Do not require an old image before selecting its host.
        selected.shared_codex.clone_from(&owners.all.shared_codex);
    }
    let mut report = plan(args, &selected, sessions)?;
    for host in &mut report.hosts {
        host.stale_muxa_processes_before = stale_count(&host.control_socket, &owners.stale);
    }
    Ok(report)
}

async fn refresh_host(
    mut host: HostResult,
    binary: &std::path::Path,
    revision: &str,
    args: &McpReloadArgs,
) -> HostResult {
    let update = match crate::mcp_revision::RevisionPlan::prepare_refresh(
        &host.codex_home,
        binary,
        revision,
        args.reconnect,
    ) {
        Ok(update) => update,
        Err(error) => {
            host.state = "unavailable";
            host.detail = error.to_string();
            return host;
        }
    };
    host.binary_revision_update = update.as_ref().is_some_and(|plan| plan.revision_changed);
    if args.dry_run {
        return host;
    }
    let started = Instant::now();
    let (state, detail) = reload_host(
        &host.codex_home,
        &host.control_socket,
        Duration::from_secs(args.timeout_secs),
        update.as_ref(),
        &mut host.config_backup,
    )
    .await;
    host.state = state;
    host.detail = detail;
    if state == "queued" {
        match verify_host(
            &host.control_socket,
            revision,
            Duration::from_secs(args.timeout_secs).saturating_sub(started.elapsed()),
        )
        .await
        {
            Ok((total, connected, current)) => {
                host.muxa_connections = Some(total);
                host.transport_connected = Some(connected);
                host.current_revision_connections = Some(current);
                if total > 0 && current == total {
                    host.state = "reloaded";
                    host.detail = format!(
                        "{current} loaded Muxa connection(s) report the installed binary revision"
                    );
                } else if total > current {
                    host.detail = format!("refresh accepted; transport connected {connected}/{total}, host-cached revision current {current}/{total}. Cached handshake data can lag a live process upgrade; use --reconnect for closed/stale cached clients without restarting agent sessions");
                }
            }
            Err(_) => {
                host.detail = "refresh accepted; connection verification is unavailable, so application is not confirmed".into();
            }
        }
    }
    host
}

async fn refresh_hosts(
    hosts: Vec<HostResult>,
    binary: &std::path::Path,
    revision: &str,
    args: &McpReloadArgs,
) -> Vec<HostResult> {
    // Different profiles can symlink the same config. Serialize those writers
    // so their version-checked host API updates cannot race each other.
    let mut locks = std::collections::BTreeMap::new();
    let hosts: Vec<_> = hosts
        .into_iter()
        .map(|host| {
            let path = host.codex_home.join("config.toml");
            let key = path.canonicalize().unwrap_or(path);
            let lock = locks
                .entry(key)
                .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
                .clone();
            (host, lock)
        })
        .collect();
    futures_util::stream::iter(hosts)
        .map(|(host, lock)| async move {
            let _guard = lock.lock().await;
            refresh_host(host, binary, revision, args).await
        })
        .buffered(4)
        .collect()
        .await
}

pub async fn run(client: &muxa::ipc::Client, args: McpReloadArgs) -> Result<()> {
    let binary = crate::stale_agents::installed_muxa()
        .context("cannot locate installed muxa to discover stale owners")?;
    let owners = crate::stale_agents::scan_inventory(&binary);
    let sessions = if args.all {
        standalone_sessions(client, &owners.stale).await
    } else {
        Sessions::new()
    };
    let mut report = plan_inventory(&args, &owners, &sessions)?;
    let revision = crate::mcp_revision::binary_revision(&binary)?;
    report.hosts = refresh_hosts(report.hosts, &binary, &revision, &args).await;
    if !args.dry_run {
        let after = crate::stale_agents::scan(&binary);
        for host in &mut report.hosts {
            host.stale_muxa_processes_after = Some(stale_count(&host.control_socket, &after));
        }
    }
    if args.json {
        println!("{}", serde_json::to_string(&report)?);
    } else {
        println!("MCP reload scope: {}", report.scope);
        for host in &report.hosts {
            println!(
                "{}  {} — {}",
                host.state,
                host.codex_home.display(),
                host.detail
            );
        }
        if report.hosts.is_empty() && !report.manual_reconnect.is_empty() {
            println!("No shared Codex profile needed a refresh.");
        }
        if !report.manual_reconnect.is_empty() {
            println!("Needs a manual reconnect:");
        }
        for owner in &report.manual_reconnect {
            println!("manual  {owner}");
        }
        if report.hosts.is_empty() && report.manual_reconnect.is_empty() {
            println!("No stale MCP owners were discovered.");
        }
    }
    if !args.dry_run {
        require_acknowledged(&report)?;
        if !report.manual_reconnect.is_empty() {
            // Everything muxa can refresh was refreshed; what is left is a
            // client only its owner can restart. That is unfinished, not a
            // failure: exit 2, the same as a refresh still pending.
            eprintln!(
                "{} MCP owner(s) still run an older muxa and need the reconnect listed above",
                report.manual_reconnect.len()
            );
            std::process::exit(2);
        }
    }
    if !args.dry_run && pending_refresh(&report) {
        // A queued API acknowledgement does not mean old tool connections were replaced.
        std::process::exit(2);
    }
    Ok(())
}

fn require_acknowledged(report: &Report) -> Result<()> {
    let failed = report
        .hosts
        .iter()
        .filter(|host| !matches!(host.state, "queued" | "reloaded"))
        .count();
    if failed > 0 {
        bail!("{failed} host request(s) were not acknowledged; {} owner(s) need manual reconnect; inspect the per-profile result", report.manual_reconnect.len());
    }
    Ok(())
}

fn pending_refresh(report: &Report) -> bool {
    report.hosts.iter().any(|host| {
        host.state == "queued"
            && (host.muxa_connections.is_none()
                || host.current_revision_connections != host.muxa_connections)
    })
}

async fn reload_host(
    home: &std::path::Path,
    socket: &std::path::Path,
    timeout: Duration,
    revision: Option<&crate::mcp_revision::RevisionPlan>,
    backup: &mut Option<PathBuf>,
) -> (&'static str, String) {
    if !home.is_dir() {
        return (
            "unavailable",
            "Codex profile directory does not exist; pass an existing --codex-home".into(),
        );
    }
    if !is_control_socket(socket) {
        return (
            "unavailable",
            "existing Codex control socket is unavailable; no app-server was started".into(),
        );
    }
    let mut submitted = false;
    let mut revision_written = false;
    let result = tokio::time::timeout(timeout, async {
        #[cfg(unix)]
        {
            let stream = tokio::net::UnixStream::connect(socket).await?;
            let (mut connection, _) = tokio_tungstenite::client_async_with_config(
                "ws://localhost/",
                stream,
                Some(
                    WebSocketConfig::default()
                        .max_message_size(Some(1024 * 1024))
                        .max_frame_size(Some(1024 * 1024)),
                ),
            )
            .await?;
            exchange(
                &mut connection,
                &mut submitted,
                revision,
                backup,
                &mut revision_written,
            )
            .await
        }
        // Codex exposes its control socket on Unix only; nothing on this
        // platform could have been submitted or written.
        #[cfg(not(unix))]
        {
            let _ = (&mut submitted, &mut revision_written, revision, backup);
            bail!("local Codex control socket reload is unsupported on this platform")
        }
    })
    .await;
    let (state, detail) = match result {
        Ok(Ok(())) => ("queued", "host accepted the refresh request; verify updated MCP tools at the next host refresh boundary".into()),
        Ok(Err(error)) => (failure_state(&error, submitted), error.to_string()),
        Err(_) => (if submitted { "unknown" } else { "unavailable" },
            if submitted { "reload was submitted but no acknowledgement arrived before the deadline; verify before retrying" }
            else { "Codex control connection did not complete initialization before the deadline" }.into()),
    };
    if revision_written && state != "queued" {
        (
            state,
            format!("binary revision was written; reload is unconfirmed: {detail}"),
        )
    } else {
        (state, detail)
    }
}

fn failure_state(error: &anyhow::Error, submitted: bool) -> &'static str {
    if let Some(rejection) = error.downcast_ref::<Rejection>() {
        if rejection.0 == Some(-32601) {
            "unsupported"
        } else {
            "rejected"
        }
    } else if submitted {
        "unknown"
    } else {
        "unavailable"
    }
}

// The WebSocket exchange runs over Codex's Unix control socket only.
#[cfg_attr(not(unix), allow(dead_code))]
async fn send<S: AsyncRead + AsyncWrite + Unpin>(
    connection: &mut WebSocketStream<S>,
    value: Value,
) -> Result<()> {
    connection
        .send(Message::Text(serde_json::to_string(&value)?.into()))
        .await?;
    Ok(())
}

#[cfg_attr(not(unix), allow(dead_code))]
async fn response<S: AsyncRead + AsyncWrite + Unpin>(
    connection: &mut WebSocketStream<S>,
    id: u64,
) -> Result<Value> {
    while let Some(message) = connection.next().await {
        let message = message?;
        let Message::Text(text) = message else {
            continue;
        };
        let mut value: Value = serde_json::from_str(&text)?;
        if value.get("method").is_none() && value.get("id") == Some(&json!(id)) {
            if let Some(error) = value.get("error") {
                return Err(Rejection(error.get("code").and_then(Value::as_i64)).into());
            }
            if !value
                .as_object()
                .is_some_and(|value| value.contains_key("result"))
            {
                bail!("Codex response has neither result nor error");
            }
            return Ok(value["result"].take());
        }
    }
    bail!("Codex control connection closed before acknowledging the request")
}

#[cfg_attr(not(unix), allow(dead_code))]
async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    connection: &mut WebSocketStream<S>,
    submitted: &mut bool,
    revision: Option<&crate::mcp_revision::RevisionPlan>,
    backup: &mut Option<PathBuf>,
    revision_written: &mut bool,
) -> Result<()> {
    send(
        connection,
        json!({"id":1,"method":"initialize","params":{
        "clientInfo":{"name":"muxa_mcp_reload","version":env!("CARGO_PKG_VERSION")}}}),
    )
    .await?;
    response(connection, 1).await?;
    send(connection, json!({"method":"initialized"})).await?;
    let id = if let Some(revision) = revision {
        send(
            connection,
            json!({"id":2,"method":"config/read","params":{"includeLayers":true}}),
        )
        .await?;
        let config = response(connection, 2).await?;
        let expected = revision.expected_version(&config)?;
        *backup = Some(revision.backup()?);
        *submitted = true;
        send(connection, json!({"id":3,"method":"config/value/write","params":{
            "keyPath":if revision.environment.is_some() {"mcp_servers.muxa.env".to_owned()} else {format!("mcp_servers.muxa.env.{}", crate::mcp_revision::ENV_KEY)},
            "value":revision.environment.clone().unwrap_or_else(||json!(revision.revision)),"mergeStrategy":"upsert","filePath":revision.path,"expectedVersion":expected
        }})).await?;
        let written = response(connection, 3).await?;
        if written["status"] != "ok" {
            bail!("Muxa revision is overridden by another configuration layer; reconnect that entry manually");
        }
        *revision_written = true;
        if revision.prune_backups().is_err() {
            eprintln!("MCP config backup cleanup failed; existing backups were preserved");
        }
        4
    } else {
        2
    };
    // Installed Codex schema specifies null/unit params, not an empty object.
    *submitted = true;
    send(
        connection,
        json!({"id":id,"method":"config/mcpServer/reload","params":null}),
    )
    .await?;
    response(connection, id).await?;
    Ok(())
}

#[cfg(unix)]
async fn verify_host(
    socket: &std::path::Path,
    revision: &str,
    timeout: Duration,
) -> Result<(usize, usize, usize)> {
    let mut observed = None;
    let outcome = tokio::time::timeout(timeout, async {
        let stream = tokio::net::UnixStream::connect(socket).await?;
        let (mut connection, _) = tokio_tungstenite::client_async_with_config("ws://localhost/", stream,
            Some(WebSocketConfig::default().max_message_size(Some(1024 * 1024)).max_frame_size(Some(1024 * 1024)))).await?;
        send(&mut connection, json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"muxa_reload_verify","version":env!("CARGO_PKG_VERSION")}}})).await?;
        response(&mut connection, 1).await?;
        send(&mut connection, json!({"method":"initialized"})).await?;
        let tag = crate::mcp_revision::revision_tag(revision).context("invalid installed binary revision")?;
        let suffix = format!("+{tag}");
        let mut id = 2;
        loop {
        let (mut total, mut connected, mut current) = (0, 0, 0);
        let mut cursor = Value::Null;
        loop {
            send(&mut connection, json!({"id":id,"method":"thread/loaded/list","params":{"limit":100,"cursor":cursor}})).await?;
            let page = response(&mut connection, id).await?;
            id += 1;
            let threads = page.get("data").and_then(Value::as_array).context("Codex did not return loaded thread IDs")?;
            for thread in threads {
                let thread = thread.as_str().context("invalid loaded thread ID")?;
                send(&mut connection, json!({"id":id,"method":"mcpServerStatus/list","params":{"threadId":thread,"serverName":"muxa","detail":"toolsAndAuthOnly","limit":1}})).await?;
                let status = response(&mut connection, id).await?;
                id += 1;
                for server in status.get("data").and_then(Value::as_array).context("Codex did not return MCP status data")? {
                    if server["name"] != "muxa" || server["runtimeStatus"] == "disabled" { continue; }
                    total += 1;
                    if server["runtimeStatus"]=="connected" {connected+=1;}
                    if server["runtimeStatus"] == "connected"
                        && server.pointer("/serverInfo/version").and_then(Value::as_str).is_some_and(|version| version.ends_with(&suffix))
                        && server.pointer("/tools/muxa_peer_report/inputSchema/properties/full").is_some() { current += 1; }
                }
            }
            cursor = page.get("nextCursor").cloned().unwrap_or(Value::Null);
            if cursor.is_null() { break; }
        }
        observed = Some((total, connected, current));
        if current == total { return Ok::<_, anyhow::Error>((total, connected, current)); }
        tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }).await;
    match outcome {
        Ok(Ok(counts)) => Ok(counts),
        Ok(Err(error)) => observed.ok_or(error),
        Err(_) => observed
            .context("connection verification deadline reached before observing connections"),
    }
}

#[cfg(not(unix))]
#[allow(clippy::unused_async)] // same signature as the Unix arm
async fn verify_host(
    _socket: &std::path::Path,
    _revision: &str,
    _timeout: Duration,
) -> Result<(usize, usize, usize)> {
    bail!("MCP connection verification requires a Unix control socket")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> McpReloadArgs {
        McpReloadArgs {
            codex_home: Some("/profiles/current".into()),
            all: false,
            reconnect: false,
            dry_run: true,
            timeout_secs: 2,
            json: true,
        }
    }

    #[test]
    fn reconnect_all_selects_current_shared_owners_without_calling_them_stale() {
        let mut all = crate::stale_agents::StaleReport::default();
        all.shared_codex
            .insert((42, Some("/profiles/current".into())), 3);
        all.panes.insert("current Claude".into(), 1);
        let owners = crate::stale_agents::McpOwnerInventory {
            stale: crate::stale_agents::StaleReport::default(),
            all,
        };
        let mut options = McpReloadArgs {
            all: true,
            codex_home: None,
            ..args()
        };
        assert!(plan_inventory(&options, &owners, &Sessions::new())
            .unwrap()
            .hosts
            .is_empty());
        options.reconnect = true;
        let report = plan_inventory(&options, &owners, &Sessions::new()).unwrap();
        assert_eq!(report.hosts.len(), 1);
        assert_eq!(report.hosts[0].stale_muxa_processes_before, 0);
        assert!(report.hosts[0].reconnect_requested);
        assert!(
            report.manual_reconnect.is_empty(),
            "current non-shared owners are not falsely marked stale"
        );
    }

    #[test]
    fn acknowledgement_with_retained_processes_is_pending_not_finished() {
        let owners = crate::stale_agents::StaleReport::default();
        let mut report = plan(&args(), &owners, &Sessions::new()).unwrap();
        let host = &mut report.hosts[0];
        host.state = "queued";
        host.muxa_connections = Some(3);
        host.current_revision_connections = Some(0);
        assert!(pending_refresh(&report));
        report.hosts[0].current_revision_connections = Some(3);
        assert!(!pending_refresh(&report));
        report.hosts[0].state = "unknown";
        report.hosts[0].stale_muxa_processes_after = Some(3);
        assert!(!pending_refresh(&report)); // failures retain their nonzero error path
    }

    #[cfg(unix)]
    #[test]
    fn different_profiles_pointing_to_same_socket_are_reloaded_once() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("server.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let mut stale = crate::stale_agents::StaleReport::default();
        for (pid, name) in [(1, "a"), (2, "b")] {
            let home = directory.path().join(name);
            std::fs::create_dir_all(home.join("app-server-control")).unwrap();
            std::os::unix::fs::symlink(
                &socket,
                home.join("app-server-control/app-server-control.sock"),
            )
            .unwrap();
            stale
                .shared_codex
                .insert((pid, Some(home.to_string_lossy().into_owned())), 1);
        }
        let report = plan(
            &McpReloadArgs {
                all: true,
                codex_home: None,
                ..args()
            },
            &stale,
            &Sessions::new(),
        )
        .unwrap();
        assert_eq!(report.hosts.len(), 1);
        assert_eq!(
            report.hosts[0].control_socket,
            socket.canonicalize().unwrap()
        );
        assert!(is_control_socket(&report.hosts[0].control_socket));
    }

    #[tokio::test]
    async fn missing_socket_does_not_start_a_host_or_claim_success() {
        let directory = tempfile::tempdir().unwrap();
        let socket = control_socket(directory.path());
        let (state, detail) = reload_host(
            directory.path(),
            &socket,
            Duration::from_secs(1),
            None,
            &mut None,
        )
        .await;
        assert_eq!(state, "unavailable");
        assert!(detail.contains("no app-server was started"));
        assert!(!socket.exists());
    }

    #[test]
    fn scope_is_explicit_and_all_deduplicates_without_guessing_unknown_profiles() {
        let mut stale = crate::stale_agents::StaleReport::default();
        stale
            .shared_codex
            .insert((1, Some("/profiles/a".into())), 3);
        stale
            .shared_codex
            .insert((2, Some("/profiles/a".into())), 2);
        stale.shared_codex.insert((3, None), 1);
        stale.panes.insert("claude@%9".into(), 1);
        stale.panes.insert("codex@%10".into(), 1);
        stale.standalone_codex.insert("codex@%10".into());
        let single = plan(&args(), &stale, &Sessions::new()).unwrap();
        assert_eq!(
            single.hosts[0].codex_home,
            PathBuf::from("/profiles/current")
        );
        assert!(single.manual_reconnect.is_empty());
        let all = plan(
            &McpReloadArgs {
                all: true,
                codex_home: None,
                ..args()
            },
            &stale,
            &Sessions::from([("codex@%10".to_owned(), "019a-thread".to_owned())]),
        )
        .unwrap();
        assert_eq!(all.hosts.len(), 1);
        assert_eq!(all.hosts[0].codex_home, PathBuf::from("/profiles/a"));
        assert_eq!(all.manual_reconnect.len(), 3);
        assert!(all
            .manual_reconnect
            .iter()
            .any(|message| message.contains("standalone Codex CLI")
                && message.contains("when it is idle")
                && message.contains("resume 019a-thread")));
        assert!(all.scope.contains("registered Muxa connections"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn reload_profiles_overlap_with_a_four_host_limit_and_stable_order() {
        check_reload_parallelism(false).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn profiles_sharing_a_config_do_not_race_their_config_writers() {
        check_reload_parallelism(true).await;
    }

    #[cfg(unix)]
    async fn check_reload_parallelism(shared_config: bool) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let directory = tempfile::tempdir().unwrap();
        let limit = if shared_config { 1 } else { 4 };
        let barrier = Arc::new(tokio::sync::Barrier::new(limit));
        let shared_path = directory.path().join("shared-config.toml");
        std::fs::write(&shared_path, "").unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut hosts = Vec::new();
        let mut servers = Vec::new();
        for index in 0..8 {
            let home = directory.path().join(index.to_string());
            std::fs::create_dir(&home).unwrap();
            if shared_config {
                std::os::unix::fs::symlink(&shared_path, home.join("config.toml")).unwrap();
            }
            let socket = directory.path().join(format!("{index}.sock"));
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let (barrier, active, peak) = (barrier.clone(), active.clone(), peak.clone());
            servers.push(tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut connection = tokio_tungstenite::accept_async(stream).await.unwrap();
                let _ = connection.next().await.unwrap().unwrap();
                send(&mut connection, json!({"id":1,"result":{}}))
                    .await
                    .unwrap();
                let _ = connection.next().await.unwrap().unwrap(); // initialized
                let request = connection.next().await.unwrap().unwrap();
                assert!(request
                    .to_text()
                    .unwrap()
                    .contains("config/mcpServer/reload"));
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                barrier.wait().await; // Independent profiles must reach this concurrently.
                tokio::time::sleep(Duration::from_millis(10)).await;
                active.fetch_sub(1, Ordering::SeqCst);
                send(&mut connection, json!({"id":2,"error":{"code":-32601}}))
                    .await
                    .unwrap();
            }));
            let mut options = args();
            options.codex_home = Some(home);
            let mut host = plan(
                &options,
                &crate::stale_agents::StaleReport::default(),
                &Sessions::new(),
            )
            .unwrap()
            .hosts
            .remove(0);
            host.control_socket = socket;
            hosts.push(host);
        }
        let mut options = args();
        options.dry_run = false;
        let results = tokio::time::timeout(
            Duration::from_secs(5),
            refresh_hosts(hosts, &directory.path().join("muxa"), "revision", &options),
        )
        .await
        .unwrap();
        assert_eq!(peak.load(Ordering::SeqCst), limit);
        for (index, result) in results.iter().enumerate() {
            assert_eq!(result.codex_home, directory.path().join(index.to_string()));
            assert_eq!(result.state, "unsupported");
        }
        for server in servers {
            server.await.unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn live_socket_protocol_distinguishes_queue_rejection_and_lost_ack() {
        for (reply, state) in [
            (Some(json!({"id":2,"result":{}})), "queued"),
            (Some(json!({"id":2,"error":{"code":-32601}})), "unsupported"),
            (Some(json!({"id":2,"error":{"code":-32602}})), "rejected"),
            (None, "unknown"),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("server.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let server = tokio::spawn(async move {
                let (stream, _) = listener.accept().await.unwrap();
                let mut connection = tokio_tungstenite::accept_async(stream).await.unwrap();
                let request: Value = serde_json::from_str(
                    connection.next().await.unwrap().unwrap().to_text().unwrap(),
                )
                .unwrap();
                assert_eq!(request["method"], "initialize");
                send(
                    &mut connection,
                    json!({"id":1,"method":"approval/request","params":{}}),
                )
                .await
                .unwrap();
                send(&mut connection, json!({"id":99,"result":{}}))
                    .await
                    .unwrap();
                send(&mut connection, json!({"id":1,"result":{}}))
                    .await
                    .unwrap();
                let notification: Value = serde_json::from_str(
                    connection.next().await.unwrap().unwrap().to_text().unwrap(),
                )
                .unwrap();
                assert_eq!(notification["method"], "initialized");
                let request: Value = serde_json::from_str(
                    connection.next().await.unwrap().unwrap().to_text().unwrap(),
                )
                .unwrap();
                assert_eq!(request["method"], "config/mcpServer/reload");
                assert!(request["params"].is_null());
                if let Some(reply) = reply {
                    send(&mut connection, reply).await.unwrap();
                } else {
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                }
            });
            let (actual, _) = reload_host(
                directory.path(),
                &socket,
                Duration::from_millis(500),
                None,
                &mut None,
            )
            .await;
            assert_eq!(actual, state);
            server.await.unwrap();
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn oversized_host_message_fails_before_reload_submission() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("server.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut connection = tokio_tungstenite::accept_async(stream).await.unwrap();
            let request = connection.next().await.unwrap().unwrap();
            assert!(request.to_text().unwrap().contains("initialize"));
            let _ = connection
                .send(Message::Text(" ".repeat(1024 * 1024 + 1).into()))
                .await;
        });
        let (state, detail) = reload_host(
            directory.path(),
            &socket,
            Duration::from_secs(2),
            None,
            &mut None,
        )
        .await;
        assert_eq!(state, "unavailable");
        assert!(detail.contains("too long"), "{detail}");
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn verification_counts_current_connections_without_requiring_old_processes_to_exit() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("verify.sock");
        let revision = format!("sha256:{}", "a".repeat(64));
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut connection = tokio_tungstenite::accept_async(stream).await.unwrap();
            // Initialization, initialized notification, loaded IDs, then two status reads.
            let init = connection.next().await.unwrap().unwrap();
            assert!(init.to_text().unwrap().contains("initialize"));
            send(&mut connection, json!({"id":1,"result":{}}))
                .await
                .unwrap();
            connection.next().await.unwrap().unwrap();
            connection.next().await.unwrap().unwrap();
            send(
                &mut connection,
                json!({"id":2,"result":{"data":["current","legacy"],"nextCursor":null}}),
            )
            .await
            .unwrap();
            for (id, version) in [(3, "0.8.57+aaaaaaaaaaaaaaaa"), (4, "0.8.57")] {
                let request: Value = serde_json::from_str(
                    connection.next().await.unwrap().unwrap().to_text().unwrap(),
                )
                .unwrap();
                assert_eq!(request["method"], "mcpServerStatus/list");
                assert_eq!(request["params"]["serverName"], "muxa");
                assert_eq!(request["params"]["detail"], "toolsAndAuthOnly");
                send(&mut connection, json!({"id":id,"result":{"data":[{
                    "name":"muxa","runtimeStatus":"connected","serverInfo":{"version":version},
                    "tools":{"muxa_peer_report":{"inputSchema":{"properties":{"full":{"type":"boolean"}}}}}
                }]}})).await.unwrap();
            }
        });
        assert_eq!(
            verify_host(&socket, &revision, Duration::from_secs(2))
                .await
                .unwrap(),
            (2, 2, 1)
        );
        server.await.unwrap();
        let mut report = plan(
            &args(),
            &crate::stale_agents::StaleReport::default(),
            &Sessions::new(),
        )
        .unwrap();
        report.hosts[0].state = "reloaded";
        report.hosts[0].stale_muxa_processes_after = Some(10);
        report.hosts[0].muxa_connections = Some(2);
        report.hosts[0].current_revision_connections = Some(2);
        assert!(!pending_refresh(&report));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stalled_websocket_handshake_is_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("server.sock");
        let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (state, _) = reload_host(
            directory.path(),
            &socket,
            Duration::from_millis(20),
            None,
            &mut None,
        )
        .await;
        assert_eq!(state, "unavailable");
    }
}
