use super::*;

#[tokio::test]
async fn work_compose_refuses_before_spending_a_turn_on_bad_input() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-work-compose.sock");
    let server = Server::new(sock.clone(), Store::shared());
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let blank = raw_call(
        &sock,
        &serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "work_compose",
            "description": "   ",
        }),
    )
    .await;
    assert_eq!(blank["ok"], false);
    assert!(blank["error"]
        .as_str()
        .unwrap()
        .contains("description is empty"));

    // An unknown provider fails inside the turn, so nothing is spawned
    // and nothing is retried.
    let client = Client::new(sock.clone());
    let error = client
        .work_compose(
            &WorkComposeRequest {
                description: "solo claude".into(),
                agent: Some("bard".into()),
                current: None,
            },
            None,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("is not configured"), "{error}");

    // A key for the wrong provider is refused the same way `ask_send`
    // refuses it.
    let error = client
        .work_compose(
            &WorkComposeRequest {
                description: "solo claude".into(),
                agent: Some("anthropic".into()),
                current: None,
            },
            Some(("openai", "sk-wrong")),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("selected ask agent is anthropic"), "{error}");

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn write_session_bytes_rejects_invalid_base64() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-session-bytes.sock");
    let store = Store::shared();
    let server = Server::new(sock.clone(), store);
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let resp = raw_call(
        &sock,
        &serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "write_session_bytes",
            "session_id": "pty:missing",
            "data_base64": "%%%not-base64%%%",
        }),
    )
    .await;
    assert_eq!(resp["ok"], false);
    assert!(resp["error"]
        .as_str()
        .unwrap()
        .contains("invalid base64 session input"));

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn restart_is_advertised_accepted_and_drained() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-restart.sock");
    let store = Store::shared();
    let (tx, rx) = broadcast::channel(1);
    let restart = Arc::new(RestartController::new(7, tx));
    let server = Server::new(sock.clone(), store).with_restart_controller(Arc::clone(&restart));
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let client = Client::new(sock.clone());
    let hello = client
        .hello(Duration::from_secs(2))
        .await
        .expect("hello answers");
    assert!(hello
        .capabilities
        .iter()
        .any(|cap| cap == RESTART_CAPABILITY));
    assert!(hello.capabilities.iter().any(|cap| cap == STOP_CAPABILITY));
    assert_eq!(hello.generation, Some(7));
    assert_eq!(hello.boot_id.as_deref(), Some(daemon_boot_id()));
    assert!(hello
        .capabilities
        .iter()
        .any(|cap| cap == DAEMON_BOOT_ID_CAPABILITY));

    client
        .restart(Duration::from_secs(2))
        .await
        .expect("daemon accepts restart");
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("daemon drains after accepting restart")
        .unwrap();
    assert!(restart.restart_requested());
    assert!(!sock.exists(), "drained server removes its socket");
}

#[tokio::test]
async fn signal_stop_cannot_be_rearmed_by_an_inflight_restart() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-stopping.sock");
    let store = Store::shared();
    let (tx, rx) = broadcast::channel(1);
    let restart = Arc::new(RestartController::new(0, tx));
    let server = Server::new(sock.clone(), store).with_restart_controller(Arc::clone(&restart));
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    // Get a handler accepted and parked mid-request before the stop. This
    // is the exact ordering that could re-arm the old AtomicBool design.
    let mut stream = Stream::connect(&sock).await.unwrap();
    let mut request = serde_json::to_vec(&serde_json::json!({
        "protocol": PROTOCOL_VERSION,
        "kind": "restart",
    }))
    .unwrap();
    request.push(b'\n');
    let split = request.len() - 1;
    stream.write_all(&request[..split]).await.unwrap();
    stream.flush().await.unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;

    restart.stop();
    stream.write_all(&request[split..]).await.unwrap();
    stream.flush().await.unwrap();

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let response: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(response["ok"], false);
    assert!(response["error"]
        .as_str()
        .unwrap()
        .contains("already stopping"));
    drop(reader);

    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("normal stop drains the in-flight handler")
        .unwrap();
    assert!(
        !restart.restart_requested(),
        "an in-flight restart must not override SIGTERM/SIGINT",
    );
}

#[tokio::test]
async fn stop_is_advertised_accepted_and_drained() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-stop.sock");
    let store = Store::shared();
    let (tx, rx) = broadcast::channel(1);
    let lifecycle = Arc::new(RestartController::new(3, tx));
    let server = Server::new(sock.clone(), store).with_restart_controller(Arc::clone(&lifecycle));
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let client = Client::new(sock.clone());
    let hello = client
        .hello(Duration::from_secs(2))
        .await
        .expect("hello answers");
    assert!(hello.capabilities.iter().any(|cap| cap == STOP_CAPABILITY));

    client
        .stop(Duration::from_secs(2))
        .await
        .expect("daemon accepts stop");
    tokio::time::timeout(Duration::from_secs(2), handle)
        .await
        .expect("daemon drains after accepting stop")
        .unwrap();
    assert!(!lifecycle.restart_requested());
    assert!(!sock.exists(), "drained server removes its socket");
}

#[tokio::test]
async fn lifecycle_control_is_refused_without_a_controller() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-no-restart.sock");
    let store = Store::shared();
    let server = Server::new(sock.clone(), store);
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let client = Client::new(sock.clone());
    let error = client
        .restart(Duration::from_secs(2))
        .await
        .expect_err("embedded server refuses restart");
    assert!(error.to_string().contains("restart"));
    assert!(Stream::connect(&sock).await.is_ok());

    let error = client
        .stop(Duration::from_secs(2))
        .await
        .expect_err("embedded server refuses stop");
    assert!(error.to_string().contains("stop"));
    assert!(Stream::connect(&sock).await.is_ok());

    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// After a v1-pinned `hello`, snapshot responses must downgrade
