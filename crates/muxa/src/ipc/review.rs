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
        RequestBody::ReviewList { open_only, limit } => (
            "review_list",
            reviews
                .list(open_only, limit)
                .await
                .map(Response::with_review_items),
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
        _ => return ("review", Response::err("not a review request")),
    };
    match result {
        (kind, Ok(response)) => (kind, response),
        (kind, Err(error)) => (kind, Response::err(error.to_string())),
    }
}
