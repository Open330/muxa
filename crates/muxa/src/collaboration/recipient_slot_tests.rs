//! Logical recipients: `workspace/work/role` paths, slots recorded for role
//! and alias targets, and following a recipient that moved or was replaced.

use super::*;

fn pane(id: &str, window: &str, work: Option<&str>, role: Option<&str>) -> PaneInfo {
    PaneInfo {
        session_group: None,
        agent_program: None,
        worker_launch_id: None,
        agent_role: role.map(str::to_string),
        agent_alias: None,
        workspace_id: work.map(|_| "callabo".to_string()),
        work_id: work.map(str::to_string),
        pane_id: id.into(),
        session_id: "$1".into(),
        session: "callabo".into(),
        window_id: window.into(),
        window_name: work.unwrap_or("main").into(),
        window_index: "0".into(),
        pane_index: "0".into(),
        tty: String::new(),
        current_command: "codex".into(),
        title: String::new(),
        current_path: "/repo".into(),
        pane_pid: 0,
        socket: Some("default".into()),
    }
}

fn agent(pane: &PaneInfo, session: &str, purpose: LaunchPurpose) -> Participant {
    Participant {
        agent_kind: AgentKind::Codex,
        agent_session_id: session.into(),
        pane: pane.pane_id.clone(),
        socket: pane.socket.clone(),
        room: RoomId {
            host: "rmux".into(),
            socket: Some("default".into()),
            window_id: pane.window_id.clone(),
        },
        tmux_session_id: Some("$1".into()),
        tmux_session_name: Some("callabo".into()),
        window_name: Some(pane.window_name.clone()),
        state: AgentState::Idle,
        cwd: Some("/repo".into()),
        alias: None,
        roles: pane.agent_role.iter().cloned().collect(),
        console: false,
        launch_purpose: purpose,
    }
}

const WORKER: LaunchPurpose = LaunchPurpose::Worker;

#[test]
fn slot_paths_parse_and_pane_ids_do_not() {
    assert_eq!(
        parse_slot_path("callabo/CAL-8111/implementer"),
        Some((
            "callabo".into(),
            "CAL-8111".into(),
            Some("implementer".into()),
            None
        ))
    );
    assert_eq!(
        parse_slot_path("callabo/CAL-8111/reviewer@codex"),
        Some((
            "callabo".into(),
            "CAL-8111".into(),
            Some("reviewer".into()),
            Some("codex".into())
        ))
    );
    assert_eq!(
        parse_slot_path("callabo/CAL-8111/@lead"),
        Some((
            "callabo".into(),
            "CAL-8111".into(),
            None,
            Some("lead".into())
        ))
    );
    for not_a_path in [
        "rmux:%5",
        "%5",
        "pane:%5",
        "@review",
        "role:reviewer",
        "a/b",
        "a/b/c/d",
        "a//c",
        "a/b/",
    ] {
        assert_eq!(parse_slot_path(not_a_path), None, "{not_a_path}");
    }
}

#[test]
fn a_slot_path_resolves_to_exactly_one_live_holder() {
    let orchestrator_pane = pane("rmux:%4", "@4", None, None);
    let orchestrator = agent(&orchestrator_pane, "orchestrator", LaunchPurpose::Primary);
    let impl_8111 = pane("rmux:%356", "@9", Some("CAL-8111"), Some("implementer"));
    let impl_7842 = pane("rmux:%357", "@10", Some("CAL-7842"), Some("implementer"));
    let panes = vec![orchestrator_pane, impl_8111.clone(), impl_7842.clone()];
    let participants = vec![
        orchestrator.clone(),
        agent(&impl_8111, "s-8111", WORKER),
        agent(&impl_7842, "s-7842", WORKER),
    ];

    let (recipient, slot) = resolve_slot_path(
        &orchestrator,
        "callabo/CAL-8111/implementer",
        &participants,
        &panes,
    )
    .unwrap()
    .unwrap();
    assert_eq!(recipient.agent_session_id, "s-8111");
    assert_eq!(slot.work_id.as_deref(), Some("CAL-8111"));
    assert_eq!(slot.role.as_deref(), Some("implementer"));
    assert_eq!(slot.room.window_id, "@9");

    assert!(matches!(
        resolve_slot_path(
            &orchestrator,
            "callabo/CAL-9999/implementer",
            &participants,
            &panes
        ),
        Some(Err(CollaborationError::UnknownTarget(_)))
    ));
    let mut doubled = participants.clone();
    doubled.push(agent(&impl_8111, "s-8111-twin", WORKER));
    doubled.last_mut().unwrap().pane = "rmux:%999".into();
    let mut twin_pane = impl_8111.clone();
    twin_pane.pane_id = "rmux:%999".into();
    let mut panes_twice = panes.clone();
    panes_twice.push(twin_pane);
    assert!(matches!(
        resolve_slot_path(
            &orchestrator,
            "callabo/CAL-8111/implementer",
            &doubled,
            &panes_twice
        ),
        Some(Err(CollaborationError::AmbiguousTarget(_)))
    ));
}

