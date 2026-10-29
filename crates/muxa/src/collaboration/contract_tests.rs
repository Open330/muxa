use super::*;
use serde_json::json;

fn actor(session: &str, pane: &str) -> Participant {
    serde_json::from_value(json!({"agent_kind":"codex","agent_session_id":session,"pane":pane,"room":{"host":"rmux","socket":"test","window_id":"@1"},"socket":"test","state":"idle","launch_purpose":"worker"})).unwrap()
}

fn contract() -> TaskContract {
    TaskContract {
        version: 1,
        task_id: "review-change".into(),
        objective: "Review the exact submitted commit".into(),
        acceptance_criteria: vec!["Check correctness".into(), "Check compatibility".into()],
        basis_revision: Some("a".repeat(40)),
        inputs: vec![],
    }
}

fn input() -> NewRequest {
    NewRequest {
        kind: RequestKind::Review,
        body: "Review this change".into(),
        task_contract: Some(contract()),
        ..Default::default()
    }
}

fn report(request: &CollaborationRequest) -> TaskReport {
    TaskReport {
        attempt_id: request.attempt.as_ref().unwrap().id.clone(),
        basis_revision: request.attempt.as_ref().unwrap().basis_revision.clone(),
        criteria: (0..2)
            .map(|criterion| CriterionReport {
                criterion,
                outcome: CriterionOutcome::Pass,
                evidence: vec![],
            })
            .collect(),
        evidence: vec![EvidenceReference {
            kind: EvidenceKind::AgentEvent,
            locator: "event:review-evidence".into(),
            sha256: None,
        }],
    }
}

async fn create(store: &CollaborationStore) -> CollaborationRequest {
    store
        .create(actor("sender", "%1"), actor("reviewer", "%2"), input())
        .await
        .unwrap()
}

async fn finish(
    store: &CollaborationStore,
    request: &CollaborationRequest,
    value: Option<TaskReport>,
) -> Result<CollaborationRequest, CollaborationError> {
    store
        .reply_with_report(
            &actor("reviewer", "%2"),
            &request.id,
            RequestStatus::Completed,
            "Findings and evidence".into(),
            vec![],
            vec![],
            value,
        )
        .await
}

#[tokio::test]
async fn completed_contract_requires_claim_matching_attempt_revision_and_all_evidence() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let request = create(&store).await;
    assert!(
        finish(&store, &request, Some(report(&request)))
            .await
            .is_err(),
        "queued work cannot claim completion"
    );
    store
        .claim_by_id(&actor("reviewer", "%2"), &request.id)
        .await
        .unwrap();
    assert!(finish(&store, &request, None).await.is_err());
    let mut wrong = report(&request);
    wrong.attempt_id = uuid::Uuid::new_v4().to_string();
    assert!(finish(&store, &request, Some(wrong)).await.is_err());
    let mut wrong = report(&request);
    wrong.basis_revision = Some("b".repeat(40));
    assert!(finish(&store, &request, Some(wrong)).await.is_err());
    let mut missing = report(&request);
    missing.criteria.pop();
    assert!(finish(&store, &request, Some(missing)).await.is_err());
    let mut duplicate = report(&request);
    duplicate.criteria[1].criterion = 0;
    assert!(finish(&store, &request, Some(duplicate)).await.is_err());
    let mut failed = report(&request);
    failed.criteria[1].outcome = CriterionOutcome::Fail;
    assert!(finish(&store, &request, Some(failed)).await.is_err());
    let mut no_evidence = report(&request);
    no_evidence.evidence.clear();
    assert!(finish(&store, &request, Some(no_evidence)).await.is_err());
    let valid = report(&request);
    let done = finish(&store, &request, Some(valid.clone())).await.unwrap();
    assert_eq!(done.status, RequestStatus::Completed);
    assert_eq!(done.reply.unwrap().report, Some(valid));
}

#[tokio::test]
async fn blockers_do_not_need_to_fabricate_completed_checks() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let request = create(&store).await;
    let blocked = store
        .reply(
            &actor("reviewer", "%2"),
            &request.id,
            RequestStatus::Blocked,
            "No checkout available".into(),
            vec![],
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(blocked.status, RequestStatus::Blocked);
    assert!(blocked.reply.unwrap().report.is_none());
}