/// `waiting_choice` to `waiting_input` so the old client's serde
/// deserializer doesn't fail on the unknown variant.
#[tokio::test]
async fn v1_hello_downgrades_waiting_choice_in_snapshot() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-v1.sock");
    let store = Store::shared();
    let server = Server::new(sock.clone(), store.clone());
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let id = AgentId {
        tmux_socket: None,
        kind: AgentKind::ClaudeCode,
        session_id: "v1-test".into(),
        surface: None,
        pane: Some("%1".into()),
        cwd: None,
    };
    store
        .apply(&AgentEvent::Started {
            id: id.clone(),
            at: OffsetDateTime::now_utc(),
        })
        .await;
    // Drive into WaitingChoice via NeedsChoice notification.
    store
        .apply(&AgentEvent::NotificationFired {
            id: id.clone(),
            level: crate::event::NotificationLevel::NeedsChoice,
            message: "pick one".into(),
            at: OffsetDateTime::now_utc(),
        })
        .await;

    // Open one connection: hello v1, then snapshot.
    let mut stream = Stream::connect(&sock).await.unwrap();
    let mut hello = serde_json::to_vec(&serde_json::json!({
        "protocol": 1, "kind": "hello", "client": "v1-test",
    }))
    .unwrap();
    hello.push(b'\n');
    stream.write_all(&hello).await.unwrap();
    let mut snap = serde_json::to_vec(&serde_json::json!({
        "kind": "snapshot",
    }))
    .unwrap();
    snap.push(b'\n');
    stream.write_all(&snap).await.unwrap();
    stream.flush().await.unwrap();

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let hello_resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(hello_resp["protocol"], 1);
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let snap_resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    let agents = snap_resp["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["state"], "waiting_input");
    // The literal v2 string must not appear anywhere in the payload.
    let body = line.clone();
    assert!(
        !body.contains("waiting_choice"),
        "v1 snapshot still contains waiting_choice: {body}"
    );

    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// A `task` row (v3 `AgentKind`) must be downgraded to `unknown` for a
/// client that negotiated v2, so older `tend status`/`watch` can still
/// deserialize the snapshot.
#[tokio::test]
async fn v2_hello_downgrades_task_kind_in_snapshot() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-task-v2.sock");
    let store = Store::shared();
    let server = Server::new(sock.clone(), store.clone());
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    store
        .register_task("job".into(), Some(std::process::id()), None, None, None)
        .await
        .unwrap();

    let mut stream = Stream::connect(&sock).await.unwrap();
    let mut hello = serde_json::to_vec(&serde_json::json!({
        "protocol": 2, "kind": "hello", "client": "v2-test",
    }))
    .unwrap();
    hello.push(b'\n');
    stream.write_all(&hello).await.unwrap();
    let mut snap = serde_json::to_vec(&serde_json::json!({ "kind": "snapshot" })).unwrap();
    snap.push(b'\n');
    stream.write_all(&snap).await.unwrap();
    stream.flush().await.unwrap();

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap(); // hello ack
    line.clear();
    reader.read_line(&mut line).await.unwrap(); // snapshot
    let snap_resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    let agents = snap_resp["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0]["kind"], "unknown");
    assert!(
        !line.contains("\"task\""),
        "v2 snapshot still contains task kind: {line}"
    );

    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// A v2-pinned `hello` keeps `waiting_choice` intact.
#[tokio::test]
async fn v2_hello_keeps_waiting_choice_in_snapshot() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-v2.sock");
    let store = Store::shared();
    let server = Server::new(sock.clone(), store.clone());
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let id = AgentId {
        tmux_socket: None,
        kind: AgentKind::ClaudeCode,
        session_id: "v2-test".into(),
        surface: None,
        pane: Some("%2".into()),
        cwd: None,
    };
    store
        .apply(&AgentEvent::Started {
            id: id.clone(),
            at: OffsetDateTime::now_utc(),
        })
        .await;
    store
        .apply(&AgentEvent::NotificationFired {
            id: id.clone(),
            level: crate::event::NotificationLevel::NeedsChoice,
            message: "pick".into(),
            at: OffsetDateTime::now_utc(),
        })
        .await;

    let mut stream = Stream::connect(&sock).await.unwrap();
    let mut hello = serde_json::to_vec(&serde_json::json!({
        "protocol": 2, "kind": "hello", "client": "v2-test",
    }))
    .unwrap();
    hello.push(b'\n');
    stream.write_all(&hello).await.unwrap();
    let mut snap = serde_json::to_vec(&serde_json::json!({ "kind": "snapshot" })).unwrap();
    snap.push(b'\n');
    stream.write_all(&snap).await.unwrap();
    stream.flush().await.unwrap();

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap(); // hello resp
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let snap_resp: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    let agents = snap_resp["agents"].as_array().unwrap();
    assert_eq!(agents[0]["state"], "waiting_choice");

    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// `hello` with a protocol outside `[MIN, MAX]` is rejected without
/// pinning the connection — the legacy strict-match remains in force
/// for the rest of the connection.
#[tokio::test]
async fn hello_rejects_out_of_range_protocol() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-bad-hello.sock");
    let store = Store::shared();
    let server = Server::new(sock.clone(), store);
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let resp = raw_call(
        &sock,
        &serde_json::json!({ "protocol": 999, "kind": "hello" }),
    )
    .await;
    assert_eq!(resp["ok"], false);
    assert!(resp["error"]
        .as_str()
        .unwrap()
        .contains("unsupported protocol"));

    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// A `backend_pane_snapshot` push reaches the server's
/// `SharedBackend`, so the zellij backend's cache — and thus
/// `list_panes` / `resolve_pane` — reflects it. The request/response
/// is synchronous, so once the client call returns the ingest has
/// already run daemon-side; no sleep/poll needed.
#[tokio::test]
async fn backend_pane_snapshot_push_updates_shared_backend() {
    use crate::backend::zellij::ZellijBackend;
    use crate::backend::PaneBackend;
    use std::sync::Arc;

    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-backend-snap.sock");
    let store = Store::shared();
    let backend = Arc::new(ZellijBackend::new());
    let server = Server::new(sock.clone(), store).with_backend(backend.clone());
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    // No plugin push yet → empty.
    assert!(backend.list_panes().is_empty());

    let pane = PaneInfo {
        session_group: None,
        agent_program: None,
        worker_launch_id: None,
        agent_role: None,
        agent_alias: None,
        workspace_id: None,
        work_id: None,
        socket: None,
        pane_id: "zellij:3".into(),
        session_id: String::new(),
        session: "z".into(),
        window_id: String::new(),
        window_name: String::new(),
        window_index: "0".into(),
        pane_index: "3".into(),
        tty: String::new(),
        current_command: "claude".into(),
        title: String::new(),
        pane_pid: 0,
        current_path: String::new(),
    };
    Client::new(sock.clone())
        .push_pane_snapshot(&[pane])
        .await
        .unwrap();

    // The same Arc the test holds now sees the pushed pane.
    let panes = backend.list_panes();
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0].pane_id, "zellij:3");
    assert_eq!(
        backend.resolve_pane("zellij:3").unwrap().current_command,
        "claude"
    );

    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// `send_prompt` routes to the backend that governs the pane's
/// namespace and, with `submit`, follows the text with a carriage
/// return as a second injection.
#[tokio::test]
async fn send_prompt_injects_text_then_submit_cr() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-send.sock");
    let (backend, sends) = RecordingBackend::new(HostKind::Tmux, true);
    let (tx, handle) = serve(&sock, Store::shared(), vec![backend]).await;

    Client::new(sock.clone())
        .send_prompt_scoped("%1", "fix the bug", true, Some("default"), None)
        .await
        .expect("send_prompt ok");

    let recorded = sends.lock().unwrap().clone();
    assert_eq!(
        recorded,
        vec![
            ("%1".to_string(), "fix the bug".to_string()),
            ("%1".to_string(), "\r".to_string()),
        ],
    );

    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// `send_prompt_state_gate_v1`: a gated send types only into an Idle agent;
/// a busy one, one at a choice menu, or an untracked pane is refused with
/// nothing typed. Forced and legacy (no `force`) sends type as before.
#[tokio::test]
async fn send_prompt_state_gate_refuses_a_busy_target_unless_forced() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-send-gate.sock");
    let store = Store::shared();
    let id = AgentId {
        kind: AgentKind::Codex,
        session_id: "busy".into(),
        surface: None,
        pane: Some("%1".into()),
        tmux_socket: Some("default".into()),
        cwd: None,
    };
    store
        .apply(&AgentEvent::PromptSubmitted {
            id: id.clone(),
            prompt: "long task".into(),
            at: OffsetDateTime::now_utc(),
        })
        .await;
    let (backend, sends) = RecordingBackend::new(HostKind::Tmux, true);
    let (tx, handle) = serve(&sock, store.clone(), vec![backend]).await;
    let client = Client::new(sock);
    let send = |pane: &'static str, force: Option<bool>| {
        let client = client.clone();
        async move {
            client
                .send_prompt_with_gate(pane, "hi", true, Some("default"), None, force)
                .await
        }
    };

    // The stall/merge this prevents: text typed into a working turn.
    let refused = send("%1", Some(false)).await.unwrap_err().to_string();
    assert!(refused.contains("target_not_idle"), "{refused}");
    assert!(refused.contains("working"), "{refused}");
    let untracked = send("%7", Some(false)).await.unwrap_err().to_string();
    assert!(untracked.contains("target_not_idle"), "{untracked}");
    assert!(
        sends.lock().unwrap().is_empty(),
        "nothing typed when refused"
    );

    store
        .apply(&AgentEvent::TurnStopped {
            id: id.clone(),
            response: None,
            recap: None,
            ai_title: None,
            idle_confirmed: false,
            at: OffsetDateTime::now_utc(),
        })
        .await;
    send("%1", Some(false))
        .await
        .expect("an idle target is typed into");
    assert_eq!(sends.lock().unwrap().len(), 2);
    sends.lock().unwrap().clear();

    store
        .apply(&AgentEvent::PromptSubmitted {
            id,
            prompt: "again".into(),
            at: OffsetDateTime::now_utc(),
        })
        .await;
    send("%1", Some(true)).await.expect("forced");
    send("%1", None)
        .await
        .expect("legacy callers are unchanged");
    assert_eq!(sends.lock().unwrap().len(), 4);

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn send_prompt_confirms_hook_without_repeating_body_or_enter() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-send-confirm.sock");
    let store = Store::shared();
    let id = AgentId {
        kind: AgentKind::Codex,
        session_id: "original".into(),
        surface: None,
        pane: Some("%1".into()),
        tmux_socket: Some("default".into()),
        cwd: None,
    };
    store
        .apply(&AgentEvent::Started {
            id: id.clone(),
            at: OffsetDateTime::now_utc(),
        })
        .await;
    let (backend, sends) = RecordingBackend::new(HostKind::Tmux, true);
    let (tx, handle) = serve(&sock, store.clone(), vec![backend]).await;
    let observed = sends.clone();
    let hook = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if observed
                    .lock()
                    .unwrap()
                    .last()
                    .is_some_and(|(_, t)| t == "\r")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        store
            .apply(&AgentEvent::PromptSubmitted {
                id,
                prompt: "continue".into(),
                at: OffsetDateTime::now_utc(),
            })
            .await;
    });
    let outcome = Client::new(sock)
        .send_prompt("%1", "continue", true)
        .await
        .unwrap();
    assert!(outcome.sent && outcome.submitted);
    assert_eq!(outcome.submission_confirmed, Some(true));
    hook.await.unwrap();
    assert_eq!(
        *sends.lock().unwrap(),
        vec![("%1".into(), "continue".into()), ("%1".into(), "\r".into()),]
    );
    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// Without `submit`, only the text is injected — no trailing CR.
