//! Review items and the decision queue.

#![allow(clippy::wildcard_imports)] // the arms name the IPC module's items as `dispatch.rs` does

use super::*;

pub(super) async fn handle(
    ctx: &ServerCtx,
    conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let collaboration = &ctx.collaboration;
    let reviews = &ctx.reviews;
    let collaboration_actor = &mut conn.collaboration_actor;
    let kind = &mut turn.kind;
    let pending_audit = &mut turn.pending_audit;
    let response = match body {
        RequestBody::ReviewCreate { mut review } => {
            // A result an agent files on a Work with declared checks is
            // verified by tend first; a failure keeps it from the person.
            match Box::pin(verify_result(ctx, &mut review)).await {
                Ok(()) => {
                    let (name, response) = Box::pin(review_response(
                        reviews,
                        RequestBody::ReviewCreate { review },
                    ))
                    .await;
                    *kind = name;
                    response
                }
                Err(refusal) => {
                    *kind = "review_create";
                    Response::err(refusal)
                }
            }
        }
        body @ (RequestBody::ReviewGet { .. }
        | RequestBody::ReviewList { .. }
        | RequestBody::ReviewSummary
        | RequestBody::ReviewDispose { .. }
        | RequestBody::ReviewRecord { .. }) => {
            // One boxed arm keeps this dispatch frame from growing.
            let (name, response) = Box::pin(review_response(reviews, body)).await;
            *kind = name;
            response
        }
        body @ (RequestBody::DecisionQueue { .. } | RequestBody::DecisionHistory { .. }) => {
            // An operator read of every room's human requests goes
            // in the collaboration ledger like a widened listing.
            let audit = match &body {
                RequestBody::DecisionQueue { origin, limit }
                | RequestBody::DecisionHistory { origin, limit } => origin
                    .as_ref()
                    .filter(|origin| origin.console)
                    .map(|origin| {
                        let mut context = CollaborationAuditContext::new(
                            CollaborationAuditOperation::List,
                            origin.clone(),
                        );
                        context.scope = Some(MailboxScope::All);
                        context.read_view = Some(collaboration::CollaborationReadView::Full);
                        context.query_limit = Some(*limit);
                        context
                    }),
                _ => None,
            };
            let (name, response) = Box::pin(super::review::decision_response(
                reviews,
                collaboration,
                store,
                body,
            ))
            .await;
            *kind = name;
            if let Some(context) = audit {
                *pending_audit = Some(collaboration_audit_entry(
                    collaboration_actor,
                    context,
                    &response,
                ));
            }
            response
        }
        other => unreachable!("dispatch routed {other:?} to decisions"),
    };
    Ok(Flow::Respond(response))
}

/// Run the declared checks of the Work an agent's result belongs to, and
/// put their outcome on the record. Any client claim that tend ran its
/// evidence is cleared, verified or not.
async fn verify_result(
    ctx: &ServerCtx,
    review: &mut crate::review::NewReview,
) -> Result<(), String> {
    crate::work_verify::strip_claims(&mut review.decision);
    if review.requested_by.kind != crate::review::ActorKind::Agent {
        return Ok(());
    }
    let identity = match &review.target {
        crate::review::ReviewTarget::Work {
            workspace, work, ..
        } => Some(crate::work::WorkIdentity::new(
            workspace.clone(),
            work.clone(),
        )),
        crate::review::ReviewTarget::Request { request_id, .. } => ctx
            .collaboration
            .request_work(request_id)
            .await
            .map(|(workspace, work)| crate::work::WorkIdentity::new(workspace, work)),
        crate::review::ReviewTarget::Session { .. } => None,
    };
    let Some(identity) = identity else {
        return Ok(());
    };
    match crate::work_verify::verify(&ctx.work_store, &ctx.pipeline_runs, &identity).await {
        crate::work_verify::Gate::NoChecks => Ok(()),
        crate::work_verify::Gate::Passed(outcomes) => {
            crate::work_verify::attach(&mut review.decision, &outcomes);
            Ok(())
        }
        crate::work_verify::Gate::Failed(outcomes) => Err(crate::work_verify::refusal(&outcomes)),
    }
}
