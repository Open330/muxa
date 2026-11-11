use super::*;
impl Client {
    /// Inject `text` into `pane` via the daemon, resolving the backend
    /// from the pane-id namespace. When `submit`, the daemon follows the
    /// text with a carriage return so the agent's line is committed.
    ///
    /// `Ok` means the **text landed** — the returned [`SendPromptOutcome`]
    /// reports the two non-atomic injections distinctly (`sent` / `submitted`)
    /// so a caller can tell a partial failure (text in, Enter not) from a total
    /// one and must NOT resend the text on the former. `Err` means nothing
    /// landed (structured refusal — unavailable namespace / unsupported backend
    /// — or a failed text send), so the whole send is safe to retry. Backs
    /// `tend mcp`'s `tend_send_prompt`.
    pub async fn send_prompt(
        &self,
        pane: &str,
        text: &str,
        submit: bool,
    ) -> Result<SendPromptOutcome, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "send_prompt",
            "pane": pane,
            "text": text,
            "submit": submit,
        });
        let resp = self.call_checked(&req).await?;
        Ok(SendPromptOutcome {
            // On `Ok` the text landed; default to `true` for forward-compat
            // with a daemon that predates the explicit field.
            sent: resp["sent"].as_bool().unwrap_or(true),
            // Absent field (older daemon) → fall back to the requested intent.
            submitted: resp["submitted"].as_bool().unwrap_or(submit),
            submission_confirmed: resp["submission_confirmed"].as_bool(),
        })
    }

    /// Registers a prompt this caller is about to type into `pane` itself,
    /// so the daemon stamps the hook that reports it with `origin`
    /// (`prompt_origin_v1`). A daemon without it refuses; callers ignore
    /// that and the prompt stays of unknown origin.
    pub async fn expect_prompt(
        &self,
        pane: &str,
        socket: Option<&str>,
        text: &str,
        origin: crate::PromptOrigin,
    ) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "expect_prompt",
            "pane": pane,
            "socket": socket,
            "text": text,
            "origin": origin,
        });
        self.call_checked(&req).await.map(|_| ())
    }

    /// Drops a registration from [`Self::expect_prompt`] after typing failed.
    pub async fn forget_expected_prompt(&self, pane: &str, text: &str) -> Result<(), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "forget_expected_prompt",
            "pane": pane,
            "text": text,
        });
        self.call_checked(&req).await.map(|_| ())
    }

    /// Capture the visible contents of `pane` through the daemon's
    /// namespace-resolved backend. Returns `None` when the pane is gone or
    /// the backend can't capture. Backs `tend mcp`'s `tend_capture_pane`.
    pub async fn capture(&self, pane: &str) -> Result<Option<String>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "capture",
            "pane": pane,
        });
        let resp = self.call_checked(&req).await?;
        Ok(resp["capture"].as_str().map(str::to_owned))
    }

    /// [`Self::recent_prompts`] under an explicit deadline, for callers on
    /// a redraw budget. The daemon serves this from an in-memory deque, so
    /// the deadline guards against a wedged daemon rather than a slow read.
    pub async fn recent_prompts_with_timeout(
        &self,
        pane: Option<&str>,
        limit: Option<usize>,
        deadline: Duration,
    ) -> Result<Vec<crate::history::HistoryEntry>, RuntimeError> {
        let mut req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "recent_prompts",
        });
        if let Some(p) = pane {
            req["pane"] = serde_json::Value::String(p.to_string());
        }
        if let Some(l) = limit {
            req["limit"] = serde_json::Value::from(l);
        }
        let resp = self.call_with_timeout(&req, deadline).await?;
        Ok(resp["prompts"]
            .as_array()
            .cloned()
            .map(|v| serde_json::from_value(serde_json::Value::Array(v)).unwrap_or_default())
            .unwrap_or_default())
    }

    /// Query the daemon's prompt history. `pane = None` returns prompts
    /// across every tracked pane (newest first); otherwise filters to
    /// one pane. `limit = None` or 0 returns everything available.
    pub async fn recent_prompts(
        &self,
        pane: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<crate::history::HistoryEntry>, RuntimeError> {
        let mut req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "recent_prompts",
        });
        if let Some(p) = pane {
            req["pane"] = serde_json::Value::String(p.to_string());
        }
        if let Some(l) = limit {
            req["limit"] = serde_json::Value::from(l);
        }
        let resp = self.call(&req).await?;
        Ok(resp["prompts"]
            .as_array()
            .cloned()
            .map(|v| serde_json::from_value(serde_json::Value::Array(v)).unwrap_or_default())
            .unwrap_or_default())
    }
}