#[tokio::test]
async fn send_prompt_without_submit_omits_cr() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-send-nosub.sock");
    let (backend, sends) = RecordingBackend::new(HostKind::Tmux, true);
    let (tx, handle) = serve(&sock, Store::shared(), vec![backend]).await;

    Client::new(sock.clone())
        .send_prompt_scoped("%1", "note", false, Some("default"), None)
        .await
        .expect("send_prompt ok");

    assert_eq!(
        sends.lock().unwrap().clone(),
        vec![("%1".to_string(), "note".to_string())],
    );

    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// A backend that lacks the `send_text` capability is refused with a
/// structured error — never a panic, and no injection is attempted.
#[tokio::test]
async fn send_prompt_refused_when_backend_lacks_cap() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-send-refuse.sock");
    let (backend, sends) = RecordingBackend::new(HostKind::Zellij, false);
    let (tx, handle) = serve(&sock, Store::shared(), vec![backend]).await;

    let err = Client::new(sock.clone())
        .send_prompt("zellij:3", "hi", true)
        .await
        .expect_err("must refuse without send_text cap");
    assert!(
        format!("{err}").contains("does not support send_text"),
        "unexpected error: {err}",
    );
    assert!(
        sends.lock().unwrap().is_empty(),
        "no injection when the cap is absent",
    );

    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// With a mixed backend set, `send_prompt` resolves the target by the
