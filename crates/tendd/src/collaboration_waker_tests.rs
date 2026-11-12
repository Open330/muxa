use super::*;
use crate::test_support::*;
use std::sync::{Arc, Mutex};
use tend::collaboration::{
    CollaborationPaneEvidence, CollaborationProvenance, NewRequest, RequestKind, RequestStatus,
    WorkMode,
};
use tend::event::{AgentEvent, AgentId, AgentKind};
use tend::tmux::PaneInfo;
use time::OffsetDateTime;

/// A request still waiting on a launching agent must not be offered to
/// whatever agent later reuses that pane id in another window: delivery
/// refuses it, so picking it only repeats a failing reservation forever.
#[test]
fn a_pending_request_is_offered_only_to_its_own_room() {
    let to = room_participant(
        "rmux:%75",
        "pending-pane:/tmp/rmux-1/default:rmux:%75",
        "@42",
    );
    let elsewhere = room_participant("rmux:%75", "thread-other", "@13");
    assert!(idle_pending_recipient(std::slice::from_ref(&elsewhere), &to).is_none());

    let launched = room_participant("rmux:%75", "thread-launched", "@42");
    let found = idle_pending_recipient(&[elsewhere, launched.clone()], &to).cloned();
    assert_eq!(found, Some(launched));
}

