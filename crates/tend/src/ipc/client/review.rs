use super::{Client, RuntimeError, PROTOCOL_VERSION};
use crate::review::{
    DispositionKind, NewReview, ReviewActor, ReviewItem, ReviewPage, ReviewQuery, ReviewSummary,
};
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
        // The daemon may run the Work's declared checks first.
        let response = self
            .call_with_timeout(&req, crate::work_verify::VERIFY_CALL_TIMEOUT)
            .await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            return Err(super::response_error(&response));
        }
        field(&response, "review_item")
    }

    pub async fn review_get(&self, id: &str) -> Result<ReviewItem, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "review_get",
            "id": id,
        });
        field(&self.call_checked(&req).await?, "review_item")
    }

    pub async fn review_list(&self, query: &ReviewQuery) -> Result<ReviewPage, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "review_list",
            "open_only": query.open_only,
            "oldest_first": query.oldest_first,
            "limit": query.limit,
            "offset": query.offset,
        });
        let response = self.call_checked(&req).await?;
        Ok(ReviewPage {
            items: field(&response, "review_items")?,
            has_more: response["review_has_more"].as_bool().unwrap_or(false),
        })
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

    /// Record a claim and its disposition together. Requires
    /// `review_record_v1` and an idempotency key on `review`.
    pub async fn review_record(
        &self,
        review: &NewReview,
        disposition: DispositionKind,
        note: Option<&str>,
    ) -> Result<ReviewItem, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "review_record",
            "review": review,
            "disposition": disposition,
            "note": note,
        });
        // Like review_create, the daemon may run the Work's checks first.
        let response = self
            .call_with_timeout(&req, crate::work_verify::VERIFY_CALL_TIMEOUT)
            .await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            return Err(super::response_error(&response));
        }
        field(&response, "review_item")
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

impl Client {
    /// Everything pending on the person, in one ordered list. Requires
    /// `decision_queue_v1`.
    /// Human requests come back only for a console `origin`.
    pub async fn decision_queue(
        &self,
        origin: &crate::collaboration::CollaborationOrigin,
        limit: usize,
    ) -> Result<crate::decision_queue::PendingQueue, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "decision_queue",
            "limit": limit,
            "origin": origin,
        });
        field(&self.call_checked(&req).await?, "decision_queue")
    }

    /// Decided review items and answered human requests, newest first.
    /// Requires `decision_queue_v1`.
    pub async fn decision_history(
        &self,
        origin: &crate::collaboration::CollaborationOrigin,
        limit: usize,
    ) -> Result<crate::decision_queue::DecisionHistory, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "decision_history",
            "limit": limit,
            "origin": origin,
        });
        field(&self.call_checked(&req).await?, "decision_history")
    }
}

impl Client {
    /// Apply a Fleet review decision on this host, as the operator, and
    /// say exactly how it ended: the option must be one the item offers,
    /// and the item must end up holding this disposition and note (a
    /// repeat of it is a replay; anything else already recorded is a
    /// conflict). The relay's side of `fleet_review_dispose_v1`.
    ///
    /// Works against an older daemon too: its `review_dispose` keeps an
    /// existing same-kind decision whatever the note, so the outcome is
    /// judged from the item it returns, not from the call succeeding.
    pub async fn review_dispose_exactly(
        &self,
        request: &crate::fleet::FleetReviewDispose,
    ) -> Result<crate::fleet::FleetReviewOutcome, RuntimeError> {
        use crate::fleet::FleetReviewOutcome as Outcome;
        let not_found = |error: &RuntimeError| matches!(error, RuntimeError::Daemon(message) if message.contains("no review item"));
        let before = match self.review_get(&request.id).await {
            Ok(item) => item,
            Err(error) if not_found(&error) => {
                return Ok(Outcome::NotFound {
                    message: format!("no review item {}", request.id),
                });
            }
            Err(error) => return Err(error),
        };
        if let Some(option) = &request.option {
            if !before.offers(option) {
                return Ok(Outcome::Invalid {
                    message: "that option is not one this item offers".into(),
                });
            }
        }
        let note = crate::review::decision_note(request.option.as_deref(), request.note.as_deref());
        if before.decided_as(request.disposition, note.as_deref()) {
            return Ok(Outcome::Recorded {
                item: Box::new(before),
                replay: true,
            });
        }
        if let Some(existing) = &before.disposition {
            return Ok(Outcome::Conflict {
                message: format!(
                    "review {} is already {}",
                    request.id,
                    existing.kind.as_str()
                ),
            });
        }
        let by = ReviewActor {
            kind: crate::review::ActorKind::User,
            label: Some(request.by.clone()),
        };
        let result = self
            .review_dispose(&request.id, request.disposition, &by, note.as_deref())
            .await;
        // Read back what the store holds: another decision may have landed
        // first, and an older daemon returns that one as if it were ours.
        let after = match result {
            Ok(item) => item,
            Err(RuntimeError::Daemon(message)) if message.contains("invalid review request") => {
                return Ok(Outcome::Invalid { message });
            }
            Err(error) => match self.review_get(&request.id).await {
                Ok(item) if item.disposition.is_some() => item,
                _ => return Err(error),
            },
        };
        if after.id != request.id {
            return Err(RuntimeError::Daemon(format!(
                "asked to decide review {} and got {}",
                request.id, after.id
            )));
        }
        Ok(if after.decided_as(request.disposition, note.as_deref()) {
            // The same decision by someone else won the race: a replay of
            // theirs, not a new one in this person's name.
            let replay = after
                .disposition
                .as_ref()
                .is_some_and(|existing| existing.by != by);
            Outcome::Recorded {
                item: Box::new(after),
                replay,
            }
        } else {
            Outcome::Conflict {
                message: match &after.disposition {
                    Some(existing) => {
                        format!(
                            "review {} is already {}",
                            request.id,
                            existing.kind.as_str()
                        )
                    }
                    None => format!("review {} was not decided", request.id),
                },
            }
        })
    }
}