/// pane-id namespace: a `herdr:` pane routes to the herdr backend even
/// though tmux is primary (`backends[0]`).
#[tokio::test]
async fn send_prompt_resolves_by_pane_namespace() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-send-ns.sock");
    let (tmux, tmux_sends) = RecordingBackend::new(HostKind::Tmux, true);
    let (herdr, herdr_sends) = RecordingBackend::new(HostKind::Herdr, true);
    // tmux leads (primary); herdr trails.
    let backends: Vec<SharedBackend> = vec![tmux as SharedBackend, herdr as SharedBackend];
    let server = Server::new(sock.clone(), Store::shared()).with_backends(backends);
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    Client::new(sock.clone())
        .send_prompt("herdr:p9", "hey", false)
        .await
        .expect("send_prompt ok");

    assert!(
        tmux_sends.lock().unwrap().is_empty(),
        "tmux must not receive"
    );
    assert_eq!(
        herdr_sends.lock().unwrap().clone(),
        vec![("herdr:p9".to_string(), "hey".to_string())],
    );

    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// `capture` routes to the namespace backend and returns its screen text.
#[tokio::test]
async fn capture_returns_backend_screen_text() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-capture.sock");
    let (backend, _sends) = RecordingBackend::new(HostKind::Tmux, true);
    let (tx, handle) = serve(&sock, Store::shared(), vec![backend]).await;

    let text = Client::new(sock.clone())
        .capture("%7")
        .await
        .expect("capture ok");
    assert_eq!(text.as_deref(), Some("captured:%7"));

    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// Fix 3 — routing: a pane whose namespace classifies to a KNOWN host
/// (`herdr:`) but whose backend is NOT in the active set is a structured
/// refusal (`Err(kind)`), never a silent fall-through to the primary. An
/// unclassified id still falls back to `backends[0]`.
#[test]
fn resolve_backend_refuses_known_but_absent_namespace() {
    let (tmux, _s) = RecordingBackend::new(HostKind::Tmux, true);
    let backends: Vec<SharedBackend> = vec![tmux as SharedBackend];

    // Known + present → the tmux backend.
    assert!(matches!(
        resolve_backend(&backends, "%5"),
        Ok(b) if b.kind() == HostKind::Tmux
    ));
    // Known (herdr:) + absent from the set → refusal carrying the kind.
    assert!(matches!(
        resolve_backend(&backends, "herdr:p1"),
        Err(HostKind::Herdr)
    ));
    // Unclassified id → fall back to the primary (backends[0]).
    assert!(matches!(
        resolve_backend(&backends, "weird-legacy-id"),
        Ok(b) if b.kind() == HostKind::Tmux
    ));
}

/// Fix 3 — end to end: `send_prompt` to a pane in an unobserved namespace
/// is refused with a `namespace unavailable` error and injects nothing —
/// it must NOT type into the tmux (primary) backend.
#[tokio::test]
async fn send_prompt_refused_for_unavailable_namespace() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-send-ns-absent.sock");
    let (tmux, sends) = RecordingBackend::new(HostKind::Tmux, true);
    let (tx, handle) = serve(&sock, Store::shared(), vec![tmux]).await;

    let err = Client::new(sock.clone())
        .send_prompt("herdr:p1", "hi", true)
        .await
        .expect_err("must refuse an unobserved namespace");
    assert!(
        format!("{err}").contains("namespace unavailable"),
        "unexpected error: {err}",
    );
    assert!(
        sends.lock().unwrap().is_empty(),
        "no injection when the namespace is unavailable",
    );

    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// Fix 1 — the control op is pinned to the pane's RECORDED server: the
