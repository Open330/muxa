//! Account-swap jobs.

#![allow(clippy::wildcard_imports)] // the arms name the IPC module's items as `dispatch.rs` does

use super::*;

pub(super) async fn handle(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let swap = &ctx.swap;
    let kind = &mut turn.kind;
    let response = match body {
        RequestBody::SwapJobStart {
            request_key,
            overrides,
        } => {
            *kind = "swap_job_start";
            swap_call(swap.as_ref(), move |swap| {
                swap.submit(&request_key, &overrides)
                    .and_then(|accepted| to_swap_value(&accepted))
            })
            .await
        }
        RequestBody::SwapJobRetry {
            request_key,
            parent,
            overrides,
        } => {
            *kind = "swap_job_retry";
            swap_call(swap.as_ref(), move |swap| {
                swap.retry(&request_key, &parent, &overrides)
                    .and_then(|accepted| to_swap_value(&accepted))
            })
            .await
        }
        RequestBody::SwapAccounts => {
            *kind = "swap_accounts";
            let mut response = swap_call(swap.as_ref(), move |swap| {
                swap.accounts()
                    .and_then(|accounts| to_swap_value(&accounts))
            })
            .await;
            response.swap_accounts = response.swap.take();
            response
        }
        RequestBody::SwapHealth => {
            *kind = "swap_health";
            swap_health_response(swap.as_ref()).await
        }
        RequestBody::SwapJobCancel { job } => {
            *kind = "swap_job_cancel";
            swap_call(swap.as_ref(), move |swap| {
                swap.cancel(&job).and_then(|view| to_swap_value(&view))
            })
            .await
        }
        RequestBody::SwapJobStatus { job } => {
            *kind = "swap_job_status";
            swap_call(swap.as_ref(), move |swap| {
                swap.status(&job).and_then(|view| to_swap_value(&view))
            })
            .await
        }
        RequestBody::SwapPlan { overrides } => {
            *kind = "swap_plan";
            swap_call(swap.as_ref(), move |swap| {
                swap.plan(&overrides).and_then(|plan| to_swap_value(&plan))
            })
            .await
        }
        other => unreachable!("dispatch routed {other:?} to swap_jobs"),
    };
    Ok(Flow::Respond(response))
}