#[tokio::test]
async fn a_contract_is_immutable_on_dispatch_retry_and_attempt_id_is_stable() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let mut value = input();
    value.dispatch_id = Some(uuid::Uuid::new_v4().to_string());
    let first = store
        .create(
            actor("sender", "%1"),
            actor("reviewer", "%2"),
            value.clone(),
        )
        .await
        .unwrap();
    let retry = store
        .create(
            actor("sender", "%1"),
            actor("reviewer", "%2"),
            value.clone(),
        )
        .await
        .unwrap();
    assert_eq!(first.attempt, retry.attempt);
    value.task_contract.as_mut().unwrap().basis_revision = Some("b".repeat(40));
    assert!(matches!(
        store
            .create(actor("sender", "%1"), actor("reviewer", "%2"), value)
            .await,
        Err(CollaborationError::DispatchConflict(_))
    ));
}

#[tokio::test]
async fn superseding_a_claimed_review_retains_late_evidence_without_completing_the_new_attempt() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let old = create(&store).await;
    store
        .claim_by_id(&actor("reviewer", "%2"), &old.id)
        .await
        .unwrap();
    let mut replacement = input();
    replacement.supersedes_request_id = Some(old.id.clone());
    replacement.task_contract.as_mut().unwrap().basis_revision = Some("b".repeat(40));
    let new = store
        .create(
            actor("sender", "%1"),
            actor("second-reviewer", "%3"),
            replacement,
        )
        .await
        .unwrap();
    assert_ne!(new.attempt, old.attempt);
    let late = finish(&store, &old, Some(report(&old))).await.unwrap();
    assert_eq!(late.status, RequestStatus::Cancelled);
    assert_eq!(late.superseded_by.as_deref(), Some(new.id.as_str()));
    assert!(late.reply.is_none());
    assert_eq!(late.late_replies.len(), 1);
    assert!(!late.reply_wake_owed());
    assert!(store
        .reserve_reply_wakes(&actor("sender", "%1"))
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        store
            .peek_for(&actor("sender", "%1"), &new.id)
            .await
            .unwrap()
            .status,
        RequestStatus::Queued
    );
    store
        .claim_by_id(&actor("second-reviewer", "%3"), &new.id)
        .await
        .unwrap();
    assert!(store
        .reply_with_report(
            &actor("second-reviewer", "%3"),
            &new.id,
            RequestStatus::Completed,
            "Wrong attempt".into(),
            vec![],
            vec![],
            Some(report(&old))
        )
        .await
        .is_err());
    let completed = store
        .reply_with_report(
            &actor("second-reviewer", "%3"),
            &new.id,
            RequestStatus::Completed,
            "Current findings".into(),
            vec![],
            vec![],
            Some(report(&new)),
        )
        .await
        .unwrap();
    assert_eq!(completed.status, RequestStatus::Completed);
    let batch = store
        .reserve_reply_wakes(&actor("sender", "%1"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        batch
            .requests
            .iter()
            .map(|r| r.id.as_str())
            .collect::<Vec<_>>(),
        vec![new.id]
    );
}

#[tokio::test]
async fn supersession_cannot_take_over_editing_work_or_another_senders_task() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let mut editing = input();
    editing.work_mode = WorkMode::Execute;
    let old = store
        .create(actor("sender", "%1"), actor("reviewer", "%2"), editing)
        .await
        .unwrap();
    let mut next = input();
    next.supersedes_request_id = Some(old.id.clone());
    assert!(store
        .create(actor("sender", "%1"), actor("second", "%3"), next)
        .await
        .is_err());
    let old = create(&store).await;
    let mut next = input();
    next.supersedes_request_id = Some(old.id.clone());
    assert!(store
        .create(actor("intruder", "%4"), actor("second", "%3"), next.clone())
        .await
        .is_err());
    next.task_contract.as_mut().unwrap().task_id = "other-task".into();
    assert!(store
        .create(actor("sender", "%1"), actor("second", "%3"), next)
        .await
        .is_err());
    let untouched = store
        .peek_for(&actor("sender", "%1"), &old.id)
        .await
        .unwrap();
    assert_eq!(untouched.status, RequestStatus::Queued);
    assert!(untouched.superseded_by.is_none());
}

