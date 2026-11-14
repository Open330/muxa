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
        // Every way a result enters the store: an agent's result on a Work
        // with declared checks is verified by tend first, and a failure
        // keeps it from the person.
        RequestBody::ReviewCreate { mut review } => {
            let caller_is_agent = caller_is_agent(ctx, conn).await;
            *kind = "review_create";
            match Box::pin(verify_result(ctx, &mut review, caller_is_agent)).await {
                Ok(verification) => match reviews.create_verified(*review, verification).await {
                    Ok(item) => Response::with_review_item(item),
                    Err(error) => Response::err(error.to_string()),
                },
                Err(refusal) => Response::err(refusal),
            }
        }
        RequestBody::ReviewRecord {
            mut review,
            disposition,
            note,
        } => {
            // Normally the person recording what they decided; from an
            // agent's pane it is a result like any other.
            let caller_is_agent = caller_is_agent(ctx, conn).await;
            // A decided record keeps no verification: the person decided.
            match Box::pin(verify_result(ctx, &mut review, caller_is_agent)).await {
                Ok(_) => {
                    let (name, response) = Box::pin(review_response(
                        reviews,
                        RequestBody::ReviewRecord {
                            review,
                            disposition,
                            note,
                        },
                    ))
                    .await;
                    *kind = name;
                    response
                }
                Err(refusal) => {
                    *kind = "review_record";
                    Response::err(refusal)
                }
            }
        }
        body @ RequestBody::ReviewDisposeRemote { .. } => {
            *kind = "review_dispose_remote";
            Box::pin(review_dispose_remote(ctx, conn, body)).await
        }
        body @ (RequestBody::ReviewGet { .. }
        | RequestBody::ReviewList { .. }
        | RequestBody::ReviewSummary
        | RequestBody::ReviewDispose { .. }) => {
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
    caller_is_agent: bool,
) -> Result<Option<crate::work_verify::Verification>, String> {
    crate::work_verify::strip_claims(&mut review.decision);
    if !crate::work_verify::result_needs_checks(caller_is_agent, review.requested_by.kind) {
        return Ok(None);
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
        return Ok(None);
    };
    match crate::work_verify::verify(&ctx.work_store, &identity).await {
        crate::work_verify::Gate::NoChecks => Ok(None),
        crate::work_verify::Gate::Passed(outcomes) => {
            // Also as evidence, for clients that render only the decision.
            crate::work_verify::attach(&mut review.decision, &outcomes);
            Ok(Some(crate::work_verify::Verification::new(outcomes)))
        }
        crate::work_verify::Gate::Failed(outcomes) => Err(crate::work_verify::refusal(&outcomes)),
    }
}

/// A review decision on another Fleet host, as the Mac app asked for it.
struct RemoteDecision {
    host: String,
    id: String,
    disposition: crate::review::DispositionKind,
    option: Option<String>,
    note: Option<String>,
}

/// The actor label a decision taken in the Mac app is recorded under.
const APP_ACTOR: &str = "app:mac";

/// Decide a review item on another Fleet host for the person at the Mac app.
/// The caller must be the signed Tend app, as macOS reports the process
/// that connected ([`crate::app_identity`]); a caller tendd observes in an
/// agent's pane is refused before that. Then the same checks as the
/// dashboard ([`crate::fleet_review::decide`]), and one audit line either way.
async fn review_dispose_remote(ctx: &ServerCtx, conn: &mut Conn, body: RequestBody) -> Response {
    let RequestBody::ReviewDisposeRemote {
        host,
        id,
        disposition,
        option,
        note,
    } = body
    else {
        return Response::err("not a remote review decision");
    };
    let decision = RemoteDecision {
        host,
        id,
        disposition,
        option,
        note,
    };
    let token = conn.collaboration_actor.audit_token;
    let in_agent_pane = caller_is_agent(ctx, conn).await;
    let is_app = !in_agent_pane
        && tokio::task::spawn_blocking(move || crate::app_identity::is_tend_app(token))
            .await
            .unwrap_or(false);
    let (outcome, response) = if is_app {
        remote_outcome(ctx, &decision).await
    } else {
        let message = "only the Tend app can decide a result on another host: use the dashboard's Needs you page, or decide it on that host";
        (
            "refused: not the Tend app".to_owned(),
            Response::err(message),
        )
    };
    crate::fleet_review::audit(
        ctx.decision_audit.as_ref(),
        serde_json::json!({
            "at": crate::fleet_review::now(), "by": APP_ACTOR, "via": "app",
            "caller_pid": conn.collaboration_actor.caller_pid,
            "action": "review", "host": decision.host, "id": decision.id,
            "disposition": decision.disposition, "option": decision.option,
            "outcome": outcome,
        }),
    )
    .await;
    response
}

async fn remote_outcome(ctx: &ServerCtx, decision: &RemoteDecision) -> (String, Response) {
    const MAX_NOTE_BYTES: usize = 1024;
    let Some(fleet) = &ctx.fleet else {
        return (
            "refused: no Fleet".into(),
            Response::err("fleet is not enabled in tendd"),
        );
    };
    let note = crate::review::decision_note(decision.option.as_deref(), decision.note.as_deref());
    if note
        .as_ref()
        .is_some_and(|note| note.len() > MAX_NOTE_BYTES)
    {
        let message = format!("note exceeds {MAX_NOTE_BYTES} bytes");
        return (format!("invalid: {message}"), Response::err(message));
    }
    let hosts = fleet.store.snapshot_selected(None).await.hosts;
    match crate::fleet_review::decide(
        fleet,
        &hosts,
        &decision.host,
        &decision.id,
        decision.disposition,
        decision.option.as_deref(),
        decision.note.as_deref(),
        APP_ACTOR,
    )
    .await
    {
        Ok((item, replay)) => (
            if replay {
                "already recorded"
            } else {
                "recorded"
            }
            .into(),
            Response::with_review_item(*item),
        ),
        Err((refusal, message)) => (format!("{refusal:?}: {message}"), Response::err(message)),
    }
}

/// Whether tendd observes the caller running in a tracked agent's pane.
async fn caller_is_agent(ctx: &ServerCtx, conn: &mut Conn) -> bool {
    matches!(
        Box::pin(super::work_ops::declarer(ctx, conn)).await,
        crate::work_verify::Declarer::Agent { .. }
    )
}
