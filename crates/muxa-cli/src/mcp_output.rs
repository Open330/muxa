//! Bounded MCP views; daemon and CLI history/topology contracts stay intact.
use muxa::ipc::Client;
use serde_json::{json, Value};
use std::time::Duration;

pub const DEFAULT_HISTORY_LIMIT: usize = 20;
pub const MAX_HISTORY_LIMIT: usize = 100;
const MAX_FLEET_AGENTS: usize = 20;

pub fn history_limit(args: &Value) -> Result<usize, &'static str> {
    let full = args.get("full").and_then(Value::as_bool).unwrap_or(false);
    let Some(value) = args.get("limit") else {
        return Ok(if full { 0 } else { DEFAULT_HISTORY_LIMIT });
    };
    let limit = value
        .as_u64()
        .and_then(|n| usize::try_from(n).ok())
        .ok_or("limit must be a non-negative integer")?;
    if !full && !(1..=MAX_HISTORY_LIMIT).contains(&limit) {
        return Err("limit must be 1..100; use full=true for an explicit larger/full history read");
    }
    Ok(limit)
}

pub async fn recent_prompts(client: &Client, args: &Value) -> Result<Value, String> {
    let limit = history_limit(args).map_err(str::to_owned)?;
    // One extra entry detects truncation without downloading retained history.
    let fetch = if limit == 0 {
        0
    } else {
        limit.saturating_add(1)
    };
    let mut prompts = client
        .recent_prompts(args.get("pane").and_then(Value::as_str), Some(fetch))
        .await
        .map_err(|error| error.to_string())?;
    let has_more = limit > 0 && prompts.len() > limit;
    if limit > 0 {
        prompts.truncate(limit);
    }
    Ok(json!({"prompts":prompts,"has_more":has_more}))
}

pub fn validate_message_read(args: &Value) -> Result<(), &'static str> {
    let exact = args
        .get("request_id")
        .and_then(Value::as_str)
        .is_some_and(|id| !id.trim().is_empty());
    if args.get("after_update").is_some() && !exact {
        return Err("after_update requires an exact request_id");
    }
    if args.get("include_body").and_then(Value::as_bool) == Some(true)
        && !exact
        && args.get("full").and_then(Value::as_bool) != Some(true)
    {
        return Err("include_body requires an exact request_id; use inbox for pending instructions or full=true for explicit full history");
    }
    if args
        .get("after_update")
        .is_some_and(|value| value.as_u64().is_none())
    {
        return Err("after_update must be a non-negative integer");
    }
    Ok(())
}

fn timestamp(at: time::OffsetDateTime) -> Option<String> {
    at.format(&time::format_description::well_known::Rfc3339)
        .ok()
}

pub fn fleet_view(snapshot: &muxa::fleet::FleetSnapshot, full: bool) -> Value {
    if full {
        return json!(snapshot);
    }
    let hosts: Vec<_> = snapshot.hosts.iter().map(|host| {
        let mut active: Vec<_> = host.remote.iter().flat_map(|r| &r.agents)
            .filter(|agent| agent.state != muxa::AgentState::Stopped).collect();
        active.sort_by(|a,b| (&a.pane, &a.session_id).cmp(&(&b.pane, &b.session_id)));
        let count = active.len();
        let agents: Vec<_> = active.into_iter().take(MAX_FLEET_AGENTS).map(|agent| {
            json!({"pane":agent.pane,"socket":agent.tmux_socket,"kind":agent.kind,
                "agent_session_id":agent.session_id,"state":agent.state,
                "rate_limit_scope":agent.rate_limit_scope,"rate_limit_source":agent.rate_limit_source,
                "rate_limited_until":agent.rate_limited_until.and_then(timestamp),
                "attention":agent.last_notification.as_deref().map(|s| crate::truncate_cell(s,160))})
        }).collect();
        json!({"alias":host.alias,"local":host.local,"node_id":host.node_id,
            "labels":host.labels,"capabilities":host.capabilities,"mode":host.mode,"state":host.state,
            "muxa_version":host.muxa_version,"daemon_generation":host.daemon_generation,"latency_ms":host.latency_ms,
            "last_seen_at":host.last_seen_at.and_then(timestamp),"observed_at":host.remote.as_ref().and_then(|r| timestamp(r.observed_at)),
            "error":host.error.as_deref().map(|s| crate::truncate_cell(s,160)),
            "agent_count":host.agent_count(),"active_agent_count":count,
            "pane_count":host.pane_count(),"needs_attention":host.needs_attention(),
            "agents":agents,"agents_truncated":count > MAX_FLEET_AGENTS})
    }).collect();
    json!({"generated_at":timestamp(snapshot.generated_at),"hosts":hosts})
}

pub async fn human_actions_supported(client: &Client) -> bool {
    client
        .hello(Duration::from_secs(2))
        .await
        .is_ok_and(|hello| {
            hello
                .capabilities
                .iter()
                .any(|c| c == muxa::collaboration::HUMAN_ACTIONS_CAPABILITY)
        })
}

