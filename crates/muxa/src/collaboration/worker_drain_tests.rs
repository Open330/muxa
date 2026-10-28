use super::*;
use crate::collaboration::{
    CollaborationOptions, CollaborationOrigin, NewRequest, WorkerSpawnAcquisition, WorkerSpawnSpec,
};

pub(crate) fn worker() -> Participant {
    serde_json::from_value(serde_json::json!({
        "agent_kind":"codex","agent_session_id":"worker","pane":"%2","socket":"default",
        "room":{"host":"tmux","socket":"default","window_id":"@1"},"state":"idle","launch_purpose":"worker"
    })).unwrap()
}
fn sender() -> Participant {
    let mut p = worker();
    p.agent_session_id = "sender".into();
    p.pane = "%1".into();
    p
}
fn policy() -> WorkerDrainPolicy {
    WorkerDrainPolicy {
        min_observed_idle_secs: 0,
        allow_error: false,
    }
}
async fn reserve(store: &CollaborationStore, phase: WorkerDrainPhase) -> WorkerDrainToken {
    let token = WorkerDrainToken {
        drain_id: uuid::Uuid::new_v4().to_string(),
        generation: 1,
        target: WorkerDrainTarget::from(&worker()),
        process_fence: ProcessFence {
            pid: std::process::id(),
            birth: "unknown-fixture-birth".into(),
        },
        expires_at: OffsetDateTime::now_utc() + time::Duration::seconds(30),
    };
    let record = WorkerDrainRecord {
        view: WorkerDrainView {
            token: token.clone(),
            phase,
            operation_id: (phase == WorkerDrainPhase::StopRequested)
                .then(|| uuid::Uuid::new_v4().to_string()),
            detail: "fixture".into(),
        },
        policy: policy(),
        roles: Vec::new(),
        finished_at: None,
        stop_outcome: None,
    };
    store.persist_drain(&record).unwrap();
    store.worker_drains.lock().await.push(record);
    token
}
fn input() -> NewRequest {
    NewRequest {
        body: "work".into(),
        ..NewRequest::default()
    }
}

#[tokio::test]
async fn worker_drain_exact_work_vetoes_before_stop_and_is_refused_after_commit() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    reserve(&store, WorkerDrainPhase::Draining).await;
    store.create(sender(), worker(), input()).await.unwrap();
    assert_eq!(
        store.worker_drains().await[0].phase,
        WorkerDrainPhase::Aborted
    );
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    reserve(&store, WorkerDrainPhase::StopRequested).await;
    let error = store
        .create(sender(), worker(), input())
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("worker_retiring"), "{error}");
    assert!(store.requests.read().await.is_empty());
    assert!(store.create(worker(), sender(), input()).await.is_err());
}

#[tokio::test]
async fn worker_drain_blocks_claim_direct_wake_and_reply_reservation_even_with_stale_participant() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let request = store.create(sender(), worker(), input()).await.unwrap();
    reserve(&store, WorkerDrainPhase::Draining).await;
    assert!(store.claim_for(&worker()).await.is_err());
    assert!(store.claim_by_id(&worker(), &request.id).await.is_err());
    assert!(store
        .prepare_direct_wake(&worker(), &request.id)
        .await
        .unwrap()
        .is_none());
    assert!(store
        .reserve_reply_wakes(&worker())
        .await
        .unwrap()
        .is_none());
    assert_eq!(
        store.requests.read().await[&request.id].status,
        RequestStatus::Queued
    );
}

#[tokio::test]
async fn worker_drain_availability_and_capacity_do_not_spawn_around_a_drain() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let token = reserve(&store, WorkerDrainPhase::Draining).await;
    let mut participants = vec![worker()];
    store.enrich_worker_drains(&mut participants).await;
    assert!(!participants[0].accepts_peer_work());
    let from = sender();
    let origin = CollaborationOrigin {
        exact_session: false,
        pane: from.pane.clone(),
        socket: from.socket.clone(),
        console: false,
        agent_session_id: Some(from.agent_session_id.clone()),
    };
    let outcome = store
        .acquire_worker_spawn(
            &from,
            origin,
            WorkerSpawnSpec {
                agent_kind: AgentKind::Codex,
                role: None,
                launch_role: "reviewer".into(),
            },
            &[worker()],
            &[],
        )
        .await
        .unwrap();
    assert!(
        matches!(outcome,WorkerSpawnAcquisition::Draining{drain_id,..} if drain_id==token.drain_id)
    );
    assert!(store.worker_spawn_records().await.is_empty());
}