/// daemon looks the agent row up by pane and threads its `tmux_socket`
/// into `send_text_on` (both the text and the submit CR), so a shared
/// pane id like `%5` reaches the right tmux server.
#[tokio::test]
async fn send_prompt_threads_recorded_socket() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-send-socket.sock");
    let store = Store::shared();
    // Seed an agent on pane %5 recorded against the `amux` server.
    store
        .apply(&AgentEvent::Started {
            id: AgentId {
                tmux_socket: Some("amux".into()),
                kind: AgentKind::ClaudeCode,
                session_id: "sock-sess".into(),
                surface: None,
                pane: Some("%5".into()),
                cwd: None,
            },
            at: OffsetDateTime::now_utc(),
        })
        .await;

    let (backend, _sends, sockets) = RecordingBackend::new_full(HostKind::Tmux, true, true, false);
    let (tx, handle) = serve(&sock, store, vec![backend]).await;

    Client::new(sock.clone())
        .send_prompt("%5", "go", true)
        .await
        .expect("send_prompt ok");

    // Both injections (text + CR) were pinned to the recorded socket.
    assert_eq!(
        sockets.lock().unwrap().clone(),
        vec![Some("amux".to_string()), Some("amux".to_string())],
    );

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn rmux_send_prompt_routes_namespace_and_preserves_full_endpoint() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-send-rmux.sock");
    let store = Store::shared();
    store
        .apply(&AgentEvent::Started {
            id: AgentId {
                tmux_socket: Some("/tmp/rmux-501/default".into()),
                kind: AgentKind::ClaudeCode,
                session_id: "rmux-sess".into(),
                surface: None,
                pane: Some("rmux:%5".into()),
                cwd: None,
            },
            at: OffsetDateTime::now_utc(),
        })
        .await;

    let (backend, sends, sockets) = RecordingBackend::new_full(HostKind::Rmux, true, true, false);
    let (tx, handle) = serve(&sock, store, vec![backend]).await;

    Client::new(sock.clone())
        .send_prompt("rmux:%5", "go", false)
        .await
        .expect("rmux send_prompt ok");

    assert_eq!(
        sends.lock().unwrap().clone(),
        vec![("rmux:%5".to_string(), "go".to_string())],
    );
    assert_eq!(
        sockets.lock().unwrap().clone(),
        vec![Some("/tmp/rmux-501/default".to_string())],
    );

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn rmux_send_prompt_refuses_same_pane_id_on_multiple_endpoints() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-send-rmux-ambiguous.sock");
    let store = Store::shared();
    for (session_id, endpoint) in [
        ("rmux-one", "/tmp/rmux-one/default"),
        ("rmux-two", "/tmp/rmux-two/default"),
    ] {
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    tmux_socket: Some(endpoint.into()),
                    kind: AgentKind::ClaudeCode,
                    session_id: session_id.into(),
                    surface: None,
                    pane: Some("rmux:%5".into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;
    }

    let (backend, sends, sockets) = RecordingBackend::new_full(HostKind::Rmux, true, true, false);
    let (tx, handle) = serve(&sock, store, vec![backend]).await;

    let error = Client::new(sock.clone())
        .send_prompt("rmux:%5", "do not misroute", false)
        .await
        .expect_err("ambiguous endpoint must be refused");
    assert!(format!("{error}").contains("multiple endpoints"));
    assert!(sends.lock().unwrap().is_empty());
    let client = Client::new(sock);
    client
        .send_prompt_scoped(
            "rmux:%5",
            "first",
            false,
            Some("/tmp/rmux-one/default"),
            Some("rmux-one"),
        )
        .await
        .unwrap();
    client
        .send_prompt_scoped("rmux:%5", "second", false, None, Some("rmux-two"))
        .await
        .unwrap();
    assert_eq!(
        *sockets.lock().unwrap(),
        vec![
            Some("/tmp/rmux-one/default".into()),
            Some("/tmp/rmux-two/default".into()),
        ]
    );

    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// Legacy/discovery rows may identify the agent without recording its socket.
#[tokio::test]
async fn send_prompt_preserves_registered_rows_without_socket() {
    for (kind, pane) in [(HostKind::Tmux, "%9"), (HostKind::Rmux, "rmux:%9")] {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("legacy-row.sock");
        let store = Store::shared();
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "legacy".into(),
                    surface: None,
                    pane: Some(pane.into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;
        let (backend, sends, sockets) = RecordingBackend::new_full(kind, true, true, false);
        let (tx, handle) = serve(&sock, store, vec![backend]).await;
        let client = Client::new(sock);
        client.send_prompt(pane, "legacy", false).await.unwrap();
        client
            .send_prompt_scoped(pane, "same agent", false, None, Some("legacy"))
            .await
            .unwrap();
        assert_eq!(*sockets.lock().unwrap(), vec![None, None]);
        assert_eq!(sends.lock().unwrap().len(), 2);
        tx.send(()).unwrap();
        handle.await.unwrap();
    }
}

/// A forgotten agent must not send into a colliding pane on the default server.
#[tokio::test]
async fn send_prompt_refuses_untracked_pane_without_endpoint() {
    for (kind, pane) in [(HostKind::Tmux, "%9"), (HostKind::Rmux, "rmux:%9")] {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("tend-send-unknown.sock");
        let store = Store::shared();
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    tmux_socket: Some("amux".into()),
                    kind: AgentKind::ClaudeCode,
                    session_id: "forgotten".into(),
                    surface: None,
                    pane: Some(pane.into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;
        assert!(store.forget_agent("forgotten").await);
        let (backend, sends, sockets) = RecordingBackend::new_full(kind, true, true, false);
        let (tx, handle) = serve(&sock, store, vec![backend]).await;
        let client = Client::new(sock);
        let err = client
            .send_prompt(pane, "do not type", true)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown pane"), "{err}");
        assert!(sends.lock().unwrap().is_empty());
        assert!(sockets.lock().unwrap().is_empty());
        client
            .send_prompt_scoped(pane, "explicit", false, Some("/tmp/custom/amux"), None)
            .await
            .unwrap();
        assert_eq!(
            *sockets.lock().unwrap(),
            vec![Some("/tmp/custom/amux".into())]
        );
        tx.send(()).unwrap();
        handle.await.unwrap();
    }
}

#[tokio::test]
async fn send_prompt_requires_matching_session_and_endpoint() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("session-precondition.sock");
    let store = Store::shared();
    store
        .apply(&AgentEvent::Started {
            id: AgentId {
                tmux_socket: Some("amux".into()),
                kind: AgentKind::ClaudeCode,
                session_id: "current".into(),
                surface: None,
                pane: Some("%5".into()),
                cwd: None,
            },
            at: OffsetDateTime::now_utc(),
        })
        .await;
    let (backend, sends, sockets) = RecordingBackend::new_full(HostKind::Tmux, true, true, false);
    let (tx, handle) = serve(&sock, store.clone(), vec![backend]).await;
    let client = Client::new(sock);
    for (socket, session) in [(None, "previous"), (Some("default"), "current")] {
        let err = client
            .send_prompt_scoped("%5", "stale", true, socket, Some(session))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("session mismatch"), "{err}");
    }
    assert!(sends.lock().unwrap().is_empty());
    client
        .send_prompt_scoped("%5", "current", false, None, Some("current"))
        .await
        .unwrap();
    assert_eq!(*sockets.lock().unwrap(), vec![Some("amux".into())]);
    // A new session in the same pane leaves the predecessor stopped in the
    // registry. Its identity must no longer satisfy the precondition.
    store
        .apply(&AgentEvent::Started {
            id: AgentId {
                tmux_socket: Some("amux".into()),
                kind: AgentKind::ClaudeCode,
                session_id: "replacement".into(),
                surface: None,
                pane: Some("%5".into()),
                cwd: None,
            },
            at: OffsetDateTime::now_utc(),
        })
        .await;
    let err = client
        .send_prompt_scoped("%5", "stale", true, Some("amux"), Some("current"))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("session mismatch"), "{err}");
    let err = client
        .send_prompt_scoped("%5", "blank socket", true, Some(""), None)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("explicit endpoint"), "{err}");
    assert_eq!(sends.lock().unwrap().len(), 1);
    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// Fix 6 — honest partial-failure signal: when the text lands but the
