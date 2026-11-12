use super::*;

#[tokio::test]
async fn subscribe_streams_transitions_to_client() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-sub.sock");
    let store = Store::shared();
    let server = Server::new(sock.clone(), store.clone());
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Open subscription before any events fire.
    let client = Client::new(sock.clone());
    let mut stream = client.subscribe().await.expect("subscribe");

    // Drive a state transition: Started → Idle (initial).
    let id = AgentId {
        tmux_socket: None,
        kind: AgentKind::ClaudeCode,
        session_id: "sub-test".into(),
        surface: None,
        pane: Some("%9".into()),
        cwd: None,
    };
    store
        .apply(&AgentEvent::Started {
            id: id.clone(),
            at: OffsetDateTime::now_utc(),
        })
        .await;

    // Then Idle → Working via PromptSubmitted.
    store
        .apply(&AgentEvent::PromptSubmitted {
            id: id.clone(),
            prompt: "hi".into(),
            at: OffsetDateTime::now_utc(),
        })
        .await;

    // Stream should deliver both transitions in order.
    let t1 = tokio::time::timeout(std::time::Duration::from_secs(2), stream.recv())
        .await
        .expect("first transition arrives within timeout")
        .expect("recv ok")
        .expect("transition present");
    assert_eq!(t1.from, AgentState::Starting);
    assert_eq!(t1.to, AgentState::Idle);

    let t2 = tokio::time::timeout(std::time::Duration::from_secs(2), stream.recv())
        .await
        .expect("second transition arrives within timeout")
        .expect("recv ok")
        .expect("transition present");
    assert_eq!(t2.from, AgentState::Idle);
    assert_eq!(t2.to, AgentState::Working);

    // Drop the stream to close the connection, then shut down.
    drop(stream);
    tx.send(()).unwrap();
    let _ = handle.await;
}

#[tokio::test]
async fn pipeline_done_is_atomic_and_opens_downstream_over_ipc() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-pipeline.sock");
    let pipeline_runs = PipelineRunStore::in_memory();
    let server =
        Server::new(sock.clone(), Store::shared()).with_pipeline_runs(pipeline_runs.clone());
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let desired = |alias: &str, after: Vec<String>| crate::pipeline::DesiredAgent {
        alias: alias.to_string(),
        program: "codex".to_string(),
        role: None,
        task: None,
        prompt: None,
        options: Vec::new(),
        direction: None,
        after,
        purpose: crate::collaboration::LaunchPurpose::Primary,
    };
    let identity = WorkIdentity::new("ws", "WORK-1");
    let client = Client::new(sock.clone());
    let run = client
        .pipeline_register(&PipelineRunRegistration {
            identity: identity.clone(),
            pipeline: "chain".to_string(),
            desired: vec![
                desired("plan", Vec::new()),
                desired("impl", vec!["plan".to_string()]),
            ],
            cwd: PathBuf::from("/tmp"),
            window_id: Some("@1".to_string()),
            observed: Vec::new(),
            invalidate: Vec::new(),
        })
        .await
        .unwrap();
    let root = client
        .pipeline_claim(&identity, run.generation)
        .await
        .unwrap();
    assert_eq!(root.len(), 1);
    assert_eq!(root[0].agent.alias, "plan");

    client
        .pipeline_done(&identity, "plan", run.generation)
        .await
        .unwrap();
    let downstream = client
        .pipeline_claim(&identity, run.generation)
        .await
        .unwrap();
    assert_eq!(downstream.len(), 1);
    assert_eq!(downstream[0].agent.alias, "impl");

    let invalidated = client
        .pipeline_invalidate(&identity, "plan", run.generation)
        .await
        .unwrap();
    assert!(client
        .pipeline_done(&identity, "plan", run.generation)
        .await
        .is_err());
    assert_eq!(
        invalidated.aliases["impl"].status,
        PipelineAliasStatus::Pending
    );

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn not_connected_when_socket_missing() {
    // ENOENT path: tempdir exists but the socket file doesn't.
    let dir = tempdir().unwrap();
    let sock = dir.path().join("does-not-exist.sock");
    let client = Client::new(sock.clone());
    let err = client
        .call(&serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "snapshot" }))
        .await
        .expect_err("expected NotConnected when socket does not exist");
    match err {
        RuntimeError::NotConnected(p) => assert_eq!(p, sock),
        other => panic!("expected NotConnected, got {other:?}"),
    }
}

#[tokio::test]
async fn not_connected_when_socket_is_stale_file() {
    // Stale-file path: a regular file exists at the socket path but
    // nothing is listening. On Linux, connect(2) returns ECONNREFUSED for
    // a non-socket path; `tokio` may also surface ENOTSOCK. We accept any
    // mapping into NotConnected — the user-visible behaviour is the same.
    let dir = tempdir().unwrap();
    let sock = dir.path().join("stale.sock");
    std::fs::write(&sock, b"").unwrap();
    let client = Client::new(sock.clone());
    let res = client
        .call(&serde_json::json!({ "protocol": PROTOCOL_VERSION, "kind": "snapshot" }))
        .await;
    // If the platform returns a kind we don't remap (e.g. ENOTSOCK on
    // some libc), the call still errors — just not necessarily with
    // NotConnected. Only assert the friendly mapping when we got it.
    if let Err(RuntimeError::NotConnected(p)) = &res {
        assert_eq!(p, &sock);
    }
    // Either way, the call must not succeed.
    assert!(res.is_err());
}

#[tokio::test]
async fn shutdown_cancels_all_subscription_kinds_without_drain_timeout() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("subscriptions.sock");
    let (fleet, _commands) = FleetRuntime::new(Arc::new(crate::fleet::FleetStore::new()));
    let server = Server::new(sock.clone(), Store::shared()).with_fleet(fleet);
    let (shutdown, receiver) = broadcast::channel(1);
    let task = tokio::spawn(server.run(receiver));
    wait_for_socket(&sock).await;

    // Keep each client alive through shutdown. No state changes or
    // keepalive writes should be necessary to release these handlers.
    let mut clients = Vec::new();
    for kind in [
        "subscribe",
        "subscribe_agent_changes",
        "fleet_subscribe",
        "collaboration_subscribe",
        "ask_subscribe",
        "pipeline_subscribe",
    ] {
        let mut stream = Stream::connect(&sock).await.unwrap();
        let request = serde_json::json!({"protocol": PROTOCOL_VERSION, "kind": kind});
        stream
            .write_all(&encode_line(&request, PROTOCOL_VERSION).unwrap())
            .await
            .unwrap();
        let mut reader = BufReader::new(stream);
        let mut ack = String::new();
        tokio::time::timeout(Duration::from_secs(1), reader.read_line(&mut ack))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&ack).unwrap()["ok"],
            true,
            "{kind}: {ack}"
        );
        clients.push(reader);
    }
    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("subscriptions must not consume the five-second drain deadline")
        .unwrap()
        .unwrap();
    for mut client in clients {
        let mut line = String::new();
        assert_eq!(client.read_line(&mut line).await.unwrap(), 0);
    }
}

#[tokio::test]
async fn stream_takeover_after_shutdown_does_not_poll_the_pump() {
    let (shutdown, mut receiver) = watch::channel(false);
    shutdown.send_replace(true);
    let result = until_server_shutdown(&mut receiver, async {
        panic!("a stream admitted after shutdown must not start");
    })
    .await;
    assert!(result.is_none());
}

// Fills a socket send buffer through `OwnedWriteHalf::try_write`; a named
// pipe has no non-blocking write to fill it with.
#[cfg(unix)]
#[tokio::test]
async fn shutdown_cancels_a_backpressured_subscription_ack() {
    let (server, _client) = Stream::pair().await.unwrap();
    let (_reader, mut writer) = server.into_split();
    writer.writable().await.unwrap();
    let bytes = vec![0_u8; 65536];
    let mut written = 0;
    loop {
        match writer.try_write(&bytes) {
            Ok(n) => written += n,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("fill socket: {error}"),
        }
    }
    assert!(written > 0);
    let (stop, mut stopping) = watch::channel(false);
    let mut ack = Box::pin(write_subscription_ack(&mut writer, b"{}\n", &mut stopping));
    assert!(tokio::time::timeout(Duration::from_millis(30), &mut ack)
        .await
        .is_err());
    stop.send_replace(true);
    assert!(!tokio::time::timeout(Duration::from_secs(1), ack)
        .await
        .unwrap()
        .unwrap());
}

// Fills a socket send buffer through `OwnedWriteHalf::try_write`; a named
// pipe has no non-blocking write to fill it with.
#[cfg(unix)]
#[tokio::test]
async fn shutdown_cancels_a_backpressured_subscription_write() {
    let (server, _client) = Stream::pair().await.unwrap();
    let (_reader, writer) = server.into_split();
    // Fill the send buffer while the peer deliberately reads nothing.
    let bytes = vec![0_u8; 65536];
    writer.writable().await.unwrap();
    let mut written = 0;
    loop {
        match writer.try_write(&bytes) {
            Ok(n) => written += n,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("fill socket: {error}"),
        }
    }
    assert!(written > 0, "the test must fill the actual socket buffer");
    let (changes, receiver) = watch::channel(0_u64);
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    changes.send_replace(1);
    let mut pump = Box::pin(until_server_shutdown(
        &mut shutdown_rx,
        stream_revision_updates(writer, receiver, PROTOCOL_VERSION),
    ));
    assert!(tokio::time::timeout(Duration::from_millis(30), &mut pump)
        .await
        .is_err());
    shutdown.send_replace(true);
    assert!(tokio::time::timeout(Duration::from_secs(1), pump)
        .await
        .expect("blocked stream write must be cancellable")
        .is_none());
}