/// The junia case: a coordinator's review request to a window's reviewer
/// whose pane was replaced before it was delivered. Addressed by role,
/// it follows to the new reviewer in the same work; nothing is sent to
/// the vanished pane.
#[tokio::test]
async fn a_role_addressed_request_follows_a_replaced_reviewer() {
    let store = tend::Store::shared();
    add_agent(&store, "%1", "coordinator", AgentKind::ClaudeCode).await;
    add_agent(&store, "%2", "old-reviewer", AgentKind::Codex).await;
    let stamp = |mut pane: PaneInfo| {
        pane.workspace_id = Some("junia".into());
        pane.work_id = Some("S1".into());
        pane.agent_role = Some("reviewer".into());
        pane
    };
    let coordinator_pane = collaboration_pane("%1", "0");
    let old_pane = stamp(collaboration_pane("%2", "1"));
    let panes = vec![coordinator_pane.clone(), old_pane];
    let participants = tend::collaboration::participants_from(&store.snapshot().await, &panes);
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    authorize_test_workers(&mailbox, &participants).await;
    let coordinator = participants
        .iter()
        .find(|p| p.pane == "%1")
        .unwrap()
        .clone();
    let old = participants
        .iter()
        .find(|p| p.pane == "%2")
        .unwrap()
        .clone();
    let request = mailbox
        .create(
            coordinator.clone(),
            old.clone(),
            NewRequest {
                body: "review S1".into(),
                recipient_slot: Some(tend::collaboration::RecipientSlot::of(
                    &old,
                    &panes,
                    Some("reviewer"),
                    None,
                )),
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();

    // A quiet notice to the same slot wakes nobody but must follow too.
    let notice = mailbox
        .create(
            coordinator.clone(),
            old.clone(),
            NewRequest {
                kind: RequestKind::Notice,
                body: "fyi".into(),
                expects_reply: false,
                notify: Some(false),
                recipient_slot: Some(tend::collaboration::RecipientSlot::of(
                    &old,
                    &panes,
                    Some("reviewer"),
                    None,
                )),
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();

    // The reviewer pane goes away and a new reviewer session starts in a
    // new pane of the same work window.
    store.forget_agent("old-reviewer").await;
    add_agent(&store, "%3", "new-reviewer", AgentKind::Codex).await;
    let new_pane = stamp(collaboration_pane("%3", "1"));
    let panes = vec![coordinator_pane, new_pane];
    let fresh = tend::collaboration::participants_from(&store.snapshot().await, &panes);
    authorize_test_workers(&mailbox, &fresh).await;

    let sends = Arc::new(Mutex::new(Vec::new()));
    let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
        panes,
        sends: sends.clone(),
    });
    for _ in 0..2 {
        wake_idle_collaboration_peers(&mailbox, &store, std::slice::from_ref(&backend)).await;
    }
    {
        let sends = sends.lock().unwrap();
        assert!(sends.iter().all(|(pane, _)| pane != "%2"), "{sends:?}");
        assert!(
            sends
                .iter()
                .any(|(pane, text)| pane == "%3" && text.contains(&request.id)),
            "the new reviewer was not woken: {sends:?}"
        );
    }
    let stored = mailbox.get_for(&coordinator, &request.id).await.unwrap();
    assert_eq!(stored.to.agent_session_id, "new-reviewer");
    assert_eq!(stored.rebound_from, ["old-reviewer"]);
    let notice = mailbox.get_for(&coordinator, &notice.id).await.unwrap();
    assert_eq!(
        notice.to.agent_session_id, "new-reviewer",
        "quiet notice stranded"
    );
}

async fn deliver_quiet_progress_burst(
    mailbox: &CollaborationStore,
    store: &tend::SharedStore,
    backends: &[tend::SharedBackend],
    payload: CollaborationWakePayload,
    sender: &tend::collaboration::Participant,
    recipient: &tend::collaboration::Participant,
) -> Vec<String> {
    let mut inflight = HashSet::new();
    let mut progress_ids = Vec::new();
    // Reproduce the reported volume and let the waker scan after each
    // message, including fresh idle generations and periodic rescans.
    for index in 0..39 {
        let progress = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Notice,
                    body: format!("checkpoint {index}"),
                    expects_reply: false,
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        progress_ids.push(progress.id);
        inflight.clear();
        for _ in 0..2 {
            wake_idle_collaboration_peers_with_inflight(
                mailbox,
                store,
                backends,
                payload,
                &mut inflight,
            )
            .await;
        }
    }
    progress_ids
}

#[tokio::test]
async fn collaboration_waker_suppresses_progress_but_delivers_handoffs_without_ack() {
    for payload in [
        CollaborationWakePayload::Notice,
        CollaborationWakePayload::OperatorFull,
        CollaborationWakePayload::Adaptive,
        CollaborationWakePayload::Full,
    ] {
        let store = tend::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let participants = worker_participants(&store.snapshot().await, &panes);
        let sender = participants
            .iter()
            .find(|p| p.pane == "%1")
            .unwrap()
            .clone();
        let recipient = participants
            .iter()
            .find(|p| p.pane == "%2")
            .unwrap()
            .clone();
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        authorize_test_workers(&mailbox, &participants).await;
        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes,
            sends: sends.clone(),
        });
        let backends = vec![backend];
        let mut inflight = HashSet::new();
        let progress_ids =
            deliver_quiet_progress_burst(&mailbox, &store, &backends, payload, &sender, &recipient)
                .await;
        assert!(
            sends.lock().unwrap().is_empty(),
            "39 quiet notices must cause zero terminal writes"
        );
        let handoff = mailbox
            .create(
                sender,
                recipient.clone(),
                NewRequest {
                    kind: RequestKind::Notice,
                    body: "device released".into(),
                    expects_reply: false,
                    notify: Some(true),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        for prompt in [
            mailbox_collaboration_request_prompt(&handoff),
            full_collaboration_request_prompt(&handoff),
            interrupted_collaboration_request_prompt(&handoff),
        ] {
            assert!(prompt.contains("No reply is expected"));
            assert!(!prompt.contains("respond with tend_reply"));
        }
        for _ in 0..2 {
            wake_idle_collaboration_peers_with_inflight(
                &mailbox,
                &store,
                &backends,
                payload,
                &mut inflight,
            )
            .await;
        }
        {
            let delivered = sends.lock().unwrap();
            assert_eq!(
                delivered.len(),
                2,
                "one prompt and one submit, no duplicate wake"
            );
            assert!(delivered[0].1.contains(&handoff.id));
            assert!(
                delivered[0].1.contains("Also waiting: 39 one-way notices"),
                "a wake that goes out anyway names the quiet notices: {}",
                delivered[0].1
            );
            assert!(progress_ids.iter().all(|id| !delivered[0].1.contains(id)));
            assert!(delivered[0].1.contains("No reply is expected"));
        }
        let inbox = mailbox.claim_for(&recipient).await.unwrap();
        assert_eq!(
            inbox
                .iter()
                .filter(|request| progress_ids.contains(&request.id))
                .count(),
            39,
            "all quiet notices must remain readable at the checkpoint"
        );
        assert_eq!(
            mailbox.pending_reply_unnotified().await,
            [] as [tend::collaboration::CollaborationRequest; 0]
        );
    }
}

#[tokio::test]
async fn peer_recovery_runs_when_terminal_wake_is_disabled() {
    let store = tend::Store::shared();
    add_capped_agent(&store, "%1", "sender").await;
    add_capped_agent(&store, "%2", "recipient").await;
    let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
    let participants = worker_participants(&store.snapshot().await, &panes);
    let sender = participants
        .iter()
        .find(|p| p.pane == "%1")
        .unwrap()
        .clone();
    let recipient = participants
        .iter()
        .find(|p| p.pane == "%2")
        .unwrap()
        .clone();
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    authorize_test_workers(&mailbox, &participants).await;
    mailbox
        .create(
            sender,
            recipient,
            NewRequest {
                body: "review".into(),
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();
    let mut cfg = Config::default();
    cfg.collaboration.enabled = true;
    cfg.collaboration.wake = CollaborationWake::Never;
    let (shutdown, _) = broadcast::channel(1);
    let worker =
        spawn_collaboration_waker_task(&cfg, mailbox.clone(), store, vec![], &shutdown).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if mailbox.pending_human_notifications().await.len() == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    shutdown.send(()).unwrap();
    worker.await.unwrap();
}

#[tokio::test]
async fn collaboration_waker_reacts_to_mailbox_revision() {
    let store = tend::Store::shared();
    add_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
    let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
    let participants = worker_participants(&store.snapshot().await, &panes);
    let sender = participants
        .iter()
        .find(|participant| participant.pane == "%1")
        .unwrap()
        .clone();
    let recipient = participants
        .iter()
        .find(|participant| participant.pane == "%2")
        .unwrap()
        .clone();
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    authorize_test_workers(&mailbox, &participants).await;
    let sends = Arc::new(Mutex::new(Vec::new()));
    let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
        panes,
        sends: sends.clone(),
    });
    let mut cfg = Config::default();
    cfg.collaboration.enabled = true;
    cfg.collaboration.wake = CollaborationWake::IdleOnly;
    let (shutdown_tx, _) = broadcast::channel(1);
    let waker =
        spawn_collaboration_waker_task(&cfg, mailbox.clone(), store, vec![backend], &shutdown_tx)
            .expect("enabled collaboration should spawn a waker");

    let request = mailbox
        .create(
            sender,
            recipient,
            NewRequest {
                allow_path_overlap: false,
                decision: None,
                user_authorized: false,
                task_contract: None,
                supersedes_request_id: None,
                dispatch_id: None,
                source_node_id: None,
                delegation_parent: None,
                human_action: None,
                initiator: None,
                kind: RequestKind::Question,
                body: "wake from revision".into(),
                expects_reply: true,
                notify: None,
                work_mode: WorkMode::ReadOnly,
                thread_id: None,
                parent_request_id: None,
                workspace_id: None,
                work_id: None,
                run_id: None,
                paths: Vec::new(),
                artifacts: Vec::new(),
                links: Vec::new(),
                withdraw_reply_wakes: Vec::new(),
                recipient_slot: None,
            },
        )
        .await
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_millis(200), async {
        loop {
            let prompt_and_submit_sent = sends.lock().unwrap().len() >= 2;
            if prompt_and_submit_sent && mailbox.pending_unnotified().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("mailbox revision should trigger prompt and submit without a timer tick");
    {
        let delivered = sends.lock().unwrap();
        assert_eq!(delivered[0].0, "%2");
        assert!(delivered[0].1.contains(&request.id));
    }

    shutdown_tx.send(()).unwrap();
    waker.await.unwrap();
}

/// A pending recipient's debounce is keyed by the placeholder session id,
/// which no registry row will ever name. Clearing has to be endpoint-based
/// or a second request to the same pane stays suppressed until the slow
/// reconcile tick.
#[tokio::test]
async fn wake_debounce_for_a_pending_pane_clears_on_that_pane_s_transition() {
    let store = tend::Store::shared();
    add_agent(&store, "%2", "synthetic-7:default:%2", AgentKind::Codex).await;
    let row = store
        .snapshot()
        .await
        .into_iter()
        .find(|agent| agent.pane.as_deref() == Some("%2"))
        .expect("the pane has a row");

    let mut wake_inflight = HashSet::new();
    wake_inflight.insert((
        "%2".to_string(),
        Some("default".to_string()),
        "pending-pane:default:%2".to_string(),
    ));
    wake_inflight.insert((
        "%3".to_string(),
        Some("default".to_string()),
        "other-session".to_string(),
    ));

    clear_wake_inflight_for_agent(&mut wake_inflight, &row);

    assert!(
        !wake_inflight.iter().any(|(pane, _, _)| pane == "%2"),
        "the pending key for this pane must clear",
    );
    assert_eq!(wake_inflight.len(), 1, "other panes are untouched");
}

#[tokio::test]
async fn primary_receives_no_peer_prompt_in_any_payload_mode() {
    for payload in [
        CollaborationWakePayload::Notice,
        CollaborationWakePayload::OperatorFull,
        CollaborationWakePayload::Adaptive,
        CollaborationWakePayload::Full,
    ] {
        let store = tend::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "human", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let participants = tend::collaboration::participants_from(&store.snapshot().await, &panes);
        let sender = participants
            .iter()
            .find(|p| p.pane == "%1")
            .unwrap()
            .clone();
        let recipient = participants
            .iter()
            .find(|p| p.pane == "%2")
            .unwrap()
            .clone();
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let request = mailbox
            .create(
                sender,
                recipient.clone(),
                NewRequest {
                    body: "peer must not interrupt human".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes,
            sends: sends.clone(),
        });
        wake_idle_collaboration_peers_with_inflight(
            &mailbox,
            &store,
            &[backend],
            payload,
            &mut HashSet::new(),
        )
        .await;
        assert!(sends.lock().unwrap().is_empty());
        assert_eq!(
            mailbox
                .peek_for(&recipient, &request.id)
                .await
                .unwrap()
                .status,
            tend::collaboration::RequestStatus::Queued
        );
    }
}

/// The spawn deadlock, end to end: a pane tend launched carries only a
/// synthetic row — no session has registered, so it is not a participant —
/// yet the request queued against it is delivered as soon as the pane
/// reads idle. Without this the sender would wait for a registration that
/// codex cannot produce until something types at it.
#[cfg(target_os = "linux")]
#[tokio::test]
#[allow(clippy::too_many_lines)] // end-to-end wake setup is intentionally kept in one scenario
async fn collaboration_waker_delivers_to_a_launched_pane_before_it_registers() {
    struct Worker(std::process::Child);
    impl Drop for Worker {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let worker = Worker(
        std::process::Command::new("bash")
            .args(["-c", "exec -a 'codex --no-daemon' sleep 30"])
            .spawn()
            .unwrap(),
    );
    let store = tend::Store::shared();
    add_agent(&store, "%1", "sender", AgentKind::ClaudeCode).await;
    add_agent(
        &store,
        "%2",
        &format!("{}default:%2", tend::state::SYNTHETIC_SESSION_PREFIX),
        AgentKind::Codex,
    )
    .await;
    let mut launched = collaboration_pane("%2", "1");
    launched.agent_role = Some("peer".into());
    launched.agent_program = Some("codex".into());
    launched.pane_pid = worker.0.id();
    let panes = vec![collaboration_pane("%1", "0"), launched];
    let agents = store.snapshot().await;
    let participants = worker_participants(&agents, &panes);
    assert!(
        participants
            .iter()
            .all(|participant| participant.pane != "%2"),
        "a synthetic row is not a participant — that is the whole problem",
    );
    let sender = participants
        .iter()
        .find(|participant| participant.pane == "%1")
        .unwrap()
        .clone();
    let pending = tend::collaboration::resolve_pending_pane_target(
        &sender,
        "pane:%2",
        &participants,
        &agents,
        &panes,
        tend::config::CollaborationScope::Window,
    )
    .expect("a launched agent pane is addressable by pane id");

    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    authorize_test_workers(&mailbox, &participants).await;
    mailbox
        .register_worker_launch(
            uuid::Uuid::new_v4().to_string(),
            panes[1].clone(),
            AgentKind::Codex,
        )
        .await
        .unwrap();
    let sends = Arc::new(Mutex::new(Vec::new()));
    let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
        panes,
        sends: sends.clone(),
    });
    let mut cfg = Config::default();
    cfg.collaboration.enabled = true;
    cfg.collaboration.wake = CollaborationWake::IdleOnly;
    let (shutdown_tx, _) = broadcast::channel(1);
    let waker = spawn_collaboration_waker_task(
        &cfg,
        mailbox.clone(),
        store.clone(),
        vec![backend],
        &shutdown_tx,
    )
    .expect("enabled collaboration should spawn a waker");

    let request = mailbox
        .create(
            sender,
            pending,
            NewRequest {
                decision: None,
                allow_path_overlap: false,
                user_authorized: false,
                task_contract: None,
                supersedes_request_id: None,
                dispatch_id: None,
                source_node_id: None,
                delegation_parent: None,
                human_action: None,
                initiator: None,
                kind: RequestKind::Review,
                body: "review the pending diff".into(),
                expects_reply: true,
                notify: None,
                work_mode: WorkMode::ReadOnly,
                thread_id: None,
                parent_request_id: None,
                workspace_id: None,
                work_id: None,
                run_id: None,
                paths: Vec::new(),
                artifacts: Vec::new(),
                links: Vec::new(),
                withdraw_reply_wakes: Vec::new(),
                recipient_slot: None,
            },
        )
        .await
        .unwrap();

    tokio::time::timeout(std::time::Duration::from_millis(500), async {
        loop {
            if sends.lock().unwrap().len() >= 2 && mailbox.pending_unnotified().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("a pending pane recipient should be woken like any other");
    {
        let delivered = sends.lock().unwrap();
        assert_eq!(delivered[0].0, "%2");
        assert!(delivered[0].1.contains(&request.id));
    }

    // The session that finally registers adopts the request.
    add_agent(&store, "%2", "codex-session", AgentKind::Codex).await;
    let registered = tend::collaboration::participants_from(
        &store.snapshot().await,
        &[collaboration_pane("%1", "0"), collaboration_pane("%2", "1")],
    )
    .into_iter()
    .find(|participant| participant.pane == "%2")
    .expect("the hook row registers the pane");
    mailbox
        .observe_worker_hook(&AgentId {
            kind: AgentKind::Codex,
            session_id: "codex-session".into(),
            surface: None,
            pane: Some("%2".into()),
            tmux_socket: Some("default".into()),
            cwd: None,
        })
        .await
        .unwrap();
    let mut registered = vec![registered];
    mailbox.enrich_launch_purposes(&mut registered, &[]).await;
    let inbox = mailbox.claim_for(&registered[0]).await.unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].id, request.id);

    shutdown_tx.send(()).unwrap();
    waker.await.unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // end-to-end wake setup is intentionally kept in one scenario
async fn full_wake_claims_and_injects_the_structured_request_body() {
    let store = tend::Store::shared();
    add_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
    let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
    let participants = worker_participants(&store.snapshot().await, &panes);
    let sender = participants
        .iter()
        .find(|participant| participant.pane == "%1")
        .unwrap()
        .clone();
    let recipient = participants
        .iter()
        .find(|participant| participant.pane == "%2")
        .unwrap()
        .clone();
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    authorize_test_workers(&mailbox, &participants).await;
    let request = mailbox
        .create(
            sender.clone(),
            recipient.clone(),
            NewRequest {
                allow_path_overlap: false,
                decision: None,
                user_authorized: false,
                task_contract: None,
                supersedes_request_id: None,
                dispatch_id: None,
                source_node_id: None,
                delegation_parent: None,
                human_action: None,
                initiator: None,
                kind: RequestKind::Task,
                body: "change only the authorized file".into(),
                expects_reply: true,
                notify: None,
                work_mode: WorkMode::Execute,
                thread_id: None,
                parent_request_id: None,
                workspace_id: None,
                work_id: None,
                run_id: None,
                paths: vec!["src/auth.rs".into()],
                artifacts: Vec::new(),
                links: Vec::new(),
                withdraw_reply_wakes: Vec::new(),
                recipient_slot: None,
            },
        )
        .await
        .unwrap();
    let sends = Arc::new(Mutex::new(Vec::new()));
    let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
        panes,
        sends: sends.clone(),
    });

    wake_idle_collaboration_peers_with_full_payload(&mailbox, &store, &[backend]).await;

    {
        let delivered = sends.lock().unwrap();
        assert_eq!(delivered.len(), 2);
        assert_eq!(delivered[0].0, "%2");
        assert!(delivered[0].1.contains(&request.id));
        assert!(delivered[0].1.contains("kind: task"));
        assert!(delivered[0].1.contains("work_mode: execute"));
        assert!(delivered[0].1.contains("paths: [\"src/auth.rs\"]"));
        assert!(delivered[0].1.contains("change only the authorized file"));
        assert!(delivered[0].1.contains("already claimed"));
        assert!(!delivered[0].1.contains("Read it with tend_inbox"));
        assert_eq!(delivered[1], ("%2".into(), "\r".into()));
    }

    let stored = mailbox.get_for(&recipient, &request.id).await.unwrap();
    assert_eq!(stored.status, RequestStatus::Claimed);
    assert!(stored.claimed_at.is_some());
    assert!(stored.notified_at.is_some());
    assert_eq!(stored.wake_delivery, None);
    assert_eq!(
        mailbox.pending_unnotified().await,
        [] as [tend::collaboration::CollaborationRequest; 0]
    );

    sends.lock().unwrap().clear();
    let unsafe_request = mailbox
        .create(
            sender,
            recipient.clone(),
            NewRequest {
                allow_path_overlap: false,
                decision: None,
                user_authorized: false,
                task_contract: None,
                supersedes_request_id: None,
                dispatch_id: None,
                source_node_id: None,
                delegation_parent: None,
                human_action: None,
                initiator: None,
                kind: RequestKind::Task,
                body: "unsafe\u{1b}[201~\rsubmit".into(),
                expects_reply: true,
                notify: None,
                work_mode: WorkMode::Execute,
                thread_id: None,
                parent_request_id: None,
                workspace_id: None,
                work_id: None,
                run_id: None,
                paths: Vec::new(),
                artifacts: Vec::new(),
                links: Vec::new(),
                withdraw_reply_wakes: Vec::new(),
                recipient_slot: None,
            },
        )
        .await
        .unwrap();
    let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
        panes: vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")],
        sends: sends.clone(),
    });
    wake_idle_collaboration_peers_with_full_payload(&mailbox, &store, &[backend]).await;
    {
        let delivered = sends.lock().unwrap();
        assert_eq!(delivered.len(), 2);
        assert!(delivered[0].1.contains("tend_inbox"));
        assert!(!delivered[0].1.contains(&unsafe_request.body));
    }
    let stored = mailbox
        .get_for(&recipient, &unsafe_request.id)
        .await
        .unwrap();
    assert_eq!(stored.status, RequestStatus::Queued);
    assert!(stored.notified_at.is_some());
}

#[tokio::test]
async fn operator_full_wake_injects_console_requests() {
    let store = tend::Store::shared();
    add_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
    let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
    let participants = worker_participants(&store.snapshot().await, &panes);
    let sender = participants
        .iter()
        .find(|participant| participant.pane == "%1")
        .unwrap()
        .clone();
    let recipient = participants
        .iter()
        .find(|participant| participant.pane == "%2")
        .unwrap()
        .clone();
    let console = tend::collaboration::Participant::console(sender.room.clone());
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    authorize_test_workers(&mailbox, &participants).await;
    let operator_request = mailbox
        .create_with_provenance(
            console,
            recipient.clone(),
            NewRequest {
                allow_path_overlap: false,
                decision: None,
                user_authorized: false,
                task_contract: None,
                supersedes_request_id: None,
                dispatch_id: None,
                source_node_id: None,
                delegation_parent: None,
                human_action: None,
                initiator: None,
                kind: RequestKind::Task,
                body: "operator request body".into(),
                expects_reply: true,
                notify: None,
                work_mode: WorkMode::Execute,
                thread_id: None,
                parent_request_id: None,
                workspace_id: None,
                work_id: None,
                run_id: None,
                paths: Vec::new(),
                artifacts: Vec::new(),
                links: Vec::new(),
                withdraw_reply_wakes: Vec::new(),
                recipient_slot: None,
            },
            Some(CollaborationProvenance {
                client_kind: CollaborationClientKind::Watch,
                caller_pid: Some(4242),
                caller_uid: Some(1000),
                caller_gid: Some(1000),
                executable: Some("tend".into()),
                observed_pane: Some("%1".into()),
                pane_evidence: Some(CollaborationPaneEvidence::ProcessAncestry),
                origin_match: CollaborationOriginMatch::Matched,
            }),
        )
        .await
        .unwrap();
    let sends = Arc::new(Mutex::new(Vec::new()));
    let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
        panes: panes.clone(),
        sends: sends.clone(),
    });
    let mut wake_inflight = HashSet::new();

    wake_idle_collaboration_peers_with_inflight(
        &mailbox,
        &store,
        std::slice::from_ref(&backend),
        CollaborationWakePayload::OperatorFull,
        &mut wake_inflight,
    )
    .await;
    {
        let delivered = sends.lock().unwrap();
        assert_eq!(delivered.len(), 2);
        assert!(delivered[0].1.contains("operator request body"));
        assert!(delivered[0]
            .1
            .contains("via tend watch representing console"));
        assert!(!delivered[0].1.contains("Read it with tend_inbox"));
    }
    let stored = mailbox
        .get_for(&recipient, &operator_request.id)
        .await
        .unwrap();
    assert_eq!(stored.status, RequestStatus::Claimed);
}