/// submit CR fails, the response is `ok:true` with `sent:true,
/// submitted:false` (NOT a total failure), so a caller knows the text is
/// already in the pane and must not resend it.
#[tokio::test]
async fn send_prompt_reports_partial_failure_when_cr_fails() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-send-partial.sock");
    // Text send succeeds; the `\r` submit fails.
    let (backend, sends, _sockets) = RecordingBackend::new_full(HostKind::Tmux, true, true, true);
    let (tx, handle) = serve(&sock, Store::shared(), vec![backend]).await;

    let outcome = Client::new(sock.clone())
        .send_prompt_scoped("%1", "hello", true, Some("default"), None)
        .await
        .expect("text landed → Ok, not a total failure");
    assert!(outcome.sent, "text landed");
    assert!(!outcome.submitted, "submit CR failed");

    // The CR WAS attempted (text-send succeeded first), and the text was
    // sent exactly once — no double-inject.
    assert_eq!(
        sends.lock().unwrap().clone(),
        vec![
            ("%1".to_string(), "hello".to_string()),
            ("%1".to_string(), "\r".to_string()),
        ],
    );

    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// Fix 6 — a total text-send failure is an `Err` (nothing landed, safe to
/// retry the whole send), and the submit CR is NOT attempted.
#[tokio::test]
async fn send_prompt_total_failure_skips_cr() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-send-total-fail.sock");
    // Every send fails.
    let (backend, sends, _sockets) = RecordingBackend::new_full(HostKind::Tmux, true, false, false);
    let (tx, handle) = serve(&sock, Store::shared(), vec![backend]).await;

    let err = Client::new(sock.clone())
        .send_prompt_scoped("%1", "hello", true, Some("default"), None)
        .await
        .expect_err("nothing landed → Err");
    assert!(format!("{err}").contains("send_text failed"), "err: {err}");
    // Only the text was attempted; the CR is skipped when the text fails.
    assert_eq!(
        sends.lock().unwrap().clone(),
        vec![("%1".to_string(), "hello".to_string())],
    );

    tx.send(()).unwrap();
    handle.await.unwrap();
}

/// Fix 7 — the lagged marker is gated on the connection's opt-in: an
/// un-opted subscriber gets `None` (silently continue, pre-marker
/// behavior), an opted-in one gets the encoded `{"event":"lagged",…}` frame.
#[test]
fn lagged_marker_bytes_gated_on_opt_in() {
    // Not opted in → nothing on the wire.
    assert!(lagged_marker_bytes(false, 7, PROTOCOL_VERSION)
        .unwrap()
        .is_none());
    // Opted in → a newline-terminated lagged frame carrying the drop count.
    let bytes = lagged_marker_bytes(true, 7, PROTOCOL_VERSION)
        .unwrap()
        .expect("opted-in subscriber gets the marker");
    let line = String::from_utf8(bytes).unwrap();
    assert!(line.ends_with('\n'));
    let v: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(v["event"], "lagged");
    assert_eq!(v["dropped"], 7);
}

