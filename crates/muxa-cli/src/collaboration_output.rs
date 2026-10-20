//! Transport observations, separate from the recipient's declared work result.
use muxa::collaboration::{CollaborationRequest, Participant, RequestStatus};
use serde_json::{json, Value};

fn endpoint(participant: &Participant) -> Value {
    let mut value = json!({"pane": participant.pane, "agent_kind": participant.agent_kind,
        "agent_session_id": participant.agent_session_id,
        "room": participant.room, "socket": participant.socket});
    if let Some(alias) = &participant.alias {
        value["alias"] = json!(alias);
    }
    if !participant.roles.is_empty() {
        value["roles"] = json!(participant.roles);
    }
    value
}

fn delivery_state(request: &CollaborationRequest) -> &'static str {
    match request.status {
        RequestStatus::Queued => "queued",
        RequestStatus::Cancelled => "cancelled",
        RequestStatus::Expired => "expired",
        _ if request.reply.is_some() => "reply_received",
        RequestStatus::Completed if !request.expects_reply => "delivered",
        status if status.is_terminal() => "closed",
        _ => "claimed",
    }
}

pub(crate) fn receipt(request: &CollaborationRequest) -> Value {
    let mut value = mutation_receipt(request);
    value["from"] = endpoint(&request.from);
    value["to"] = endpoint(&request.to);
    for (key, field) in [
        ("thread_id", &request.thread_id),
        ("parent_request_id", &request.parent_request_id),
        ("workspace_id", &request.workspace_id),
        ("work_id", &request.work_id),
        ("run_id", &request.run_id),
    ] {
        if let Some(field) = field {
            value[key] = json!(field);
        }
    }
    if let Some(initiator) = &request.initiator {
        value["initiator"] = endpoint(initiator);
    }
    if let Some(source_node) = &request.source_node_id {
        value["source_node_id"] = json!(source_node);
    }
    if let Some(parent) = &request.delegation_parent {
        value["delegation_parent"] = json!(parent);
    }
    value
}

/// A subsequent mutation needs correlation and transport state, not repeated routing.
pub(crate) fn mutation_receipt(request: &CollaborationRequest) -> Value {
    let mut value = json!({
        "request_id": request.id,
        "delivery_state": delivery_state(request),
        "final_reply_received": request.reply.is_some(),
        "expects_reply": request.expects_reply,
    });
    if let Some(reply) = &request.reply {
        value["reported_status"] = json!(reply.status);
    }
    if let Some(update) = request.updates.last() {
        value["next_after_update"] = json!(update.sequence);
    }
    if let Some(action) = request.human_action {
        value["human_action"] = json!(action);
    }
    if let Some(interruption) = &request.interruption {
        value["interruption"] = json!({"active":interruption.active,
            "reason":interruption.reason, "action_request_id":interruption.action_request_id});
    }
    value
}

pub(crate) fn outcome(request: &CollaborationRequest) -> Value {
    let mut value = receipt(request);
    if let Some(reply) = &request.reply {
        value["reply"] = json!(reply);
    }
    if let Some(interruption) = &request.interruption {
        value["interruption"] = json!(interruption);
    }
    if request.peer_interrupted() {
        value["reason"] = json!("peer_interrupted");
    }
    value
}

/// Preserve all instructions and artifacts needed to act, without audit metadata.
pub(crate) fn message(request: &CollaborationRequest, include_body: bool) -> Value {
    let mut value = receipt(request);
    value["kind"] = json!(request.kind);
    value["work_mode"] = json!(request.work_mode);
    if let Some(update) = request.updates.last() {
        value["next_after_update"] = json!(update.sequence);
    }
    if include_body {
        value["body"] = json!(request.body);
        value["paths"] = json!(request.paths);
        value["artifacts"] = json!(request.artifacts);
        value["links"] = json!(request.links);
        value["air_artifacts"] = json!(request.air_artifacts);
        if !request.updates.is_empty() {
            value["updates"] = updates(request, 0);
            value["first_retained_sequence"] = json!(request.updates.first().map(|u| u.sequence));
        }
    }
    value
}

/// Progress reads never repeat routing, instructions, or a potentially large reply.
pub(crate) fn progress(request: &CollaborationRequest, after: u64) -> Value {
    let mut value = mutation_receipt(request);
    value["updates"] = updates(request, after);
    value["first_retained_sequence"] = json!(request.updates.first().map(|u| u.sequence));
    value
}

pub(crate) fn updates(request: &CollaborationRequest, after: u64) -> Value {
    json!(request
        .updates
        .iter()
        .filter(|u| u.sequence > after)
        .map(|u| { json!({"sequence":u.sequence, "author":endpoint(&u.author), "body":u.body}) })
        .collect::<Vec<_>>())
}