#[tokio::test]
async fn operator_full_wake_keeps_agent_requests_in_the_mailbox() {
    let store = tend::Store::shared();
    add_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
    let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
    let participants = worker_participants(&store.snapshot().await, &panes);
    let sender = participants
        .iter()
        .find(|participant| participant.pane == "%1")
        .unwrap()
        .clone();
    let recipient = participants
        .iter()
        .find(|participant| participant.pane == "%2")
        .unwrap()
        .clone();
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    authorize_test_workers(&mailbox, &participants).await;
    let agent_request = mailbox
        .create(
            sender,
            recipient.clone(),
            NewRequest {
                allow_path_overlap: false,
                decision: None,
                user_authorized: false,
                task_contract: None,
                supersedes_request_id: None,
                dispatch_id: None,
                source_node_id: None,
                delegation_parent: None,
                human_action: None,
                initiator: None,
                kind: RequestKind::Task,
                body: "agent delegated body".into(),
                expects_reply: true,
                notify: None,
                work_mode: WorkMode::Execute,
                thread_id: None,
                parent_request_id: None,
                workspace_id: None,
                work_id: None,
                run_id: None,
                paths: Vec::new(),
                artifacts: Vec::new(),
                links: Vec::new(),
                withdraw_reply_wakes: Vec::new(),
                recipient_slot: None,
            },
        )
        .await
        .unwrap();
    let sends = Arc::new(Mutex::new(Vec::new()));
    let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
        panes,
        sends: sends.clone(),
    });
    let mut wake_inflight = HashSet::new();

    wake_idle_collaboration_peers_with_inflight(
        &mailbox,
        &store,
        &[backend],
        CollaborationWakePayload::OperatorFull,
        &mut wake_inflight,
    )
    .await;
    {
        let delivered = sends.lock().unwrap();
        assert_eq!(delivered.len(), 2);
        assert!(delivered[0]
            .1
            .contains(&format!("tend_inbox(request_id=\"{}\")", agent_request.id)));
        assert!(delivered[0]
            .1
            .contains(&format!("tend msg accept {} --json", agent_request.id)));
        assert!(!delivered[0].1.contains("tend msg inbox --json"));
        assert!(!delivered[0].1.contains("agent delegated body"));
    }
    let stored = mailbox
        .get_for(&recipient, &agent_request.id)
        .await
        .unwrap();
    assert_eq!(stored.status, RequestStatus::Queued);
}