#[tokio::test]
async fn agent_change_stream_coalesces_bursts_and_keeps_subscribers_independent() {
    use tokio::io::AsyncBufReadExt;
    let store = crate::state::Store::shared();
    let mut other = store.subscribe_changes();
    let (client, server) = Stream::pair().await.unwrap();
    let (_, writer) = server.into_split();
    let pump = tokio::spawn(stream_agent_changes(
        writer,
        store.subscribe_changes(),
        PROTOCOL_VERSION,
    ));
    let mut reader = BufReader::new(client);
    let mut line = String::new();
    for _ in 0..100 {
        store
            .apply(&crate::event::AgentEvent::Started {
                id: crate::event::AgentId {
                    session_id: "burst".into(),
                    kind: crate::event::AgentKind::ClaudeCode,
                    pane: None,
                    surface: None,
                    cwd: None,
                    tmux_socket: None,
                },
                at: time::OffsetDateTime::now_utc(),
            })
            .await;
    }
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        reader.read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&line).unwrap()["event"],
        "agents_changed"
    );
    assert!(other.has_changed().unwrap());
    other.borrow_and_update();
    line.clear();
    assert!(tokio::time::timeout(
        std::time::Duration::from_millis(250),
        reader.read_line(&mut line)
    )
    .await
    .is_err());
    pump.abort();
}

/// Fix 7 — tend's own client opts in: the `subscribe` request it sends
/// carries `lagged_markers: true` so `tend watch` / `tend mcp` receive the
/// overflow signal their `TransitionStream` reader knows how to skip.
#[test]
fn subscribe_request_defaults_and_opt_in_parse() {
    // Absent field → default false (a pre-marker client stays legacy).
    let req: Request = serde_json::from_str(r#"{"protocol":3,"kind":"subscribe"}"#).unwrap();
    assert!(matches!(
        req.body,
        RequestBody::Subscribe {
            lagged_markers: false
        }
    ));
    // Explicit opt-in parses through.
    let req: Request =
        serde_json::from_str(r#"{"protocol":3,"kind":"subscribe","lagged_markers":true}"#).unwrap();
    assert!(matches!(
        req.body,
        RequestBody::Subscribe {
            lagged_markers: true
        }
    ));
}

#[test]
fn v3_wire_downgrade_restores_legacy_agent_session_key() {
    let mut value = serde_json::json!({
        "agent": {
            "agent_session_id": "codex-session",
            "kind": "codex"
        }
    });
    downgrade_wire(&mut value, 3);
    assert_eq!(value["agent"]["session_id"], "codex-session");
    assert!(value["agent"].get("agent_session_id").is_none());
}

/// `is_lagged_marker` recognizes the daemon's overflow marker and
/// rejects a normal transition line.
#[test]
fn lagged_marker_is_recognized() {
    assert!(is_lagged_marker(r#"{"event":"lagged","dropped":5}"#));
    assert!(!is_lagged_marker(
        r#"{"from":"idle","to":"working","agent":{}}"#
    ));
    assert!(!is_lagged_marker("not json"));
}

/// `TransitionStream::recv` skips a lagged marker frame and returns the
/// next real `Transition`.
#[tokio::test]
async fn transition_stream_skips_lagged_marker() {
    use crate::event::{AgentEvent, AgentId, AgentKind};
    use time::OffsetDateTime;

    // Produce a real serialized Transition via a store.
    let store = Store::shared();
    let mut rx = store.subscribe();
    store
        .apply(&AgentEvent::Started {
            id: AgentId {
                tmux_socket: None,
                kind: AgentKind::ClaudeCode,
                session_id: "lag".into(),
                surface: None,
                pane: Some("%1".into()),
                cwd: None,
            },
            at: OffsetDateTime::now_utc(),
        })
        .await;
    let transition = rx.recv().await.unwrap();
    let transition_json = serde_json::to_string(&transition).unwrap();

    // Wire the marker + transition into a TransitionStream's reader.
    let (client_side, server_side) = Stream::pair().await.unwrap();
    let (cr, _cw) = client_side.into_split();
    let mut ts = TransitionStream {
        reader: BufReader::new(cr),
        line: String::new(),
    };
    let (_sr, mut sw) = server_side.into_split();
    let mut payload = String::from(r#"{"event":"lagged","dropped":3}"#);
    payload.push('\n');
    payload.push_str(&transition_json);
    payload.push('\n');
    sw.write_all(payload.as_bytes()).await.unwrap();
    sw.flush().await.unwrap();

    let got = ts
        .recv()
        .await
        .expect("recv ok")
        .expect("a transition after the skipped marker");
    assert_eq!(got.to, transition.to);
    assert_eq!(got.agent.session_id, "lag");
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one end-to-end flow verifies selector stream membership
async fn fleet_snapshot_selector_and_command_round_trip_over_ipc() {
    use crate::fleet::{
        FleetHostSnapshot, FleetHostState, FleetOperation, FleetRuntime, FleetStore,
        HostAccessMode, FLEET_PROTOCOL_VERSION,
    };

    let dir = tempdir().unwrap();
    let socket = dir.path().join("fleet-ipc.sock");
    let fleet_store = Arc::new(FleetStore::new());
    fleet_store
        .upsert_host(FleetHostSnapshot {
            alias: "dev".into(),
            local: false,
            ssh_target: "devbox".into(),
            labels: std::collections::BTreeMap::from([(
                "environment".into(),
                "development".into(),
            )]),
            annotations: std::collections::BTreeMap::new(),
            mode: HostAccessMode::Control,
            state: FleetHostState::Online,
            node_id: None,
            hostname: Some("devbox".into()),
            os: Some("linux".into()),
            arch: Some("x86_64".into()),
            tend_version: Some(env!("CARGO_PKG_VERSION").into()),
            protocol: Some(FLEET_PROTOCOL_VERSION),
            capabilities: Vec::new(),
            daemon_generation: Some(0),
            boot_id: Some("boot".into()),
            latency_ms: Some(3),
            last_seen_at: Some(OffsetDateTime::now_utc()),
            received_at: Some(OffsetDateTime::now_utc()),
            error: None,
            remote: None,
        })
        .await;
    let mut production = fleet_store.snapshot().await.hosts[0].clone();
    production.alias = "prod".into();
    production.ssh_target = "prodbox".into();
    production
        .labels
        .insert("environment".into(), "production".into());
    fleet_store.upsert_host(production).await;
    let (runtime, mut commands) = FleetRuntime::new(fleet_store.clone());
    let command_task = tokio::spawn(async move {
        let command = commands.recv().await.expect("fleet command");
        assert_eq!(command.host, "dev");
        assert!(matches!(command.operation, FleetOperation::Refresh));
        let _ = command
            .reply
            .send(Ok(FleetCommandResult::accepted("refreshed")));
    });
    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);
    let server = Server::new(socket.clone(), Store::shared()).with_fleet(runtime);
    let server_task = tokio::spawn(async move { server.run(shutdown_rx).await.unwrap() });
    wait_for_socket(&socket).await;

    let client = Client::new(socket);
    let mut updates = client
        .fleet_subscribe(Some("environment=development"))
        .await
        .expect("fleet update subscription");
    fleet_store
        .mutate_host("prod", |host| host.latency_ms = Some(8))
        .await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), updates.recv())
            .await
            .is_err()
    );
    fleet_store
        .mutate_host("dev", |host| host.latency_ms = Some(4))
        .await;
    let update = tokio::time::timeout(Duration::from_secs(1), updates.recv())
        .await
        .expect("fleet update timeout")
        .expect("fleet update read")
        .expect("fleet update stream closed");
    assert_eq!(update.host, "dev");
    assert_eq!(update.state, FleetHostState::Online);
    fleet_store
        .mutate_host("dev", |host| {
            host.labels
                .insert("environment".into(), "production".into());
        })
        .await;
    let leaving = tokio::time::timeout(Duration::from_secs(1), updates.recv())
        .await
        .expect("selector leaving update timeout")
        .expect("selector leaving update read")
        .expect("fleet update stream closed");
    assert_eq!(leaving.host, "dev");
    fleet_store
        .mutate_host("dev", |host| host.latency_ms = Some(5))
        .await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), updates.recv())
            .await
            .is_err()
    );
    fleet_store
        .mutate_host("dev", |host| {
            host.labels
                .insert("environment".into(), "development".into());
        })
        .await;
    let entering = tokio::time::timeout(Duration::from_secs(1), updates.recv())
        .await
        .expect("selector entering update timeout")
        .expect("selector entering update read")
        .expect("fleet update stream closed");
    assert_eq!(entering.host, "dev");
    let selected = client
        .fleet_snapshot(Some("environment=development"))
        .await
        .expect("fleet snapshot");
    assert_eq!(selected.hosts.len(), 1);
    let excluded = client
        .fleet_snapshot(Some("environment=staging"))
        .await
        .expect("filtered fleet snapshot");
    assert!(excluded.hosts.is_empty());
    let result = client
        .fleet_execute("dev", &FleetOperation::Refresh)
        .await
        .expect("fleet command");
    assert_eq!(result.message.as_deref(), Some("refreshed"));

    command_task.await.unwrap();
    drop(updates);
    fleet_store
        .mutate_host("dev", |host| host.latency_ms = Some(6))
        .await;
    tokio::task::yield_now().await;
    let _ = shutdown_tx.send(());
    server_task.await.unwrap();
}

