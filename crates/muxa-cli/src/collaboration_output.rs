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
    if request.status == RequestStatus::Queued
        && !request.from.console
        && !request.to.accepts_peer_work()
    {
        value["deferred"] = json!("protected_primary");
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
    if let Some(attempt) = &request.attempt {
        value["attempt"] = json!(attempt);
        value["requested_basis_revision"] = json!(attempt.basis_revision);
        if request.reply.as_ref().is_some_and(|r| r.report.is_some()) {
            value["report_basis_matched"] = json!(true);
        }
        if request.reply.is_some() || !request.late_replies.is_empty() {
            value["evidence_verification"] = json!("not_independently_verified");
        }
    }
    if request.superseded_by.is_some() {
        value["result_applicability"] = json!("superseded");
    }
    if let Some(id) = &request.supersedes_request_id {
        value["supersedes_request_id"] = json!(id);
    }
    if let Some(id) = &request.superseded_by {
        value["superseded_by"] = json!(id);
    }
    if !request.late_replies.is_empty() {
        value["late_reply_count"] = json!(request.late_replies.len());
    }

    if let Some(update) = request.updates.last() {
        value["next_after_update"] = json!(update.sequence);
    }
    if let Some(action) = request.human_action {
        value["human_action"] = json!(action);
    }
    if let Some(retirement) = &request.retirement {
        value["retirement"] = json!(retirement);
        value["result_applicability"] = json!("retired");
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
    if !request.late_replies.is_empty() {
        value["late_replies"] = json!(request.late_replies);
    }
    if request.peer_interrupted() {
        value["reason"] = json!("peer_interrupted");
    }
    value
}

/// A completed synchronous short call needs its result and actual recipient,
/// rather than three copies of physical routing and the caller's own identity.
/// Preserve reply data and all transport/lineage warnings from the full outcome.
pub(crate) fn short_call_outcome(mut value: Value, request: &CollaborationRequest) -> Value {
    let object = value.as_object_mut().expect("call outcome is an object");
    object.remove("from");
    object.remove("to");
    if let Some(original) = object
        .get("selected_peer")
        .filter(|p| {
            p.get("agent_session_id").and_then(Value::as_str)
                != Some(request.to.agent_session_id.as_str())
        })
        .cloned()
    {
        let mut previous = json!({});
        for key in ["agent_session_id", "pane", "agent_kind", "alias", "roles"] {
            if let Some(field) = original.get(key).filter(|v| !v.is_null()) {
                previous[key] = field.clone();
            }
        }
        object.insert("original_selected_peer".into(), previous);
    }
    let mut recipient = json!({"agent_session_id":request.to.agent_session_id,
        "pane":request.to.pane,"agent_kind":request.to.agent_kind});
    if let Some(alias) = &request.to.alias {
        recipient["alias"] = json!(alias);
    }
    if !request.to.roles.is_empty() {
        recipient["roles"] = json!(request.to.roles);
    }
    object.insert("selected_peer".into(), recipient);
    if !request.rebound_from.is_empty() {
        object.insert("rebound_from".into(), json!(request.rebound_from));
    }
    for key in [
        "expanded_skill",
        "spawned",
        "parent_request_id",
        "workspace_id",
        "work_id",
        "run_id",
    ] {
        if object.get(key).is_some_and(Value::is_null) {
            object.remove(key);
        }
    }
    if object.get("thread_id").and_then(Value::as_str) == Some(request.id.as_str()) {
        object.remove("thread_id");
    }
    value
}

/// CLI mutation acknowledgement: initial routing, then correlation only.
/// Bodies already supplied by the caller are never echoed by default.
pub(crate) fn acknowledgement(request: &CollaborationRequest, full: bool) -> Value {
    if full {
        json!(request)
    } else if request.reply.is_some()
        || matches!(
            request.status,
            RequestStatus::Cancelled | RequestStatus::Expired
        )
    {
        mutation_receipt(request)
    } else {
        receipt(request)
    }
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
        if let Some(contract) = &request.task_contract {
            value["task_contract"] = json!(contract);
        }
        if !request.late_replies.is_empty() {
            value["late_replies"] = json!(request.late_replies);
        }
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

pub(crate) use muxa::collaboration::instruction::inbox_instruction;

/// Shared CLI/MCP inbox projection. Full records remain opt-in for diagnostics.
pub(crate) fn inbox(requests: &[CollaborationRequest], full: bool) -> Value {
    json!(requests
        .iter()
        .map(|request| {
            if full {
                json!(request)
            } else {
                inbox_instruction(request)
            }
        })
        .collect::<Vec<_>>())
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
        .map(|u| { json!({"sequence":u.sequence, "author":{"label":u.author.label(), "agent_session_id":u.author.agent_session_id,"pane":u.author.pane,"agent_kind":u.author.agent_kind}, "body":u.body}) })
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

#[cfg(all(test, unix))]
pub(crate) fn mock_collaboration_responses(
    socket: &std::path::Path,
    responses: Vec<(Value, Value)>,
) -> tokio::task::JoinHandle<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    tokio::spawn(async move {
        for (expected, response) in responses {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&line).unwrap()["kind"],
                "hello"
            );
            write.write_all(b"{\"ok\":true}\n").await.unwrap();
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            for (path, value) in expected.as_object().unwrap() {
                assert_eq!(
                    request.pointer(path).unwrap_or(&Value::Null),
                    value,
                    "request field {path}"
                );
            }
            let mut bytes = serde_json::to_vec(&response).unwrap();
            bytes.push(b'\n');
            write.write_all(&bytes).await.unwrap();
        }
    })
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
                report: None,
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
    #[test]
    fn retired_attempt_receipt_never_claims_a_peer_result() {
        let mut request = fixture();
        request.status = RequestStatus::Cancelled;
        request.reply = None;
        request.retirement = Some(muxa::collaboration::RequestRetirement {
            reason: "withdrawn review".into(),
            evidence: "update:2".into(),
            at: time::OffsetDateTime::UNIX_EPOCH,
        });
        let receipt = mutation_receipt(&request);
        assert_eq!(receipt["result_applicability"], "retired");
        assert_eq!(receipt["final_reply_received"], false);
        assert!(receipt.get("reported_status").is_none());
        assert!(receipt.get("body").is_none());
        assert_eq!(receipt["retirement"]["evidence"], "update:2");
    }

    #[test]
    fn compact_contract_results_keep_attribution_and_hide_evidence_bodies() {
        let mut value = serde_json::to_value(fixture()).unwrap();
        value["task_contract"] = json!({"version":1,"task_id":"review","objective":"private-objective","acceptance_criteria":["private-criterion"],"basis_revision":"a".repeat(40)});
        value["attempt"] =
            json!({"id":"attempt-id","task_id":"review","basis_revision":"a".repeat(40)});
        value["status"] = json!("cancelled");
        value["superseded_by"] = json!("new-request");
        value["late_replies"] = json!([{"status":"completed","body":"private-late-result","report":{"attempt_id":"attempt-id","basis_revision":"a".repeat(40),"criteria":[],"evidence":[{"kind":"agent_event","locator":"private-evidence"}]},"at":"2026-10-14T00:01:00Z"}]);
        let request: CollaborationRequest = serde_json::from_value(value).unwrap();
        let mut metadata = request.clone();
        muxa::collaboration::CollaborationReadView::Metadata.apply(&mut metadata);
        let compact = mutation_receipt(&metadata);
        assert_eq!(compact["result_applicability"], "superseded");
        assert_eq!(compact["late_reply_count"], 1);
        assert_eq!(
            compact["evidence_verification"],
            "not_independently_verified"
        );
        assert_eq!(compact["final_reply_received"], false);
        let serialized = serde_json::to_string(&metadata).unwrap();
        for private in [
            "private-objective",
            "private-criterion",
            "private-late-result",
            "private-evidence",
        ] {
            assert!(!serialized.contains(private));
        }
        let mut read = request;
        muxa::collaboration::CollaborationReadView::Outcome.apply(&mut read);
        let result = outcome(&read);
        assert_eq!(
            result["late_replies"][0]["report"]["evidence"][0]["locator"],
            "private-evidence"
        );
        assert_eq!(result["final_reply_received"], false);
    }
    #[test]
    fn metadata_keeps_report_presence_without_criterion_or_evidence_bodies() {
        let mut value = serde_json::to_value(fixture()).unwrap();
        value["attempt"] =
            json!({"id":"attempt","task_id":"review","basis_revision":"a".repeat(40)});
        value["status"] = json!("completed");
        value["reply"] = json!({"status":"completed","body":"private-result","at":"2026-10-14T00:00:00Z","report":{"attempt_id":"attempt","basis_revision":"a".repeat(40),"criteria":[{"criterion":0,"outcome":"pass","evidence":[{"kind":"artifact","locator":"private-evidence"}]}],"evidence":[]}});
        let mut request: CollaborationRequest = serde_json::from_value(value).unwrap();
        muxa::collaboration::CollaborationReadView::Metadata.apply(&mut request);
        let compact = mutation_receipt(&request);
        assert_eq!(compact["report_basis_matched"], true);
        assert_eq!(compact["requested_basis_revision"], "a".repeat(40));
        let serialized = serde_json::to_string(&request).unwrap();
        assert!(!serialized.contains("private-result"));
        assert!(!serialized.contains("private-evidence"));
        request.reply.as_mut().unwrap().report = None;
        assert!(mutation_receipt(&request)
            .get("report_basis_matched")
            .is_none());
    }
    #[test]
    fn compact_inbox_preserves_instructions_and_stable_authors_with_full_record_opt_in() {
        let mut request = fixture();
        request.from.cwd = Some("/private/project".into());
        request.updates = vec![CollaborationUpdate {
            sequence: 1,
            author: request.from.clone(),
            body: "required follow-up".into(),
            at: OffsetDateTime::UNIX_EPOCH,
        }];
        let records = vec![request.clone()];
        let compact = inbox(&records, false);
        for field in ["kind", "work_mode", "paths", "body", "expects_reply"] {
            assert_eq!(compact[0][field], json!(request)[field]);
        }
        assert_eq!(compact[0]["request_id"], request.id);
        assert_eq!(
            compact[0]["updates"][0]["author"]["agent_session_id"],
            json!(request.from.agent_session_id)
        );
        assert!(compact[0]["updates"][0]["author"].get("room").is_none());
        assert!(!compact.to_string().contains("/private/project"));
        assert!(compact[0].get("provenance").is_none());
        let full = inbox(&records, true);
        let restored: Vec<CollaborationRequest> = serde_json::from_value(full.clone()).unwrap();
        assert_eq!(serde_json::to_value(restored).unwrap(), full);
        assert_eq!(full[0], json!(request));
        assert!(compact.to_string().len() < full.to_string().len());
    }
    fn overhead_case(body: String) -> CollaborationRequest {
        let mut request = fixture();
        request.id = "req_0123456789abcdef_1".into();
        request.from.agent_session_id = "01a1820e-a385-79d0-acfd-8ebda41e2d5b".into();
        request.from.pane = "rmux:%290".into();
        request.from.cwd = Some("unused-audit".repeat(500));
        request.created_at = OffsetDateTime::now_utc();
        request.body = body;
        request.paths.clear();
        request.artifacts.clear();
        request.links.clear();
        request
    }

    fn overhead_measurement(name: &str, request: &CollaborationRequest) -> Value {
        let data = inbox(std::slice::from_ref(request), false);
        let cli = data.to_string().len() + 1;
        let mcp = json!({"requests":data}).to_string();
        let escaped = json!(request.body).to_string().len() - 2;
        let body = request.body.len();
        let mut payload = escaped + json!(request.paths).to_string().len();
        for key in [
            "task_contract",
            "attempt",
            "artifacts",
            "links",
            "air_artifacts",
        ] {
            if let Some(value) = data[0].get(key) {
                payload += value.to_string().len();
            }
        }
        payload += request
            .updates
            .iter()
            .map(|update| json!(update.body).to_string().len() - 2)
            .sum::<usize>();
        // MCP text payload and JSON wire bytes are different measurements.
        let wire = json!({"content":[{"type":"text","text":mcp}]})
            .to_string()
            .len();
        json!({"case":name,"body_utf8_bytes":body,"escaped_body_bytes":escaped,
            "json_escape_bytes":escaped-body,"cli_stdout_bytes":cli,
            "extra_over_raw_body_bytes":cli-body,"extra_over_raw_body_pct":f64::from(u32::try_from(cli-body).unwrap())/f64::from(u32::try_from(body).unwrap())*100.0,
            "instruction_value_bytes":payload,"structural_extra_bytes":cli-payload,
            "mcp_text_bytes":mcp.len(),"serialized_mcp_result_bytes":wire, "sender_routing_receipt_bytes":acknowledgement(request,false).to_string().len()+1, "legacy_sender_full_record_bytes":serde_json::to_string_pretty(request).unwrap().len()+1})
    }

    #[test]
    fn inbox_overhead_is_bounded_above_body_and_required_payload_values() {
        let mut cases = vec![
            ("ascii_100", overhead_case("x".repeat(100))),
            (
                "korean_4k",
                overhead_case(format!("{}x", "한".repeat(1365))),
            ),
            (
                "escaped",
                overhead_case(
                    [
                        char::from(34),
                        char::from(92),
                        char::from(10),
                        char::from(9),
                    ]
                    .iter()
                    .collect::<String>()
                    .repeat(128),
                ),
            ),
        ];
        let mut contracted = overhead_case("review this revision".into());
        let mut value = json!(contracted);
        value["task_contract"] = json!({"version":1,"task_id":"review","objective":"Review this revision","acceptance_criteria":["check compatibility"],"basis_revision":"a".repeat(40)});
        value["attempt"] =
            json!({"id":"attempt-test","task_id":"review","basis_revision":"a".repeat(40)});
        contracted = serde_json::from_value(value).unwrap();
        cases.push(("contract", contracted));
        let mut amended = overhead_case("original instructions".into());
        amended.updates = (1..=3)
            .map(|sequence| CollaborationUpdate {
                sequence,
                author: amended.from.clone(),
                body: format!("required guidance {sequence}"),
                at: OffsetDateTime::UNIX_EPOCH,
            })
            .collect();
        cases.push(("three_updates", amended));
        let mut report = Vec::new();
        for (name, request) in &cases {
            let measured = overhead_measurement(name, request);
            let budget = 240 + request.updates.len() * 140;
            assert!(
                measured["structural_extra_bytes"].as_u64().unwrap() <= budget as u64,
                "{measured}; budget={budget}"
            );
            let data = inbox_instruction(request);
            for field in ["body", "kind", "work_mode", "expects_reply"] {
                assert_eq!(data[field], json!(request)[field]);
            }
            assert_eq!(data["paths"], json!(request.paths));
            assert!(data.get("to").is_none());
            assert!(data.get("run_id").is_none());
            assert!(data.get("final_reply_received").is_none());
            assert!(data.get("delivery_state").is_none());
            assert!(data["from"].get("room").is_none());
            assert!(data.get("requested_basis_revision").is_none());
            if let Some(contract) = &request.task_contract {
                assert_eq!(data["task_contract"], json!(contract));
                assert_eq!(data["attempt"], json!(request.attempt));
            }
            if !request.updates.is_empty() {
                assert_eq!(
                    data["updates"].as_array().unwrap().len(),
                    request.updates.len()
                );
                assert_eq!(data["updates"][0]["body"], request.updates[0].body);
                assert_eq!(data["next_after_update"], 3);
                assert_eq!(data["first_retained_sequence"], 1);
            }
            report.push(measured);
        }
        // A regression to the old physical sender endpoint must exceed the
        // base budget, even though all functional instruction fields remain.
        let mut bloated = inbox_instruction(&cases[0].1);
        bloated["from"] = endpoint(&cases[0].1.from);
        assert!(json!([bloated]).to_string().len() + 1 - cases[0].1.body.len() > 240);
        assert_eq!(
            report[0]["structural_extra_bytes"],
            report[1]["structural_extra_bytes"]
        );
        assert_eq!(
            report[1]["structural_extra_bytes"],
            report[2]["structural_extra_bytes"]
        );
        if let Ok(path) = std::env::var("MUXA_OVERHEAD_REPORT") {
            if let Ok(source) = std::env::var("MUXA_OVERHEAD_REQUEST") {
                let requests: Vec<CollaborationRequest> =
                    serde_json::from_slice(&std::fs::read(source).unwrap()).unwrap();
                report.push(overhead_measurement("previous_live_body", &requests[0]));
            }
            std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        }
    }

    #[test]
    fn instruction_projection_retains_authority_context_and_omits_only_unneeded_routing() {
        let mut request = overhead_case("note".into());
        request.from.console = true;
        request.expects_reply = false;
        request.thread_id = Some(request.id.clone());
        request.parent_request_id = Some("parent".into());
        request.superseded_by = Some("replacement".into());
        let value = inbox_instruction(&request);
        assert_eq!(value["from"]["console"], true);
        assert_eq!(value["expects_reply"], false);
        assert_eq!(value["paths"], json!([]));
        assert_eq!(value["superseded_by"], "replacement");
        assert_eq!(value["parent_request_id"], "parent");
        assert!(value.get("thread_id").is_none());
        let mut raw = json!(request);
        raw["provenance"] =
            json!({"client_kind":"watch","origin_match":"mismatched","caller_pid":42});
        raw["to_slot"] = json!({"room":{"host":"local","window_id":"@1"},"role":"reviewer"});
        let request: CollaborationRequest = serde_json::from_value(raw).unwrap();
        let value = inbox_instruction(&request);
        assert_eq!(value["provenance"]["origin_match"], "mismatched");
        assert!(value["provenance"].get("caller_pid").is_none());
        assert_eq!(value["to_slot"]["role"], "reviewer");
        let mut stale = request.clone();
        stale.created_at = OffsetDateTime::now_utc() - time::Duration::hours(2);
        assert!(
            inbox_instruction(&stale)["request_age_minutes"]
                .as_i64()
                .unwrap()
                >= 120
        );
        assert!(inbox_instruction(&request)
            .get("request_age_minutes")
            .is_none());
        assert_eq!(
            inbox(std::slice::from_ref(&request), true)[0],
            json!(request)
        );
    }
    #[test]
    fn acknowledgements_do_not_repeat_supplied_bodies_and_full_remains_lossless() {
        let mut request = fixture();
        request.body = "original-body-marker".repeat(512);
        let first = acknowledgement(&request, false);
        assert!(first.get("body").is_none());
        assert!(first.get("to").is_some());
        request.reply = Some(CollaborationReply {
            status: RequestStatus::Completed,
            body: "reply-body-marker".repeat(512),
            artifacts: vec![],
            air_artifacts: vec![],
            at: OffsetDateTime::UNIX_EPOCH,
            report: None,
        });
        request.status = RequestStatus::Completed;
        let ack = acknowledgement(&request, false);
        assert_eq!(ack["reported_status"], "completed");
        assert_eq!(ack["final_reply_received"], true);
        assert!(ack.to_string().len() < 220);
        assert!(ack.get("from").is_none());
        assert!(ack.get("reply").is_none());
        assert_eq!(acknowledgement(&request, true), json!(request));
        request.reply = None;
        request.status = RequestStatus::Cancelled;
        assert_eq!(
            acknowledgement(&request, false)["delivery_state"],
            "cancelled"
        );
    }
}