#[cfg(test)]
pub(crate) fn fixture() -> CollaborationRequest {
    let participant = |pane: &str| {
        json!({
            "agent_kind":"codex", "agent_session_id":pane, "pane":pane,
            "room":{"host":"local", "window_id":"@1"}, "state":"idle"
        })
    };
    serde_json::from_value(json!({
        "id":"req-test", "from":participant("%1"), "to":participant("%2"),
        "kind":"task", "body":"original instructions", "expects_reply":true,
        "work_mode":"execute", "paths":["src/allowed.rs"],
        "artifacts":["commit:abc"], "links":["https://example.test"],
        "status":"claimed", "created_at":"2026-10-14T00:00:00Z"
    }))
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::collaboration::{CollaborationReply, CollaborationUpdate};
    use time::OffsetDateTime;

    #[test]
    fn interruption_keeps_compact_transport_and_parent_work_open() {
        let mut request = fixture();
        request.interruption = Some(muxa::collaboration::PeerInterruption {
            request_id: request.id.clone(),
            reason: "stopped".into(),
            active: true,
            observed_at: OffsetDateTime::UNIX_EPOCH,
            reset_at: None,
            action_request_id: Some("human-choice".into()),
            decision: None,
        });
        let observed = outcome(&request);
        assert_eq!(observed["reason"], "peer_interrupted");
        assert_eq!(observed["final_reply_received"], false);
        assert_eq!(
            observed["interruption"]["action_request_id"],
            "human-choice"
        );
        assert!(observed.get("body").is_none());
        assert!(observed.get("completed").is_none());
        assert!(observed.get("reported_status").is_none());
        let mutation = mutation_receipt(&request);
        assert_eq!(
            mutation["interruption"]["action_request_id"],
            "human-choice"
        );
        assert_eq!(mutation["final_reply_received"], false);
        let listed = receipt(&request);
        assert!(listed["interruption"].get("decision").is_none());
        assert!(listed["interruption"].get("observed_at").is_none());
    }

    #[test]
    fn receipt_reports_reply_arrival_without_asserting_work_success() {
        for status in [
            RequestStatus::Completed,
            RequestStatus::Failed,
            RequestStatus::Blocked,
            RequestStatus::Declined,
        ] {
            let mut request = fixture();
            request.status = status;
            request.reply = Some(CollaborationReply {
                status,
                body: "pending is discussed here; this is a final reply".into(),
                artifacts: vec![],
                air_artifacts: vec![],
                at: OffsetDateTime::UNIX_EPOCH,
            });
            let mutation = mutation_receipt(&request);
            assert_eq!(mutation["final_reply_received"], true);
            assert_eq!(mutation["reported_status"], json!(status));
            for field in ["from", "to", "initiator", "body", "reply", "completed"] {
                assert!(mutation.get(field).is_none(), "unexpected {field}");
            }
            let result = outcome(&request);
            assert_eq!(result["final_reply_received"], true);
            assert_eq!(result["reported_status"], json!(status));
            assert_eq!(result["delivery_state"], "reply_received");
            for field in ["completed", "status", "body", "provenance", "created_at"] {
                assert!(result.get(field).is_none(), "unexpected {field}");
            }
        }
    }

    #[test]
    fn compact_receipts_keep_review_alias_and_legacy_closure() {
        let mut request = fixture();
        request.to.alias = Some("claude-reviewer".into());
        request.to.roles = vec!["reviewer".into()];
        request.status = RequestStatus::Completed;
        let result = outcome(&request);
        assert_eq!(result["to"]["alias"], "claude-reviewer");
        assert_eq!(result["to"]["roles"], json!(["reviewer"]));
        assert_eq!(result["delivery_state"], "closed");
        assert_eq!(result["final_reply_received"], false);
        assert!(result.get("reported_status").is_none());
    }

    #[test]
    fn notice_delivery_and_cancellation_are_not_final_replies() {
        let mut request = fixture();
        request.expects_reply = false;
        for (status, delivery) in [
            (RequestStatus::Completed, "delivered"),
            (RequestStatus::Cancelled, "cancelled"),
            (RequestStatus::Expired, "expired"),
        ] {
            request.status = status;
            let result = outcome(&request);
            assert_eq!(result["delivery_state"], delivery);
            assert_eq!(result["final_reply_received"], false);
            assert!(result.get("reported_status").is_none());
            assert!(result.get("reply").is_none());
        }
    }

    #[test]
    fn inbox_preserves_execution_contract_and_progress_cursor_is_incremental() {
        let mut request = fixture();
        request.updates = (1..=3)
            .map(|sequence| CollaborationUpdate {
                sequence,
                author: request.from.clone(),
                body: format!("update {sequence}"),
                at: OffsetDateTime::UNIX_EPOCH,
            })
            .collect();
        let inbox = message(&request, true);
        for field in ["body", "kind", "work_mode", "paths", "artifacts", "links"] {
            assert_eq!(inbox[field], json!(request)[field]);
        }
        assert_eq!(inbox["air_artifacts"], json!(request.air_artifacts));
        assert!(message(&request, false).get("body").is_none());
        assert_eq!(updates(&request, 2)[0]["body"], "update 3");
        assert_eq!(updates(&request, 3), json!([]));
        assert_eq!(inbox["next_after_update"], 3);
        assert_eq!(inbox["updates"].as_array().unwrap().len(), 3);
    }
}
