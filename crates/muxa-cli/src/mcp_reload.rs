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
use tokio_tungstenite::{
    tungstenite::{protocol::WebSocketConfig, Message},
    WebSocketStream,
};

#[derive(Debug, Args)]
pub struct McpReloadArgs {
    /// Codex profile to refresh. Defaults to explicit `CODEX_HOME`; otherwise pass this flag.
    #[arg(long, conflicts_with = "all")]
    codex_home: Option<PathBuf>,
    /// Refresh every discovered stale shared Codex profile; report other owners needing manual reconnect.
    #[arg(long)]
    all: bool,
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
    config_backup: Option<PathBuf>,
    muxa_connections: Option<usize>,
    current_revision_connections: Option<usize>,
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

fn plan(args: &McpReloadArgs, stale: &crate::stale_agents::StaleReport) -> Result<Report> {
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
            manual.push(format!("{owner}: reconnect MCP in its agent client"));
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
                    config_backup: None,
                    muxa_connections: None,
                    current_revision_connections: None,
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

pub async fn run(args: McpReloadArgs) -> Result<()> {
    let binary = crate::stale_agents::installed_muxa()
        .context("cannot locate installed muxa to discover stale owners")?;
    let owners = crate::stale_agents::scan(&binary);
    let mut report = plan(&args, &owners)?;
    let revision = crate::mcp_revision::binary_revision(&binary)?;
    for host in &mut report.hosts {
        let update = match crate::mcp_revision::RevisionPlan::prepare(
            &host.codex_home,
            &binary,
            &revision,
        ) {
            Ok(update) => update,
            Err(error) => {
                host.state = "unavailable";
                host.detail = error.to_string();
                continue;
            }
        };
        host.binary_revision_update = update.is_some();
        if args.dry_run {
            continue;
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
                &revision,
                Duration::from_secs(args.timeout_secs).saturating_sub(started.elapsed()),
            )
            .await
            {
                Ok((total, current)) => {
                    host.muxa_connections = Some(total);
                    host.current_revision_connections = Some(current);
                    if total > 0 && current == total {
                        host.state = "reloaded";
                        host.detail = format!("{current} loaded Muxa connection(s) report the installed binary revision");
                    } else if total > current {
                        host.detail = format!("refresh accepted; {current}/{total} loaded Muxa connection(s) report the current revision; finish active steps or reconnect in the client");
                    }
                }
                Err(_) => {
                    host.detail = "refresh accepted; connection verification is unavailable, so application is not confirmed".into();
                }
            }
        }
    }
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
        for owner in &report.manual_reconnect {
            println!("manual  {owner}");
        }
        if report.hosts.is_empty() && report.manual_reconnect.is_empty() {
            println!("No stale MCP owners were discovered.");
        }
    }
    if !args.dry_run {
        let failed = report
            .hosts
            .iter()
            .filter(|host| !matches!(host.state, "queued" | "reloaded"))
            .count();
        if failed > 0 || !report.manual_reconnect.is_empty() {
            bail!("{failed} host request(s) were not acknowledged; {} owner(s) need manual reconnect; inspect the per-profile result", report.manual_reconnect.len());
        }
    }
    if !args.dry_run && pending_refresh(&report) {
        // A queued API acknowledgement does not mean old tool connections were replaced.
        std::process::exit(2);
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
        #[cfg(not(unix))]
        {
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

async fn send<S: AsyncRead + AsyncWrite + Unpin>(
    connection: &mut WebSocketStream<S>,
    value: Value,
) -> Result<()> {
    connection
        .send(Message::Text(serde_json::to_string(&value)?.into()))
        .await?;
    Ok(())
}

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
            "keyPath":format!("mcp_servers.muxa.env.{}", crate::mcp_revision::ENV_KEY),
            "value":revision.revision,"mergeStrategy":"upsert","filePath":revision.path,"expectedVersion":expected
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
) -> Result<(usize, usize)> {
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
        let (mut total, mut current) = (0, 0);
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
                    if server["runtimeStatus"] == "connected"
                        && server.pointer("/serverInfo/version").and_then(Value::as_str).is_some_and(|version| version.ends_with(&suffix))
                        && server.pointer("/tools/muxa_peer_report/inputSchema/properties/full").is_some() { current += 1; }
                }
            }
            cursor = page.get("nextCursor").cloned().unwrap_or(Value::Null);
            if cursor.is_null() { break; }
        }
        observed = Some((total, current));
        if current == total { return Ok::<_, anyhow::Error>((total, current)); }
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
async fn verify_host(
    _socket: &std::path::Path,
    _revision: &str,
    _timeout: Duration,
) -> Result<(usize, usize)> {
    bail!("MCP connection verification requires a Unix control socket")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> McpReloadArgs {
        McpReloadArgs {
            codex_home: Some("/profiles/current".into()),
            all: false,
            dry_run: true,
            timeout_secs: 2,
            json: true,
        }
    }

    #[test]
    fn acknowledgement_with_retained_processes_is_pending_not_finished() {
        let owners = crate::stale_agents::StaleReport::default();
        let mut report = plan(&args(), &owners).unwrap();
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
        let single = plan(&args(), &stale).unwrap();
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
        )
        .unwrap();
        assert_eq!(all.hosts.len(), 1);
        assert_eq!(all.hosts[0].codex_home, PathBuf::from("/profiles/a"));
        assert_eq!(all.manual_reconnect.len(), 2);
        assert!(all.scope.contains("registered Muxa connections"));
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
            (2, 1)
        );
        server.await.unwrap();
        let mut report = plan(&args(), &crate::stale_agents::StaleReport::default()).unwrap();
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
