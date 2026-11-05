use super::{Arc, Duration, Response};

/// `swap_jobs_v1`: `swap_job_start` and `swap_job_status`, exactly.
pub const SWAP_JOBS_CAPABILITY: &str = "swap_jobs_v1";
/// `swap_plan_v1`: `swap_plan`.
pub const SWAP_PLAN_CAPABILITY: &str = "swap_plan_v1";
/// `swap_health_v1`: `swap_health`, answered whatever the service's state,
/// including one that never opened.
pub const SWAP_HEALTH_CAPABILITY: &str = "swap_health_v1";

/// How long `swap_health` waits on the service before saying so.
pub(super) const SWAP_HEALTH_ANSWER: Duration = Duration::from_secs(2);

/// The swap service's health, off the reactor and bounded: a check that
/// cannot answer in time is reported as such, never waited on.
pub(super) async fn swap_health_response(
    swap: Option<&Arc<dyn crate::swap_service::SwapJobs>>,
) -> Response {
    let Some(swap) = swap.cloned() else {
        return Response::err("swap jobs are not offered by this daemon".to_owned());
    };
    let check = tokio::task::spawn_blocking(move || swap.health());
    match tokio::time::timeout(SWAP_HEALTH_ANSWER, check).await {
        Ok(Ok(health)) => match serde_json::to_value(&health) {
            Ok(value) => Response::with_swap(value),
            Err(error) => Response::err(error.to_string()),
        },
        Ok(Err(error)) => Response::err(format!("swap health check failed: {error}")),
        Err(_) => Response::err(format!(
            "swap health check did not answer within {}s",
            SWAP_HEALTH_ANSWER.as_secs()
        )),
    }
}

/// `swap_cancel_v1`: `swap_job_cancel`. Advertised whenever this daemon has
/// a swap service, healthy or not: a job can always be told to stop.
pub const SWAP_CANCEL_CAPABILITY: &str = "swap_cancel_v1";
/// `swap_retry_v1`: `swap_job_retry`.
pub const SWAP_RETRY_CAPABILITY: &str = "swap_retry_v1";
/// `swap_service_v1`: this daemon runs swaps as jobs, whether or not it can
/// take one right now. With it and without [`SWAP_JOBS_CAPABILITY`] the
/// service is unavailable (starting, halted, draining, slow), which a
/// client must not mistake for a daemon that predates swap jobs.
pub const SWAP_SERVICE_CAPABILITY: &str = "swap_service_v1";

/// How long `hello` waits to learn whether swap jobs can be taken.
pub(super) const SWAP_HEALTH_TIMEOUT: Duration = Duration::from_millis(500);

/// Whether the swap service would take a job now. Its check reads the job
/// store, so it runs off the reactor, and a slow answer counts as no.
pub(super) async fn swap_healthy(swap: Option<&Arc<dyn crate::swap_service::SwapJobs>>) -> bool {
    let Some(swap) = swap.cloned() else {
        return false;
    };
    let check = tokio::task::spawn_blocking(move || swap.active().is_ok());
    matches!(
        tokio::time::timeout(SWAP_HEALTH_TIMEOUT, check).await,
        Ok(Ok(true))
    )
}

/// Run a swap request against the service off the reactor. No service is a
/// refusal: there is no other path a swap could take.
pub(super) async fn swap_call(
    swap: Option<&Arc<dyn crate::swap_service::SwapJobs>>,
    call: impl FnOnce(&dyn crate::swap_service::SwapJobs) -> Result<serde_json::Value, String>
        + Send
        + 'static,
) -> Response {
    let Some(swap) = swap.cloned() else {
        return Response::err("swap jobs are unavailable on this daemon".to_owned());
    };
    match tokio::task::spawn_blocking(move || call(swap.as_ref())).await {
        Ok(Ok(value)) => Response::with_swap(value),
        Ok(Err(error)) => Response::err(error),
        Err(error) => Response::err(format!("swap request failed: {error}")),
    }
}

pub(super) fn to_swap_value(value: &impl serde::Serialize) -> Result<serde_json::Value, String> {
    serde_json::to_value(value).map_err(|error| error.to_string())
}
