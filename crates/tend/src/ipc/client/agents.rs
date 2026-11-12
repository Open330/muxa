use super::{
    decode_agents, response_error, Agent, AgentEvent, Client, Duration, PaneInfo, RuntimeError,
    HOOK_CALL_TIMEOUT, PROTOCOL_VERSION, WORK_COMMAND_CLIENT_TIMEOUT,
};
impl Client {
    pub fn ingest<'a>(
        &'a self,
        event: &'a AgentEvent,
    ) -> impl std::future::Future<Output = Result<(), RuntimeError>> + Send + 'a {
        self.ingest_with_launch_claim(event, None)
    }

    pub async fn ingest_with_launch_claim(
        &self,
        event: &AgentEvent,
        launch_id: Option<String>,
    ) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ingest",
            "ask_entry": std::env::var_os(crate::ask::ASK_ENV_MARKER).map(|_|std::env::var("TEND_ASK_ENTRY").unwrap_or_else(|_|"headless".into())),
            "launch_id": launch_id,
            "event": event
        });
        // Hook ingest is on the agent's critical path — use the tighter
        // deadline so a wedged daemon fails fast (the caller treats any error
        // as a best-effort no-op) instead of stalling the agent.
        let response = self.call_with_timeout(&req, HOOK_CALL_TIMEOUT).await?;
        // A refusal (a protocol mismatch during a staggered upgrade, an event
        // this daemon cannot parse) used to read as success and vanish.
        if response["ok"].as_bool().unwrap_or(false) {
            Ok(())
        } else {
            Err(super::response_error(&response))
        }
    }

    /// Only the opt-in agents JSON view consumes this additive provenance sidecar.
    pub async fn agents_with_launch_identity(
        &self,
    ) -> Result<Vec<serde_json::Value>, RuntimeError> {
        let supported = self
            .hello(Duration::from_secs(2))
            .await?
            .capabilities
            .iter()
            .any(|cap| cap == crate::launch_identity::CAPABILITY);
        let response = self.call_checked(&serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"snapshot","launch_identity":supported})).await?;
        let agents = decode_agents(&response)?;
        agents
            .iter()
            .map(|agent| {
                let mut value = serde_json::to_value(agent)?;
                if let Some(identity) = response
                    .get("launch_identities")
                    .and_then(|m| m.get(&agent.session_id))
                {
                    value["launch_identity"] = identity.clone();
                }
                Ok(value)
            })
            .collect()
    }

    /// Push a wholesale pane snapshot to the daemon. The zellij
    /// WASM-plugin bridge calls this to forward `PaneUpdate` events; the
    /// daemon hands the panes to its `SharedBackend::ingest_pane_snapshot`.
    pub async fn push_pane_snapshot(&self, panes: &[PaneInfo]) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "backend_pane_snapshot",
            "panes": panes,
        });
        let _ = self.call(&req).await?;
        Ok(())
    }

    pub async fn snapshot(&self) -> Result<Vec<Agent>, RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "snapshot" });
        let resp = self.call(&req).await?;
        decode_agents(&resp)
    }

    pub async fn refresh_pane_locations(&self, pane: &str) -> Result<(), RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "refresh_pane_locations", "pane": pane });
        self.call_checked(&req).await?;
        Ok(())
    }

    // WS-B: new agent
    /// Start one allowlisted agent through the daemon, on its own host or a
    /// control-mode Fleet host. Requires the `agent_start_v1` capability.
    pub async fn agent_start(
        &self,
        request: &crate::agent_control::AgentStartRequest,
    ) -> Result<crate::agent_control::AgentStartResult, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "agent_start",
            "request": request,
        });
        let response = self
            .call_with_timeout(&req, WORK_COMMAND_CLIENT_TIMEOUT)
            .await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            return Err(RuntimeError::Json(serde::de::Error::custom(
                response["error"]
                    .as_str()
                    .unwrap_or("agent start failed")
                    .to_string(),
            )));
        }
        serde_json::from_value(response["agent_start"].clone()).map_err(RuntimeError::Json)
    }

    /// Forget one exact agent session. Returns false when it is already absent.
    /// Does not terminate the agent or its pane; later activity may recreate it.
    pub async fn forget_agent(&self, session_id: &str) -> Result<bool, RuntimeError> {
        let resp = self
            .call_checked(&serde_json::json!({
                "protocol": PROTOCOL_VERSION,
                "kind": "forget_agent",
                "session_id": session_id,
            }))
            .await?;
        Ok(resp["pruned"].as_u64() == Some(1))
    }

    /// Ask the daemon to delete fully orphaned rows (no pane, surface, or
    /// pid) idle longer than `max_age`. `max_age = Duration::ZERO` removes
    /// every orphan regardless of age. Returns the number removed. Backs
    /// `tend prune`.
    pub async fn prune(&self, max_age: Duration) -> Result<usize, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "prune",
            "max_age_secs": max_age.as_secs(),
        });
        let resp = self.call(&req).await?;
        Ok(usize::try_from(resp["pruned"].as_u64().unwrap_or(0)).unwrap_or(usize::MAX))
    }

    pub async fn invalidate_agent_directory(&self) -> Result<(), RuntimeError> {
        let request =
            serde_json::json!({"protocol": PROTOCOL_VERSION, "kind":"invalidate_agent_directory"});
        let response = self
            .call_with_timeout(&request, Duration::from_secs(1))
            .await?;
        if response["ok"].as_bool() == Some(true) {
            Ok(())
        } else {
            Err(response_error(&response))
        }
    }

    /// Returns None only when an older daemon explicitly rejects this request kind.
    pub async fn agent_find(
        &self,
        query: &str,
        limit: usize,
        exclude_sessions: &[String],
        refresh: bool,
    ) -> Result<Option<crate::agent_locator::LocationResults>, RuntimeError> {
        let request = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "agent_find",
            "query": query, "limit": limit, "exclude_sessions": exclude_sessions, "refresh": refresh });
        match self.call_checked(&request).await {
            Ok(response) => serde_json::from_value(response["agent_locations"].clone())
                .map(Some)
                .map_err(RuntimeError::Json),
            Err(RuntimeError::Daemon(error)) if error.contains("unknown variant `agent_find`") => {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    pub async fn snapshot_with_timeout(
        &self,
        deadline: Duration,
    ) -> Result<Vec<Agent>, RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "snapshot" });
        let resp = self.call_with_timeout(&req, deadline).await?;
        decode_agents(&resp)
    }

    pub async fn by_pane(&self, pane: &str) -> Result<Vec<Agent>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "by_pane",
            "pane": pane
        });
        let resp = self.call(&req).await?;
        decode_agents(&resp)
    }

    pub async fn by_pane_with_timeout(
        &self,
        pane: &str,
        deadline: Duration,
    ) -> Result<Vec<Agent>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "by_pane",
            "pane": pane
        });
        let resp = self.call_with_timeout(&req, deadline).await?;
        decode_agents(&resp)
    }

    pub async fn by_surface(&self, surface_id: &str) -> Result<Vec<Agent>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "by_surface",
            "surface_id": surface_id
        });
        let resp = self.call(&req).await?;
        Ok(resp["agents"]
            .as_array()
            .cloned()
            .map(|v| serde_json::from_value(serde_json::Value::Array(v)).unwrap_or_default())
            .unwrap_or_default())
    }

    /// Register an arbitrary background process as a pid-tracked `Task` row.
    /// Backs the `tend register` CLI.
    pub async fn register(
        &self,
        name: &str,
        pid: Option<u32>,
        cwd: Option<&str>,
        pane: Option<&str>,
        command: Option<&str>,
    ) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "register",
            "name": name,
            "pid": pid,
            "cwd": cwd,
            "pane": pane,
            "command": command,
        });
        let _ = self.call_checked(&req).await?;
        Ok(())
    }
}
