use super::*;
impl Client {
    pub async fn spawn_session(
        &self,
        spawn: crate::session::SpawnSession,
    ) -> Result<SessionRef, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "spawn_session",
            "command": spawn.command,
            "args": spawn.args,
            "env": spawn.env,
            "cwd": spawn.cwd,
            "name": spawn.name,
            "cols": spawn.cols,
            "rows": spawn.rows,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["session"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionRef>, RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "list_sessions" });
        let resp = self.call_checked(&req).await?;
        Ok(resp["sessions"]
            .as_array()
            .cloned()
            .map(|v| serde_json::from_value(serde_json::Value::Array(v)).unwrap_or_default())
            .unwrap_or_default())
    }

    pub async fn capture_session(
        &self,
        session_id: &str,
    ) -> Result<TerminalSnapshot, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "capture_session",
            "session_id": session_id,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["terminal"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn read_session(
        &self,
        session_id: &str,
        offset: u64,
    ) -> Result<SessionOutput, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "read_session",
            "session_id": session_id,
            "offset": offset,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["output"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn write_session(&self, session_id: &str, data: &str) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "write_session",
            "session_id": session_id,
            "data": data,
        });
        let _ = self.call_checked(&req).await?;
        Ok(())
    }

    pub async fn write_session_bytes(
        &self,
        session_id: &str,
        data: &[u8],
    ) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "write_session_bytes",
            "session_id": session_id,
            "data_base64": BASE64_STANDARD.encode(data),
        });
        let _ = self.call_checked(&req).await?;
        Ok(())
    }

    pub async fn resize_session(
        &self,
        session_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "resize_session",
            "session_id": session_id,
            "cols": cols,
            "rows": rows,
        });
        let _ = self.call_checked(&req).await?;
        Ok(())
    }

    pub async fn set_session_attached(
        &self,
        session_id: &str,
        attached: bool,
    ) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "set_session_attached",
            "session_id": session_id,
            "attached": attached,
        });
        let _ = self.call_checked(&req).await?;
        Ok(())
    }

    pub async fn set_session_client_attached(
        &self,
        session_id: &str,
        client_id: &str,
        attached: bool,
    ) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "set_session_attached",
            "session_id": session_id,
            "client_id": client_id,
            "attached": attached,
        });
        let _ = self.call_checked(&req).await?;
        Ok(())
    }

    pub async fn terminate_session(&self, session_id: &str) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "terminate_session",
            "session_id": session_id,
        });
        let _ = self.call_checked(&req).await?;
        Ok(())
    }
}
