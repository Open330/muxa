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
            record,
        } => {
            *kind = "pipeline_done";
            // A record for another alias or generation is refused before
            // any check runs.
            if let Some(Err(refusal)) = record.as_deref().map(|record| {
                crate::work_complete::check_record(record, &identity, &alias, generation)
            }) {
                return Ok(Flow::Respond(Response::err(refusal)));
            }
            // A Work with declared checks completes only once they pass,
            // run now: a pass from before cannot show the work being
            // completed is the work that was checked.
            match Box::pin(crate::work_verify::verify(&ctx.work_store, &identity)).await {
                crate::work_verify::Gate::Failed(outcomes) => {
                    Response::err(crate::work_verify::refusal(&outcomes))
                }
                gate => match record {
                    None => match pipeline_runs.done(&identity, &alias, generation).await {
                        Ok(run) => Response::with_pipeline_run(run),
                        Err(error) => Response::err(error.to_string()),
                    },
                    Some(record) => {
                        Box::pin(done_with_record(
                            ctx, &identity, &alias, generation, *record, gate,
                        ))
                        .await
                    }
                },
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

/// `pipeline_done` with a result record: the record is filed and the alias
/// completed as one operation, using the checks this call already ran.
async fn done_with_record(
    ctx: &ServerCtx,
    identity: &crate::work::WorkIdentity,
    alias: &str,
    generation: u64,
    mut record: crate::review::NewReview,
    gate: crate::work_verify::Gate,
) -> Response {
    // Only tendd marks evidence as run by tend.
    crate::work_verify::strip_claims(&mut record.decision);
    let verification = match gate {
        crate::work_verify::Gate::Passed(outcomes) => {
            crate::work_verify::attach(&mut record.decision, &outcomes);
            Some(crate::work_verify::Verification::new(outcomes))
        }
        crate::work_verify::Gate::NoChecks | crate::work_verify::Gate::Failed(_) => None,
    };
    let pending = crate::pipeline_run::PendingCompletion {
        generation,
        record,
        verification,
    };
    match crate::work_complete::complete_with_record(
        &ctx.pipeline_runs,
        &ctx.reviews,
        identity,
        alias,
        pending,
    )
    .await
    {
        Ok((run, item)) => {
            let mut response = Response::with_pipeline_run(run);
            response.review_item = Some(Box::new(item));
            response
        }
        Err(error) => Response::err(error),
    }
}
