use super::{
    read_limited_line, AsyncWriteExt, BufReader, Client, CollaborationRequest, Duration,
    FleetCommandResult, FleetOperation, FleetSnapshot, FleetUpdateStream, RuntimeError, Stream,
    CLIENT_CALL_TIMEOUT, MAX_COLLABORATION_WAIT_SECS, PROTOCOL_VERSION,
};
impl Client {
    /// Read the central physical-host cache. This is a local Unix-socket
    /// operation; SSH collection runs continuously in tendd's `FleetManager`.
    pub async fn fleet_snapshot(
        &self,
        selector: Option<&str>,
    ) -> Result<FleetSnapshot, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "fleet_snapshot",
            "selector": selector,
        });
        let response = self.call_checked(&req).await?;
        serde_json::from_value(response["fleet"].clone()).map_err(RuntimeError::Json)
    }

    /// Subscribe to compact Fleet cache invalidations. The stream carries no
    /// remote terminal contents and grants no additional authority; callers
    /// fetch a normal selector-filtered snapshot after coalescing updates.
    pub async fn fleet_subscribe(
        &self,
        selector: Option<&str>,
    ) -> Result<FleetUpdateStream, RuntimeError> {
        tokio::time::timeout(CLIENT_CALL_TIMEOUT, self.fleet_subscribe_inner(selector))
            .await
            .map_err(|_| RuntimeError::Timeout(CLIENT_CALL_TIMEOUT))?
    }

    pub(super) async fn fleet_subscribe_inner(
        &self,
        selector: Option<&str>,
    ) -> Result<FleetUpdateStream, RuntimeError> {
        let stream =
            Stream::connect(&self.socket_path)
                .await
                .map_err(|error| match error.kind() {
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound => {
                        RuntimeError::NotConnected(self.socket_path.clone())
                    }
                    _ => RuntimeError::Io(error),
                })?;
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        self.send_hello(&mut reader, &mut writer).await?;

        let mut request = serde_json::to_vec(&serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "fleet_subscribe",
            "selector": selector,
        }))?;
        request.push(b'\n');
        writer.write_all(&request).await?;
        writer.flush().await?;

        let mut ack = String::new();
        read_limited_line(&mut reader, &mut ack).await?;
        let ack: serde_json::Value = serde_json::from_str(ack.trim())?;
        if !ack["ok"].as_bool().unwrap_or(false) {
            return Err(RuntimeError::Json(serde::de::Error::custom(format!(
                "fleet subscribe rejected: {}",
                ack["error"].as_str().unwrap_or("(no error message)")
            ))));
        }
        drop(writer);
        Ok(FleetUpdateStream {
            reader,
            line: String::new(),
        })
    }

    pub async fn fleet_execute(
        &self,
        host: &str,
        operation: &FleetOperation,
    ) -> Result<FleetCommandResult, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": if operation.requires_task_contract() { "fleet_command_contract" } else {"fleet_command"},
            "host": host,
            "operation": operation,
        });
        let response = self
            .call_with_timeout(&req, Duration::from_secs(25))
            .await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            return Err(RuntimeError::Json(serde::de::Error::custom(
                response["error"]
                    .as_str()
                    .unwrap_or("fleet command failed")
                    .to_string(),
            )));
        }
        serde_json::from_value(response["fleet_result"].clone()).map_err(RuntimeError::Json)
    }

    /// Wait through the controller's shared event subscription, without pinning a relay command.
    pub async fn fleet_wait_reply(
        &self,
        host: &str,
        pane: &crate::topology::PaneKey,
        request_id: &str,
        timeout_secs: u64,
        after_update: Option<u64>,
    ) -> Result<Option<CollaborationRequest>, RuntimeError> {
        let timeout_secs = timeout_secs.clamp(1, MAX_COLLABORATION_WAIT_SECS);
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION, "kind": "fleet_wait_reply",
            "host": host, "pane": pane, "request_id": request_id, "timeout_secs": timeout_secs, "after_update":after_update,
        });
        let response = self
            .call_with_timeout(
                &req,
                Duration::from_secs(timeout_secs).saturating_add(CLIENT_CALL_TIMEOUT),
            )
            .await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            return Err(RuntimeError::Json(serde::de::Error::custom(
                response["error"]
                    .as_str()
                    .unwrap_or("fleet reply wait failed")
                    .to_string(),
            )));
        }
        serde_json::from_value(response["collaboration_request"].clone())
            .map_err(RuntimeError::Json)
    }
}