#[test]
fn daemon_boot_id_is_random_shaped_and_stable_for_the_process() {
    let first = daemon_boot_id();
    assert_eq!(first.len(), 32, "{first}");
    assert!(first.chars().all(|c| c.is_ascii_hexdigit()), "{first}");
    assert_eq!(daemon_boot_id(), first);
    assert!(
        std::ptr::eq(first, daemon_boot_id()),
        "drawn once per process"
    );
}

#[test]
fn hello_response_serializes_boot_id_and_capability() {
    let value = serde_json::to_value(Response::hello(None)).unwrap();
    assert_eq!(value["boot_id"], daemon_boot_id());
    assert!(
        value["generation"].is_null(),
        "no controller, no generation"
    );
    assert!(value["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|cap| cap == DAEMON_BOOT_ID_CAPABILITY));
    assert!(CAPABILITIES.contains(&DAEMON_BOOT_ID_CAPABILITY));
    // Every other response kind keeps the field off the wire.
    let plain = serde_json::to_value(Response::ok()).unwrap();
    assert!(plain.get("boot_id").is_none());
}

#[tokio::test]
async fn hello_from_a_daemon_without_boot_id_still_decodes() {
    // A pre-`daemon_boot_id_v1` daemon: hand-rolled hello with no boot_id.
    let dir = tempdir().unwrap();
    let sock = dir.path().join("tend-old-hello.sock");
    let listener = crate::transport::Listener::bind(&sock).unwrap();
    let server = tokio::spawn(async move {
        let stream = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut line = String::new();
        // The per-connection handshake, then the hello itself.
        reader.read_line(&mut line).await.unwrap();
        writer.write_all(b"{\"ok\":true}\n").await.unwrap();
        writer.flush().await.unwrap();
        line.clear();
        reader.read_line(&mut line).await.unwrap();
        let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(request["kind"], "hello");
        let reply = serde_json::json!({
            "ok": true,
            "protocol": PROTOCOL_VERSION,
            "min_protocol": 1,
            "max_protocol": PROTOCOL_VERSION,
            "capabilities": ["restart"],
            "generation": 0,
            "version": "0.0.1",
        });
        let mut bytes = serde_json::to_vec(&reply).unwrap();
        bytes.push(b'\n');
        writer.write_all(&bytes).await.unwrap();
        writer.flush().await.unwrap();
    });
    let hello = Client::new(sock)
        .hello(Duration::from_secs(2))
        .await
        .expect("an old hello still decodes");
    assert_eq!(hello.boot_id, None);
    assert_eq!(hello.generation, Some(0));
    server.await.unwrap();
}