/// `Server::run` must wait for in-flight handlers to finish before
/// returning. Otherwise, an ingest landing during shutdown could
/// call `Store::apply` *after* the snapshotter's final flush, losing
/// the event on next restart.
///
/// We exercise this by piping a slow request through a handler:
/// fire shutdown while the handler is mid-read, then verify
/// `server.run` returns only after the handler has finished applying
/// its event (visible in the store snapshot).
#[tokio::test]
async fn shutdown_drains_in_flight_handlers_before_returning() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-drain.sock");
    let store = Store::shared();
    let server = Server::new(sock.clone(), store.clone());
    let (tx, rx) = broadcast::channel(1);

    let server_handle = tokio::spawn(server.run(rx));

    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Open a raw stream and write the request *header* but withhold
    // the trailing newline so the handler is stuck inside
    // `read_line`. This simulates an in-flight handler at the moment
    // shutdown lands.
    let mut stream = Stream::connect(&sock).await.unwrap();
    let req = serde_json::json!({
        "protocol": PROTOCOL_VERSION,
        "kind": "ingest",
        "event": {
            "type": "started",
            "id": {
                "kind": "claude_code",
                "session_id": "drain-test",
                "pane": "%9",
                "cwd": null,
            },
            "at": "2026-04-28T00:00:00Z",
        },
    });
    let bytes = serde_json::to_vec(&req).unwrap();
    // Note: no trailing '\n' yet.
    stream.write_all(&bytes).await.unwrap();
    stream.flush().await.unwrap();

    // Yield to give the spawned handler a chance to enter `read_line`.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;

    // Fire shutdown. Server stops accepting; existing handler is
    // still blocked on its read.
    tx.send(()).unwrap();

    // Finish the admitted request after shutdown. The handler must apply
    // it and close the connection itself rather than waiting for a new
    // request; the client deliberately remains open while reading EOF.
    stream.write_all(b"\n").await.unwrap();
    stream.flush().await.unwrap();
    // Read the single response so we know the apply landed before
    // we drop the stream — this also gives the handler enough time
    // to write its reply.
    let mut response_buf = Vec::new();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        tokio::io::AsyncReadExt::read_to_end(&mut stream, &mut response_buf),
    )
    .await;
    drop(stream);

    // `server.run` must wait for the handler to finish before
    // returning. The bounded timeout here is the test's deadline,
    // not the production drain timeout — we expect this to complete
    // in milliseconds.
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), server_handle)
        .await
        .expect("server.run did not return after handler finished")
        .expect("server task panicked");
    outcome.expect("server.run returned an error");

    // The drained handler must have applied its event before
    // server.run returned. If we'd returned without waiting, the
    // store could be empty and we'd race the assertion.
    let snap = store.snapshot().await;
    assert_eq!(snap.len(), 1, "drained handler must have applied event");
    assert_eq!(snap[0].session_id, "drain-test");
}

#[tokio::test]
async fn client_disconnect_before_response_is_clean_handler_exit() {
    let (server_stream, mut client_stream) = Stream::pair().await.unwrap();
    let store = Store::shared();
    let (_stopping_tx, stopping_rx) = watch::channel(false);
    let ctx = crate::ipc::dispatch::ServerCtx {
        store,
        backend: default_backend(),
        backends: vec![default_backend()],
        sessions: PtySessionBackend::shared(),
        collaboration: CollaborationStore::in_memory(CollaborationOptions::default()),
        collaboration_audit: CollaborationAuditLog::in_memory(),
        ask: crate::ask::AskStore::in_memory(crate::ask::AskOptions::default()),
        agent_directory: Arc::default(),
        automation: AutomationStore::in_memory(crate::automation::AutomationConfig::default()),
        restart: None,
        fleet: None,
        pipeline_runs: PipelineRunStore::in_memory(),
        reviews: crate::review::ReviewStore::in_memory(),
        work_store: Arc::new(tokio::sync::Mutex::new(
            crate::dashboard::work_store::WorkStore::memory(),
        )),
        work_up: WorkUpManager::new(PathBuf::from("/tmp/tend-disconnect-test.sock")),
        mux_snapshots: crate::mux_snapshot_control::MuxSnapshotControl::new(PathBuf::from(
            "/tmp/tend-disconnect-test.sock",
        )),
        config_path: None,
        swap: None,
    };
    let handle = tokio::spawn(handle(server_stream, ctx, stopping_rx));

    let req = serde_json::json!({
        "protocol": PROTOCOL_VERSION,
        "kind": "snapshot",
    });
    let mut bytes = serde_json::to_vec(&req).unwrap();
    bytes.push(b'\n');
    client_stream.write_all(&bytes).await.unwrap();
    client_stream.flush().await.unwrap();
    drop(client_stream);

    let outcome = tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .expect("handler should exit promptly")
        .expect("handler task panicked");
    outcome.expect("client disconnect should not be treated as a handler failure");
}

#[tokio::test]
async fn shutdown_closes_idle_connections_and_all_local_subscriptions_promptly() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("shutdown-streams.sock");
    let server = Server::new(socket.clone(), Store::shared());
    let (stop, receiver) = broadcast::channel(1);
    let task = tokio::spawn(server.run(receiver));
    wait_for_socket(&socket).await;
    let mut connections = Vec::new();
    for kind in [
        "subscribe",
        "subscribe_agent_changes",
        "pipeline_subscribe",
        "ask_subscribe",
        "collaboration_subscribe",
    ] {
        let stream = Stream::connect(&socket).await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut bytes =
            serde_json::to_vec(&serde_json::json!({"protocol": PROTOCOL_VERSION, "kind": kind}))
                .unwrap();
        bytes.push(b'\n');
        reader.get_mut().write_all(&bytes).await.unwrap();
        let mut ack = String::new();
        reader.read_line(&mut ack).await.unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&ack).unwrap()["ok"],
            true,
            "{kind}"
        );
        connections.push(reader);
    }
    // A persistent client has completed its request and is now idle.
    let mut idle = BufReader::new(Stream::connect(&socket).await.unwrap());
    idle.get_mut()
        .write_all(
            format!("{{\"protocol\":{PROTOCOL_VERSION},\"kind\":\"snapshot\"}}\n").as_bytes(),
        )
        .await
        .unwrap();
    let mut ack = String::new();
    idle.read_line(&mut ack).await.unwrap();
    connections.push(idle);
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("subscriptions must close without waiting for the five-second drain timeout")
        .unwrap()
        .unwrap();
    for mut reader in connections {
        let mut line = String::new();
        assert_eq!(reader.read_line(&mut line).await.unwrap(), 0);
    }
}

#[tokio::test]
async fn shutdown_abandons_an_in_flight_session_wait() {
    // `read_session_wait` parks a `spawn_blocking` thread for as long as
    // the client asked for — up to 30s. `spawn_blocking` cannot be
    // cancelled and `JoinSet::abort_all` only drops the join handle, so a
    // handler that kept awaiting it held the drain for the full
    // `HANDLER_DRAIN_TIMEOUT` on the way out. The macOS app keeps one of
    // these open per live terminal, which turned every tendd restart into
    // a five-second blackout: long enough that the app's own three-second
    // socket timeout fired first and reported the restart as a failure.
    if !std::path::Path::new("/bin/sh").exists() {
        return;
    }
    let dir = tempdir().unwrap();
    let socket = dir.path().join("shutdown-session-wait.sock");
    let sessions = PtySessionBackend::shared();
    // Silent and long-lived, so the wait can only end by being abandoned.
    let session = sessions
        .spawn_session(SpawnSession {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            env: Vec::new(),
            cwd: None,
            name: None,
            cols: Some(80),
            rows: Some(24),
        })
        .unwrap();
    let server = Server::new(socket.clone(), Store::shared()).with_sessions(sessions.clone());
    let (stop, receiver) = broadcast::channel(1);
    let task = tokio::spawn(server.run(receiver));
    wait_for_socket(&socket).await;

    let mut waiting = BufReader::new(Stream::connect(&socket).await.unwrap());
    let mut bytes = serde_json::to_vec(&serde_json::json!({
        "protocol": PROTOCOL_VERSION,
        "kind": "read_session_wait",
        "session_id": session.id,
        "offset": 0,
        "timeout_ms": 20_000,
    }))
    .unwrap();
    bytes.push(b'\n');
    waiting.get_mut().write_all(&bytes).await.unwrap();
    // Let the handler reach the blocking wait before the signal lands.
    tokio::time::sleep(Duration::from_millis(150)).await;

    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("a session wait must not hold the drain for its client's timeout")
        .unwrap()
        .unwrap();
    let mut line = String::new();
    assert_eq!(
        waiting.read_line(&mut line).await.unwrap(),
        0,
        "the abandoned wait must close the connection, not answer it"
    );
    let _ = sessions.terminate(&session.id);
}

#[cfg(unix)]
#[tokio::test]
async fn a_second_server_leaves_a_running_daemons_socket_alone() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-claimed.sock");
    let server = Server::new(sock.clone(), Store::shared());
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;
    let bound = transport::endpoint_identity(&sock);

    // However the probe reads the first daemon — a full backlog refuses
    // like an orphan on macOS — its claim keeps the second one out.
    let (_tx2, rx2) = broadcast::channel(1);
    let second = Server::new(sock.clone(), Store::shared()).run(rx2).await;
    assert!(matches!(second, Err(RuntimeError::SocketInUse(_))));
    assert_eq!(transport::endpoint_identity(&sock), bound);
    Client::new(sock.clone())
        .hello(Duration::from_secs(2))
        .await
        .unwrap();

    tx.send(()).unwrap();
    handle.await.unwrap();
    assert!(!sock.exists(), "the daemon removes the socket it bound");
    assert!(
        !dir.path().join("tend-claimed.sock.lock").exists(),
        "and its claim"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_removed_socket_file_is_bound_again() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-unlinked.sock");
    let server =
        Server::new(sock.clone(), Store::shared()).with_endpoint_check(Duration::from_millis(20));
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    std::fs::remove_file(&sock).unwrap();
    wait_for_socket(&sock).await;
    Client::new(sock.clone())
        .hello(Duration::from_secs(2))
        .await
        .unwrap();

    // A file someone else put at the path is not the daemon's to delete.
    tx.send(()).unwrap();
    handle.await.unwrap();
    let server =
        Server::new(sock.clone(), Store::shared()).with_endpoint_check(Duration::from_secs(60));
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;
    std::fs::remove_file(&sock).unwrap();
    std::fs::write(&sock, b"not a socket").unwrap();
    tx.send(()).unwrap();
    handle.await.unwrap();
    assert_eq!(std::fs::read(&sock).unwrap(), b"not a socket");
}