#[cfg(test)]
mod short_call_tests {
    use super::*;
    #[test]
    fn short_result_retains_reply_warnings_lineage_and_real_recipient() {
        let mut request = fixture();
        request.to.alias = Some("reviewer".into());
        request.to.roles = vec!["reviewer".into()];
        request.rebound_from = vec!["previous-worker".into()];
        for status in ["completed", "blocked", "declined", "failed"] {
            let original = json!({"request_id":request.id,"thread_id":request.id,
                "from":request.from,"to":request.to,"selected_peer":{"agent_session_id":"stale-selection"},
                "sent":true,"final_reply_received":true,"reported_status":status,
                "reply":{"body":"complete original reply","status":status,"artifacts":["proof"]},
                "selection_reason":"explicit","expanded_skill":null,"spawned":null,
                "parent_request_id":null,"workspace_id":null,"work_id":null,"run_id":null,
                "result_applicability":"superseded","superseded_by":"next-request"});
            let compact = short_call_outcome(original.clone(), &request);
            assert_eq!(compact["reply"], original["reply"]);
            assert_eq!(compact["selected_peer"]["alias"], "reviewer");
            assert_eq!(compact["selected_peer"]["roles"], json!(["reviewer"]));
            assert_eq!(
                compact["original_selected_peer"]["agent_session_id"],
                "stale-selection"
            );
            assert_eq!(compact["rebound_from"], json!(["previous-worker"]));
            assert_eq!(compact["reported_status"], status);
            assert_eq!(compact["result_applicability"], "superseded");
            assert_eq!(compact["superseded_by"], "next-request");
            assert_eq!(
                compact["selected_peer"]["agent_session_id"],
                request.to.agent_session_id
            );
            assert!(compact.get("from").is_none() && compact.get("to").is_none());
            assert!(compact.get("thread_id").is_none() && compact.get("spawned").is_none());
            assert!(compact.to_string().len() < original.to_string().len());
            let mut threaded = original;
            threaded["thread_id"] = json!("parent-thread");
            threaded["parent_request_id"] = json!("parent");
            threaded["spawned"] = json!({"pane":"%2"});
            threaded["work_id"] = json!("work");
            threaded["expanded_skill"] = json!("review");
            let compact = short_call_outcome(threaded.clone(), &request);
            for key in [
                "thread_id",
                "parent_request_id",
                "spawned",
                "work_id",
                "expanded_skill",
            ] {
                assert_eq!(compact[key], threaded[key]);
            }
        }
    }
}