#[tokio::test]
async fn full_wake_submits_only_one_request_per_idle_generation() {
    let store = tend::Store::shared();
    add_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
    let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
    let participants = worker_participants(&store.snapshot().await, &panes);
    let sender = participants
        .iter()
        .find(|participant| participant.pane == "%1")
        .unwrap()
        .clone();
    let recipient = participants
        .iter()
        .find(|participant| participant.pane == "%2")
        .unwrap()
        .clone();
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    authorize_test_workers(&mailbox, &participants).await;
    for body in ["first body", "second body"] {
        mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    allow_path_overlap: false,
                    decision: None,
                    user_authorized: false,
                    task_contract: None,
                    supersedes_request_id: None,
                    dispatch_id: None,
                    source_node_id: None,
                    delegation_parent: None,
                    human_action: None,
                    initiator: None,
                    kind: RequestKind::Task,
                    body: body.into(),
                    expects_reply: true,
                    notify: None,
                    work_mode: WorkMode::Execute,
                    thread_id: None,
                    parent_request_id: None,
                    workspace_id: None,
                    work_id: None,
                    run_id: None,
                    paths: Vec::new(),
                    artifacts: Vec::new(),
                    links: Vec::new(),
                    withdraw_reply_wakes: Vec::new(),
                    recipient_slot: None,
                },
            )
            .await
            .unwrap();
    }
    let sends = Arc::new(Mutex::new(Vec::new()));
    let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
        panes,
        sends: sends.clone(),
    });
    let mut wake_inflight = HashSet::new();

    wake_idle_collaboration_peers_with_inflight(
        &mailbox,
        &store,
        std::slice::from_ref(&backend),
        CollaborationWakePayload::Full,
        &mut wake_inflight,
    )
    .await;
    assert_eq!(sends.lock().unwrap().len(), 2);
    assert_eq!(mailbox.pending_unnotified().await.len(), 1);

    // Mailbox revisions emitted by the first delivery can rescan while
    // the state hook still says Idle. The generation gate must suppress
    // a second prompt until a later Idle transition clears it.
    wake_idle_collaboration_peers_with_inflight(
        &mailbox,
        &store,
        std::slice::from_ref(&backend),
        CollaborationWakePayload::Full,
        &mut wake_inflight,
    )
    .await;
    assert_eq!(sends.lock().unwrap().len(), 2);
    assert_eq!(mailbox.pending_unnotified().await.len(), 1);

    wake_inflight.clear();
    wake_idle_collaboration_peers_with_inflight(
        &mailbox,
        &store,
        &[backend],
        CollaborationWakePayload::Full,
        &mut wake_inflight,
    )
    .await;
    assert_eq!(sends.lock().unwrap().len(), 4);
    assert_eq!(
        mailbox.pending_unnotified().await,
        [] as [tend::collaboration::CollaborationRequest; 0]
    );
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // explicit durable request/recovery fixtures
async fn direct_wake_recovers_without_reinjecting_the_request_body() {
    for payload in [
        CollaborationWakePayload::Full,
        CollaborationWakePayload::Adaptive,
    ] {
        let store = tend::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let participants = worker_participants(&store.snapshot().await, &panes);
        let sender = participants
            .iter()
            .find(|participant| participant.pane == "%1")
            .unwrap()
            .clone();
        let recipient = participants
            .iter()
            .find(|participant| participant.pane == "%2")
            .unwrap()
            .clone();
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        authorize_test_workers(&mailbox, &participants).await;
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    human_action: None,
                    initiator: None,
                    kind: RequestKind::Task,
                    body: "do not inject this twice".into(),
                    work_mode: WorkMode::Execute,
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        mailbox
            .prepare_direct_wake(&recipient, &request.id)
            .await
            .unwrap()
            .unwrap();
        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes: panes.clone(),
            sends: sends.clone(),
        });

        wake_idle_collaboration_peers_with_inflight(
            &mailbox,
            &store,
            std::slice::from_ref(&backend),
            payload,
            &mut HashSet::new(),
        )
        .await;
        {
            let delivered = sends.lock().unwrap();
            assert_eq!(delivered.len(), 2);
            assert!(delivered[0].1.contains("delivery was interrupted"));
            assert!(delivered[0].1.contains("tend_inbox"));
            assert!(!delivered[0].1.contains("do not inject this twice"));
            assert_eq!(delivered[1], ("%2".into(), "\r".into()));
        }
        assert_eq!(
            mailbox.pending_unnotified().await,
            [] as [tend::collaboration::CollaborationRequest; 0]
        );

        let second = mailbox
            .create(
                sender,
                recipient.clone(),
                NewRequest {
                    allow_path_overlap: false,
                    decision: None,
                    user_authorized: false,
                    task_contract: None,
                    supersedes_request_id: None,
                    dispatch_id: None,
                    source_node_id: None,
                    delegation_parent: None,
                    human_action: None,
                    initiator: None,
                    kind: RequestKind::Task,
                    body: "the prompt text is already buffered".into(),
                    expects_reply: true,
                    notify: None,
                    work_mode: WorkMode::Execute,
                    thread_id: None,
                    parent_request_id: None,
                    workspace_id: None,
                    work_id: None,
                    run_id: None,
                    paths: Vec::new(),
                    artifacts: Vec::new(),
                    links: Vec::new(),
                    withdraw_reply_wakes: Vec::new(),
                    recipient_slot: None,
                },
            )
            .await
            .unwrap();
        mailbox
            .prepare_direct_wake(&recipient, &second.id)
            .await
            .unwrap()
            .unwrap();
        mailbox.mark_wake_prompt_written(&second.id).await.unwrap();
        sends.lock().unwrap().clear();

        wake_idle_collaboration_peers_with_inflight(
            &mailbox,
            &store,
            &[backend],
            payload,
            &mut HashSet::new(),
        )
        .await;
        assert_eq!(
            sends.lock().unwrap().as_slice(),
            &[("%2".into(), "\r".into())]
        );
        assert_eq!(
            mailbox.pending_unnotified().await,
            [] as [tend::collaboration::CollaborationRequest; 0]
        );
    }
}

