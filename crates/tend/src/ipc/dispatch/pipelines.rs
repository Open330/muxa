//! Pipeline run registration and reporting.

#![allow(clippy::wildcard_imports)] // the arms name the IPC module's items as `dispatch.rs` does

use super::*;

pub(super) async fn handle(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let pipeline_runs = &ctx.pipeline_runs;
    let kind = &mut turn.kind;
    let response = match body {
        RequestBody::PipelineRuns => {
            *kind = "pipeline_runs";
            Response::with_pipeline_runs(pipeline_runs.list().await)
        }
        RequestBody::PipelineRegister { registration } => {
            *kind = "pipeline_register";
            match pipeline_runs.register(registration).await {
                Ok(run) => Response::with_pipeline_run(run),
                Err(error) => Response::err(error.to_string()),
            }
        }
        RequestBody::PipelineDone {
            identity,
            alias,
            generation,
        } => {
            *kind = "pipeline_done";
            // A Work with declared checks completes only once they pass,
            // run now: a pass from before cannot show the work being
            // completed is the work that was checked.
            match Box::pin(crate::work_verify::verify(&ctx.work_store, &identity)).await {
                crate::work_verify::Gate::Failed(outcomes) => {
                    Response::err(crate::work_verify::refusal(&outcomes))
                }
                crate::work_verify::Gate::NoChecks | crate::work_verify::Gate::Passed(_) => {
                    match pipeline_runs.done(&identity, &alias, generation).await {
                        Ok(run) => Response::with_pipeline_run(run),
                        Err(error) => Response::err(error.to_string()),
                    }
                }
            }
        }
        RequestBody::PipelineInvalidate {
            identity,
            alias,
            generation,
        } => {
            *kind = "pipeline_invalidate";
            match pipeline_runs
                .invalidate(&identity, &alias, generation)
                .await
            {
                Ok(run) => Response::with_pipeline_run(run),
                Err(error) => Response::err(error.to_string()),
            }
        }
        RequestBody::PipelineClaim {
            identity,
            generation,
        } => {
            *kind = "pipeline_claim";
            match pipeline_runs.claim_ready(&identity, generation).await {
                Ok(claims) => Response::with_pipeline_claims(claims),
                Err(error) => Response::err(error.to_string()),
            }
        }
        RequestBody::PipelineReport {
            identity,
            alias,
            generation,
            status,
            pane,
            error,
            window_id,
        } => {
            *kind = "pipeline_report";
            match pipeline_runs
                .report(
                    &identity, &alias, generation, status, pane, error, window_id,
                )
                .await
            {
                Ok(run) => Response::with_pipeline_run(run),
                Err(error) => Response::err(error.to_string()),
            }
        }
        other => unreachable!("dispatch routed {other:?} to pipelines"),
    };
    Ok(Flow::Respond(response))
}
