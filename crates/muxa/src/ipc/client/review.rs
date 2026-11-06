use super::{Client, RuntimeError, PROTOCOL_VERSION};
use crate::review::{DispositionKind, NewReview, ReviewActor, ReviewItem, ReviewSummary};
use std::time::Duration;

fn field<T: serde::de::DeserializeOwned>(
    response: &serde_json::Value,
    name: &str,
) -> Result<T, RuntimeError> {
    serde_json::from_value(response[name].clone()).map_err(RuntimeError::Json)
}

impl Client {
    /// Record a claim that a result needs review. Requires `review_items_v1`.
    pub async fn review_create(&self, review: &NewReview) -> Result<ReviewItem, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "review_create",
            "review": review,
        });
        field(&self.call_checked(&req).await?, "review_item")
    }

    pub async fn review_get(&self, id: &str) -> Result<ReviewItem, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "review_get",
            "id": id,
        });
        field(&self.call_checked(&req).await?, "review_item")
    }

    pub async fn review_list(
        &self,
        open_only: bool,
        limit: usize,
    ) -> Result<Vec<ReviewItem>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "review_list",
            "open_only": open_only,
            "limit": limit,
        });
        field(&self.call_checked(&req).await?, "review_items")
    }

    /// Bounded aggregate under `deadline`, for status surfaces that must
    /// never stall.
    pub async fn review_summary_with_timeout(
        &self,
        deadline: Duration,
    ) -> Result<ReviewSummary, RuntimeError> {
        let req = serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "review_summary" });
        let response = self.call_with_timeout(&req, deadline).await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            return Err(super::response_error(&response));
        }
        field(&response, "review_summary")
    }

    pub async fn review_dispose(
        &self,
        id: &str,
        disposition: DispositionKind,
        by: &ReviewActor,
        note: Option<&str>,
    ) -> Result<ReviewItem, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "review_dispose",
            "id": id,
            "disposition": disposition,
            "by": by,
            "note": note,
        });
        field(&self.call_checked(&req).await?, "review_item")
    }
}