#[tokio::test]
async fn handler_budget_is_reserved_before_accepting_connections() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-budget.sock");
    let store = Store::shared();
    let server = Server::new(sock.clone(), store).with_handler_limit(1);
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let mut holder = Stream::connect(&sock).await.unwrap();
    holder.write_all(b"{").await.unwrap();
    holder.flush().await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let mut second = Stream::connect(&sock).await.unwrap();
    let req = serde_json::json!({
        "protocol": PROTOCOL_VERSION,
        "kind": "snapshot",
    });
    let mut bytes = serde_json::to_vec(&req).unwrap();
    bytes.push(b'\n');
    second.write_all(&bytes).await.unwrap();
    second.flush().await.unwrap();
    let mut reader = BufReader::new(second);
    let mut line = String::new();

    let early = tokio::time::timeout(
        std::time::Duration::from_millis(100),
        reader.read_line(&mut line),
    )
    .await;
    assert!(
        early.is_err(),
        "server accepted a connection while no handler permit was available",
    );

    drop(holder);
    line.clear();
    let n = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        reader.read_line(&mut line),
    )
    .await
    .expect("queued connection should be served after permit is released")
    .expect("read response");
    assert!(n > 0, "queued connection closed without a response");
    let resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(resp["ok"], true);

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn worker_spawn_release_is_console_scoped_and_audits_owner_and_operator_attempts() {
    use collaboration::{
        WorkerSpawnAcquisition, WorkerSpawnAction, WorkerSpawnPhase, WorkerSpawnSpec,
    };
    let dir = tempdir().unwrap();
    let socket = dir.path().join("worker-release.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "owner", AgentKind::ClaudeCode).await;
    add_collaboration_agent(&store, "%2", "other", AgentKind::ClaudeCode).await;
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    let audit = CollaborationAuditLog::in_memory();
    let backend: SharedBackend = Arc::new(CollaborationTestBackend {
        panes: vec![
            collaboration_test_pane("%1", "0"),
            collaboration_test_pane("%2", "1"),
        ],
    });
    let server = Server::new(socket.clone(), store)
        .with_backends(vec![backend])
        .with_collaboration(mailbox)
        .with_collaboration_audit(audit.clone());
    let (stop, rx) = broadcast::channel(1);
    let serving = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&socket).await;
    let client = Client::new(socket);
    let spec = WorkerSpawnSpec {
        agent_kind: AgentKind::Codex,
        role: None,
        launch_role: "reviewer".into(),
    };
    let mut console = pane_origin("%2");
    console.console = true;
    for owner_release in [true, false] {
        let WorkerSpawnAcquisition::LaunchToken { ticket } = client
            .collaboration_worker_acquire(&pane_origin("%1"), &spec)
            .await
            .unwrap()
        else {
            panic!("capacity not released")
        };
        client
            .collaboration_worker_spawn(&ticket, &WorkerSpawnAction::Begin)
            .await
            .unwrap();
        client
            .collaboration_worker_spawn(
                &ticket,
                &WorkerSpawnAction::Uncertain {
                    detail: "create result unknown".into(),
                },
            )
            .await
            .unwrap();
        assert!(client
            .collaboration_worker_spawn_release(
                &pane_origin("%2"),
                &ticket.launch_id,
                "not a console"
            )
            .await
            .is_err());
        if owner_release {
            client
                .collaboration_worker_spawn(
                    &ticket,
                    &WorkerSpawnAction::Release {
                        reason: "owner verified no effects and stopped launcher".into(),
                    },
                )
                .await
                .unwrap();
        } else {
            client
                .collaboration_worker_spawn_release(
                    &console,
                    &ticket.launch_id,
                    "operator verified no effects and stopped launcher",
                )
                .await
                .unwrap();
        }
        assert!(client
            .collaboration_worker_spawns()
            .await
            .unwrap()
            .iter()
            .any(|r| r.launch_id == ticket.launch_id && r.phase == WorkerSpawnPhase::Failed));
    }
    let entries = audit.entries().await;
    let releases: Vec<_> = entries
        .iter()
        .filter(|e| e.operation == CollaborationAuditOperation::WorkerSpawnRelease)
        .collect();
    assert_eq!(releases.len(), 4);
    assert_eq!(releases.iter().filter(|e| e.ok).count(), 2);
    assert!(releases
        .iter()
        .all(|e| e.target.is_some() && e.release_reason.is_some()));
    stop.send(()).unwrap();
    serving.await.unwrap();
}

