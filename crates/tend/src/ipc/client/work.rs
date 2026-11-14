use super::{
    Client, CollaborationUpdateStream, PipelineAliasStatus, PipelineClaim, PipelineRun,
    PipelineRunRegistration, RuntimeError, WorkCommandLimits, WorkCommandOutput, WorkComposeOutput,
    WorkComposeRequest, WorkIdentity, CLIENT_CALL_TIMEOUT, PROTOCOL_VERSION,
    WORK_COMMAND_CLIENT_TIMEOUT,
};
impl Client {
    pub async fn pipeline_subscribe(&self) -> Result<CollaborationUpdateStream, RuntimeError> {
        tokio::time::timeout(
            CLIENT_CALL_TIMEOUT,
            self.revision_subscribe_inner(
                &serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"pipeline_subscribe"}),
            ),
        )
        .await
        .map_err(|_| RuntimeError::Timeout(CLIENT_CALL_TIMEOUT))?
    }

    /// Run one allowlisted `tend work …` argv through the daemon, on its own
    /// host or on the Fleet host `host`, and return the child's exit code and
    /// streams. Requires the `work_command_v1` capability.
    pub async fn work_command(
        &self,
        host: Option<&str>,
        args: &[String],
        stdin: Option<&str>,
    ) -> Result<WorkCommandOutput, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "work_command",
            "host": host,
            "args": args,
            "stdin": stdin,
        });
        let response = self
            .call_with_timeout(
                &req,
                WorkCommandLimits::for_args(args).timeout + WORK_COMMAND_CLIENT_TIMEOUT,
            )
            .await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            return Err(RuntimeError::Json(serde::de::Error::custom(
                response["error"]
                    .as_str()
                    .unwrap_or("work command failed")
                    .to_string(),
            )));
        }
        serde_json::from_value(response["work_command"].clone()).map_err(RuntimeError::Json)
    }

    /// Draft one pipeline from a description with a read-only headless
    /// turn. `credential` is a one-turn `(agent, api_key)` pair handled
    /// like `ask_send`'s. Requires the `work_compose_v1` capability.
    pub async fn work_compose(
        &self,
        request: &WorkComposeRequest,
        credential: Option<(&str, &str)>,
    ) -> Result<WorkComposeOutput, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "work_compose",
            "description": request.description,
            "agent": request.agent,
            "current": request.current,
            "credential": credential.map(|(agent, api_key)| serde_json::json!({
                "agent": agent,
                "api_key": api_key,
            })),
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["work_compose"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn pipeline_runs(&self) -> Result<Vec<PipelineRun>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "pipeline_runs",
        });
        let response = self.call_checked(&req).await?;
        serde_json::from_value(response["pipeline_runs"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn pipeline_register(
        &self,
        registration: &PipelineRunRegistration,
    ) -> Result<PipelineRun, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "pipeline_register",
            "registration": registration,
        });
        let response = self.call_checked(&req).await?;
        serde_json::from_value(response["pipeline_run"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn pipeline_done(
        &self,
        identity: &WorkIdentity,
        alias: &str,
        generation: u64,
    ) -> Result<PipelineRun, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "pipeline_done",
            "identity": identity,
            "alias": alias,
            "generation": generation,
        });
        // The daemon may run the Work's declared checks first.
        let response = self
            .call_with_timeout(&req, crate::work_verify::VERIFY_CALL_TIMEOUT)
            .await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            return Err(super::response_error(&response));
        }
        serde_json::from_value(response["pipeline_run"].clone()).map_err(RuntimeError::Json)
    }

    /// [`Self::pipeline_done`] that also files `record` as one daemon
    /// operation (`work_done_record_v1`): a crash between the two cannot
    /// leave the record without the completion. Check the capability first;
    /// an older daemon ignores the record.
    pub async fn pipeline_done_with_record(
        &self,
        identity: &WorkIdentity,
        alias: &str,
        generation: u64,
        record: &crate::review::NewReview,
    ) -> Result<(PipelineRun, crate::review::ReviewItem), RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "pipeline_done",
            "identity": identity,
            "alias": alias,
            "generation": generation,
            "record": record,
        });
        let response = self
            .call_with_timeout(&req, crate::work_verify::VERIFY_CALL_TIMEOUT)
            .await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            return Err(super::response_error(&response));
        }
        let run =
            serde_json::from_value(response["pipeline_run"].clone()).map_err(RuntimeError::Json)?;
        let item =
            serde_json::from_value(response["review_item"].clone()).map_err(RuntimeError::Json)?;
        Ok((run, item))
    }

    pub async fn pipeline_invalidate(
        &self,
        identity: &WorkIdentity,
        alias: &str,
        generation: u64,
    ) -> Result<PipelineRun, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "pipeline_invalidate",
            "identity": identity,
            "alias": alias,
            "generation": generation,
        });
        let response = self.call_checked(&req).await?;
        serde_json::from_value(response["pipeline_run"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn pipeline_claim(
        &self,
        identity: &WorkIdentity,
        generation: u64,
    ) -> Result<Vec<PipelineClaim>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "pipeline_claim",
            "identity": identity,
            "generation": generation,
        });
        let response = self.call_checked(&req).await?;
        serde_json::from_value(response["pipeline_claims"].clone()).map_err(RuntimeError::Json)
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn pipeline_report(
        &self,
        identity: &WorkIdentity,
        alias: &str,
        generation: u64,
        status: PipelineAliasStatus,
        pane: Option<&str>,
        error: Option<&str>,
        window_id: Option<&str>,
    ) -> Result<PipelineRun, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "pipeline_report",
            "identity": identity,
            "alias": alias,
            "generation": generation,
            "status": status,
            "pane": pane,
            "error": error,
            "window_id": window_id,
        });
        let response = self.call_checked(&req).await?;
        serde_json::from_value(response["pipeline_run"].clone()).map_err(RuntimeError::Json)
    }
}