#[tokio::test]
async fn supersession_and_late_reports_survive_restart_and_do_not_resurrect_wakes() {
    let dir = tempfile::tempdir().unwrap();
    let options = CollaborationOptions {
        path: Some(dir.path().join("mailbox.sqlite")),
        ..Default::default()
    };
    let store = CollaborationStore::load(options.clone()).await.unwrap();
    let old = create(&store).await;
    store
        .claim_by_id(&actor("reviewer", "%2"), &old.id)
        .await
        .unwrap();
    let mut next = input();
    next.supersedes_request_id = Some(old.id.clone());
    let new = store
        .create(actor("sender", "%1"), actor("second", "%3"), next)
        .await
        .unwrap();
    finish(&store, &old, Some(report(&old))).await.unwrap();
    drop(store);
    let loaded = CollaborationStore::load(options).await.unwrap();
    let stored = loaded
        .peek_for(&actor("sender", "%1"), &old.id)
        .await
        .unwrap();
    assert_eq!(stored.late_replies.len(), 1);
    assert_eq!(stored.superseded_by.as_ref(), Some(&new.id));
    assert!(loaded.pending_reply_unnotified().await.is_empty());
    assert_eq!(
        loaded
            .peek_for(&actor("sender", "%1"), &new.id)
            .await
            .unwrap()
            .attempt,
        new.attempt
    );
}

