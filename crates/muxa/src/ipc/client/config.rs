use super::{Client, RuntimeError, PROTOCOL_VERSION};
impl Client {
    /// The daemon's `config.toml` as text. Requires the `config_edit_v1`
    /// capability.
    pub async fn config_read(&self) -> Result<crate::config_file::ConfigDocument, RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "config_read" });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["config"].clone()).map_err(RuntimeError::Json)
    }

    /// Replace `config.toml`. `expected` pins the text the caller edited, so
    /// a concurrent change is refused instead of overwritten. The daemon
    /// parses and validates before writing, so a refusal leaves the file
    /// exactly as it was. Requires the `config_edit_v1` capability.
    pub async fn config_write(
        &self,
        text: &str,
        expected: Option<&str>,
    ) -> Result<crate::config_file::ConfigDocument, RuntimeError> {
        let mut req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "config_write",
            "text": text,
        });
        if let Some(expected) = expected {
            req["expected_text"] = serde_json::Value::String(expected.to_string());
        }
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["config"].clone()).map_err(RuntimeError::Json)
    }
}
