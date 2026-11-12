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
        body @ (RequestBody::ReviewCreate { .. }
        | RequestBody::ReviewGet { .. }
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