#[tokio::test]
async fn failed_claimed_supersession_keeps_notices_withdrawals_and_old_attempt_atomic() {
    let dir = tempfile::tempdir().unwrap();
    let options = CollaborationOptions {
        path: Some(dir.path().join("mailbox.sqlite")),
        ..Default::default()
    };
    let store = CollaborationStore::load(options).await.unwrap();
    let old = create(&store).await;
    store
        .claim_by_id(&actor("reviewer", "%2"), &old.id)
        .await
        .unwrap();
    let other = store
        .create(
            actor("sender", "%1"),
            actor("second", "%3"),
            NewRequest {
                body: "independent request".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let before = store.requests.read().await.clone();
    let changes = store.subscribe();
    let db = Connection::open(&store.database.as_ref().unwrap().path).unwrap();
    db.execute_batch("CREATE TRIGGER reject_staged BEFORE INSERT ON collaboration_requests BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
    let mut next = input();
    next.supersedes_request_id = Some(old.id.clone());
    next.withdraw_reply_wakes = vec![other.id];
    assert!(store
        .create(actor("sender", "%1"), actor("second", "%3"), next)
        .await
        .is_err());
    assert_eq!(*store.requests.read().await, before);
    assert!(!changes.has_changed().unwrap());
    let stored: HashMap<_, _> = store
        .query_for_offset(
            &Participant::console(old.from.room),
            RequestMailbox::All,
            MailboxScope::All,
            &CollaborationQuery::default(),
            0,
        )
        .await
        .unwrap()
        .requests
        .into_iter()
        .map(|r| (r.id.clone(), r))
        .collect();
    assert_eq!(stored, before);
}

#[tokio::test]
async fn failed_supersession_persistence_rolls_back_the_old_request_and_new_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let options = CollaborationOptions {
        path: Some(dir.path().join("mailbox.sqlite")),
        ..Default::default()
    };
    let store = CollaborationStore::load(options).await.unwrap();
    let old = create(&store).await;
    let db = Connection::open(&store.database.as_ref().unwrap().path).unwrap();
    db.execute_batch("CREATE TRIGGER reject_supersession BEFORE INSERT ON collaboration_requests BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
    let mut next = input();
    next.supersedes_request_id = Some(old.id.clone());
    assert!(store
        .create(actor("sender", "%1"), actor("second", "%3"), next)
        .await
        .is_err());
    assert_eq!(store.requests.read().await.len(), 1);
    let intact = store
        .peek_for(&actor("sender", "%1"), &old.id)
        .await
        .unwrap();
    assert_eq!(intact.status, RequestStatus::Queued);
    assert!(intact.superseded_by.is_none());
}

#[tokio::test]
async fn moving_sender_preserves_reply_ownership_and_wakes_the_new_endpoint() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let old = store
        .create(
            actor("sender", "%1"),
            actor("reviewer", "%2"),
            NewRequest {
                body: "legacy request".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    store
        .reply(
            &actor("reviewer", "%2"),
            &old.id,
            RequestStatus::Completed,
            "done".into(),
            vec![],
            vec![],
        )
        .await
        .unwrap();
    let moved = actor("sender", "%9");
    let reserved = store.reserve_reply_wakes(&moved).await.unwrap().unwrap();
    assert_eq!(reserved.requests[0].id, old.id);
    assert!(store
        .reserve_reply_wakes(&actor("different", "%1"))
        .await
        .unwrap()
        .is_none());
}

#[test]
fn contracts_reject_unknown_versions_unbounded_data_and_malformed_revisions() {
    let mut value = contract();
    value.version = 2;
    assert!(value.validate().is_err());
    let mut value = contract();
    value.acceptance_criteria = vec![];
    assert!(value.validate().is_err());
    let mut value = contract();
    value.basis_revision = Some("main".into());
    assert!(value.validate().is_err());
    let mut value = contract();
    value
        .acceptance_criteria
        .push(value.acceptance_criteria[0].clone());
    assert!(value.validate().is_err());
    let mut value = contract();
    value.inputs = vec![EvidenceReference {
        kind: EvidenceKind::Artifact,
        locator: "artifact".into(),
        sha256: Some("wrong".into()),
    }];
    assert!(value.validate().is_err());
    let mut value = serde_json::to_value(contract()).unwrap();
    value["may_commit"] = json!(true);
    assert!(TaskContract::from_input(value).is_err());
}

#[tokio::test]
async fn legacy_requests_remain_uncontracted_and_reply_without_new_fields() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let request = store
        .create(
            actor("sender", "%1"),
            actor("reviewer", "%2"),
            NewRequest {
                body: "legacy".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let done = store
        .reply(
            &actor("reviewer", "%2"),
            &request.id,
            RequestStatus::Completed,
            "done".into(),
            vec![],
            vec![],
        )
        .await
        .unwrap();
    assert!(done.task_contract.is_none());
    assert!(done.attempt.is_none());
    let encoded = serde_json::to_value(done).unwrap();
    for key in [
        "task_contract",
        "attempt",
        "superseded_by",
        "supersedes_request_id",
        "late_replies",
    ] {
        assert!(encoded.get(key).is_none());
    }
    assert!(encoded["reply"].get("report").is_none());
}

#[tokio::test]
async fn retrying_the_same_structured_reply_does_not_duplicate_the_terminal_or_late_report() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let old = create(&store).await;
    store
        .claim_by_id(&actor("reviewer", "%2"), &old.id)
        .await
        .unwrap();
    let first = finish(&store, &old, Some(report(&old))).await.unwrap();
    let retry = finish(&store, &old, Some(report(&old))).await.unwrap();
    assert_eq!(first.reply, retry.reply);
    let mut next = input();
    next.supersedes_request_id = Some(old.id.clone());
    store
        .create(actor("sender", "%1"), actor("second", "%3"), next)
        .await
        .unwrap();
    let original = create(&store).await;
    store
        .claim_by_id(&actor("reviewer", "%2"), &original.id)
        .await
        .unwrap();
    let mut next = input();
    next.supersedes_request_id = Some(original.id.clone());
    store
        .create(actor("sender", "%1"), actor("second", "%3"), next)
        .await
        .unwrap();
    finish(&store, &original, Some(report(&original)))
        .await
        .unwrap();
    let retry = finish(&store, &original, Some(report(&original)))
        .await
        .unwrap();
    assert_eq!(retry.late_replies.len(), 1);
}

#[tokio::test]
async fn legacy_only_storage_remains_v1_and_contract_upgrade_is_transactional() {
    let dir = tempfile::tempdir().unwrap();
    let options = CollaborationOptions {
        path: Some(dir.path().join("legacy.sqlite")),
        ..CollaborationOptions::default()
    };
    let store = CollaborationStore::load(options.clone()).await.unwrap();
    let legacy = store
        .create(
            actor("sender", "%1"),
            actor("reviewer", "%2"),
            NewRequest {
                body: "legacy request".into(),
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();
    let db = Connection::open(&store.database.as_ref().unwrap().path).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        1
    );
    db.execute_batch("CREATE TRIGGER reject_contract BEFORE INSERT ON collaboration_requests BEGIN SELECT RAISE(ABORT,'injected failure'); END;").unwrap();
    assert!(store
        .create(actor("sender", "%1"), actor("reviewer", "%2"), input())
        .await
        .is_err());
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        1,
        "failed contracted writes must not lock out older daemons"
    );
    db.execute_batch("DROP TRIGGER reject_contract").unwrap();
    let request = create(&store).await;
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, u32>(0))
            .unwrap(),
        2
    );
    drop(store);
    let loaded = CollaborationStore::load(options).await.unwrap();
    assert!(loaded
        .peek_for(&actor("sender", "%1"), &legacy.id)
        .await
        .unwrap()
        .task_contract
        .is_none());
    assert_eq!(
        loaded
            .peek_for(&actor("sender", "%1"), &request.id)
            .await
            .unwrap()
            .task_contract,
        request.task_contract
    );
}

#[tokio::test]
async fn stored_contracts_are_forward_readable_while_new_inputs_reject_unknown_fields() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let request = create(&store).await;
    let mut encoded = serde_json::to_value(&request).unwrap();
    encoded["task_contract"]["future_metadata"] = json!("new-server-field");
    encoded["attempt"]["started_at"] = json!("2026-10-28T00:00:00Z");
    let read: CollaborationRequest = serde_json::from_value(encoded.clone()).unwrap();
    assert_eq!(read.attempt, request.attempt);
    assert!(TaskContract::from_input(encoded["task_contract"].clone()).is_err());
    let mut wire = serde_json::to_value(input()).unwrap();
    wire["task_contract"]["typo"] = json!(true);
    assert!(serde_json::from_value::<NewRequest>(wire).is_err());
}

#[tokio::test]
async fn console_supersession_cannot_cross_the_initiating_agent() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let sender = Participant::console(actor("sender", "%1").room);
    let mut original = input();
    original.initiator = Some(Box::new(actor("agent-a", "%1")));
    let old = store
        .create(sender.clone(), actor("reviewer", "%2"), original)
        .await
        .unwrap();
    let mut next = input();
    next.supersedes_request_id = Some(old.id.clone());
    next.initiator = Some(Box::new(actor("agent-b", "%3")));
    assert!(store
        .create(sender.clone(), actor("reviewer", "%2"), next.clone())
        .await
        .is_err());
    next.initiator = None;
    assert!(store
        .create(sender.clone(), actor("reviewer", "%2"), next.clone())
        .await
        .is_err());
    next.initiator = Some(Box::new(actor("agent-a", "%9")));
    store
        .create(sender, actor("reviewer", "%2"), next)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_managed_work_sender_can_move_rooms_and_keep_its_supersession_thread() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let mut original = input();
    original.workspace_id = Some("workspace".into());
    original.work_id = Some("work".into());
    let old = store
        .create(
            actor("sender", "%1"),
            actor("reviewer", "%2"),
            original.clone(),
        )
        .await
        .unwrap();
    let mut moved = actor("sender", "%9");
    moved.room.window_id = "@9".into();
    let mut next = original;
    next.supersedes_request_id = Some(old.id.clone());
    let new = store
        .create(moved, actor("second", "%3"), next)
        .await
        .unwrap();
    assert_eq!(new.thread_id, old.thread_id);
    assert_eq!(
        store
            .peek_for(&actor("reviewer", "%2"), &old.id)
            .await
            .unwrap()
            .updates
            .len(),
        0,
        "unclaimed work does not need a stop notification"
    );
}