#[tokio::test]
async fn worker_drain_expiry_token_tampering_and_committed_stop_replay() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let token = reserve(&store, WorkerDrainPhase::Draining).await;
    let mut forged = token.clone();
    forged.generation += 1;
    assert!(store.worker_drain_renew(&forged).await.is_err());
    store.worker_drains.lock().await[0].view.token.expires_at =
        OffsetDateTime::now_utc() - time::Duration::seconds(1);
    assert!(!store.worker_input_blocked("%2", Some("default")).await);
    assert!(store.worker_drain_renew(&token).await.is_err());
    let token = reserve(&store, WorkerDrainPhase::StopRequested).await;
    let op = store
        .worker_drains
        .lock()
        .await
        .last()
        .unwrap()
        .view
        .operation_id
        .clone()
        .unwrap();
    let (result, dispatch) = store
        .worker_drain_begin_stop(&token, &op, &[], &[], &[])
        .await
        .unwrap();
    assert!(!dispatch);
    assert_eq!(result.outcome, WorkerDrainStopOutcome::Uncertain);
    assert!(store
        .worker_drain_begin_stop(&token, &uuid::Uuid::new_v4().to_string(), &[], &[], &[])
        .await
        .is_err());
    assert!(store
        .worker_drain_finish(&token, WorkerDrainFinish::Aborted, "retry")
        .await
        .is_err());
}

#[tokio::test]
async fn worker_drain_restart_keeps_committed_stops_uncertain_and_expires_pre_stop_leases() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mailbox.json");
    let opts = CollaborationOptions {
        path: Some(path),
        ..CollaborationOptions::default()
    };
    let store = CollaborationStore::load(opts.clone()).await.unwrap();
    reserve(&store, WorkerDrainPhase::StopRequested).await;
    let token = reserve(&store, WorkerDrainPhase::Draining).await;
    {
        let mut records = store.worker_drains.lock().await;
        let r = records
            .iter_mut()
            .find(|r| r.view.token.drain_id == token.drain_id)
            .unwrap();
        r.view.token.expires_at = OffsetDateTime::now_utc() - time::Duration::seconds(1);
        store.persist_drain(r).unwrap();
    }
    drop(store);
    let loaded = CollaborationStore::load(opts).await.unwrap();
    let rows = loaded.worker_drains().await;
    assert_eq!(rows[0].phase, WorkerDrainPhase::Uncertain);
    assert_eq!(rows[1].phase, WorkerDrainPhase::Aborted);
    assert!(loaded.worker_input_blocked("%2", Some("default")).await);
}

#[tokio::test]
async fn worker_drain_automation_veto_and_action_serialization() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    reserve(&store, WorkerDrainPhase::Draining).await;
    let gate = store.worker_action_guard("%2").await;
    let pending = {
        let store = store.clone();
        tokio::spawn(async move {
            store
                .reserve_worker_automation("worker", "%2", Some("default"))
                .await
        })
    };
    tokio::task::yield_now().await;
    assert!(!pending.is_finished());
    drop(gate);
    let reservation = pending.await.unwrap().unwrap();
    assert_eq!(
        store.worker_drains().await[0].phase,
        WorkerDrainPhase::Aborted
    );
    assert_eq!(store.worker_automation.read().await["worker"], 1);
    drop(reservation);
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    assert!(store.worker_automation.read().await.is_empty());
    reserve(&store, WorkerDrainPhase::StopRequested).await;
    assert!(store
        .reserve_worker_automation("worker", "%2", Some("default"))
        .await
        .is_none());
}

#[tokio::test]
async fn worker_drain_fences_endpoint_and_session_independently() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    reserve(&store, WorkerDrainPhase::Draining).await;
    assert!(!store.worker_input_blocked("%2", Some("other")).await);
    assert!(store.worker_input_blocked("%2", None).await);
    let mut replacement = worker();
    replacement.agent_session_id = "new-session".into();
    store
        .enrich_worker_drains(std::slice::from_mut(&mut replacement))
        .await;
    assert!(!replacement.draining);
    assert!(store.worker_input_blocked("%2", Some("default")).await);
}

#[tokio::test]
async fn worker_drain_owed_and_reserved_reply_wakes_exclude_retirement() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let request = store.create(worker(), sender(), input()).await.unwrap();
    store
        .reply(
            &sender(),
            &request.id,
            RequestStatus::Completed,
            "done".into(),
            Vec::new(),
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        store.work_exclusion(&worker()).await.as_deref(),
        Some("open_requests_or_reply_wakes")
    );
    reserve(&store, WorkerDrainPhase::Draining).await;
    assert!(store
        .reserve_reply_wakes(&worker())
        .await
        .unwrap()
        .is_none());
    store.worker_drains.lock().await.clear();
    let batch = store.reserve_reply_wakes(&worker()).await.unwrap().unwrap();
    assert_eq!(
        store.work_exclusion(&worker()).await.as_deref(),
        Some("open_requests_or_reply_wakes")
    );
    store.complete_reply_wakes(&batch, true).await.unwrap();
    assert!(store.work_exclusion(&worker()).await.is_none());
}
