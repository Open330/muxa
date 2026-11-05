use super::{
    AskConversation, AskEntry, AskProviderAdd, AskProviderEdit, AskProviderInfo,
    AutomationLedgerEntry, AutomationRule, AutomationRules, AutomationTestReport, Client,
    CollaborationUpdateStream, RuntimeError, CLIENT_CALL_TIMEOUT, PROTOCOL_VERSION,
};
impl Client {
    pub async fn ask_subscribe(&self) -> Result<CollaborationUpdateStream, RuntimeError> {
        tokio::time::timeout(
            CLIENT_CALL_TIMEOUT,
            self.revision_subscribe_inner("ask_subscribe"),
        )
        .await
        .map_err(|_| RuntimeError::Timeout(CLIENT_CALL_TIMEOUT))?
    }

    /// Queue a headless question; the returned entry is Running.
    pub async fn ask_send(&self, prompt: &str) -> Result<AskEntry, RuntimeError> {
        self.ask_send_with_credential(prompt, None, None).await
    }

    /// Queue a question as the first turn of a new conversation. Creation and
    /// send are one daemon mutation, so a cancelled composer never creates an
    /// empty durable conversation.
    pub async fn ask_send_new(&self, prompt: &str) -> Result<AskEntry, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ask_send_new",
            "prompt": prompt,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["ask_entry"].clone()).map_err(RuntimeError::Json)
    }

    /// Queue a headless question with an optional one-turn API key. The key
    /// is serialized only on the owner-only socket and is absent from the
    /// response and durable Ask history.
    pub async fn ask_send_with_credential(
        &self,
        prompt: &str,
        agent: Option<&str>,
        api_key: Option<&str>,
    ) -> Result<AskEntry, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ask_send",
            "prompt": prompt,
            "credential": agent.zip(api_key).map(|(agent, api_key)| serde_json::json!({
                "agent": agent,
                "api_key": api_key,
            })),
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["ask_entry"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn ask_list(&self) -> Result<Vec<AskEntry>, RuntimeError> {
        self.ask_list_filtered(None, None).await
    }

    pub async fn ask_list_filtered(
        &self,
        id: Option<&str>,
        conversation_id: Option<&str>,
    ) -> Result<Vec<AskEntry>, RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "ask_list", "id": id, "conversation_id": conversation_id });
        let resp = self.call_checked(&req).await?;
        let entries: Vec<AskEntry> =
            serde_json::from_value(resp["ask_entries"].clone()).map_err(RuntimeError::Json)?;
        // Older controllers ignore optional filters. Keep client semantics
        // identical while those installations fall back to a full response.
        Ok(entries
            .into_iter()
            .filter(|entry| {
                id.is_none_or(|id| entry.id == id)
                    && conversation_id.is_none_or(|conversation| {
                        entry.conversation_id.as_deref() == Some(conversation)
                    })
            })
            .collect())
    }

    pub async fn ask_conversation_list(
        &self,
    ) -> Result<(Vec<AskConversation>, Option<AskConversation>), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ask_conversation_list",
        });
        let resp = self.call_checked(&req).await?;
        let conversations = serde_json::from_value(resp["ask_conversations"].clone())
            .map_err(RuntimeError::Json)?;
        let active =
            serde_json::from_value(resp["ask_conversation"].clone()).map_err(RuntimeError::Json)?;
        Ok((conversations, active))
    }

    pub async fn ask_conversation_select(
        &self,
        conversation_id: &str,
    ) -> Result<AskConversation, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ask_conversation_select",
            "conversation_id": conversation_id,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["ask_conversation"].clone()).map_err(RuntimeError::Json)
    }

    /// Return the daemon's startup-time Global Ask grant.
    pub async fn ask_status(&self) -> Result<bool, RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "ask_status" });
        let resp = self.call_checked(&req).await?;
        resp["ask_enabled"]
            .as_bool()
            .ok_or_else(|| RuntimeError::Json(serde::de::Error::custom("missing ask_enabled")))
    }

    /// Read the selected agent (`None`) or switch to another (`Some`).
    pub async fn ask_agent(&self, agent: Option<&str>) -> Result<String, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ask_agent",
            "agent": agent,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["ask_agent"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn ask_reset(&self) -> Result<AskConversation, RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "ask_reset" });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["ask_conversation"].clone()).map_err(RuntimeError::Json)
    }

    /// Delete completed ask history while leaving active work and the current
    /// per-agent conversation ids intact. Returns the number removed.
    pub async fn ask_clear(&self) -> Result<usize, RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "ask_clear" });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["pruned"].clone()).map_err(RuntimeError::Json)
    }

    /// Delete one completed ask history entry. Returns whether an entry was
    /// removed; running and unknown ids return `false`.
    pub async fn ask_delete(&self, id: &str) -> Result<bool, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ask_delete",
            "id": id,
        });
        let resp = self.call_checked(&req).await?;
        let removed: usize =
            serde_json::from_value(resp["pruned"].clone()).map_err(RuntimeError::Json)?;
        Ok(removed == 1)
    }

    /// Every provider instance the daemon can ask, with the engine behind
    /// it, its effective model, and which one is selected. Requires the
    /// `ask_providers_v1` capability.
    pub async fn ask_providers(&self) -> Result<Vec<AskProviderInfo>, RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "ask_providers" });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["ask_providers"].clone()).map_err(RuntimeError::Json)
    }

    /// Edit `[ask.providers.<provider>]` and read back the updated provider
    /// list. Each key of `edit` is tri-state: `None` is not sent and leaves
    /// the key unchanged, `Some(None)` sends `null` to clear it,
    /// `Some(Some(v))` sets it. Requires the `ask_providers_v1` capability.
    pub async fn ask_provider_configure(
        &self,
        provider: &str,
        edit: &AskProviderEdit,
    ) -> Result<Vec<AskProviderInfo>, RuntimeError> {
        let mut req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ask_provider_configure",
            "provider": provider,
        });
        for (key, change) in [
            ("title", edit.title.as_ref()),
            ("model", edit.model.as_ref()),
            ("api_key_env", edit.api_key_env.as_ref()),
            ("executable", edit.executable.as_ref()),
        ] {
            if let Some(change) = change {
                req[key] = serde_json::json!(change);
            }
        }
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["ask_providers"].clone()).map_err(RuntimeError::Json)
    }

    /// Add an `[ask.providers.<id>]` instance and read back the updated
    /// list. Requires the `ask_providers_v1` capability.
    pub async fn ask_provider_add(
        &self,
        request: &AskProviderAdd,
    ) -> Result<Vec<AskProviderInfo>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ask_provider_add",
            "id": request.id,
            "engine": request.engine,
            "title": request.title,
            "model": request.model,
            "api_key_env": request.api_key_env,
            "executable": request.executable,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["ask_providers"].clone()).map_err(RuntimeError::Json)
    }

    /// Remove an `[ask.providers.<id>]` instance and read back the updated
    /// list. Requires the `ask_providers_v1` capability.
    /// Select `provider` and persist it as `[ask] agent`. Requires the
    /// `ask_provider_default_v1` capability.
    pub async fn ask_provider_default(
        &self,
        provider: &str,
    ) -> Result<Vec<AskProviderInfo>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ask_provider_default",
            "provider": provider,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["ask_providers"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn ask_provider_remove(
        &self,
        id: &str,
    ) -> Result<Vec<AskProviderInfo>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ask_provider_remove",
            "id": id,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["ask_providers"].clone()).map_err(RuntimeError::Json)
    }

    // --- automation_v1 -----------------------------------------------------

    /// Every automation rule with its effective timing, guards, and recent
    /// activity, plus the engine's master switch and pause.
    pub async fn automation_list(&self) -> Result<AutomationRules, RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "automation_list" });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["automation_rules"].clone()).map_err(RuntimeError::Json)
    }

    /// The firing ledger, newest first.
    pub async fn automation_log(
        &self,
        limit: Option<usize>,
    ) -> Result<Vec<AutomationLedgerEntry>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "automation_log",
            "limit": limit,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["automation_log"].clone()).map_err(RuntimeError::Json)
    }

    /// Flip one rule's `enabled`, live and in `config.toml`.
    pub async fn automation_set_enabled(
        &self,
        name: &str,
        enabled: bool,
    ) -> Result<AutomationRules, RuntimeError> {
        self.automation_set_enabled_target(Some(name), enabled)
            .await
    }

    /// `None` flips the engine's own `[automation] enabled`; a name flips
    /// that rule.
    pub async fn automation_set_enabled_target(
        &self,
        name: Option<&str>,
        enabled: bool,
    ) -> Result<AutomationRules, RuntimeError> {
        let mut req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "automation_set_enabled",
            "enabled": enabled,
        });
        if let Some(name) = name {
            req["name"] = serde_json::Value::String(name.to_string());
        }
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["automation_rules"].clone()).map_err(RuntimeError::Json)
    }

    /// Hold every rule until `until`; `None` lifts the hold.
    pub async fn automation_pause(
        &self,
        until: Option<time::OffsetDateTime>,
    ) -> Result<AutomationRules, RuntimeError> {
        let until = until
            .map(|until| until.format(&time::format_description::well_known::Rfc3339))
            .transpose()
            .map_err(|error| {
                RuntimeError::Json(serde::de::Error::custom(format!(
                    "formatting pause deadline: {error}"
                )))
            })?;
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "automation_pause",
            "until": until,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["automation_rules"].clone()).map_err(RuntimeError::Json)
    }

    /// Upsert one rule into `config.toml` — replacing the one with the same
    /// name in place, appending otherwise.
    pub async fn automation_set_rule(
        &self,
        rule: &AutomationRule,
    ) -> Result<AutomationRules, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "automation_set_rule",
            "rule": rule,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["automation_rules"].clone()).map_err(RuntimeError::Json)
    }

    /// Remove one rule. An unknown name is an error.
    pub async fn automation_remove_rule(
        &self,
        name: &str,
    ) -> Result<AutomationRules, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "automation_remove_rule",
            "name": name,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["automation_rules"].clone()).map_err(RuntimeError::Json)
    }

    /// Evaluate one rule against the live registry without firing it.
    pub async fn automation_test(&self, name: &str) -> Result<AutomationTestReport, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "automation_test",
            "name": name,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["automation_test"].clone()).map_err(RuntimeError::Json)
    }
}
