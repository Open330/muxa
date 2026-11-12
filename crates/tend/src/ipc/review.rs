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

/// Serve `decision_queue` and `decision_history`. A source that cannot be
/// read is named in `unavailable` rather than failing the whole read, so
/// the person still sees what the other sources hold.
pub(super) async fn decision_response(
    reviews: &ReviewStore,
    collaboration: &crate::collaboration::CollaborationStore,
    store: &super::SharedStore,
    body: RequestBody,
) -> (&'static str, Response) {
    use crate::decision_queue::{assemble_history, assemble_queue, MAX_QUEUE_LIMIT};
    match body {
        RequestBody::DecisionQueue { limit } => {
            let limit = limit.clamp(1, MAX_QUEUE_LIMIT);
            let mut unavailable = Vec::new();
            let mut truncated = false;
            let human = match collaboration.open_human_requests(limit).await {
                Ok((requests, more)) => {
                    truncated |= more;
                    requests
                }
                Err(crate::collaboration::CollaborationError::Disabled) => Vec::new(),
                Err(error) => {
                    unavailable.push(format!("human requests: {error}"));
                    Vec::new()
                }
            };
            let page = reviews
                .list(crate::review::ReviewQuery {
                    open_only: true,
                    oldest_first: true,
                    limit,
                    offset: 0,
                })
                .await;
            let reviews = match page {
                Ok(page) => {
                    truncated |= page.has_more;
                    page.items
                }
                Err(error) => {
                    unavailable.push(format!("review items: {error}"));
                    Vec::new()
                }
            };
            let agents = store.snapshot().await;
            let mut queue = assemble_queue(&human, &agents, reviews, limit);
            queue.truncated |= truncated;
            queue.unavailable = unavailable;
            (
                "decision_queue",
                Response {
                    decision_queue: Some(Box::new(queue)),
                    ..Response::ok()
                },
            )
        }
        RequestBody::DecisionHistory { limit } => {
            let limit = limit.clamp(1, MAX_QUEUE_LIMIT);
            let mut unavailable = Vec::new();
            let mut truncated = false;
            let answered = match collaboration.answered_human_requests(limit).await {
                Ok((requests, more)) => {
                    truncated |= more;
                    requests
                }
                Err(crate::collaboration::CollaborationError::Disabled) => Vec::new(),
                Err(error) => {
                    unavailable.push(format!("human requests: {error}"));
                    Vec::new()
                }
            };
            let decided = match reviews.decided(limit).await {
                Ok(page) => {
                    truncated |= page.has_more;
                    page.items
                }
                Err(error) => {
                    unavailable.push(format!("review items: {error}"));
                    Vec::new()
                }
            };
            let mut history = assemble_history(decided, &answered, limit);
            history.truncated |= truncated;
            history.unavailable = unavailable;
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