#[tokio::test]
async fn worker_capacity_is_shared_by_independent_ipc_clients() {
    use collaboration::{WorkerSpawnAcquisition, WorkerSpawnAction, WorkerSpawnSpec};
    let dir = tempdir().unwrap();
    let socket = dir.path().join("worker-capacity.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "first-caller", AgentKind::ClaudeCode).await;
    add_collaboration_agent(&store, "%2", "second-caller", AgentKind::ClaudeCode).await;
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    let (stop, serving) = wait_test_server(&socket, &store, &mailbox).await;
    // Distinct clients/connections model two separate MCP server processes;
    // neither can rely on a local mutex or the other's cached room view.
    let first = Client::new(socket.clone());
    let second = Client::new(socket);
    assert!(first
        .hello(Duration::from_secs(2))
        .await
        .unwrap()
        .capabilities
        .iter()
        .any(|c| c == collaboration::WORKER_SPAWN_CAPABILITY));
    let provider = WorkerSpawnSpec {
        agent_kind: AgentKind::Codex,
        role: None,
        launch_role: "reviewer".into(),
    };
    let role = WorkerSpawnSpec {
        role: Some("reviewer".into()),
        ..provider.clone()
    };
    let first_origin = pane_origin("%1");
    let mut second_origin = pane_origin("%2");
    second_origin.socket = Some("/tmp/tmux-1000/default".into());
    let (a, b) = tokio::join!(
        first.collaboration_worker_acquire(&first_origin, &provider),
        second.collaboration_worker_acquire(&second_origin, &role),
    );
    let outcomes = [a.unwrap(), b.unwrap()];
    let tickets = outcomes
        .iter()
        .filter_map(|o| match o {
            WorkerSpawnAcquisition::LaunchToken { ticket } => Some(ticket),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tickets.len(), 1);
    let pending = outcomes
        .iter()
        .find_map(|o| match o {
            WorkerSpawnAcquisition::Pending { launch_id, .. } => Some(launch_id),
            _ => None,
        })
        .unwrap();
    assert_eq!(pending, &tickets[0].launch_id);
    first
        .collaboration_worker_spawn(tickets[0], &WorkerSpawnAction::Begin)
        .await
        .unwrap();
    assert!(second
        .collaboration_worker_spawn(tickets[0], &WorkerSpawnAction::Begin)
        .await
        .is_err());
    first
        .collaboration_worker_spawn(tickets[0], &WorkerSpawnAction::Renew)
        .await
        .unwrap();
    stop.send(()).unwrap();
    serving.await.unwrap();
}

#[tokio::test]
async fn the_worker_inventory_lists_only_sessions_that_are_workers() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("workers.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "primary", AgentKind::ClaudeCode).await;
    add_collaboration_agent(&store, "%2", "worker", AgentKind::Codex).await;
    let mailbox = collaboration::CollaborationStore::in_memory(
        collaboration::CollaborationOptions::default(),
    );
    let (stop, serving) = wait_test_server(&socket, &store, &mailbox).await;
    let client = Client::new(socket.clone());
    assert!(client
        .collaboration_workers(3600)
        .await
        .unwrap()
        .workers
        .is_empty());

    client
        .collaboration_purpose(&pane_origin("%2"), collaboration::LaunchPurpose::Worker)
        .await
        .unwrap();
    client
        .collaboration_send(
            &pane_origin("%1"),
            "pane:%2",
            &NewRequest {
                body: "review".into(),
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();
    let inventory = client.collaboration_workers(3600).await.unwrap();
    assert_eq!(inventory.workers.len(), 1, "{inventory:?}");
    let worker = &inventory.workers[0];
    assert_eq!(worker.agent_session_id, "worker");
    assert_eq!(worker.queued_requests, 1);
    assert_eq!(inventory.idle_threshold_secs, 3600);
    stop.send(()).unwrap();
    serving.await.unwrap();
}

#[tokio::test]
async fn a_wait_cut_short_by_a_restart_is_answered_and_retried_until_the_reply() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("wait-restart.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_collaboration_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
    let mailbox = collaboration::CollaborationStore::in_memory(
        collaboration::CollaborationOptions::default(),
    );
    let (stop, serving) = wait_test_server(&socket, &store, &mailbox).await;
    let client = Client::new(socket.clone());
    let request = client
        .collaboration_send(
            &pane_origin("%1"),
            "pane:%2",
            &NewRequest {
                task_contract: None,
                supersedes_request_id: None,
                body: "review".into(),
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();
    client
        .collaboration_claim(&pane_origin("%2"), &request.id)
        .await
        .unwrap();

    let waiting = client.clone();
    let id = request.id.clone();
    let waiter = tokio::spawn(async move {
        waiting
            .collaboration_wait(&pane_origin("%1"), &id, 20)
            .await
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The interrupted wait is answered, so shutdown does not have to burn
    // the drain budget aborting it.
    let stopped = std::time::Instant::now();
    stop.send(()).unwrap();
    serving.await.unwrap();
    assert!(
        stopped.elapsed() < HANDLER_DRAIN_TIMEOUT,
        "shutdown waited {:?} on the wait handler",
        stopped.elapsed()
    );
    assert!(!waiter.is_finished(), "the wait must ride out the restart");

    let (stop, serving) = wait_test_server(&socket, &store, &mailbox).await;
    let recipient: collaboration::Participant =
        serde_json::from_value(serde_json::to_value(&request.to).unwrap()).unwrap();
    mailbox
        .reply(
            &recipient,
            &request.id,
            RequestStatus::Completed,
            "real reply".into(),
            vec![],
        )
        .await
        .unwrap();
    let observed = tokio::time::timeout(Duration::from_secs(10), waiter)
        .await
        .expect("the retried wait sees the reply")
        .unwrap()
        .expect("the wait succeeds after the restart");
    assert_eq!(observed.status, RequestStatus::Completed);
    assert_eq!(observed.reply.unwrap().body, "real reply");
    stop.send(()).unwrap();
    serving.await.unwrap();
}

#[tokio::test]
async fn retries_stop_at_the_original_deadline_and_an_unreached_daemon_fails_at_once() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("wait-deadline.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_collaboration_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
    let mailbox = collaboration::CollaborationStore::in_memory(
        collaboration::CollaborationOptions::default(),
    );
    let (stop, serving) = wait_test_server(&socket, &store, &mailbox).await;
    let client = Client::new(socket.clone());
    let request = client
        .collaboration_send(
            &pane_origin("%1"),
            "pane:%2",
            &NewRequest {
                task_contract: None,
                supersedes_request_id: None,
                body: "review".into(),
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();
    let waiting = client.clone();
    let id = request.id.clone();
    let started = std::time::Instant::now();
    let waiter =
        tokio::spawn(async move { waiting.collaboration_wait(&pane_origin("%1"), &id, 2).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    stop.send(()).unwrap();
    serving.await.unwrap();
    // The daemon never comes back: the wait gives up near its own
    // deadline instead of retrying forever or resetting the budget.
    let outcome = waiter.await.unwrap();
    assert!(outcome.is_err(), "{outcome:?}");
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "gave up after {:?}",
        started.elapsed()
    );

    // A daemon this wait never reached is down, not restarting.
    let started = std::time::Instant::now();
    let missing = Client::new(dir.path().join("missing.sock"));
    let error = missing
        .collaboration_wait(&pane_origin("%1"), &request.id, 30)
        .await
        .unwrap_err();
    assert!(matches!(error, RuntimeError::NotConnected(_)), "{error}");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[cfg(unix)]
#[tokio::test]
async fn a_wait_retries_when_the_daemon_closes_right_after_hello() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("closing-after-hello.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let accepted = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&accepted);
    let closing = tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            // Answer hello, then stop without reading the request: the
            // client's write of the wait itself is what fails.
            let _ = writer.write_all(b"{\"ok\":true}\n").await;
        }
    });
    let started = std::time::Instant::now();
    let error = Client::new(socket)
        .collaboration_wait(&pane_origin("%1"), "req", 2)
        .await
        .unwrap_err();
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "gave up after {:?} on {error}",
        started.elapsed()
    );
    assert!(accepted.load(std::sync::atomic::Ordering::SeqCst) >= 2);
    closing.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn a_connection_closed_without_an_answer_is_disconnected_not_bad_json() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("closing.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let closing = tokio::spawn(async move {
        for answer_hello in [false, true] {
            let (stream, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            if answer_hello {
                writer.write_all(b"{\"ok\":true}\n").await.unwrap();
                line.clear();
                reader.read_line(&mut line).await.unwrap();
            }
            // Close without answering.
        }
    });
    let client = Client::new(socket);
    let request = serde_json::json!({"protocol": PROTOCOL_VERSION, "kind": "status"});
    for _ in 0..2 {
        let error = client.call(&request).await.unwrap_err();
        assert!(matches!(error, RuntimeError::Disconnected), "{error}");
    }
    closing.await.unwrap();
}

/// Legacy strict-match path: a client that never sends `hello` and
/// pins a mismatched `protocol` on a snapshot request gets the
/// "protocol mismatch" error. Negotiation is opt-in; pre-`hello`
/// connections keep the old behaviour.
#[tokio::test]
async fn rejects_wrong_protocol_without_hello() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-test.sock");
    let store = Store::shared();
    let server = Server::new(sock.clone(), store);
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let resp = raw_call(
        &sock,
        &serde_json::json!({ "protocol": 999, "kind": "snapshot" }),
    )
    .await;
    assert_eq!(resp["ok"], false);
    assert!(resp["error"]
        .as_str()
        .unwrap()
        .contains("protocol mismatch"));

    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// `hello` returns the supported protocol range and the capability
/// tag list, and echoes the requested `protocol` back in the
/// response envelope.
#[tokio::test]
async fn hello_returns_capabilities_and_negotiated_protocol() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-hello.sock");
    let store = Store::shared();
    let server = Server::new(sock.clone(), store);
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let resp = raw_call(
        &sock,
        &serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "hello",
            "client": "tend-test/0.0.0",
        }),
    )
    .await;
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["protocol"], i64::from(PROTOCOL_VERSION));
    assert_eq!(resp["min_protocol"], i64::from(MIN_PROTOCOL_VERSION));
    assert_eq!(resp["max_protocol"], i64::from(PROTOCOL_VERSION));
    let caps: Vec<&str> = resp["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(caps.contains(&"waiting_choice"));
    assert!(caps.contains(&"needs_choice"));
    assert!(caps.contains(&"rate_limited"));
    assert!(caps.contains(&"collaboration_wait"));
    assert!(caps.contains(&"fleet_raw_capture_v1"));
    assert!(caps.contains(&"session_bytes_v1"));
    assert!(caps.contains(&"work_control_v1"));
    assert!(caps.contains(&"session_attachment_identity_v1"));
    assert!(caps.contains(&"session_wait_v1"));
    assert!(caps.contains(&"collaboration_subscribe"));
    assert!(caps.contains(&"pipeline_subscribe"));
    assert!(caps.contains(&"ask_subscribe"));
    assert!(caps.contains(&"ask_one_turn_credential_v1"));
    assert!(caps.contains(&"ask_status_v1"));
    assert!(caps.contains(&"ask_conversations_v1"));
    assert!(caps.contains(&"ask_send_new_v1"));
    assert!(caps.contains(&"ask_providers_v1"));
    assert!(caps.contains(&"ask_provider_default_v1"));
    assert!(caps.contains(&"work_compose_v1"));
    assert!(caps.contains(&"ask_one_shot_v1"));
    assert!(!caps.contains(&RESTART_CAPABILITY));
    assert!(!caps.contains(&STOP_CAPABILITY));
    assert!(resp["generation"].is_null());

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swap_requests_go_to_the_service_off_the_reactor() {
    let swap = FakeSwap::new(true);
    let (_dir, client, tx, handle) =
        swap_server(Some(Arc::clone(&swap) as Arc<dyn crate::swap_api::SwapJobs>)).await;
    assert!(advertises_swap(
        &client.hello(Duration::from_secs(2)).await.unwrap()
    ));

    let overrides = crate::swap_api::SwapOverrides::default();
    let first = client.swap_job_start("key-1", &overrides).await.unwrap();
    assert!(!first.existing);
    // A lost acknowledgement: the same key again, or a status by key,
    // finds the same job.
    let again = client.swap_job_start("key-1", &overrides).await.unwrap();
    assert_eq!(
        (again.job_id.as_str(), again.existing),
        (first.job_id.as_str(), true)
    );
    let by_key = client
        .swap_job_status(&crate::swap_api::JobRef::Key("key-1".into()))
        .await
        .unwrap();
    assert_eq!(by_key.job_id, first.job_id);
    assert!(client.swap_plan(&overrides).await.unwrap().executable);

    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// A halted service is not advertised and takes no job, yet its jobs
/// stay readable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_halted_swap_service_still_answers_status() {
    let swap = FakeSwap::new(true);
    let (_dir, client, tx, handle) =
        swap_server(Some(Arc::clone(&swap) as Arc<dyn crate::swap_api::SwapJobs>)).await;
    let overrides = crate::swap_api::SwapOverrides::default();
    let accepted = client.swap_job_start("key", &overrides).await.unwrap();
    swap.healthy
        .store(false, std::sync::atomic::Ordering::SeqCst);

    let hello = client.hello(Duration::from_secs(2)).await.unwrap();
    assert!(!advertises_swap(&hello));
    // A halted service can still be told to stop a job.
    assert!(hello
        .capabilities
        .iter()
        .any(|c| c == SWAP_CANCEL_CAPABILITY));
    assert!(client
        .swap_job_cancel(&crate::swap_api::JobRef::Id(accepted.job_id.clone()))
        .await
        .is_ok());
    assert!(client.swap_job_start("next", &overrides).await.is_err());
    let status = client
        .swap_job_status(&crate::swap_api::JobRef::Id(accepted.job_id.clone()))
        .await
        .unwrap();
    assert_eq!(status.job_id, accepted.job_id);
    let plan = client.swap_plan(&overrides).await.unwrap();
    assert!(!plan.executable);
    assert!(plan.unavailable.is_some());

    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// Health answers in every state: ready, halted, a service that never
/// opened (its reason, and no cancel offered), none at all, and a check
/// too slow to wait for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swap_health_answers_in_every_state() {
    let swap = FakeSwap::new(true);
    let (_dir, client, tx, handle) =
        swap_server(Some(Arc::clone(&swap) as Arc<dyn crate::swap_api::SwapJobs>)).await;
    let hello = client.hello(Duration::from_secs(2)).await.unwrap();
    assert!(hello
        .capabilities
        .iter()
        .any(|c| c == SWAP_HEALTH_CAPABILITY));
    let ready = client.swap_health().await.unwrap();
    assert_eq!(ready.readiness, crate::swap_api::SwapReadiness::Ready);
    swap.healthy
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let halted = client.swap_health().await.unwrap();
    assert_eq!(halted.readiness, crate::swap_api::SwapReadiness::Halted);
    tx.send(()).unwrap();
    handle.await.unwrap();

    let unopened: Arc<dyn crate::swap_api::SwapJobs> = Arc::new(
        crate::swap_api::UnopenedSwap::new("schema 9, this build reads 2".into(), 2),
    );
    let (_dir, client, tx, handle) = swap_server(Some(unopened)).await;
    let hello = client.hello(Duration::from_secs(2)).await.unwrap();
    assert!(hello
        .capabilities
        .iter()
        .any(|c| c == SWAP_HEALTH_CAPABILITY));
    assert!(!hello
        .capabilities
        .iter()
        .any(|c| c == SWAP_CANCEL_CAPABILITY));
    let health = client.swap_health().await.unwrap();
    assert_eq!(
        health.readiness,
        crate::swap_api::SwapReadiness::Unavailable
    );
    assert!(health.reason.unwrap().contains("schema 9"));
    tx.send(()).unwrap();
    handle.await.unwrap();

    let (_dir, client, tx, handle) = swap_server(None).await;
    assert!(client.swap_health().await.is_err());
    tx.send(()).unwrap();
    handle.await.unwrap();

    let mut slow = FakeSwap::new(true);
    Arc::get_mut(&mut slow).unwrap().slow_health = true;
    let (_dir, client, tx, handle) =
        swap_server(Some(slow as Arc<dyn crate::swap_api::SwapJobs>)).await;
    let started = std::time::Instant::now();
    let error = client.swap_health().await.unwrap_err().to_string();
    assert!(error.contains("did not answer"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(3));
    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// No service: nothing advertised, every swap request refused — no
/// other path is taken.
#[tokio::test]
async fn without_a_swap_service_every_swap_request_is_refused() {
    let (_dir, client, tx, handle) = swap_server(None).await;
    let hello = client.hello(Duration::from_secs(2)).await.unwrap();
    assert!(!advertises_swap(&hello));
    assert!(!hello
        .capabilities
        .iter()
        .any(|c| c == SWAP_CANCEL_CAPABILITY));
    let overrides = crate::swap_api::SwapOverrides::default();
    let refused = client.swap_job_start("key", &overrides).await.unwrap_err();
    assert!(refused.to_string().contains("unavailable"), "{refused}");
    assert!(client
        .swap_job_status(&crate::swap_api::JobRef::Key("key".into()))
        .await
        .is_err());
    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// A health check that cannot answer in time is not advertised, and
/// `hello` does not wait for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_slow_swap_health_check_is_not_advertised() {
    let swap = Arc::new(FakeSwap {
        healthy: std::sync::atomic::AtomicBool::new(true),
        slow_health: true,
        jobs: std::sync::Mutex::new(Vec::new()),
    });
    let (_dir, client, tx, handle) =
        swap_server(Some(swap as Arc<dyn crate::swap_api::SwapJobs>)).await;
    let started = std::time::Instant::now();
    let hello = client.hello(Duration::from_secs(5)).await.unwrap();
    assert!(!advertises_swap(&hello));
    assert!(started.elapsed() < Duration::from_millis(1500));
    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn ask_subscription_wakes_on_store_revision_without_polling() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-ask-subscribe.sock");
    let ask = crate::ask::AskStore::in_memory(crate::ask::AskOptions::default());
    let server = Server::new(sock.clone(), Store::shared()).with_ask(Arc::clone(&ask));
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let mut updates = Client::new(sock.clone()).ask_subscribe().await.unwrap();
    let next_agent = if ask.agent().await == "claude" {
        "codex"
    } else {
        "claude"
    };
    ask.set_agent(next_agent).await.unwrap();
    let revision = tokio::time::timeout(Duration::from_secs(1), updates.recv())
        .await
        .expect("ask revision should be pushed")
        .unwrap()
        .expect("stream should remain open");
    assert_eq!(revision, 1);

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn ask_status_reports_the_explicit_runtime_grant() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-ask-status.sock");
    let store = Store::shared();
    let server = Server::new(sock.clone(), store);
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let client = Client::new(sock);
    assert!(!client.ask_status().await.unwrap());

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn ask_conversations_can_be_created_listed_and_reselected_over_ipc() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-ask-conversations.sock");
    let store = Store::shared();
    let server = Server::new(sock.clone(), store);
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let client = Client::new(sock);
    let first = client.ask_reset().await.unwrap();
    let second = client.ask_reset().await.unwrap();
    assert_ne!(first.id, second.id);

    let (conversations, active) = client.ask_conversation_list().await.unwrap();
    assert_eq!(conversations.len(), 2);
    assert_eq!(active.unwrap().id, second.id);

    let selected = client.ask_conversation_select(&first.id).await.unwrap();
    assert_eq!(selected.id, first.id);
    let (_, active) = client.ask_conversation_list().await.unwrap();
    assert_eq!(active.unwrap().id, first.id);

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn ask_send_new_creates_the_conversation_with_its_first_turn() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-ask-send-new.sock");
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "claude".into(),
        crate::config::AskProviderConfig {
            executable: Some("/definitely/missing/tend-test-agent".into()),
            ..crate::config::AskProviderConfig::default()
        },
    );
    let ask = crate::ask::AskStore::in_memory(crate::ask::AskOptions {
        enabled: true,
        providers,
        ..crate::ask::AskOptions::default()
    });
    let server = Server::new(sock.clone(), Store::shared()).with_ask(ask);
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let client = Client::new(sock);
    let entry = client.ask_send_new("independent question").await.unwrap();
    let (conversations, active) = client.ask_conversation_list().await.unwrap();

    assert_eq!(conversations.len(), 1);
    assert_eq!(entry.conversation_id, active.map(|item| item.id));

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[test]
fn automation_requests_deserialize_from_their_documented_shapes() {
    let list: RequestBody =
        serde_json::from_value(serde_json::json!({"kind": "automation_list"})).unwrap();
    assert!(matches!(list, RequestBody::AutomationList {}));

    let log: RequestBody =
        serde_json::from_value(serde_json::json!({"kind": "automation_log", "limit": 20})).unwrap();
    assert!(matches!(
        log,
        RequestBody::AutomationLog { limit: Some(20) }
    ));
    // `limit` is optional; absent means "the whole retained ledger".
    let log: RequestBody =
        serde_json::from_value(serde_json::json!({"kind": "automation_log"})).unwrap();
    assert!(matches!(log, RequestBody::AutomationLog { limit: None }));

    let toggle: RequestBody = serde_json::from_value(serde_json::json!({
        "kind": "automation_set_enabled",
        "name": "resume-after-limit",
        "enabled": false,
    }))
    .unwrap();
    match toggle {
        RequestBody::AutomationSetEnabled { name, enabled } => {
            assert_eq!(name.as_deref(), Some("resume-after-limit"));
            assert!(!enabled);
        }
        other => panic!("unexpected {other:?}"),
    }

    let pause: RequestBody = serde_json::from_value(serde_json::json!({
        "kind": "automation_pause",
        "until": "2026-09-03T13:00:00Z",
    }))
    .unwrap();
    match pause {
        RequestBody::AutomationPause { until } => assert_eq!(
            until,
            Some(time::macros::datetime!(2026-09-03 13:00:00 UTC))
        ),
        other => panic!("unexpected {other:?}"),
    }
    // `null` is how a client lifts the hold.
    let resume: RequestBody =
        serde_json::from_value(serde_json::json!({"kind": "automation_pause", "until": null}))
            .unwrap();
    assert!(matches!(
        resume,
        RequestBody::AutomationPause { until: None }
    ));

    let set: RequestBody = serde_json::from_value(serde_json::json!({
        "kind": "automation_set_rule",
        "rule": {
            "name": "resume-after-limit",
            "on": "rate_limited",
            "action": "send_prompt",
            "text": "continue",
            "wait": "reset+2m",
            "agent": ["claude", "codex"],
        },
    }))
    .unwrap();
    match set {
        RequestBody::AutomationSetRule { rule } => {
            assert_eq!(rule.name, "resume-after-limit");
            assert_eq!(rule.text.as_deref(), Some("continue"));
            assert_eq!(rule.agent.len(), 2);
            rule.validate().unwrap();
        }
        other => panic!("unexpected {other:?}"),
    }

    let remove: RequestBody = serde_json::from_value(
        serde_json::json!({"kind": "automation_remove_rule", "name": "resume-after-limit"}),
    )
    .unwrap();
    assert!(
        matches!(remove, RequestBody::AutomationRemoveRule { name } if name == "resume-after-limit")
    );

    let test: RequestBody = serde_json::from_value(
        serde_json::json!({"kind": "automation_test", "name": "resume-after-limit"}),
    )
    .unwrap();
    assert!(matches!(test, RequestBody::AutomationTest { name } if name == "resume-after-limit"));

    // Clients feature-gate on the tag, not the protocol number.
    assert!(CAPABILITIES.contains(&"automation_v1"));
}

#[tokio::test]
async fn automation_rules_can_be_written_toggled_and_removed_over_ipc() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-automation.sock");
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "# tend\n").unwrap();
    let automation = crate::automation::AutomationStore::new(
        crate::automation::AutomationConfig::default(),
        Some(config_path.clone()),
        crate::automation::AutomationLedger::in_memory(),
    );
    let server = Server::new(sock.clone(), Store::shared()).with_automation(automation);
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;
    let client = Client::new(sock);

    // A fresh install ships no rules.
    let rules = client.automation_list().await.unwrap();
    assert!(rules.enabled);
    assert!(rules.rules.is_empty());

    let mut rule = crate::automation::AutomationRule::new(
        "resume-after-limit",
        crate::automation::AutomationEvent::RateLimited,
        crate::automation::AutomationAction::SendPrompt,
    );
    rule.text = Some("continue".into());
    rule.wait = Some(crate::automation::parse_wait("reset+2m").unwrap());
    let rules = client.automation_set_rule(&rule).await.unwrap();
    assert_eq!(rules.rules.len(), 1);
    // The daemon rewrites the anchor into the template spelling every
    // other tend value uses.
    assert_eq!(rules.rules[0].wait, "{{reset}}+2m");
    assert!(std::fs::read_to_string(&config_path)
        .unwrap()
        .contains("[[automation.rule]]"));

    // Same name upserts rather than duplicating.
    rule.text = Some("keep going".into());
    let rules = client.automation_set_rule(&rule).await.unwrap();
    assert_eq!(rules.rules.len(), 1);
    assert_eq!(rules.rules[0].text.as_deref(), Some("keep going"));

    let rules = client
        .automation_set_enabled("resume-after-limit", false)
        .await
        .unwrap();
    assert!(!rules.rules[0].enabled);

    let until = time::OffsetDateTime::now_utc() + time::Duration::hours(1);
    let rules = client.automation_pause(Some(until)).await.unwrap();
    assert!(rules.paused_until.is_some());
    let rules = client.automation_pause(None).await.unwrap();
    assert!(rules.paused_until.is_none());

    // Nothing has fired, so the ledger is empty and `test` finds no
    // capped agent in an empty registry.
    assert!(client.automation_log(Some(10)).await.unwrap().is_empty());
    let report = client.automation_test("resume-after-limit").await.unwrap();
    assert!(report.candidates.is_empty());

    let rules = client
        .automation_remove_rule("resume-after-limit")
        .await
        .unwrap();
    assert!(rules.rules.is_empty());
    // An unknown name is refused rather than silently succeeding.
    assert!(client.automation_remove_rule("nope").await.is_err());

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[allow(clippy::too_many_lines)] // one documented wire shape per block
#[test]
fn provider_and_compose_requests_deserialize_from_their_documented_shapes() {
    let providers: RequestBody =
        serde_json::from_value(serde_json::json!({"kind": "ask_providers"})).unwrap();
    assert!(matches!(providers, RequestBody::AskProviders {}));

    let configure: RequestBody = serde_json::from_value(serde_json::json!({
        "kind": "ask_provider_configure",
        "provider": "anthropic",
        "model": "claude-opus-5",
        "api_key_env": null,
        "title": "Anthropic (work)",
        "executable": null,
    }))
    .unwrap();
    match configure {
        RequestBody::AskProviderConfigure {
            provider,
            title,
            model,
            api_key_env,
            executable,
        } => {
            assert_eq!(provider, "anthropic");
            // A string sets, `null` clears…
            assert_eq!(model, Some(Some("claude-opus-5".to_string())));
            assert_eq!(title, Some(Some("Anthropic (work)".to_string())));
            assert_eq!(api_key_env, Some(None));
            assert_eq!(executable, Some(None));
        }
        other => panic!("unexpected {other:?}"),
    }
    // …and an absent key means "leave it unchanged".
    let partial: RequestBody = serde_json::from_value(serde_json::json!({
        "kind": "ask_provider_configure",
        "provider": "openai",
        "model": "gpt-5-mini",
    }))
    .unwrap();
    assert!(matches!(
        partial,
        RequestBody::AskProviderConfigure {
            model: Some(Some(_)),
            title: None,
            api_key_env: None,
            executable: None,
            ..
        }
    ));

    // `add` carries the engine and whatever settings came with it…
    let add: RequestBody = serde_json::from_value(serde_json::json!({
        "kind": "ask_provider_add",
        "id": "anthropic-work",
        "engine": "anthropic",
        "title": "Anthropic (work)",
        "api_key_env": "WORK_ANTHROPIC_KEY",
    }))
    .unwrap();
    match add {
        RequestBody::AskProviderAdd {
            id,
            engine,
            title,
            model,
            api_key_env,
            executable,
        } => {
            assert_eq!(id, "anthropic-work");
            assert_eq!(engine, "anthropic");
            assert_eq!(title.as_deref(), Some("Anthropic (work)"));
            assert_eq!(model, None);
            assert_eq!(api_key_env.as_deref(), Some("WORK_ANTHROPIC_KEY"));
            assert_eq!(executable, None);
        }
        other => panic!("unexpected {other:?}"),
    }
    // …with only the id and engine required.
    let minimal: RequestBody = serde_json::from_value(serde_json::json!({
        "kind": "ask_provider_add",
        "id": "personal",
        "engine": "openai",
    }))
    .unwrap();
    assert!(matches!(
        minimal,
        RequestBody::AskProviderAdd {
            title: None,
            model: None,
            api_key_env: None,
            executable: None,
            ..
        }
    ));

    let remove: RequestBody = serde_json::from_value(serde_json::json!({
        "kind": "ask_provider_remove",
        "id": "anthropic-work",
    }))
    .unwrap();
    match remove {
        RequestBody::AskProviderRemove { id } => assert_eq!(id, "anthropic-work"),
        other => panic!("unexpected {other:?}"),
    }

    let default: RequestBody = serde_json::from_value(serde_json::json!({
        "kind": "ask_provider_default",
        "provider": "codex",
    }))
    .unwrap();
    match default {
        RequestBody::AskProviderDefault { provider } => assert_eq!(provider, "codex"),
        other => panic!("unexpected {other:?}"),
    }

    let compose: RequestBody = serde_json::from_value(serde_json::json!({
        "kind": "work_compose",
        "description": "implementer in claude, reviewer in codex after it",
        "agent": "claude",
        "current": {
            "name": "pair", "description": null, "layout": null, "prompt": null,
            "agents": [{"alias": "impl", "program": "claude", "role": null, "task": null,
                        "prompt": null, "direction": null, "after": []}],
        },
        "credential": {"agent": "claude", "api_key": "sk-one-turn"},
    }))
    .unwrap();
    match compose {
        RequestBody::WorkCompose {
            description,
            agent,
            current,
            credential,
        } => {
            assert_eq!(
                description,
                "implementer in claude, reviewer in codex after it"
            );
            assert_eq!(agent.as_deref(), Some("claude"));
            let current = current.unwrap();
            assert_eq!(current.name.as_deref(), Some("pair"));
            assert_eq!(current.agents[0].alias, "impl");
            let credential = credential.unwrap();
            assert_eq!(credential.agent, "claude");
            assert_eq!(credential.api_key, "sk-one-turn");
        }
        other => panic!("unexpected {other:?}"),
    }
    // Every optional field really is optional.
    let minimal: RequestBody = serde_json::from_value(serde_json::json!({
        "kind": "work_compose",
        "description": "solo claude",
    }))
    .unwrap();
    assert!(matches!(
        minimal,
        RequestBody::WorkCompose {
            agent: None,
            current: None,
            credential: None,
            ..
        }
    ));
}

#[test]
fn provider_and_compose_responses_serialize_with_their_documented_fields() {
    let response = Response::with_work_compose(WorkComposeOutput {
        pipeline: crate::work_pipeline_spec::PipelineSpec {
            name: Some("pair".into()),
            ..Default::default()
        },
        notes: "two agents".into(),
        raw: "raw".into(),
    });
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["work_compose"]["pipeline"]["name"], "pair");
    assert_eq!(value["work_compose"]["notes"], "two agents");
    assert_eq!(value["work_compose"]["raw"], "raw");
    let response = Response::with_ask_providers(crate::ask::provider_infos(
        &std::collections::BTreeMap::new(),
        "claude",
        |_| None,
    ));
    let value = serde_json::to_value(&response).unwrap();
    assert_eq!(value["ask_providers"][0]["id"], "claude");
    assert_eq!(value["ask_providers"][0]["kind"], "cli");
    assert_eq!(value["ask_providers"][0]["engine"], "claude");
    assert_eq!(value["ask_providers"][0]["builtin"], true);
    assert_eq!(value["ask_providers"][0]["configured"], false);
    assert_eq!(value["ask_providers"][3]["kind"], "api");
    assert!(value["ask_providers"][3]["credential_present"].is_boolean());
}

#[tokio::test]
async fn automation_set_enabled_without_a_name_flips_the_engine() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-automation-master.sock");
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[automation]\nenabled = true\n").unwrap();
    let automation = crate::automation::AutomationStore::new(
        crate::automation::AutomationConfig::default(),
        Some(config_path.clone()),
        crate::automation::AutomationLedger::in_memory(),
    );
    let server = Server::new(sock.clone(), Store::shared()).with_automation(automation);
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;
    let client = Client::new(sock);

    client
        .automation_set_enabled_target(None, false)
        .await
        .unwrap();

    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.contains("enabled = false"), "{text}");

    // A named target still means that one rule, and an unknown name is
    // refused rather than silently treated as the engine.
    let error = client
        .automation_set_enabled("no-such-rule", false)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("no automation rule"), "{error}");

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn automation_judge_test_rejects_ambiguous_panes_before_charging() {
    let store = Store::shared();
    add_collaboration_agent(&store, "%42", "first", AgentKind::ClaudeCode).await;
    add_collaboration_agent(&store, "%42", "second", AgentKind::Codex).await;
    let (backend, sends) = RecordingBackend::new(HostKind::Tmux, true);
    let automation = AutomationStore::in_memory(crate::automation::AutomationConfig::default());
    let ask = AskStore::in_memory(crate::ask::AskOptions::default());
    let rule = serde_json::from_value(serde_json::json!({
        "name":"judge", "on":"waiting_input", "action":"notify", "message":"ready",
        "ask_condition":{"prompt":"Ready?", "provider":"openai"}
    }))
    .unwrap();
    let error = automation_judge_test(
        &rule,
        "%42",
        &store,
        &[backend as SharedBackend],
        &automation,
        &ask,
    )
    .await
    .unwrap_err();
    assert!(error.to_lowercase().contains("ambiguous"));
    assert!(sends.lock().unwrap().is_empty());
    assert!(automation.ledger().all().await.is_empty());
}

#[tokio::test]
async fn automation_judge_test_is_action_free_and_bounded() {
    let directory = tempdir().unwrap();
    let socket = directory.path().join("judge.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%42", "session", AgentKind::ClaudeCode).await;
    let (backend, sends) = RecordingBackend::new(HostKind::Tmux, true);
    let automation = AutomationStore::in_memory(crate::automation::AutomationConfig::default());
    let ask = AskStore::in_memory(crate::ask::AskOptions {
        enabled: false,
        ..Default::default()
    });
    let server = Server::new(socket.clone(), store)
        .with_backends(vec![backend as SharedBackend])
        .with_automation(automation.clone())
        .with_ask(ask.clone());
    let (shutdown, receiver) = broadcast::channel(1);
    let task = tokio::spawn(async move { server.run(receiver).await.unwrap() });
    wait_for_socket(&socket).await;
    let client = Client::new(socket);
    let request = serde_json::json!({
        "protocol": PROTOCOL_VERSION, "kind":"automation_judge_test", "pane":"%42",
        "rule": {"name":"judge", "on":"waiting_input", "action":"send_prompt", "text":"continue",
            "ask_condition":{"prompt":"Ready?", "provider":"openai", "observe_only":false}}
    });
    let result = client.call(&request).await.unwrap();
    assert_eq!(result["ok"], true);
    assert_eq!(result["automation_judgment"]["decision"], "unknown");
    assert_eq!(result["automation_judgment"]["reason"], "ask is disabled");
    assert!(sends.lock().unwrap().is_empty());
    assert!(ask.list().await.is_empty());
    assert_eq!(client.call(&request).await.unwrap()["ok"], false);
    assert_eq!(automation.ledger().all().await.len(), 2);
    shutdown.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn orchestration_settings_ipc_roundtrips_and_rejects_stale_writes() {
    let directory = tempdir().unwrap();
    let socket = directory.path().join("orchestration.sock");
    let path = directory.path().join("config.toml");
    std::fs::write(&path, "# existing\n").unwrap();
    let server = Server::new(socket.clone(), Store::shared()).with_config_path(Some(path));
    let (shutdown, receiver) = broadcast::channel(1);
    let task = tokio::spawn(async move { server.run(receiver).await.unwrap() });
    wait_for_socket(&socket).await;
    let client = Client::new(socket);
    let read = client
        .call(&serde_json::json!({"kind":"config_orchestration_read"}))
        .await
        .unwrap();
    assert_eq!(read["orchestration"]["enabled"], false);
    let mut settings = read["orchestration"].clone();
    settings["enabled"] = serde_json::json!(true);
    let request = serde_json::json!({"kind":"config_orchestration_write", "expected_text":"# existing\n", "settings":settings});
    let saved = client.call(&request).await.unwrap();
    assert_eq!(saved["orchestration"]["enabled"], true);
    assert!(saved["config"]["text"]
        .as_str()
        .unwrap()
        .starts_with("# existing\n"));
    let stale = client.call(&request).await.unwrap();
    assert_eq!(stale["ok"], false);
    assert_eq!(stale["config"], saved["config"]);
    assert!(CAPABILITIES.contains(&"config_orchestration_v1"));
    shutdown.send(()).unwrap();
    task.await.unwrap();
}

#[tokio::test]
async fn config_launch_requests_roundtrip_and_reject_stale_writes() {
    let directory = tempdir().unwrap();
    let socket = directory.path().join("launch.sock");
    let path = directory.path().join("config.toml");
    let initial = "# preserve\n[agent.codex]\noptions = ['--search']\n";
    std::fs::write(&path, initial).unwrap();
    let server = Server::new(socket.clone(), Store::shared()).with_config_path(Some(path.clone()));
    let (shutdown, receiver) = broadcast::channel(1);
    let task = tokio::spawn(async move { server.run(receiver).await.unwrap() });
    wait_for_socket(&socket).await;
    let client = Client::new(socket);
    let read = client
        .call(&serde_json::json!({"protocol": PROTOCOL_VERSION, "kind": "config_launch_read"}))
        .await
        .unwrap();
    assert_eq!(read["config"]["text"], initial);
    assert!(read["launch"]["providers"]
        .as_array()
        .unwrap()
        .iter()
        .any(|provider| provider["program"] == "codex"
            && provider["options"] == serde_json::json!(["--search"])));
    let request = serde_json::json!({
        "protocol": PROTOCOL_VERSION, "kind": "config_launch_write", "expected_text": initial,
        "edits": [{"target": "provider", "program": "codex", "options": []}],
    });
    let written = client.call(&request).await.unwrap();
    assert!(written["config"]["text"]
        .as_str()
        .unwrap()
        .contains("# preserve"));
    // The write names the process that served it (`daemon_boot_id_v1`).
    assert_eq!(read["boot_id"], daemon_boot_id());
    assert_eq!(written["boot_id"], daemon_boot_id());
    let text = written["config"]["text"].clone();
    let raw = client
        .call(&serde_json::json!({
            "protocol": PROTOCOL_VERSION, "kind": "config_write",
            "text": text, "expected_text": text,
        }))
        .await
        .unwrap();
    assert_eq!(raw["ok"], true);
    assert_eq!(raw["boot_id"], daemon_boot_id());
    let stale = client.call(&request).await.unwrap();
    assert_eq!(stale["ok"], false);
    assert!(stale["error"].as_str().unwrap().contains("changed"));
    assert_eq!(stale["config"]["text"], written["config"]["text"]);
    shutdown.send(()).unwrap();
    task.await.unwrap();
}

#[test]
fn automation_judge_wire_contract_is_additive_and_strict() {
    let request: RequestBody = serde_json::from_value(serde_json::json!({
        "kind": "automation_judge_test", "pane": "%1", "rule": {
            "name": "judge", "on": "waiting_input", "action": "notify", "message": "Ready",
            "ask_condition": {"prompt":"Ready?", "provider":"openai"}
        }
    }))
    .unwrap();
    assert!(matches!(request, RequestBody::AutomationJudgeTest { pane, .. } if pane == "%1"));
    assert!(CAPABILITIES.contains(&"automation_ask_v1"));
    assert!(CAPABILITIES.contains(&"config_launch_v1"));
}

#[tokio::test]
async fn config_read_and_write_serve_the_daemons_config_file() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-config.sock");
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[ask]\nenabled = true\n").unwrap();
    let server =
        Server::new(sock.clone(), Store::shared()).with_config_path(Some(config_path.clone()));
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;
    let client = Client::new(sock);

    let document = client.config_read().await.unwrap();
    assert!(document.exists);
    assert_eq!(document.text, "[ask]\nenabled = true\n");
    assert_eq!(document.path, config_path);

    let written = client
        .config_write("[ask]\nenabled = false\n", Some(&document.text))
        .await
        .unwrap();
    assert_eq!(written.text, "[ask]\nenabled = false\n");
    assert_eq!(
        std::fs::read_to_string(&config_path).unwrap(),
        "[ask]\nenabled = false\n"
    );

    // A document that would not load is refused, and the file it would
    // have replaced is untouched.
    let error = client
        .config_write("[ask]\nnot_a_key = 1\n", None)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("not written"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&config_path).unwrap(),
        "[ask]\nenabled = false\n"
    );

    // A stale editor is refused rather than allowed to clobber.
    let stale = client
        .config_write("[ui]\n", Some("[ask]\nenabled = true\n"))
        .await
        .unwrap_err()
        .to_string();
    assert!(stale.contains("changed on disk"), "{stale}");

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn config_requests_refuse_a_daemon_without_a_config_path() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-config-none.sock");
    let server = Server::new(sock.clone(), Store::shared());
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;
    let client = Client::new(sock);

    let error = client.config_read().await.unwrap_err().to_string();
    assert!(error.contains("no config file path"), "{error}");

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn ask_providers_lists_every_provider_and_follows_the_selection() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-ask-providers.sock");
    let server = Server::new(sock.clone(), Store::shared());
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let client = Client::new(sock);
    let providers = client.ask_providers().await.unwrap();
    let ids: Vec<&str> = providers.iter().map(|info| info.id.as_str()).collect();
    assert_eq!(ids, crate::ask::supported_agents());
    assert!(providers[0].selected);
    assert_eq!(providers[3].model.as_deref(), Some("claude-sonnet-5"));
    assert_eq!(providers[4].model.as_deref(), Some("gpt-5"));

    assert_eq!(client.ask_agent(Some("openai")).await.unwrap(), "openai");
    let providers = client.ask_providers().await.unwrap();
    let selected: Vec<&str> = providers
        .iter()
        .filter(|info| info.selected)
        .map(|info| info.id.as_str())
        .collect();
    assert_eq!(selected, ["openai"]);

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn ask_provider_configure_writes_config_and_answers_with_the_list() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-ask-configure.sock");
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[watch]\nspinner = false\n").unwrap();
    let ask = crate::ask::AskStore::in_memory(crate::ask::AskOptions {
        config_path: Some(config_path.clone()),
        ..crate::ask::AskOptions::default()
    });
    let server = Server::new(sock.clone(), Store::shared()).with_ask(ask);
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let resp = raw_call(
        &sock,
        &serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "ask_provider_configure",
            "provider": "anthropic",
            "model": "claude-opus-5",
            "api_key_env": "WORK_ANTHROPIC_KEY",
        }),
    )
    .await;
    assert_eq!(resp["ok"], true, "{resp}");
    // A configured id leads the list; the untouched built-ins follow.
    let anthropic = &resp["ask_providers"][0];
    assert_eq!(anthropic["id"], "anthropic");
    assert_eq!(anthropic["engine"], "anthropic");
    assert_eq!(anthropic["builtin"], true);
    assert_eq!(
        anthropic["configured"], true,
        "the write gave the built-in a table"
    );
    assert_eq!(anthropic["model"], "claude-opus-5");
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(text.starts_with("[watch]\nspinner = false\n"), "{text}");
    assert!(text.contains("[ask.providers.anthropic]"), "{text}");
    assert!(
        text.contains("api_key_env = \"WORK_ANTHROPIC_KEY\""),
        "{text}"
    );

    // Sending only `model` leaves `api_key_env` as it was.
    let client = Client::new(sock.clone());
    let providers = client
        .ask_provider_configure(
            "anthropic",
            &AskProviderEdit {
                model: Some(None),
                ..AskProviderEdit::default()
            },
        )
        .await
        .unwrap();
    let anthropic = providers
        .iter()
        .find(|provider| provider.id == "anthropic")
        .unwrap();
    assert_eq!(anthropic.model.as_deref(), Some("claude-sonnet-5"));
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(!text.contains("model ="), "{text}");
    assert!(
        text.contains("api_key_env = \"WORK_ANTHROPIC_KEY\""),
        "{text}"
    );
    // An empty edit is a no-op that still answers with the list.
    let providers = client
        .ask_provider_configure("anthropic", &AskProviderEdit::default())
        .await
        .unwrap();
    assert_eq!(providers.len(), crate::ask::supported_agents().len());
    assert_eq!(std::fs::read_to_string(&config_path).unwrap(), text);
    // `null` clears the last key and the table goes with it.
    client
        .ask_provider_configure(
            "anthropic",
            &AskProviderEdit {
                api_key_env: Some(None),
                ..AskProviderEdit::default()
            },
        )
        .await
        .unwrap();
    assert!(!std::fs::read_to_string(&config_path)
        .unwrap()
        .contains("providers"));

    let refused = client
        .ask_provider_configure(
            "bard",
            &AskProviderEdit {
                model: Some(Some("x".into())),
                ..AskProviderEdit::default()
            },
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(refused.contains("is not configured"), "{refused}");

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[allow(clippy::too_many_lines)] // one add/refuse/remove lifecycle, in order
#[tokio::test]
async fn ask_provider_add_and_remove_compose_the_list_over_the_wire() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-ask-compose.sock");
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, "[ask]\nenabled = true\n").unwrap();
    let ask = crate::ask::AskStore::in_memory(crate::ask::AskOptions {
        config_path: Some(config_path.clone()),
        ..crate::ask::AskOptions::default()
    });
    let server = Server::new(sock.clone(), Store::shared()).with_ask(ask);
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;
    let client = Client::new(sock.clone());

    // Two instances of one engine, each with its own key variable.
    for (id, env) in [
        ("anthropic-work", "WORK_ANTHROPIC_KEY"),
        ("anthropic-personal", "HOME_ANTHROPIC_KEY"),
    ] {
        let providers = client
            .ask_provider_add(&crate::ask::AskProviderAdd {
                id: id.into(),
                engine: "anthropic".into(),
                api_key_env: Some(env.into()),
                ..crate::ask::AskProviderAdd::default()
            })
            .await
            .unwrap();
        let added = providers.iter().find(|info| info.id == id).unwrap();
        assert_eq!(added.engine, "anthropic");
        assert_eq!(added.kind, crate::ask::AskProviderKind::Api);
        assert!(!added.builtin && added.configured);
    }
    let providers = client.ask_providers().await.unwrap();
    let ids: Vec<&str> = providers.iter().map(|info| info.id.as_str()).collect();
    let mut expected = vec![
        "anthropic-personal",
        "anthropic-work",
        "claude",
        "codex",
        "gemini",
        "anthropic",
        "openai",
    ];
    expected.push("apple");
    assert_eq!(
        ids, expected,
        "composed instances lead, then the built-ins they do not cover"
    );
    // And they are selectable by their own ids.
    assert_eq!(
        client.ask_agent(Some("anthropic-work")).await.unwrap(),
        "anthropic-work"
    );

    // The refusals all come back as daemon errors, not silent no-ops.
    for (id, engine, expected) in [
        ("anthropic-work", "anthropic", "already exists"),
        ("has a space", "anthropic", "TOML bare key"),
        ("mine", "bard", "is not supported"),
        ("claude", "codex", "built-in provider"),
    ] {
        let error = client
            .ask_provider_add(&crate::ask::AskProviderAdd {
                id: id.into(),
                engine: engine.into(),
                ..crate::ask::AskProviderAdd::default()
            })
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{id}/{engine}: {error}");
    }

    // Removing the selected instance hands the selection to the first
    // provider that is left.
    let providers = client.ask_provider_remove("anthropic-work").await.unwrap();
    assert!(!providers.iter().any(|info| info.id == "anthropic-work"));
    assert_eq!(
        providers
            .iter()
            .find(|info| info.selected)
            .map(|info| info.id.as_str()),
        Some("anthropic-personal")
    );
    // A built-in with no config entry has nothing to remove — which is
    // exactly what `configured` tells a client before it offers to.
    assert!(providers
        .iter()
        .filter(|info| info.builtin)
        .all(|info| !info.configured));
    let error = client
        .ask_provider_remove("gemini")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("nothing to remove"), "{error}");

    client
        .ask_provider_remove("anthropic-personal")
        .await
        .unwrap();
    let text = std::fs::read_to_string(&config_path).unwrap();
    assert!(!text.contains("providers"), "{text}");
    assert!(text.contains("enabled = true"), "{text}");

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn human_requests_in_the_decision_queue_are_an_operator_read() {
    use crate::collaboration::{
        CollaborationOptions, CollaborationStore, HumanAction, NewRequest, Participant, RequestKind,
    };
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-queue.sock");
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    let sender: Participant = serde_json::from_value(serde_json::json!({
        "agent_kind":"codex", "agent_session_id":"a", "pane":"%1",
        "room":{"host":"tmux","socket":"default","window_id":"@1"},
        "state":"idle", "launch_purpose":"worker"
    }))
    .unwrap();
    let console = Participant::console(sender.room.clone());
    mailbox
        .create(
            sender,
            console,
            NewRequest {
                body: "Merge?".into(),
                kind: RequestKind::Question,
                human_action: Some(HumanAction::Choice),
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();
    let server = Server::new(sock.clone(), Store::shared()).with_collaboration(mailbox);
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;
    let client = Client::new(sock);

    let operator = client
        .decision_queue(&crate::decision_queue::operator_origin(), 10)
        .await
        .unwrap();
    assert!(
        matches!(&operator.items[..], [crate::decision_queue::PendingItem::HumanRequest(h)] if h.body == "Merge?"),
        "{operator:?}"
    );
    // An agent's origin, even one in the same room, does not read them.
    let agent = crate::collaboration::CollaborationOrigin {
        pane: "%1".into(),
        socket: Some("default".into()),
        ..crate::decision_queue::operator_origin()
    };
    let agent = crate::collaboration::CollaborationOrigin {
        console: false,
        ..agent
    };
    let refused = client.decision_queue(&agent, 10).await.unwrap();
    assert!(refused.items.is_empty(), "{refused:?}");
    assert_eq!(refused.unavailable.len(), 1, "{refused:?}");
    let history = client.decision_history(&agent, 10).await.unwrap();
    assert_eq!(history.unavailable.len(), 1, "{history:?}");
    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one daemon fixture covers create, queue, dispose and history
async fn review_items_round_trip_over_ipc() {
    use crate::review::{ActorKind, DispositionKind, NewReview, ReviewActor, ReviewTarget};
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-review.sock");
    let reviews = crate::review::ReviewStore::open(&dir.path().join("reviews.sqlite3")).unwrap();
    let server = Server::new(sock.clone(), Store::shared()).with_reviews(reviews);
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;
    let client = Client::new(sock);

    let hello = client.hello(Duration::from_secs(3)).await.unwrap();
    assert!(hello
        .capabilities
        .iter()
        .any(|c| c == crate::review::REVIEW_CAPABILITY));
    let empty = client
        .review_summary_with_timeout(Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(empty.open, 0);

    let new = NewReview {
        target: ReviewTarget::Work {
            workspace: "tend".into(),
            work: "review".into(),
            alias: Some("impl".into()),
            pipeline_generation: Some(3),
        },
        summary: "check the store migration".into(),
        anchor: Some("commit:abc".into()),
        idempotency_key: Some("tend/review/impl/3/1".into()),
        requested_by: ReviewActor {
            kind: ActorKind::Agent,
            label: Some("impl".into()),
        },
        decision: Some(crate::decision::Decision {
            needed: Some("merge or hold?".into()),
            options: vec!["merge".into(), "hold".into()],
            recommendation: Some("merge".into()),
            evidence: vec![crate::decision::Evidence::Commit {
                sha: "abc123".into(),
                repo: None,
            }],
        }),
    };
    let item = client.review_create(&new).await.unwrap();
    assert_eq!(client.review_create(&new).await.unwrap(), item);
    assert_eq!(item.decision, new.decision, "the decision round-trips");
    let queue = client
        .decision_queue(&crate::decision_queue::operator_origin(), 10)
        .await
        .unwrap();
    assert!(
        matches!(&queue.items[..], [crate::decision_queue::PendingItem::Review(r)] if r.id == item.id),
        "{queue:?}"
    );
    assert!(client
        .decision_history(&crate::decision_queue::operator_origin(), 10)
        .await
        .unwrap()
        .items
        .is_empty());
    assert_eq!(client.review_get(&item.id).await.unwrap(), item);
    let summary = client
        .review_summary_with_timeout(Duration::from_secs(3))
        .await
        .unwrap();
    assert_eq!(summary.open, 1);
    assert_eq!(summary.oldest_requested_at, Some(item.requested_at));

    let user = ReviewActor {
        kind: ActorKind::User,
        label: None,
    };
    let returned = client
        .review_dispose(&item.id, DispositionKind::Returned, &user, Some("no tests"))
        .await
        .unwrap();
    assert_eq!(
        returned.disposition.as_ref().map(|d| d.kind),
        Some(DispositionKind::Returned)
    );
    assert!(client
        .decision_queue(&crate::decision_queue::operator_origin(), 10)
        .await
        .unwrap()
        .items
        .is_empty());
    let history = client
        .decision_history(&crate::decision_queue::operator_origin(), 10)
        .await
        .unwrap();
    assert!(
        matches!(&history.items[..], [crate::decision_queue::DecidedItem::Review(r)]
            if r.disposition.as_ref().and_then(|d| d.note.as_deref()) == Some("no tests")),
        "{history:?}"
    );
    let query = |open_only| crate::review::ReviewQuery {
        open_only,
        oldest_first: false,
        limit: 10,
        offset: 0,
    };
    assert!(client
        .review_list(&query(true))
        .await
        .unwrap()
        .items
        .is_empty());
    let all = client.review_list(&query(false)).await.unwrap();
    assert_eq!(all.items.len(), 1);
    assert!(!all.has_more);

    // Clients cannot claim to be a policy, and refusals keep their message.
    let policy = ReviewActor {
        kind: ActorKind::Policy,
        label: None,
    };
    let err = client
        .review_dispose(&item.id, DispositionKind::Accepted, &policy, None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("policy"), "{err}");

    let _ = tx.send(());
    handle.await.unwrap();
}

#[tokio::test]
async fn unavailable_review_store_refuses_without_breaking_the_daemon() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-review-down.sock");
    let server = Server::new(sock.clone(), Store::shared())
        .with_reviews(crate::review::ReviewStore::unavailable("disk full"));
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;
    let client = Client::new(sock);
    let err = client
        .review_summary_with_timeout(Duration::from_secs(3))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("disk full"), "{err}");
    assert!(client.snapshot().await.unwrap().is_empty());
    let _ = tx.send(());
    handle.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swap_accounts_returns_configured_metadata_even_when_jobs_are_halted() {
    let swap = FakeSwap::new(false);
    let (_dir, client, tx, handle) = swap_server(Some(swap)).await;
    let hello = client.hello(Duration::from_secs(2)).await.unwrap();
    assert!(hello
        .capabilities
        .iter()
        .any(|cap| cap == "swap_accounts_v1"));
    let accounts = client.swap_accounts().await.unwrap();
    assert_eq!(accounts[0].source, "configured");
    assert_eq!(accounts[0].candidates[0].name, "work");
    assert!(!serde_json::to_string(&accounts)
        .unwrap()
        .contains("private"));
    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn a_planned_work_round_trips_over_ipc_with_its_result_records() {
    use crate::review::{ActorKind, NewReview, ReviewActor, ReviewTarget};
    use crate::work::{WorkEdit, WorkIdentity, WorkStage};
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-backlog.sock");
    let work_store = Arc::new(tokio::sync::Mutex::new(
        crate::dashboard::work_store::WorkStore::load(Some(dir.path().join("work.json"))),
    ));
    let server = Server::new(sock.clone(), Store::shared()).with_work_store(work_store.clone());
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;
    let client = Client::new(sock);
    assert!(client
        .hello(Duration::from_secs(3))
        .await
        .unwrap()
        .capabilities
        .iter()
        .any(|c| c == crate::work::WORK_BACKLOG_CAPABILITY));

    let id = WorkIdentity::new("web", "ENG-1");
    let planned = client
        .work_edit(
            &id,
            &WorkEdit {
                title: Some("Fix login".into()),
                prompt: Some("fix the redirect".into()),
                ..WorkEdit::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(planned.metadata.stage, WorkStage::Queued);
    assert!(client
        .work_edit(&WorkIdentity::new("web", "EMPTY"), &WorkEdit::default())
        .await
        .is_err());
    let queued = client.work_backlog(Some(WorkStage::Queued)).await.unwrap();
    assert_eq!(queued, vec![planned.clone()]);
    assert!(client
        .work_backlog(Some(WorkStage::Done))
        .await
        .unwrap()
        .is_empty());
    // The dashboard's copy is the same store.
    assert_eq!(work_store.lock().await.get(&id), Some(planned.clone()));

    client
        .review_create(&NewReview {
            target: ReviewTarget::Work {
                workspace: "web".into(),
                work: "ENG-1".into(),
                alias: Some("impl".into()),
                pipeline_generation: Some(1),
            },
            summary: "Redirect fixed".into(),
            anchor: None,
            idempotency_key: None,
            requested_by: ReviewActor {
                kind: ActorKind::Agent,
                label: None,
            },
            decision: None,
        })
        .await
        .unwrap();
    let show = client.work_record_show(&id).await.unwrap();
    assert_eq!(show.record, Some(planned));
    assert_eq!(show.reviews.len(), 1);
    assert!(show.run.is_none());
    assert!(client
        .work_record_show(&WorkIdentity::new("web", "NOPE"))
        .await
        .is_err());
    tx.send(()).unwrap();
    handle.await.unwrap();
}