/// Replies that piled up while the sender worked arrive as one prompt,
/// and one the sender withdrew (it moved on) is not announced at all.
#[tokio::test]
async fn piled_up_replies_wake_once_and_skip_withdrawn_ones() {
    let store = tend::Store::shared();
    add_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
    let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
    let participants = worker_participants(&store.snapshot().await, &panes);
    let find = |pane: &str| {
        participants
            .iter()
            .find(|participant| participant.pane == pane)
            .unwrap()
            .clone()
    };
    let (sender, recipient) = (find("%1"), find("%2"));
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    authorize_test_workers(&mailbox, &participants).await;
    let mut ids = Vec::new();
    for body in ["abandoned", "first", "second"] {
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    body: body.into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        ids.push(request.id);
    }
    mailbox.claim_for(&recipient).await.unwrap();
    mailbox
        .withdraw_reply_wakes(&sender, &ids[..1])
        .await
        .unwrap();
    for id in &ids {
        mailbox
            .reply(
                &recipient,
                id,
                RequestStatus::Completed,
                "done".into(),
                Vec::new(),
            )
            .await
            .unwrap();
    }

    let sends = Arc::new(Mutex::new(Vec::new()));
    let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
        panes,
        sends: sends.clone(),
    });
    for _ in 0..2 {
        wake_idle_collaboration_peers(&mailbox, &store, std::slice::from_ref(&backend)).await;
    }
    {
        let sends = sends.lock().unwrap();
        assert_eq!(sends.len(), 2, "one prompt and one submit: {sends:?}");
        assert_eq!(sends[0].0, "%1");
        assert!(
            sends[0].1.starts_with("[tend] 2 replies are ready: "),
            "{}",
            sends[0].1
        );
        assert!(sends[0].1.contains(&ids[1]) && sends[0].1.contains(&ids[2]));
        assert!(!sends[0].1.contains(&ids[0]), "withdrawn reply announced");
        assert_eq!(sends[1], ("%1".into(), "\r".into()));
    }
    assert_eq!(mailbox.unread_reply_count(&sender).await, 3);
}

/// The waker resolves the sender from a scan, then reserves. If the
/// sender started a turn (or its pane got a new session) in between,
/// nothing may be typed and the reply stays owed for its next idle turn.
#[tokio::test]
async fn a_sender_busy_again_since_the_scan_gets_no_reply_wake() {
    for replaced in [false, true] {
        let store = tend::Store::shared();
        add_agent(&store, "%1", "sender", AgentKind::Codex).await;
        add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
        let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
        let participants = worker_participants(&store.snapshot().await, &panes);
        let find = |pane: &str| {
            participants
                .iter()
                .find(|participant| participant.pane == pane)
                .unwrap()
                .clone()
        };
        let (sender, recipient) = (find("%1"), find("%2"));
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        authorize_test_workers(&mailbox, &participants).await;
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    body: "review".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        mailbox.claim_for(&recipient).await.unwrap();
        mailbox
            .reply(
                &recipient,
                &request.id,
                RequestStatus::Completed,
                "done".into(),
                Vec::new(),
            )
            .await
            .unwrap();
        let replies = mailbox.pending_reply_unnotified().await;

        if replaced {
            add_agent(&store, "%1", "fresh-session", AgentKind::Codex).await;
        } else {
            store
                .apply(&AgentEvent::PromptSubmitted {
                    id: AgentId {
                        kind: AgentKind::Codex,
                        session_id: "sender".into(),
                        surface: None,
                        pane: Some("%1".into()),
                        tmux_socket: Some("default".into()),
                        cwd: Some("/repo".into()),
                    },
                    prompt: "next task".into(),
                    at: OffsetDateTime::now_utc(),
                })
                .await;
        }

        let sends = Arc::new(Mutex::new(Vec::new()));
        let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
            panes,
            sends: sends.clone(),
        });
        wake_senders_of_ready_replies(
            &mailbox,
            &participants,
            std::slice::from_ref(&backend),
            replies,
            &mut HashSet::new(),
            &store,
        )
        .await;
        assert!(sends.lock().unwrap().is_empty(), "replaced={replaced}");
        assert_eq!(
            mailbox.pending_reply_unnotified().await.len(),
            1,
            "the wake stays owed (replaced={replaced})"
        );
    }
}

