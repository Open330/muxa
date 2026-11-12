use super::{RequestBody, Response};
use crate::review::ReviewStore;

/// Serve the `review_*` requests. Kept out of `handle` so the dispatch
/// table's frame does not grow with each review operation.
pub(super) async fn review_response(
    reviews: &ReviewStore,
    body: RequestBody,
) -> (&'static str, Response) {
    let result = match body {
        RequestBody::ReviewCreate { review } => (
            "review_create",
            reviews
                .create(*review)
                .await
                .map(Response::with_review_item),
        ),
        RequestBody::ReviewGet { id } => (
            "review_get",
            reviews.get(id).await.map(Response::with_review_item),
        ),
        RequestBody::ReviewList {
            open_only,
            oldest_first,
            limit,
            offset,
        } => (
            "review_list",
            reviews
                .list(crate::review::ReviewQuery {
                    open_only,
                    oldest_first,
                    limit,
                    offset,
                })
                .await
                .map(Response::with_review_page),
        ),
        RequestBody::ReviewSummary => (
            "review_summary",
            reviews.summary().await.map(Response::with_review_summary),
        ),
        RequestBody::ReviewDispose {
            id,
            disposition,
            by,
            note,
        } => (
            "review_dispose",
            reviews
                .dispose(id, disposition, by, note)
                .await
                .map(Response::with_review_item),
        ),
        RequestBody::ReviewRecord {
            review,
            disposition,
            note,
        } => (
            "review_record",
            reviews
                .record(*review, disposition, note)
                .await
                .map(Response::with_review_item),
        ),
        _ => return ("review", Response::err("not a review request")),
    };
    match result {
        (kind, Ok(response)) => (kind, response),
        (kind, Err(error)) => (kind, Response::err(error.to_string())),
    }
}

/// Human requests from every room are an operator-console read, the same
/// rule `collaboration_list` applies past the caller's own mailbox.
fn operator_only(
    origin: Option<&crate::collaboration::CollaborationOrigin>,
) -> Option<crate::collaboration::CollaborationError> {
    (!origin.is_some_and(|origin| origin.console))
        .then_some(crate::collaboration::CollaborationError::ScopeDenied)
}

/// Serve `decision_queue` and `decision_history`. A source that cannot be
/// read is named in `unavailable` rather than failing the whole read, so
/// the person still sees what the other sources hold.
pub(super) async fn decision_response(
    reviews: &ReviewStore,
    collaboration: &crate::collaboration::CollaborationStore,
    store: &super::SharedStore,
    body: RequestBody,
) -> (&'static str, Response) {
    match body {
        RequestBody::DecisionQueue { limit, origin } => {
            let operator = operator_only(origin.as_ref()).is_none();
            let agents = store.snapshot().await;
            let queue = crate::decision_queue::read_pending(
                reviews,
                collaboration,
                &agents,
                limit,
                operator,
            )
            .await;
            (
                "decision_queue",
                Response {
                    decision_queue: Some(Box::new(queue)),
                    ..Response::ok()
                },
            )
        }
        RequestBody::DecisionHistory { limit, origin } => {
            let operator = operator_only(origin.as_ref()).is_none();
            let history =
                crate::decision_queue::read_history(reviews, collaboration, limit, operator).await;
            (
                "decision_history",
                Response {
                    decision_history: Some(Box::new(history)),
                    ..Response::ok()
                },
            )
        }
        _ => ("decision", Response::err("not a decision request")),
    }
}
