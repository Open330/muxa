use super::{Client, RuntimeError, PROTOCOL_VERSION, SWAP_CLIENT_TIMEOUT};
impl Client {
    pub async fn swap_accounts(
        &self,
    ) -> Result<Vec<crate::account_provider::ProviderCandidates>, RuntimeError> {
        let response = self
            .call_with_timeout(
                &serde_json::json!({
                    "protocol": PROTOCOL_VERSION, "kind": "swap_accounts",
                }),
                SWAP_CLIENT_TIMEOUT,
            )
            .await?;
        if response["ok"] != true {
            return Err(RuntimeError::Daemon(
                response["error"]
                    .as_str()
                    .unwrap_or("account query failed")
                    .into(),
            ));
        }
        serde_json::from_value(response["swap_accounts"].clone()).map_err(RuntimeError::Json)
    }

    /// `swap_jobs_v1`: accept a swap under `request_key`, or get the job it
    /// already made.
    pub async fn swap_job_start(
        &self,
        request_key: &str,
        overrides: &crate::swap_service::SwapOverrides,
    ) -> Result<crate::swap_service::Accepted, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "swap_job_start",
            "request_key": request_key,
            "overrides": overrides,
        });
        self.swap_request(&req).await
    }

    /// `swap_jobs_v1`: a job by id or request key.
    pub async fn swap_job_status(
        &self,
        job: &crate::swap_service::JobRef,
    ) -> Result<crate::swap_service::JobView, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "swap_job_status",
            "job": job,
        });
        self.swap_request(&req).await
    }

    /// `swap_health_v1`: how the daemon's swap service is.
    pub async fn swap_health(&self) -> Result<crate::swap_service::SwapHealth, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "swap_health",
        });
        self.swap_request(&req).await
    }

    /// `swap_cancel_v1`: ask `job` to start no further target.
    pub async fn swap_job_cancel(
        &self,
        job: &crate::swap_service::JobRef,
    ) -> Result<crate::swap_service::JobView, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "swap_job_cancel",
            "job": job,
        });
        self.swap_request(&req).await
    }

    /// `swap_retry_v1`: retry `parent` under `request_key`.
    pub async fn swap_job_retry(
        &self,
        request_key: &str,
        parent: &crate::swap_service::JobRef,
        overrides: &crate::swap_service::RetryOverrides,
    ) -> Result<crate::swap_service::Accepted, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "swap_job_retry",
            "request_key": request_key,
            "parent": parent,
            "overrides": overrides,
        });
        self.swap_request(&req).await
    }

    /// `swap_plan_v1`: what a start would do.
    pub async fn swap_plan(
        &self,
        overrides: &crate::swap_service::SwapOverrides,
    ) -> Result<crate::swap_service::PlanView, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "swap_plan",
            "overrides": overrides,
        });
        self.swap_request(&req).await
    }

    pub(super) async fn swap_request<T: serde::de::DeserializeOwned>(
        &self,
        req: &serde_json::Value,
    ) -> Result<T, RuntimeError> {
        let response = self.call_with_timeout(req, SWAP_CLIENT_TIMEOUT).await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            // An answer, and a no: distinct from a request whose fate is
            // unknown, which a caller must not treat as refused.
            return Err(RuntimeError::Daemon(
                response["error"]
                    .as_str()
                    .unwrap_or("swap request failed")
                    .to_string(),
            ));
        }
        serde_json::from_value(response["swap"].clone()).map_err(RuntimeError::Json)
    }
}