pub async fn require_human_actions(client: &Client) -> Result<(), String> {
    if human_actions_supported(client).await {
        return Ok(());
    }
    Err("running muxad has not advertised human-action support; ask in the existing user conversation or upgrade the daemon. Do not retry peer aliases or impersonate the console".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn history_bounds_and_capability_are_enforced_over_ipc() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("bounded.sock");
        let store = muxa::Store::shared();
        for i in 0..25 {
            store
                .history()
                .append(muxa::history::HistoryEntry::new(
                    muxa::AgentKind::Codex,
                    "session",
                    "rmux:%1",
                    format!("prompt {i}"),
                    time::OffsetDateTime::now_utc(),
                    None,
                ))
                .await;
        }
        let server = muxa::ipc::Server::new(socket.clone(), store).with_backends(vec![]);
        let (shutdown, rx) = tokio::sync::broadcast::channel(1);
        let daemon = tokio::spawn(async move { server.run(rx).await.unwrap() });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !socket.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let client = Client::new(socket);
        for (args, count, more) in [
            (json!({}), 20, true),
            (json!({"limit":5}), 5, true),
            (json!({"full":true}), 25, false),
            (json!({"pane":"rmux:%2"}), 0, false),
        ] {
            let view = recent_prompts(&client, &args).await.unwrap();
            assert_eq!(view["prompts"].as_array().unwrap().len(), count);
            assert_eq!(view["has_more"], more);
        }
        assert!(require_human_actions(&client).await.is_ok());
        shutdown.send(()).unwrap();
        daemon.await.unwrap();
    }

    #[tokio::test]
    async fn legacy_daemon_does_not_enable_human_actions() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("legacy.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let daemon = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["kind"],
                "hello"
            );
            reader
                .get_mut()
                .write_all(b"{\"ok\":true,\"capabilities\":[]}\n")
                .await
                .unwrap();
        });
        let error = require_human_actions(&Client::new(socket))
            .await
            .unwrap_err();
        assert!(error.contains("existing user conversation"));
        daemon.await.unwrap();
    }

    #[test]
    fn history_is_bounded_unless_full_is_explicit() {
        assert_eq!(history_limit(&json!({})), Ok(20));
        assert_eq!(history_limit(&json!({"limit":5})), Ok(5));
        assert_eq!(history_limit(&json!({"limit":100})), Ok(100));
        for limit in [json!(0), json!(101), json!(-1), json!("5")] {
            assert!(history_limit(&json!({"limit":limit})).is_err());
        }
        assert_eq!(history_limit(&json!({"full":true})), Ok(0));
        assert_eq!(history_limit(&json!({"full":true,"limit":500})), Ok(500));
    }

    #[test]
    fn instruction_reads_require_an_exact_request_or_explicit_full_history() {
        for args in [
            json!({}),
            json!({"include_body":true,"request_id":"req-1"}),
            json!({"include_body":true,"full":true}),
            json!({"after_update":0,"request_id":"req-1"}),
        ] {
            assert!(validate_message_read(&args).is_ok());
        }
        for args in [
            json!({"include_body":true}),
            json!({"include_body":true,"request_id":" "}),
            json!({"after_update":0}),
            json!({"after_update":-1,"request_id":"req-1"}),
            json!({"after_update":"0","request_id":"req-1"}),
            json!({"after_update":null,"request_id":"req-1"}),
        ] {
            assert!(validate_message_read(&args).is_err());
        }
    }

    #[test]
    fn fleet_summary_omits_prompt_bodies_and_bounds_active_agents() {
        let agents: Vec<_> = (0..25)
            .map(|i| {
                json!({"kind":"codex","session_id":format!("s{i}"),
            "pane":format!("rmux:%{i}"),"state":if i==24 {"stopped"} else {"working"},
            "started_at":"2026-10-15T00:00:00Z","last_activity_at":"2026-10-15T00:00:00Z",
            "state_entered_at":"2026-10-15T00:00:00Z","last_prompt":"PRIVATE PROMPT"})
            })
            .collect();
        let snapshot: muxa::fleet::FleetSnapshot = serde_json::from_value(json!({
            "generated_at":"2026-10-15T00:00:00Z","hosts":[{"alias":"local","local":true,
            "ssh_target":"","labels":{},"annotations":{},"mode":"control","state":"online",
            "remote":{"revision":1,"observed_at":"2026-10-15T00:00:00Z","agents":agents,
                "panes":[],"sessions":[],"backends":[]}}]}))
        .unwrap();
        let summary = fleet_view(&snapshot, false);
        assert_eq!(summary["hosts"][0]["agents"].as_array().unwrap().len(), 20);
        assert_eq!(summary["generated_at"], "2026-10-15T00:00:00Z");
        assert!(summary["hosts"][0]["agents"][0]["agent_session_id"].is_string());
        assert_eq!(summary["hosts"][0]["agent_count"], 25);
        assert_eq!(summary["hosts"][0]["active_agent_count"], 24);
        assert_eq!(summary["hosts"][0]["agents_truncated"], true);
        assert!(!summary.to_string().contains("PRIVATE PROMPT"));
        assert_eq!(fleet_view(&snapshot, true), json!(snapshot));
    }
}