/// A pinned request with no live recipient: where (if anywhere) it goes.
struct Scenario {
    panes: Vec<PaneInfo>,
    request: CollaborationRequest,
}

fn scenario(slot: bool) -> Scenario {
    let sender_pane = pane("rmux:%4", "@4", None, None);
    let old_pane = pane("rmux:%414", "@163", Some("S1"), Some("reviewer"));
    let sender = agent(&sender_pane, "coordinator", LaunchPurpose::Primary);
    let old = agent(&old_pane, "old-reviewer", WORKER);
    let request = CollaborationRequest {
        to_slot: slot.then(|| {
            RecipientSlot::of(
                &old,
                std::slice::from_ref(&old_pane),
                Some("reviewer"),
                None,
            )
        }),
        ..request_between(sender, old)
    };
    Scenario {
        panes: vec![sender_pane, old_pane],
        request,
    }
}

fn request_between(from: Participant, to: Participant) -> CollaborationRequest {
    let store_request = serde_json::json!({
        "id": "req_slot", "kind": "review", "body": "review S1", "expects_reply": true,
        "work_mode": "read_only", "status": "queued",
        "created_at": "2026-10-21T06:47:00Z",
        "from": from, "to": to,
    });
    serde_json::from_value(store_request).unwrap()
}

#[test]
fn a_moved_session_is_followed_with_or_without_a_slot() {
    for slot in [false, true] {
        let Scenario { mut panes, request } = scenario(slot);
        let moved_pane = pane("rmux:%500", "@163", Some("S1"), Some("reviewer"));
        panes.push(moved_pane.clone());
        let moved = agent(&moved_pane, "old-reviewer", LaunchPurpose::Primary);
        let successor = successor_for(&request, &[moved], &[], &panes).unwrap();
        assert_eq!(successor.pane, "rmux:%500", "slot={slot}");
    }
}

#[test]
fn a_replaced_session_hands_a_slot_request_to_its_single_worker_successor() {
    let Scenario { mut panes, request } = scenario(true);
    let new_pane = pane("rmux:%600", "@163", Some("S1"), Some("reviewer"));
    panes.push(new_pane.clone());
    let fresh = agent(&new_pane, "new-reviewer", WORKER);

    let successor = successor_for(&request, std::slice::from_ref(&fresh), &[], &panes).unwrap();
    assert_eq!(successor.agent_session_id, "new-reviewer");

    // A protected successor is not chosen; neither is one of two.
    let mut primary = fresh.clone();
    primary.launch_purpose = LaunchPurpose::Primary;
    assert!(successor_for(&request, &[primary], &[], &panes).is_none());
    let mut twin = fresh.clone();
    twin.agent_session_id = "twin".into();
    twin.pane = "rmux:%601".into();
    let mut twin_pane = new_pane.clone();
    twin_pane.pane_id = "rmux:%601".into();
    panes.push(twin_pane);
    assert!(successor_for(&request, &[fresh, twin], &[], &panes).is_none());
}

#[test]
fn a_pane_pinned_request_never_switches_sessions() {
    let Scenario { mut panes, request } = scenario(false);
    let new_pane = pane("rmux:%600", "@163", Some("S1"), Some("reviewer"));
    panes.push(new_pane.clone());
    assert!(successor_for(
        &request,
        &[agent(&new_pane, "new-reviewer", WORKER)],
        &[],
        &panes
    )
    .is_none());
}

