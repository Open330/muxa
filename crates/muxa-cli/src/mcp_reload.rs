//! Refresh existing Codex MCP connections through the host API, without restarting it.
use anyhow::{bail, Context, Result};
use clap::Args;
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;
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
        scope: "all configured MCP servers and loaded threads on each selected Codex profile",
        dry_run: args.dry_run,
        hosts: homes
            .into_iter()
            .map(|codex_home| HostResult {
                control_socket: control_socket(&codex_home),
                codex_home,
                state: "planned",
                detail: "config/mcpServer/reload via the existing app-server control socket".into(),
            })
            .filter(|host| sockets.insert(host.control_socket.clone()))
            .collect(),
        manual_reconnect: manual,
    })
}

pub async fn run(args: McpReloadArgs) -> Result<()> {
    let owners = if args.all {
        crate::stale_agents::installed_muxa()
            .map(|binary| crate::stale_agents::scan(&binary))
            .context("cannot locate installed muxa to discover stale owners")?
    } else {
        crate::stale_agents::StaleReport::default()
    };
    let mut report = plan(&args, &owners)?;
    if !args.dry_run {
        for host in &mut report.hosts {
            let (state, detail) = reload_host(
                &host.codex_home,
                &host.control_socket,
                Duration::from_secs(args.timeout_secs),
            )
            .await;
            host.state = state;
            host.detail = detail;
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
            .filter(|host| host.state != "queued")
            .count();
        if failed > 0 || !report.manual_reconnect.is_empty() {
            bail!("{failed} host request(s) were not acknowledged; {} owner(s) need manual reconnect; inspect the per-profile result", report.manual_reconnect.len());
        }
    }
    Ok(())
}

async fn reload_host(
    home: &std::path::Path,
    socket: &std::path::Path,
    timeout: Duration,
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
            exchange(&mut connection, &mut submitted).await
        }
        #[cfg(not(unix))]
        {
            bail!("local Codex control socket reload is unsupported on this platform")
        }
    })
    .await;
    match result {
        Ok(Ok(())) => ("queued", "host accepted the refresh request; verify updated MCP tools at the next host refresh boundary".into()),
        Ok(Err(error)) => (failure_state(&error, submitted), error.to_string()),
        Err(_) => (if submitted { "unknown" } else { "unavailable" },
            if submitted { "reload was submitted but no acknowledgement arrived before the deadline; verify before retrying" }
            else { "Codex control connection did not complete initialization before the deadline" }.into()),
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
) -> Result<()> {
    while let Some(message) = connection.next().await {
        let message = message?;
        let Message::Text(text) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&text)?;
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
            return Ok(());
        }
    }
    bail!("Codex control connection closed before acknowledging the request")
}

async fn exchange<S: AsyncRead + AsyncWrite + Unpin>(
    connection: &mut WebSocketStream<S>,
    submitted: &mut bool,
) -> Result<()> {
    send(
        connection,
        json!({"id":1,"method":"initialize","params":{
        "clientInfo":{"name":"muxa_mcp_reload","version":env!("CARGO_PKG_VERSION")}}}),
    )
    .await?;
    response(connection, 1).await?;
    send(connection, json!({"method":"initialized"})).await?;
    // Installed Codex schema specifies null/unit params, not an empty object.
    *submitted = true;
    send(
        connection,
        json!({"id":2,"method":"config/mcpServer/reload","params":null}),
    )
    .await?;
    response(connection, 2).await?;
    Ok(())
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
        let (state, detail) = reload_host(directory.path(), &socket, Duration::from_secs(1)).await;
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
        assert!(all.scope.contains("all configured MCP servers"));
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
            let (actual, _) =
                reload_host(directory.path(), &socket, Duration::from_millis(500)).await;
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
        let (state, detail) = reload_host(directory.path(), &socket, Duration::from_secs(2)).await;
        assert_eq!(state, "unavailable");
        assert!(detail.contains("too long"), "{detail}");
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stalled_websocket_handshake_is_unavailable() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("server.sock");
        let _listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (state, _) = reload_host(directory.path(), &socket, Duration::from_millis(20)).await;
        assert_eq!(state, "unavailable");
    }
}
