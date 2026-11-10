use super::*;

#[tokio::test]
#[allow(clippy::too_many_lines)] // One isolated mutation/read lifecycle verifies transport projection only.
async fn receipt_views_do_not_echo_sent_bodies_or_strip_stored_instructions() {
    use collaboration::CollaborationReadView as View;
    let dir = tempdir().unwrap();
    let socket = dir.path().join("receipts.sock");
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
    let audit = CollaborationAuditLog::in_memory();
    let server = Server::new(socket.clone(), store)
        .with_backends(vec![backend])
        .with_collaboration(mailbox.clone())
        .with_collaboration_audit(audit.clone());
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
    let recipient = CollaborationOrigin {
        pane: "%2".into(),
        ..sender.clone()
    };
    let input = collaboration::NewRequest {
        body: format!("{}end", "original-task-한글\n".repeat(128)),
        paths: vec!["src/lib.rs".into()],
        ..Default::default()
    };
    let full = client
        .collaboration_send(&sender, "%2", &input)
        .await
        .unwrap();
    assert_eq!(
        full.body, input.body,
        "legacy/default send must remain full"
    );
    let sent = client
        .collaboration_send_view(&sender, "%2", &input, View::Metadata)
        .await
        .unwrap();
    assert!(sent.body.is_empty() && sent.paths.is_empty());
    assert_eq!(sent.from, full.from);
    assert_eq!(sent.to, full.to);
    assert_eq!(sent.status, full.status);
    let claimed = client
        .collaboration_claim(&recipient, &sent.id)
        .await
        .unwrap();
    assert_eq!(claimed[0].body, input.body);
    assert_eq!(
        claimed[0].paths, input.paths,
        "recipient must keep the execution contract"
    );
    let cancelled = client
        .collaboration_cancel_view(&sender, &full.id, View::Metadata)
        .await
        .unwrap();
    assert!(cancelled.body.is_empty());
    assert_eq!(cancelled.status, collaboration::RequestStatus::Cancelled);
    assert_eq!(
        client
            .collaboration_peek(&sender, &full.id)
            .await
            .unwrap()
            .body,
        input.body
    );
    let entries = audit.entries().await;
    let sends: Vec<_> = entries
        .iter()
        .filter(|r| r.operation == CollaborationAuditOperation::Send)
        .collect();
    assert_eq!(sends.len(), 2);
    assert_eq!(sends[0].response_body_bytes, Some(input.body.len()));
    assert_eq!(sends[1].response_body_bytes, Some(0));
    // Numeric wire evidence only: no request text in successful test output.
    println!(
        "mutation_wire_cost {}",
        serde_json::json!({
            "original_body_bytes":input.body.len(),
            "full_request_bytes":sends[0].request_bytes,
            "full_response_bytes":sends[0].response_bytes,
            "metadata_request_bytes":sends[1].request_bytes,
            "metadata_response_bytes":sends[1].response_bytes,
            "full_response_body_bytes":sends[0].response_body_bytes,
            "metadata_response_body_bytes":sends[1].response_body_bytes,
        })
    );
    assert!(
        sends[0].response_bytes.unwrap() >= sends[1].response_bytes.unwrap() + input.body.len()
    );
    let cancel = entries
        .iter()
        .find(|r| r.operation == CollaborationAuditOperation::Cancel)
        .unwrap();
    assert_eq!(cancel.response_body_bytes, Some(0));
    assert_eq!(cancel.read_view, Some(View::Metadata));
    shutdown.send(()).unwrap();
    serving.await.unwrap();
}