/// A session absent from one pane scan but still registered is probably
/// only momentarily unseen; nothing moves.
#[test]
fn a_still_registered_session_is_not_replaced() {
    let Scenario { mut panes, request } = scenario(true);
    let new_pane = pane("rmux:%600", "@163", Some("S1"), Some("reviewer"));
    panes.push(new_pane.clone());
    let store = crate::Store::shared();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let agents = runtime.block_on(async {
        store
            .apply(&crate::event::AgentEvent::Started {
                id: crate::event::AgentId {
                    kind: AgentKind::Codex,
                    session_id: "old-reviewer".into(),
                    surface: None,
                    pane: Some("rmux:%414".into()),
                    tmux_socket: Some("default".into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;
        store.snapshot().await
    });
    assert!(successor_for(
        &request,
        &[agent(&new_pane, "new-reviewer", WORKER)],
        &agents,
        &panes
    )
    .is_none());
}

#[tokio::test]
async fn rebinding_only_moves_undelivered_requests_and_records_the_old_session() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let sender_pane = pane("rmux:%4", "@4", None, None);
    let old_pane = pane("rmux:%414", "@163", Some("S1"), Some("reviewer"));
    let new_pane = pane("rmux:%600", "@163", Some("S1"), Some("reviewer"));
    let sender = agent(&sender_pane, "coordinator", LaunchPurpose::Primary);
    let old = agent(&old_pane, "old-reviewer", WORKER);
    let fresh = agent(&new_pane, "new-reviewer", WORKER);
    let create = |body: &str| NewRequest {
        body: body.into(),
        recipient_slot: Some(RecipientSlot::of(
            &old,
            std::slice::from_ref(&old_pane),
            Some("reviewer"),
            None,
        )),
        ..NewRequest::default()
    };
    let queued = store
        .create(sender.clone(), old.clone(), create("queued"))
        .await
        .unwrap();
    let claimed = store
        .create(sender.clone(), old.clone(), create("claimed"))
        .await
        .unwrap();
    store.claim_for(&old).await.unwrap();
    let _ = claimed;
    let fresh_queued = store
        .create(sender.clone(), old.clone(), create("after claim"))
        .await
        .unwrap();

    let rebound = store
        .rebind_recipient(&queued.id, &old, fresh.clone())
        .await
        .unwrap();
    assert!(rebound.is_none(), "claim_for claimed it; nothing to move");
    let rebound = store
        .rebind_recipient(&fresh_queued.id, &old, fresh.clone())
        .await
        .unwrap()
        .expect("an untouched queued request follows");
    assert_eq!(rebound.to.agent_session_id, "new-reviewer");
    assert_eq!(rebound.rebound_from, ["old-reviewer"]);
    assert_eq!(rebound.from.agent_session_id, "coordinator");
    assert!(
        store
            .rebind_recipient(&fresh_queued.id, &old, fresh)
            .await
            .unwrap()
            .is_none(),
        "a request no longer pinned to `from` is left alone"
    );
}

/// A dispatch retry resolves its slot afresh and may reach the successor
/// the request already followed to. That is the same request, not a
/// conflicting one.
#[tokio::test]
async fn a_dispatch_retry_after_rebinding_matches_by_slot() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let sender_pane = pane("rmux:%4", "@4", None, None);
    let old_pane = pane("rmux:%414", "@163", Some("S1"), Some("reviewer"));
    let new_pane = pane("rmux:%600", "@163", Some("S1"), Some("reviewer"));
    let sender = agent(&sender_pane, "coordinator", LaunchPurpose::Primary);
    let old = agent(&old_pane, "old-reviewer", WORKER);
    let fresh = agent(&new_pane, "new-reviewer", WORKER);
    let slot = RecipientSlot::of(
        &old,
        std::slice::from_ref(&old_pane),
        Some("reviewer"),
        None,
    );
    let input = NewRequest {
        dispatch_id: Some(uuid::Uuid::new_v4().to_string()),
        body: "review S1".into(),
        recipient_slot: Some(slot.clone()),
        ..NewRequest::default()
    };
    let sent = store
        .create(sender.clone(), old.clone(), input.clone())
        .await
        .unwrap();
    // The retry reaches the successor before the daemon has rebound the
    // request (pinned to `old` still), and again after it has.
    let early = store
        .create(sender.clone(), fresh.clone(), input.clone())
        .await
        .unwrap();
    assert_eq!(early.id, sent.id);
    store
        .rebind_recipient(&sent.id, &old, fresh.clone())
        .await
        .unwrap()
        .unwrap();

    let retried = store
        .create(sender.clone(), fresh.clone(), input.clone())
        .await
        .unwrap();
    assert_eq!(retried.id, sent.id);

    let mut other_slot = input;
    other_slot.recipient_slot = Some(RecipientSlot {
        role: Some("implementer".into()),
        ..slot
    });
    assert!(matches!(
        store.create(sender, fresh, other_slot).await,
        Err(CollaborationError::DispatchConflict(_))
    ));
}

fn on_server(mut participant: Participant, socket: &str) -> Participant {
    participant.socket = Some(socket.into());
    participant.room.socket = Some(socket.into());
    participant
}

/// Work ids are per server. A worker with the same work on another rmux
/// server never holds the slot, and its pane metadata is never borrowed.
#[test]
fn a_slot_is_held_only_on_its_own_server() {
    let Scenario { mut panes, request } = scenario(true);
    let mut elsewhere = pane("rmux:%600", "@163", Some("S1"), Some("reviewer"));
    elsewhere.socket = Some("/tmp/rmux-other".into());
    panes.push(elsewhere.clone());
    let other_server = on_server(
        agent(&elsewhere, "foreign-reviewer", WORKER),
        "/tmp/rmux-other",
    );
    assert!(successor_for(&request, std::slice::from_ref(&other_server), &[], &panes).is_none());

    // Same pane id on the slot's server but no scan row for that socket:
    // the other server's row must not stand in for it.
    let mut local = agent(&elsewhere, "local-reviewer", WORKER);
    local.socket = Some("default".into());
    assert!(!request.to_slot.as_ref().unwrap().held_by(&local, &panes));
}

/// A registered identity replaces the launch mark: an agent re-roled from
/// reviewer to implementer no longer answers to `…/reviewer`.
#[test]
fn a_stale_launch_role_does_not_hold_a_slot() {
    let sender_pane = pane("rmux:%4", "@4", None, None);
    let sender = agent(&sender_pane, "coordinator", LaunchPurpose::Primary);
    let marked = pane("rmux:%7", "@9", Some("CAL-8111"), Some("reviewer"));
    let mut reroled = agent(&marked, "s", WORKER);
    reroled.roles = vec!["implementer".into()];
    let panes = vec![sender_pane, marked];
    assert!(matches!(
        resolve_slot_path(
            &sender,
            "callabo/CAL-8111/reviewer",
            &[reroled.clone()],
            &panes
        ),
        Some(Err(CollaborationError::UnknownTarget(_)))
    ));
    assert!(
        resolve_slot_path(&sender, "callabo/CAL-8111/implementer", &[reroled], &panes)
            .unwrap()
            .is_ok()
    );
}

/// After a work's window is recreated the slot carries a new window id;
/// a retry is still the same request, matched by durable work identity.
#[tokio::test]
async fn a_dispatch_retry_survives_a_recreated_work_window() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let sender_pane = pane("rmux:%4", "@4", None, None);
    let old_pane = pane("rmux:%414", "@163", Some("S1"), Some("reviewer"));
    let new_pane = pane("rmux:%600", "@200", Some("S1"), Some("reviewer"));
    let sender = agent(&sender_pane, "coordinator", LaunchPurpose::Primary);
    let old = agent(&old_pane, "old-reviewer", WORKER);
    let fresh = agent(&new_pane, "new-reviewer", WORKER);
    let input = |slot_of: &Participant, slot_pane: &PaneInfo| NewRequest {
        dispatch_id: Some("7a6b3a9e-2f61-4f0e-9a51-6f7c2f6a8d10".into()),
        body: "review S1".into(),
        recipient_slot: Some(RecipientSlot::of(
            slot_of,
            std::slice::from_ref(slot_pane),
            Some("reviewer"),
            None,
        )),
        ..NewRequest::default()
    };
    let sent = store
        .create(sender.clone(), old.clone(), input(&old, &old_pane))
        .await
        .unwrap();
    store
        .rebind_recipient(&sent.id, &old, fresh.clone())
        .await
        .unwrap()
        .unwrap();
    let retried = store
        .create(sender, fresh.clone(), input(&fresh, &new_pane))
        .await
        .unwrap();
    assert_eq!(retried.id, sent.id);
}
