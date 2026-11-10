use super::*;

#[tokio::test]
#[allow(clippy::too_many_lines)] // One real socket lifecycle covers baseline, filtering, and request mutations.
async fn request_history_subscription_ignores_worker_changes_but_keeps_request_changes() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("history-events.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_collaboration_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
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
    let origin = CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: Some("default".into()),
        console: false,
        agent_session_id: None,
    };
    let room = client.collaboration_context(&origin).await.unwrap();
    // A populated store before subscribing must not make unrelated metadata look dirty.
    mailbox
        .create(
            room.current.clone(),
            room.peers[0].clone(),
            collaboration::NewRequest {
                body: "baseline".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut all = client.collaboration_subscribe().await.unwrap();
    let mut history = client.collaboration_requests_subscribe().await.unwrap();
    let baseline = mailbox.request_revision();
    mailbox
        .set_purpose(&room.current, collaboration::LaunchPurpose::Worker)
        .await
        .unwrap();
    assert_eq!(mailbox.request_revision(), baseline);
    let _ = tokio::time::timeout(Duration::from_secs(1), all.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), history.recv())
            .await
            .is_err()
    );
    // A mutation immediately after the ACK is either in the snapshot or delivered as an event.
    let request = mailbox
        .create(
            room.current.clone(),
            room.peers[0].clone(),
            collaboration::NewRequest {
                body: "after-ack".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(1), history.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let snapshot = client
        .collaboration_list(&origin, RequestMailbox::Sent)
        .await
        .unwrap();
    assert!(snapshot.iter().any(|row| row.id == request.id));
    mailbox.mark_notified(&request.id).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(1), history.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    mailbox
        .claim_by_id(&room.peers[0], &request.id)
        .await
        .unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(1), history.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(history);
    drop(all);
    shutdown.send(()).unwrap();
    serving.await.unwrap();
}

#[test]
fn request_subscription_filter_is_compatible_in_both_directions() {
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    enum OldRequest {
        CollaborationSubscribe,
    }
    let legacy = serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"collaboration_subscribe"});
    let parsed: Request = serde_json::from_value(legacy).unwrap();
    assert!(matches!(
        parsed.body,
        RequestBody::CollaborationSubscribe {
            requests_only: false
        }
    ));
    let modern = serde_json::json!({"kind":"collaboration_subscribe","requests_only":true});
    assert!(
        serde_json::from_value::<OldRequest>(modern).is_ok(),
        "old servers safely emit broader invalidations"
    );
}
