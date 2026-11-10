use super::*;

/// The hook path end to end over real IPC: an agent sends user-authorized
/// work to a human primary, a paneless exact-session hook (a Codex shared
/// app-server) is offered it without consuming it, and the model's own claim
/// is what takes it.
#[tokio::test]
async fn a_paneless_hook_is_offered_user_authorized_work_and_the_model_claims_it() {
    use collaboration::turn_offer::TurnBoundaryEvent;
    let dir = tempdir().unwrap();
    let socket = dir.path().join("turn-boundary.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_collaboration_agent(&store, "%2", "human", AgentKind::ClaudeCode).await;
    let backend: SharedBackend = Arc::new(CollaborationTestBackend {
        panes: vec![
            collaboration_test_pane("%1", "0"),
            collaboration_test_pane("%2", "1"),
        ],
    });
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    let server = Server::new(socket.clone(), store)
        .with_backends(vec![backend])
        .with_collaboration(mailbox.clone());
    let (shutdown, rx) = broadcast::channel(1);
    let serving = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&socket).await;
    let client = Client::new(socket);
    let sender = CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: Some("default".into()),
        console: false,
        agent_session_id: None,
    };
    let sent = client
        .collaboration_send(
            &sender,
            "%2",
            &collaboration::NewRequest {
                body: "plan the playbook import".into(),
                user_authorized: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(sent.user_authorized);
    assert_eq!(sent.status, collaboration::RequestStatus::Queued);

    let hook = CollaborationOrigin {
        exact_session: true,
        pane: String::new(),
        socket: None,
        console: false,
        agent_session_id: Some("human".into()),
    };
    let offers = client
        .collaboration_turn_boundary_offer(&hook, TurnBoundaryEvent::Stop)
        .await
        .unwrap();
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0]["request_id"], sent.id.as_str());
    assert_eq!(offers[0]["body"], "plan the playbook import");
    assert_eq!(
        client
            .collaboration_peek(&sender, &sent.id)
            .await
            .unwrap()
            .status,
        collaboration::RequestStatus::Queued,
        "an offer must not consume the request"
    );

    let claimed = client.collaboration_claim(&hook, &sent.id).await.unwrap();
    assert_eq!(claimed[0].status, collaboration::RequestStatus::Claimed);
    assert_eq!(claimed[0].body, "plan the playbook import");

    let unbound = CollaborationOrigin {
        agent_session_id: Some("no-such-session".into()),
        ..hook
    };
    assert!(client
        .collaboration_turn_boundary_offer(&unbound, TurnBoundaryEvent::Stop)
        .await
        .is_err());

    let _ = shutdown.send(());
    serving.await.unwrap();
}