#[tokio::test]
async fn historical_reports_retire_only_pending_supersession_notifications() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let old = create(&store).await;
    store
        .claim_by_id(&actor("reviewer", "%2"), &old.id)
        .await
        .unwrap();
    let mut next = input();
    next.supersedes_request_id = Some(old.id.clone());
    store
        .create(actor("sender", "%1"), actor("second", "%3"), next)
        .await
        .unwrap();
    assert_eq!(
        store
            .pending_unnotified()
            .await
            .iter()
            .filter(|r| r.kind == RequestKind::Notice)
            .count(),
        1
    );
    finish(&store, &old, Some(report(&old))).await.unwrap();
    assert_eq!(
        store
            .pending_unnotified()
            .await
            .iter()
            .filter(|r| r.kind == RequestKind::Notice)
            .count(),
        0
    );
    let history = store.requests.read().await;
    let notice = history
        .values()
        .find(|r| r.kind == RequestKind::Notice)
        .unwrap();
    assert_eq!(notice.status, RequestStatus::Completed);
    assert!(!notice.expects_reply);
}

#[tokio::test]
async fn unavailable_mailbox_refuses_writes_with_the_initialization_reason() {
    let store = CollaborationStore::unavailable(
        CollaborationOptions::default(),
        "invalid database header".into(),
    );
    assert!(!store.enabled());
    let error = store
        .create(actor("sender", "%1"), actor("reviewer", "%2"), input())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("invalid database header"));
    assert!(store.requests.read().await.is_empty());
}

#[test]
fn fleet_contract_attribution_cannot_be_silently_omitted() {
    let mut value = input();
    assert!(value.validate_fleet_contract().is_err());
    value.initiator = Some(Box::new(Participant::console(actor("sender", "%1").room)));
    assert!(value.validate_fleet_contract().is_err());
    value.initiator = Some(Box::new(actor("sender", "%1")));
    assert!(value.validate_fleet_contract().is_ok());
    value.task_contract = None;
    value.initiator = None;
    assert!(
        value.validate_fleet_contract().is_ok(),
        "legacy dispatch remains usable"
    );
}