/// The running waker, not one pass: a pane that refuses input must not
/// turn the reservation bookkeeping into a retry loop. The reply stays
/// owed and is delivered once when the sender next goes idle.
#[tokio::test]
#[allow(clippy::too_many_lines)] // one end-to-end run of the waker task
async fn a_refusing_pane_is_retried_on_the_next_idle_turn_not_in_a_loop() {
    let store = tend::Store::shared();
    add_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
    let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
    let participants = worker_participants(&store.snapshot().await, &panes);
    let find = |pane: &str| {
        participants
            .iter()
            .find(|participant| participant.pane == pane)
            .unwrap()
            .clone()
    };
    let (sender, recipient) = (find("%1"), find("%2"));
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    authorize_test_workers(&mailbox, &participants).await;
    let request = mailbox
        .create(
            sender.clone(),
            recipient.clone(),
            NewRequest {
                body: "review".into(),
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();
    mailbox.claim_for(&recipient).await.unwrap();

    let attempts = Arc::new(Mutex::new(Vec::new()));
    let accepting = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let backend: tend::SharedBackend = Arc::new(RefusingWakeBackend {
        panes,
        attempts: attempts.clone(),
        accepting: accepting.clone(),
    });
    let mut cfg = Config::default();
    cfg.collaboration.enabled = true;
    cfg.collaboration.wake = CollaborationWake::IdleOnly;
    let (shutdown_tx, _) = broadcast::channel(1);
    let waker = spawn_collaboration_waker_task(
        &cfg,
        mailbox.clone(),
        store.clone(),
        vec![backend],
        &shutdown_tx,
    )
    .unwrap();

    mailbox
        .reply(
            &recipient,
            &request.id,
            RequestStatus::Completed,
            "done".into(),
            Vec::new(),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let refused = attempts
        .lock()
        .unwrap()
        .iter()
        .filter(|(pane, text)| pane == "%1" && text.contains(&request.id))
        .count();
    assert!(
        (1..=2).contains(&refused),
        "expected one attempt per published change, saw {refused}"
    );
    assert_eq!(mailbox.pending_reply_unnotified().await.len(), 1);

    accepting.store(true, std::sync::atomic::Ordering::SeqCst);
    let id = AgentId {
        kind: AgentKind::Codex,
        session_id: "sender".into(),
        surface: None,
        pane: Some("%1".into()),
        tmux_socket: Some("default".into()),
        cwd: Some("/repo".into()),
    };
    store
        .apply(&AgentEvent::PromptSubmitted {
            id: id.clone(),
            prompt: "other work".into(),
            at: OffsetDateTime::now_utc(),
        })
        .await;
    store
        .apply(&AgentEvent::TurnStopped {
            id,
            response: None,
            recap: None,
            ai_title: None,
            idle_confirmed: true,
            at: OffsetDateTime::now_utc(),
        })
        .await;
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !mailbox.pending_reply_unnotified().await.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the next idle turn delivers the owed reply");
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let _ = shutdown_tx.send(());
    let _ = waker.await;
    let delivered = attempts
        .lock()
        .unwrap()
        .iter()
        .filter(|(pane, text)| pane == "%1" && text.contains(&request.id))
        .count();
    assert_eq!(
        delivered,
        refused + 1,
        "delivered exactly once after the refusals"
    );
}

#[tokio::test]
async fn reply_wake_prompt_names_stale_requests() {
    let now = time::OffsetDateTime::now_utc();
    let store = tend::Store::shared();
    add_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
    let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
    let participants = worker_participants(&store.snapshot().await, &panes);
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    authorize_test_workers(&mailbox, &participants).await;
    let template = mailbox
        .create(
            participants[0].clone(),
            participants[1].clone(),
            NewRequest {
                body: "b".into(),
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();
    let request = |id: &str, age_minutes: i64| {
        let mut request = template.clone();
        request.id = id.into();
        request.status = RequestStatus::Completed;
        request.created_at = now - time::Duration::minutes(age_minutes);
        request
    };
    let single = tend::collaboration::ReplyWakeBatch {
        token: 1,
        requests: vec![request("req_a", 5)],
        remaining: 0,
    };
    assert_eq!(
        reply_wake_prompt(&single, now),
        format!(
            "[tend:req_a] Completed reply from {} is ready. Read it with tend_wait_reply (MCP) or `tend msg wait req_a`.",
            single.requests[0].to.label()
        ),
        "a fresh single reply keeps the established text"
    );
    let stale = tend::collaboration::ReplyWakeBatch {
        token: 2,
        requests: vec![request("req_b", 9 * 60)],
        remaining: 3,
    };
    let prompt = reply_wake_prompt(&stale, now);
    assert!(prompt.contains("req_b: Completed reply from"), "{prompt}");
    assert!(prompt.contains("(requested 9h ago)"), "{prompt}");
    assert!(prompt.contains("3 more will follow."), "{prompt}");
}

/// A re-delivery waiting for the pane guard behind another Tend input
/// re-checks the recipient after it gets the guard: a prompt that input
/// submitted meanwhile cancels the re-delivery, and nothing is typed.
#[tokio::test]
#[allow(clippy::too_many_lines)] // the guard-wait scenario is kept in one place
async fn a_redelivery_rechecks_the_recipient_after_taking_the_pane_guard() {
    let store = tend::Store::shared();
    let long_ago = OffsetDateTime::now_utc() - time::Duration::hours(1);
    let id = |session: &str, pane: &str| AgentId {
        kind: AgentKind::Codex,
        session_id: session.into(),
        surface: None,
        pane: Some(pane.into()),
        tmux_socket: Some("default".into()),
        cwd: Some("/repo".into()),
    };
    for (session, pane) in [("sender", "%1"), ("recipient", "%2")] {
        store
            .apply(&AgentEvent::Started {
                id: id(session, pane),
                at: long_ago,
            })
            .await;
        store
            .apply(&AgentEvent::TurnStopped {
                id: id(session, pane),
                response: None,
                recap: None,
                ai_title: None,
                idle_confirmed: false,
                at: long_ago,
            })
            .await;
    }
    let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
    let agents = store.snapshot().await;
    let participants = worker_participants(&agents, &panes);
    let find = |pane: &str| {
        participants
            .iter()
            .find(|participant| participant.pane == pane)
            .unwrap()
            .clone()
    };
    let (sender, recipient) = (find("%1"), find("%2"));
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    authorize_test_workers(&mailbox, &participants).await;
    let request = mailbox
        .create(
            sender,
            recipient.clone(),
            NewRequest {
                body: "review".into(),
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();
    // The first wake, three minutes ago, left the recipient untouched.
    mailbox
        .mark_wake_notified(
            &request.id,
            OffsetDateTime::now_utc() - time::Duration::minutes(3),
        )
        .await
        .unwrap();
    assert_eq!(
        mailbox
            .rearm_unsubmitted_wakes(&store.snapshot().await)
            .await
            .unwrap(),
        1
    );
    let request = mailbox.pending_unnotified().await.remove(0);
    let sends = Arc::new(Mutex::new(Vec::new()));
    let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
        panes,
        sends: sends.clone(),
    });
    // Another Tend input holds the pane.
    let held = tend::prompt_delivery::lock_pane(&mailbox, "%2").await;
    let retry = {
        let (mailbox, store, recipient, request, backend) = (
            mailbox.clone(),
            store.clone(),
            recipient.clone(),
            request.clone(),
            backend.clone(),
        );
        tokio::spawn(async move {
            send_mailbox_wake(&mailbox, &recipient, &request, "wake", &[backend], &store).await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    // That input is submitted while the re-delivery waits.
    store
        .apply(&AgentEvent::PromptSubmitted {
            id: id("recipient", "%2"),
            prompt: "someone else's prompt".into(),
            at: OffsetDateTime::now_utc(),
        })
        .await;
    drop(held);
    assert!(!retry.await.unwrap());
    assert!(sends.lock().unwrap().is_empty(), "nothing typed");
    assert!(mailbox.pending_unnotified().await.is_empty(), "cancelled");
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // end-to-end wake setup is intentionally kept in one scenario
async fn terminal_reply_wakes_idle_sender_without_injecting_body() {
    let store = tend::Store::shared();
    add_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
    let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
    let agents = store.snapshot().await;
    let participants = worker_participants(&agents, &panes);
    let sender = participants
        .iter()
        .find(|participant| participant.pane == "%1")
        .unwrap()
        .clone();
    let recipient = participants
        .iter()
        .find(|participant| participant.pane == "%2")
        .unwrap()
        .clone();
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    authorize_test_workers(&mailbox, &participants).await;
    mailbox
        .set_purpose(&sender, tend::collaboration::LaunchPurpose::Primary)
        .await
        .unwrap();
    let request = mailbox
        .create_with_provenance(
            sender,
            recipient.clone(),
            NewRequest {
                allow_path_overlap: false,
                decision: None,
                user_authorized: false,
                task_contract: None,
                supersedes_request_id: None,
                dispatch_id: None,
                source_node_id: None,
                delegation_parent: None,
                human_action: None,
                initiator: None,
                kind: RequestKind::Review,
                body: "secret request body".into(),
                expects_reply: true,
                notify: None,
                work_mode: WorkMode::ReadOnly,
                thread_id: None,
                parent_request_id: None,
                workspace_id: None,
                work_id: None,
                run_id: None,
                paths: Vec::new(),
                artifacts: Vec::new(),
                links: Vec::new(),
                withdraw_reply_wakes: Vec::new(),
                recipient_slot: None,
            },
            Some(CollaborationProvenance {
                client_kind: CollaborationClientKind::Watch,
                caller_pid: Some(4242),
                caller_uid: Some(1000),
                caller_gid: Some(1000),
                executable: Some("tend".into()),
                observed_pane: Some("%1".into()),
                pane_evidence: Some(CollaborationPaneEvidence::ProcessAncestry),
                origin_match: CollaborationOriginMatch::Matched,
            }),
        )
        .await
        .unwrap();

    let sends = Arc::new(Mutex::new(Vec::new()));
    let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
        panes,
        sends: sends.clone(),
    });
    wake_idle_collaboration_peers(&mailbox, &store, std::slice::from_ref(&backend)).await;
    {
        let mut sends = sends.lock().unwrap();
        assert_eq!(sends.len(), 2);
        assert_eq!(sends[0].0, "%2");
        assert!(sends[0].1.contains("tend_inbox"));
        assert!(sends[0]
            .1
            .contains(&format!("tend msg accept {} --json", request.id)));
        assert!(sends[0].1.contains("kind/work_mode/paths"));
        assert!(sends[0].1.contains("via tend watch representing codex@%1"));
        assert!(sends[0].1.contains("caller %1, pid 4242"));
        assert!(!sends[0].1.contains("secret request body"));
        assert_eq!(sends[1], ("%2".into(), "\r".into()));
        sends.clear();
    }

    mailbox.claim_for(&recipient).await.unwrap();
    mailbox
        .reply(
            &recipient,
            &request.id,
            RequestStatus::Completed,
            "secret reply body".into(),
            Vec::new(),
        )
        .await
        .unwrap();

    wake_idle_collaboration_peers(&mailbox, &store, &[backend]).await;

    {
        let sends = sends.lock().unwrap();
        assert_eq!(sends.len(), 2);
        assert_eq!(sends[0].0, "%1");
        assert!(sends[0].1.contains("reply"));
        assert!(sends[0].1.contains(&request.id));
        assert!(sends[0].1.contains("tend_wait_reply"));
        assert!(sends[0].1.contains("tend msg wait"));
        assert!(!sends[0].1.contains("secret request body"));
        assert!(!sends[0].1.contains("secret reply body"));
        assert_eq!(sends[1], ("%1".into(), "\r".into()));
    }
    assert_eq!(
        mailbox.pending_reply_unnotified().await,
        [] as [tend::collaboration::CollaborationRequest; 0]
    );
    assert_eq!(
        mailbox
            .unread_reply_count(
                participants
                    .iter()
                    .find(|participant| participant.pane == "%1")
                    .unwrap(),
            )
            .await,
        1
    );
}

/// A console dispatches but never receives. The recipient still gets its
/// wake; the reply has nowhere to be typed, and the wake loop must reach
/// that conclusion by finding no participant rather than by trying to
/// address a pane called "console".
#[tokio::test]
async fn a_console_sender_is_woken_for_nothing() {
    let store = tend::Store::shared();
    add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
    let panes = vec![collaboration_pane("%2", "1")];
    let agents = store.snapshot().await;
    let participants = worker_participants(&agents, &panes);
    let recipient = participants[0].clone();
    let console = tend::collaboration::resolve_origin(
        &tend::collaboration::CollaborationOrigin {
            exact_session: false,
            pane: "%2".into(),
            socket: None,
            console: true,
            agent_session_id: None,
        },
        &participants,
        &panes,
    )
    .unwrap();
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    authorize_test_workers(&mailbox, &participants).await;
    let request = mailbox
        .create(
            console.clone(),
            recipient.clone(),
            NewRequest {
                allow_path_overlap: false,
                decision: None,
                user_authorized: false,
                task_contract: None,
                supersedes_request_id: None,
                dispatch_id: None,
                source_node_id: None,
                delegation_parent: None,
                human_action: None,
                initiator: None,
                kind: RequestKind::Task,
                body: "dispatched by a human".into(),
                expects_reply: true,
                notify: None,
                work_mode: WorkMode::ReadOnly,
                thread_id: None,
                parent_request_id: None,
                workspace_id: None,
                work_id: None,
                run_id: None,
                paths: Vec::new(),
                artifacts: Vec::new(),
                links: Vec::new(),
                withdraw_reply_wakes: Vec::new(),
                recipient_slot: None,
            },
        )
        .await
        .unwrap();

    let sends = Arc::new(Mutex::new(Vec::new()));
    let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
        panes,
        sends: sends.clone(),
    });
    wake_idle_collaboration_peers(&mailbox, &store, std::slice::from_ref(&backend)).await;
    {
        let mut sends = sends.lock().unwrap();
        assert_eq!(sends[0].0, "%2");
        assert!(sends[0].1.contains("from console"));
        sends.clear();
    }

    mailbox.claim_for(&recipient).await.unwrap();
    mailbox
        .reply(
            &recipient,
            &request.id,
            RequestStatus::Completed,
            "done".into(),
            Vec::new(),
        )
        .await
        .unwrap();
    wake_idle_collaboration_peers(&mailbox, &store, &[backend]).await;

    assert!(
        sends.lock().unwrap().is_empty(),
        "a console has no pane to wake"
    );
    // Unread stays pending because nothing consumed it, and the reply is
    // readable from the recipient's own mailbox — which is where `tend
    // watch` reads it, by pointing the cursor at that row.
    assert_eq!(
        mailbox
            .list_for(
                &recipient,
                tend::collaboration::RequestMailbox::Incoming,
                tend::collaboration::MailboxScope::Caller,
            )
            .await
            .unwrap()[0]
            .reply
            .as_ref()
            .map(|reply| reply.body.as_str()),
        Some("done")
    );
}

#[tokio::test]
async fn a_moved_sender_receives_its_reply_at_its_current_pane() {
    let store = tend::Store::shared();
    add_agent(&store, "%1", "moving-sender", AgentKind::Codex).await;
    add_agent(&store, "%2", "reviewer", AgentKind::ClaudeCode).await;
    let initial = worker_participants(
        &store.snapshot().await,
        &[collaboration_pane("%1", "0"), collaboration_pane("%2", "1")],
    );
    let sender = initial.iter().find(|p| p.pane == "%1").unwrap().clone();
    let reviewer = initial.iter().find(|p| p.pane == "%2").unwrap().clone();
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    let request = mailbox
        .create(
            sender,
            reviewer.clone(),
            NewRequest {
                body: "Review".into(),
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();
    mailbox
        .reply(
            &reviewer,
            &request.id,
            RequestStatus::Completed,
            "Findings".into(),
            vec![],
        )
        .await
        .unwrap();
    // Seed the post-move registry binding, rather than simulating a new
    // Started hook (which deliberately cannot replace an established pane).
    let moved_store = tend::Store::shared();
    let mut moved_agents = store.snapshot().await;
    moved_agents
        .iter_mut()
        .find(|a| a.session_id == "moving-sender")
        .unwrap()
        .pane = Some("%9".into());
    moved_store.seed_if_absent(moved_agents).await;
    let store = moved_store;
    add_agent(&store, "%1", "other-session", AgentKind::Codex).await;
    let panes = vec![
        collaboration_pane("%1", "0"),
        collaboration_pane("%2", "1"),
        collaboration_pane("%9", "2"),
    ];
    let participants = worker_participants(&store.snapshot().await, &panes);
    let sends = Arc::new(Mutex::new(Vec::new()));
    let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
        panes,
        sends: sends.clone(),
    });
    wake_senders_of_ready_replies(
        &mailbox,
        &participants,
        &[backend],
        mailbox.pending_reply_unnotified().await,
        &mut HashSet::new(),
        &store,
    )
    .await;
    {
        let sent = sends.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].0, "%9");
        assert!(sent[0].1.contains(&request.id));
    }
    assert!(mailbox.pending_reply_unnotified().await.is_empty());
}

#[tokio::test]
async fn contract_notices_preserve_the_cli_fallback_even_for_an_old_mcp() {
    let store = tend::Store::shared();
    add_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_agent(&store, "%2", "reviewer", AgentKind::ClaudeCode).await;
    let participants = worker_participants(
        &store.snapshot().await,
        &[collaboration_pane("%1", "0"), collaboration_pane("%2", "1")],
    );
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    let request = mailbox
        .create(
            participants[0].clone(),
            participants[1].clone(),
            NewRequest {
                body: "Review".into(),
                task_contract: Some(tend::collaboration::TaskContract {
                    version: 1,
                    task_id: "typed-review".into(),
                    objective: "Review this revision".into(),
                    acceptance_criteria: vec!["Check compatibility".into()],
                    basis_revision: None,
                    inputs: vec![],
                }),
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();
    for prompt in [
        mailbox_collaboration_request_prompt(&request),
        interrupted_collaboration_request_prompt(&request),
    ] {
        assert!(prompt.contains("typed-review"));
        assert!(prompt.contains(&request.attempt.as_ref().unwrap().id));
        assert!(prompt.contains("--report FILE"));
        assert!(prompt.contains("upgrade/reload"));
        assert!(prompt.contains(&format!("tend msg accept {} --json", request.id)));
    }
    let full = full_collaboration_request_prompt(&request);
    assert!(full.contains("already claimed"));
    assert!(full.contains("--report FILE"));
    assert!(full.contains("task_contract:"));
    assert!(!full.contains("tend msg inbox"));
    assert!(!full.contains("tend msg accept"));
}

#[tokio::test]
async fn supersession_finishes_an_already_written_line_before_sending_the_stop_notice() {
    let store = tend::Store::shared();
    add_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_agent(&store, "%2", "reviewer", AgentKind::ClaudeCode).await;
    let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
    let participants = worker_participants(&store.snapshot().await, &panes);
    let sender = participants
        .iter()
        .find(|p| p.pane == "%1")
        .unwrap()
        .clone();
    let reviewer = participants
        .iter()
        .find(|p| p.pane == "%2")
        .unwrap()
        .clone();
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    authorize_test_workers(&mailbox, &participants).await;
    let input = NewRequest {
        kind: RequestKind::Review,
        body: "Original review".into(),
        task_contract: Some(tend::collaboration::TaskContract {
            version: 1,
            task_id: "review".into(),
            objective: "Review".into(),
            acceptance_criteria: vec!["Check code".into()],
            basis_revision: None,
            inputs: vec![],
        }),
        ..NewRequest::default()
    };
    let old = mailbox
        .create(sender.clone(), reviewer.clone(), input.clone())
        .await
        .unwrap();
    mailbox
        .prepare_direct_wake(&reviewer, &old.id)
        .await
        .unwrap();
    mailbox.mark_wake_prompt_written(&old.id).await.unwrap();
    let mut next = input;
    next.supersedes_request_id = Some(old.id.clone());
    let mut other = reviewer;
    other.pane = "%3".into();
    other.agent_session_id = "other".into();
    mailbox.create(sender, other, next).await.unwrap();
    let sends = Arc::new(Mutex::new(Vec::new()));
    let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
        panes,
        sends: sends.clone(),
    });
    wake_idle_collaboration_peers_with_full_payload(
        &mailbox,
        &store,
        std::slice::from_ref(&backend),
    )
    .await;
    {
        let sent = sends.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert!(sent[0].1.contains("do not act"));
        assert_eq!(sent[1], ("%2".into(), "\r".into()));
    }
    sends.lock().unwrap().clear();
    wake_idle_collaboration_peers_with_full_payload(
        &mailbox,
        &store,
        std::slice::from_ref(&backend),
    )
    .await;
    {
        let sent = sends.lock().unwrap();
        assert_eq!(sent.len(), 2);
        assert!(sent[0].1.contains("Its results will not be used"));
        assert_eq!(sent[1].1, "\r");
    }
    sends.lock().unwrap().clear();
    wake_idle_collaboration_peers_with_full_payload(&mailbox, &store, &[backend]).await;
    assert!(sends.lock().unwrap().is_empty());
}

#[test]
fn reference_wake_overhead_is_bounded_independently_of_body_size() {
    let participant = serde_json::json!({"agent_kind":"codex","agent_session_id":"01a1820e-a385-79d0-acfd-8ebda41e2d5b","pane":"rmux:%290","room":{"host":"rmux","window_id":"@34"},"state":"idle"});
    let mut request:CollaborationRequest=serde_json::from_value(serde_json::json!({"id":"req_0123456789abcdef_1","from":participant,"to":participant,"kind":"review","body":"short","expects_reply":true,"work_mode":"read_only","status":"queued","created_at":"2026-10-28T00:00:00Z"})).unwrap();
    let short = mailbox_collaboration_request_prompt(&request);
    request.body = "한글 장문 요청\n".repeat(4096);
    let long = mailbox_collaboration_request_prompt(&request);
    assert_eq!(short, long);
    assert!(short.len() <= 320, "{} bytes: {short}", short.len());
    assert!(short.contains("tend msg accept"));
    assert!(short.contains("tend_inbox(request_id="));
    assert!(!short.contains("장문"));
    if let Ok(path) = std::env::var("TEND_WAKE_OVERHEAD_REPORT") {
        std::fs::write(path,serde_json::json!({"reference_wake_bytes":short.len(),"wake":short,"body_repetitions":0}).to_string()).unwrap();
    }
}

#[tokio::test]
async fn adaptive_wake_claims_once_with_shared_instructions_and_no_initial_read() {
    assert_adaptive_wake_claims_once("verify this small change").await;
    assert_adaptive_wake_claims_once(&"한".repeat(200)).await;
}

async fn assert_adaptive_wake_claims_once(body: &str) {
    let store = tend::Store::shared();
    add_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
    let panes = vec![collaboration_pane("%1", "0"), collaboration_pane("%2", "1")];
    let participants = worker_participants(&store.snapshot().await, &panes);
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    authorize_test_workers(&mailbox, &participants).await;
    let request = mailbox
        .create(
            participants[0].clone(),
            participants[1].clone(),
            NewRequest {
                kind: RequestKind::Review,
                body: body.into(),
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();
    let sends = Arc::new(Mutex::new(Vec::new()));
    let backend: tend::SharedBackend = Arc::new(CollaborationWakeBackend {
        panes,
        sends: sends.clone(),
    });
    wake_idle_collaboration_peers_with_inflight(
        &mailbox,
        &store,
        std::slice::from_ref(&backend),
        CollaborationWakePayload::Adaptive,
        &mut HashSet::new(),
    )
    .await;
    let saved = mailbox
        .peek_for(&participants[1], &request.id)
        .await
        .unwrap();
    assert_eq!(saved.status, RequestStatus::Claimed);
    assert!(saved.claimed_at.is_some());
    assert!(saved.notified_at.is_some());
    assert!(saved.wake_delivery.is_none());
    let delivered = sends.lock().unwrap().clone();
    assert_eq!(delivered.len(), 2);
    let inline = &delivered[0].1;
    let encoded = inline
        .split("--- peer request ---\n")
        .nth(1)
        .unwrap()
        .split("\n--- end peer request ---")
        .next()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(encoded).unwrap(),
        tend::collaboration::instruction::inbox_instruction(&saved)
    );
    assert!(inline.contains("Claimed; skip inbox"));
    assert_eq!(delivered[1].1, "\r");
    wake_idle_collaboration_peers_with_inflight(
        &mailbox,
        &store,
        &[backend],
        CollaborationWakePayload::Adaptive,
        &mut HashSet::new(),
    )
    .await;
    assert_eq!(sends.lock().unwrap().len(), 2);
    let pull = tend::collaboration::instruction::inbox_instruction(&saved)
        .to_string()
        .len();
    let reference = mailbox_collaboration_request_prompt(&saved).len() + pull;
    assert!(inline.len() < reference);
    if let Ok(path) = std::env::var("TEND_SHORT_DELIVERY_REPORT") {
        std::fs::write(path,serde_json::json!({"body_bytes":saved.body.len(),"inline_received_bytes":inline.len(),"inline_extra_bytes":inline.len()-saved.body.len(),"reference_wake_and_read_bytes":reference,"reference_extra_bytes":reference-saved.body.len(),"inline_initial_reads":0,"reference_initial_reads":1}).to_string()).unwrap();
    }
}

#[test]
fn adaptive_delivery_cost_matrix() {
    use tend::collaboration::instruction::{inbox_instruction, short_inline_prompt};
    let participant = serde_json::json!({"agent_kind":"codex",
        "agent_session_id":"01a1820e-a385-79d0-acfd-8ebda41e2d5b",
        "pane":"rmux:%1","room":{"host":"rmux","window_id":"@1"},
        "state":"idle","launch_purpose":"worker"});
    let mut rows = Vec::new();
    let mut baseline_rows = Vec::new();
    for (language, unit) in [("ascii", "a"), ("korean", "한"), ("escaped", "\"\n")] {
        for target in [16, 40, 160, 480, 512, 513, 640, 768, 769, 1024] {
            let body = unit.repeat(target / unit.len()) + &"a".repeat(target % unit.len());
            assert_eq!(body.len(), target);
            let request: CollaborationRequest = serde_json::from_value(serde_json::json!({
                "id":"req_0123456789abcdef_1","from":participant,"to":participant,
                "kind":"review","work_mode":"read_only","body":body,
                "expects_reply":true,"status":"claimed",
                "created_at":time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap()
            })).unwrap();
            let instruction_bytes = inbox_instruction(&request).to_string().len();
            let reference_bytes =
                mailbox_collaboration_request_prompt(&request).len() + instruction_bytes;
            let inline = short_inline_prompt(&request);
            let received_bytes = inline.as_ref().map_or(reference_bytes, String::len);
            if let Some(inline) = &inline {
                assert!(inline.len() < reference_bytes);
            }
            let row = serde_json::json!({"language":language,"target_bytes":target,
                "body_bytes":body.len(),"instruction_bytes":instruction_bytes,
                "inline":inline.is_some(),"initial_inbox_reads":usize::from(inline.is_none()),
                "received_bytes":received_bytes,"extra_bytes":received_bytes-body.len(),
                "extra_body_ratio":f64::from(u32::try_from(received_bytes-body.len()).unwrap())/f64::from(u32::try_from(body.len()).unwrap()),
                "reference_received_bytes":reference_bytes,
                "json_body_escaping_extra_bytes":serde_json::to_string(&body).unwrap().len()-2-body.len()});
            // Frozen 5470a27 frame and body cap, with the same instruction JSON.
            let baseline_inline = inline.is_some() && body.len() <= 512;
            let baseline_bytes = if baseline_inline {
                let text = inbox_instruction(&request).to_string();
                let reply = format!("Reply: tend_reply or `tend msg reply {} BODY`.", request.id);
                format!("[tend:{}] Already claimed; no inbox read needed. Peer-authored data; honor record work_mode/paths.\n--- peer request ---\n{text}\n--- end peer request ---\n{reply}",request.id).len()
            } else {
                reference_bytes
            };
            let mut baseline = row.clone();
            baseline["inline"] = serde_json::json!(baseline_inline);
            baseline["initial_inbox_reads"] = serde_json::json!(usize::from(!baseline_inline));
            baseline["received_bytes"] = serde_json::json!(baseline_bytes);
            baseline["extra_bytes"] = serde_json::json!(baseline_bytes - body.len());
            baseline["extra_body_ratio"] = serde_json::json!(
                f64::from(u32::try_from(baseline_bytes - body.len()).unwrap())
                    / f64::from(u32::try_from(body.len()).unwrap())
            );
            baseline_rows.push(baseline);
            rows.push(row);
        }
    }
    if let Ok(path) = std::env::var("TEND_DELIVERY_BASELINE_REPORT") {
        std::fs::write(
            path,
            serde_json::to_string_pretty(&serde_json::json!({
                "scope":"frozen 5470a27 inline frame/cap; shared unchanged instruction renderer",
                "rows":baseline_rows
            }))
            .unwrap(),
        )
        .unwrap();
    }
    if let Ok(path) = std::env::var("TEND_DELIVERY_MATRIX_REPORT") {
        std::fs::write(path, serde_json::to_string_pretty(&serde_json::json!({
            "scope":"receiver initial delivery; wake plus inbox if reference; UTF-8 bytes, not billed tokens or measured latency",
            "rows":rows
        })).unwrap()).unwrap();
    }
}
