use super::*;

/// Every named session must resolve to exactly one live agent; one bad
/// id refuses the batch.
#[test]
fn purpose_promotion_requires_exact_live_sessions() {
    let topology = promote_topology(&[
        ("rmux:%5", "worker"),
        ("rmux:%6", "twin"),
        ("rmux:%7", "twin"),
    ]);
    let ids = |ids: &[&str]| ids.iter().map(|id| (*id).to_string()).collect::<Vec<_>>();
    let targets = promote_purpose_targets(&topology, &ids(&["worker"])).unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].pane, "rmux:%5");
    for bad in [ids(&["worker", "missing"]), ids(&["twin"]), ids(&[])] {
        assert!(promote_purpose_targets(&topology, &bad).is_err(), "{bad:?}");
    }
}

#[test]
fn decode_agents_surfaces_a_refusal_instead_of_an_empty_registry() {
    // The regression this guards: an `ok:false` response carries no
    // `agents` array, and the lenient decoder read that absence as "no
    // agents". A refusal must not be able to impersonate an answer.
    let resp = serde_json::json!({ "ok": false, "error": "store unavailable" });
    let error = decode_agents(&resp).expect_err("a refusal must not decode as agents");
    assert!(
        matches!(&error, RuntimeError::Daemon(message) if message == "store unavailable"),
        "expected the daemon's own message, got: {error}"
    );
}

#[test]
fn decode_agents_explains_a_protocol_mismatch() {
    // Two bare version numbers do not tell the reader which half is stale
    // or what to do about it, and a mixed install answers this to every
    // single call — so this is the one refusal that earns a hint.
    let resp = serde_json::json!({
        "ok": false,
        "error": "protocol mismatch: server=4 client=5",
    });
    let error = decode_agents(&resp).expect_err("a mismatch must not decode as agents");
    let message = error.to_string();
    assert!(
        message.contains("protocol mismatch: server=4 client=5"),
        "{message}"
    );
    assert!(message.contains("older than this CLI"), "{message}");
    // The remedy has to be the one that actually works from any install:
    // the fresh binary is already on disk, only the process is stale.
    assert!(message.contains("muxa daemon restart"), "{message}");
}

#[test]
fn a_refusal_names_the_protocol_to_step_down_to() {
    // The exact sentence an out-of-range daemon returns. Parsing its upper
    // bound is what lets `hello` reach a daemon too old to answer at this
    // build's protocol — the daemon a caller most needs to restart.
    assert_eq!(
        server_protocol_ceiling(Some("unsupported protocol: server supports [1,4] client=6")),
        Some(4)
    );
}

#[test]
fn only_an_out_of_range_refusal_offers_a_ceiling() {
    // A protocol mismatch is a different failure with a different remedy,
    // and re-asking at a parsed-out number would be guessing.
    assert_eq!(
        server_protocol_ceiling(Some("protocol mismatch: server=4 client=6")),
        None
    );
    assert_eq!(server_protocol_ceiling(None), None);
    assert_eq!(
        server_protocol_ceiling(Some("unsupported protocol: mangled")),
        None
    );
}

#[test]
fn hello_reports_no_skew_against_a_matching_daemon() {
    let hello = Hello {
        collaboration_error: None,
        capabilities: Vec::new(),
        generation: None,
        protocol: PROTOCOL_VERSION,
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
    };
    assert_eq!(hello.version_skew(), None);
}

#[test]
fn hello_reports_skew_against_a_different_build() {
    // The case protocol negotiation cannot see: both halves speak the same
    // wire format, and the daemon is still months of logic behind.
    let hello = Hello {
        collaboration_error: None,
        capabilities: Vec::new(),
        generation: None,
        protocol: PROTOCOL_VERSION,
        version: Some("0.0.1-ancient".to_string()),
    };
    let skew = hello.version_skew().expect("a differing build is skew");
    assert_eq!(skew.daemon.as_deref(), Some("0.0.1-ancient"));
    assert_eq!(skew.client, env!("CARGO_PKG_VERSION"));
    let described = skew.to_string();
    assert!(described.contains("0.0.1-ancient"), "{described}");
    assert!(described.contains("muxa daemon restart"), "{described}");
}

#[test]
fn hello_treats_a_silent_daemon_as_skewed() {
    // A daemon old enough to predate the `version` field has, by that fact
    // alone, established that it is older than this client.
    let hello = Hello {
        collaboration_error: None,
        capabilities: Vec::new(),
        generation: None,
        protocol: PROTOCOL_VERSION,
        version: None,
    };
    let skew = hello.version_skew().expect("a silent daemon is skew");
    assert_eq!(skew.daemon, None);
    assert_eq!(skew.daemon_label(), "an older build");
}

#[test]
fn decode_agents_keeps_a_genuinely_empty_registry_empty() {
    // The other half of the distinction: a successful response really can
    // carry no agents, and that must still read as none rather than as an
    // error. Both the explicit empty array and an absent field count.
    assert!(
        decode_agents(&serde_json::json!({ "ok": true, "agents": [] }))
            .expect("an empty registry is a valid answer")
            .is_empty()
    );
    assert!(decode_agents(&serde_json::json!({ "ok": true }))
        .expect("an absent agents field is a valid answer")
        .is_empty());
}

#[test]
fn response_error_falls_back_when_the_daemon_sends_no_message() {
    let error = response_error(&serde_json::json!({ "ok": false }));
    assert_eq!(error.to_string(), "request failed");
}

#[tokio::test]
async fn agent_directory_round_trip_shares_cache_across_clients_and_refreshes_on_demand() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("directory.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "lookup-session", AgentKind::ClaudeCode).await;
    let counting =
        CountingPaneBackend::new(HostKind::Tmux, vec![collaboration_test_pane("%1", "0")]);
    let backend: SharedBackend = counting.clone();
    let server = Server::new(socket.clone(), store).with_backends(vec![backend]);
    let (shutdown, rx) = broadcast::channel(1);
    let serving = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&socket).await;
    let first = Client::new(socket.clone());
    let second = Client::new(socket);
    let a = first
        .agent_find("lookup-session", 10, &[], false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a.total, 1);
    assert!(!a.freshness.unwrap().reused_index);
    let b = second
        .agent_find("lookup-session", 10, &[], false)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(b.total, 1);
    assert!(b.freshness.unwrap().reused_index);
    assert_eq!(counting.counts(), (1, 0, 0));
    second
        .agent_find("lookup-session", 10, &[], true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(counting.counts(), (2, 0, 0));
    shutdown.send(()).unwrap();
    serving.await.unwrap();
}

#[tokio::test]
async fn filtered_expiry_downgrade_between_connections_never_becomes_broad_expiry() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("expiry-downgrade.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut first = BufReader::new(stream);
        for _ in 0..2 {
            let mut line = String::new();
            first.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&line).unwrap()["kind"],
                "hello"
            );
            first.get_mut().write_all(b"{\"ok\":true,\"protocol\":6,\"capabilities\":[\"collaboration_expire_filtered_v2\"]}\n").await.unwrap();
        }
        let (stream, _) = listener.accept().await.unwrap();
        let mut old = BufReader::new(stream);
        let mut line = String::new();
        old.read_line(&mut line).await.unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&line).unwrap()["kind"],
            "hello"
        );
        old.get_mut()
            .write_all(
                b"{\"ok\":true,\"protocol\":6,\"capabilities\":[\"collaboration_expire_v1\"]}\n",
            )
            .await
            .unwrap();
        line.clear();
        if old.read_line(&mut line).await.unwrap() == 0 {
            return;
        }
        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            request["kind"], "collaboration_expire_filtered",
            "a legacy daemon must never receive its broad expiry operation"
        );
        old.get_mut()
            .write_all(
                b"{\"ok\":false,\"error\":\"unknown variant collaboration_expire_filtered\"}\n",
            )
            .await
            .unwrap();
    });
    let result = Client::new(socket)
        .collaboration_expire_filtered(86400, false, true, Some("quiet-notice"))
        .await;
    assert!(result.is_err());
    server.await.unwrap();
}

#[tokio::test]
async fn filtered_expiry_refuses_legacy_daemon_before_any_mutation() {
    for (dry_run, quiet_only, request_id) in [
        (true, true, Some("notice")),
        (false, true, Some("notice")),
        (true, false, None),
        (false, false, None),
        (false, false, Some("notice")),
    ] {
        let dir = tempdir().unwrap();
        let socket = dir.path().join("legacy-expiry.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            for _ in 0..2 {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&line).unwrap()["kind"],
                    "hello"
                );
                reader.get_mut().write_all(b"{\"ok\":true,\"protocol\":6,\"capabilities\":[\"collaboration_expire_v1\",\"collaboration_expire_filtered_v1\"]}\n").await.unwrap();
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err(),
                "must not fall back to broad expiry"
            );
        });
        let result = Client::new(socket)
            .collaboration_expire_filtered(86400, dry_run, quiet_only, request_id)
            .await;
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("lacks filtered expiry"));
        server.await.unwrap();
    }
}

#[tokio::test]
async fn bounded_fleet_claim_refuses_legacy_daemon_before_any_claim() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("legacy-claim.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        // Client::hello first negotiates, then requests the hello view on
        // the same connection. Neither frame may turn into a claim.
        for _ in 0..2 {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&line).unwrap()["kind"],
                "hello"
            );
            reader
                .get_mut()
                .write_all(
                    b"{\"ok\":true,\"protocol\":6,\"capabilities\":[\"collaboration_mailbox\"]}\n",
                )
                .await
                .unwrap();
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "an old daemon must not receive an unbounded fallback claim"
        );
    });
    let origin = CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: None,
        console: false,
        agent_session_id: None,
    };
    let result = Client::new(socket)
        .collaboration_claim_recent_mailbox(&origin)
        .await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("bounded Fleet inbox claims"));
    server.await.unwrap();
}

#[tokio::test]
async fn fleet_recent_mailbox_bounds_transfer_without_losing_full_contracts() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("recent-mailbox.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "caller", AgentKind::Codex).await;
    add_collaboration_agent(&store, "%2", "peer", AgentKind::ClaudeCode).await;
    let backend: SharedBackend = CountingPaneBackend::new(
        HostKind::Tmux,
        vec![
            collaboration_test_pane("%1", "0"),
            collaboration_test_pane("%2", "1"),
        ],
    );
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
    let context = client.collaboration_context(&origin).await.unwrap();
    for index in 0..80 {
        mailbox
            .create(
                context.current.clone(),
                context.peers[0].clone(),
                collaboration::NewRequest {
                    kind: collaboration::RequestKind::Task,
                    work_mode: collaboration::WorkMode::Execute,
                    body: format!("full instructions {index}: {}", "x".repeat(1024)),
                    paths: vec!["src/allowed.rs".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    let expected = mailbox
        .list_for(&context.current, RequestMailbox::Sent, MailboxScope::Caller)
        .await
        .unwrap();
    let recent = client
        .collaboration_recent_mailbox(&origin, RequestMailbox::Sent)
        .await
        .unwrap();
    assert_eq!(recent.len(), crate::fleet::FLEET_MAILBOX_REQUEST_LIMIT);
    assert_eq!(
        recent,
        expected[..crate::fleet::FLEET_MAILBOX_REQUEST_LIMIT]
    );
    assert_eq!(recent[0].paths, ["src/allowed.rs"]);
    assert_eq!(recent[0].work_mode, collaboration::WorkMode::Execute);
    assert!(recent[0].body.len() > 1024);
    for index in 0..40 {
        mailbox
            .create(
                collaboration::Participant::console(context.current.room.clone()),
                context.current.clone(),
                collaboration::NewRequest {
                    body: format!("operator request {index}"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
    }
    let claimed = client
        .collaboration_claim_recent_mailbox(&origin)
        .await
        .unwrap();
    let delivered = FleetCommandResult::collaboration_mailbox(claimed, Vec::new());
    assert_eq!(
        delivered.collaboration_incoming.len(),
        crate::fleet::FLEET_MAILBOX_REQUEST_LIMIT
    );
    let pending = mailbox
        .list_for(
            &context.current,
            RequestMailbox::Incoming,
            MailboxScope::Caller,
        )
        .await
        .unwrap();
    assert_eq!(
        pending
            .iter()
            .filter(|r| r.status == collaboration::RequestStatus::Queued)
            .count(),
        8,
        "a capped Fleet response must not claim requests it cannot return"
    );
    shutdown.send(()).unwrap();
    serving.await.unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one live IPC fixture checks authorization, paging and measured wire cost
async fn summary_queries_enforce_scope_and_account_for_the_complete_request_line() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("summary-query.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "caller", AgentKind::Codex).await;
    let backend: SharedBackend =
        CountingPaneBackend::new(HostKind::Tmux, vec![collaboration_test_pane("%1", "0")]);
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    let audit = CollaborationAuditLog::in_memory();
    let server = Server::new(socket.clone(), store)
        .with_backends(vec![backend])
        .with_collaboration(mailbox.clone())
        .with_collaboration_audit(audit.clone());
    let (shutdown, rx) = broadcast::channel(1);
    let serving = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&socket).await;
    let client = Client::new(socket.clone());
    let origin = CollaborationOrigin {
        pane: "%1".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
        exact_session: false,
    };
    let context = client.collaboration_context(&origin).await.unwrap();
    let request = mailbox
        .create(
            context.peers[0].clone(),
            context.current.clone(),
            collaboration::NewRequest {
                body: "한글 요청".into(),
                kind: collaboration::RequestKind::Question,
                human_action: Some(collaboration::HumanAction::Choice),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut fixtures = vec![request.clone()];
    for _ in 0..64 {
        fixtures.push(
            mailbox
                .create(
                    context.peers[0].clone(),
                    context.peers[0].clone(),
                    collaboration::NewRequest {
                        body: "large instruction body ".repeat(512),
                        ..Default::default()
                    },
                )
                .await
                .unwrap(),
        );
    }
    let counts = client
        .collaboration_open_counts(&origin, MailboxScope::All)
        .await
        .unwrap();
    assert_eq!(
        counts,
        collaboration::CollaborationOpenCounts::from_requests(&fixtures)
    );
    let agent = CollaborationOrigin {
        console: false,
        ..origin.clone()
    };
    assert!(client
        .collaboration_open_counts(&agent, MailboxScope::All)
        .await
        .is_err());
    for status in [
        collaboration::RequestStatus::Queued,
        collaboration::RequestStatus::Claimed,
    ] {
        client
            .collaboration_query_view(
                &origin,
                RequestMailbox::All,
                MailboxScope::All,
                &collaboration::CollaborationQuery {
                    status: Some(status),
                    limit: Some(500),
                    ..Default::default()
                },
                0,
                collaboration::CollaborationReadView::Metadata,
            )
            .await
            .unwrap();
    }
    let raw = format!(
        "  {} \n",
        serde_json::json!({
            "protocol":PROTOCOL_VERSION, "kind":"collaboration_list", "origin":origin,
            "mailbox":"all", "scope":"all", "metadata_only":true,
            "query":{"human_only":true,"search":"한글","search_bodies":true,"limit":1}
        })
    );
    let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
    stream.write_all(raw.as_bytes()).await.unwrap();
    let mut reader = tokio::io::BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["ok"], true);
    assert_eq!(response["collaboration_page"]["total"], 1);
    assert_eq!(response["collaboration_page"]["requests"][0]["body"], "");
    let entries = audit.entries().await;
    let entry = entries
        .iter()
        .rev()
        .find(|entry| entry.operation == CollaborationAuditOperation::List)
        .unwrap();
    assert_eq!(entry.request_bytes, Some(raw.len()));
    assert_eq!(entry.response_bytes, Some(line.len()));
    assert_eq!(entry.response_body_bytes, Some(0));
    let lists: Vec<_> = entries
        .iter()
        .filter(|entry| entry.operation == CollaborationAuditOperation::List)
        .collect();
    let baseline: usize = lists[..2]
        .iter()
        .map(|entry| entry.response_bytes.unwrap())
        .sum();
    let summary_bytes = entries
        .iter()
        .find(|entry| entry.operation == CollaborationAuditOperation::Counts && entry.ok)
        .unwrap()
        .response_bytes
        .unwrap();
    assert!(baseline > summary_bytes * 10);
    assert!(baseline > line.len() * 10);
    if let Ok(path) = std::env::var("MUXA_QUERY_COST_REPORT") {
        std::fs::write(path, serde_json::to_vec_pretty(&serde_json::json!({
                "scope":"real IPC response lines on synthetic requests; excludes hello exchanges; not model tokens",
                "open_requests": fixtures.len(), "legacy_list_calls":2, "legacy_metadata_response_bytes":baseline,
                "counts_calls":1, "counts_response_bytes":summary_bytes,
                "human_calls":1, "human_response_bytes":line.len(),
                "projected_body_bytes":0, "extra_body_ratio":null,
                "human_request_wire_bytes":raw.len(),
            })).unwrap()).unwrap();
    }
    shutdown.send(()).unwrap();
    serving.await.unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one real IPC lifecycle with byte accounting and cache invalidation
async fn conditional_mailbox_reuses_legacy_display_rows_but_never_resends_bodies_or_skips_scope() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("conditional-mailbox.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "caller", AgentKind::Codex).await;
    add_collaboration_agent(&store, "%2", "peer", AgentKind::ClaudeCode).await;
    let backend: SharedBackend = CountingPaneBackend::new(
        HostKind::Tmux,
        vec![
            collaboration_test_pane("%1", "0"),
            collaboration_test_pane("%2", "1"),
        ],
    );
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    let audit = CollaborationAuditLog::in_memory();
    let server = Server::new(socket.clone(), store)
        .with_backends(vec![backend])
        .with_collaboration(mailbox.clone())
        .with_collaboration_audit(audit.clone());
    let (shutdown, rx) = broadcast::channel(1);
    let serving = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&socket).await;
    let client = Client::new(socket).with_mailbox_compat_cache();
    let origin = CollaborationOrigin {
        pane: "%1".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
        exact_session: false,
    };
    let context = client.collaboration_context(&origin).await.unwrap();
    let body = "한글\n\"".repeat(1000);
    let request = mailbox
        .create(
            context.current.clone(),
            context.peers[0].clone(),
            collaboration::NewRequest {
                body: body.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let first = client
        .collaboration_mailbox_snapshot(&origin, true, None)
        .await
        .unwrap();
    assert_eq!(first.sent[0].body, body);
    let cached = client
        .collaboration_mailbox_snapshot(&origin, true, None)
        .await
        .unwrap();
    assert!(
        !cached.unchanged,
        "legacy callers still receive their existing full display contract"
    );
    assert_eq!(cached.sent, first.sent);
    let hit = client
        .collaboration_mailbox_snapshot(&origin, true, first.token.as_deref())
        .await
        .unwrap();
    assert!(hit.unchanged && hit.sent.is_empty());
    let agent = CollaborationOrigin {
        console: false,
        ..origin.clone()
    };
    assert!(client
        .collaboration_mailbox_snapshot(&agent, true, first.token.as_deref())
        .await
        .is_err());
    mailbox
        .update_request(&context.current, &request.id, "changed guidance".into())
        .await
        .unwrap();
    let fresh = client
        .collaboration_mailbox_snapshot(&origin, true, first.token.as_deref())
        .await
        .unwrap();
    assert!(!fresh.unchanged);
    assert_eq!(fresh.sent[0].updates[0].body, "changed guidance");
    let entries = audit.entries().await;
    let reads: Vec<_> = entries
        .iter()
        .filter(|e| e.operation == CollaborationAuditOperation::List && e.ok)
        .collect();
    assert_eq!(reads.len(), 4);
    assert_eq!(reads[0].response_body_bytes, Some(body.len()));
    let encoded = serde_json::to_vec(&body).unwrap().len() - 2;
    assert_eq!(reads[0].response_body_json_bytes, Some(encoded));
    assert_eq!(
        reads[0].response_extra_bytes,
        Some(reads[0].response_bytes.unwrap() - body.len())
    );
    assert_eq!(reads[1].conditional_unchanged, Some(true));
    assert_eq!(reads[1].response_body_bytes, Some(0));
    assert!(reads[1].response_bytes.unwrap() < 300);
    assert!(reads[0].response_bytes.unwrap() > reads[1].response_bytes.unwrap() * 20);
    assert_eq!(
        reads[3].response_body_bytes,
        Some(body.len() + "changed guidance".len())
    );
    assert!(!serde_json::to_string(&entries)
        .unwrap()
        .contains("changed guidance"));
    shutdown.send(()).unwrap();
    serving.await.unwrap();
}

#[tokio::test]
async fn retirement_ipc_requires_operator_origin_and_previews_before_commit() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("retire.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "caller", AgentKind::Codex).await;
    add_collaboration_agent(&store, "%2", "peer", AgentKind::ClaudeCode).await;
    let backend: SharedBackend = CountingPaneBackend::new(
        HostKind::Tmux,
        vec![
            collaboration_test_pane("%1", "0"),
            collaboration_test_pane("%2", "1"),
        ],
    );
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    let server = Server::new(socket.clone(), store.clone())
        .with_backends(vec![backend])
        .with_collaboration(mailbox.clone());
    let (shutdown, rx) = broadcast::channel(1);
    let serving = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&socket).await;
    let client = Client::new(socket);
    let origin = CollaborationOrigin {
        pane: "%1".into(),
        socket: Some("default".into()),
        console: false,
        agent_session_id: None,
        exact_session: false,
    };
    let context = client.collaboration_context(&origin).await.unwrap();
    let request = mailbox
        .create(
            context.current.clone(),
            context.peers[0].clone(),
            collaboration::NewRequest {
                body: "retire exact interrupted review".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let mut agents = store.snapshot().await;
    for agent in &mut agents {
        if agent.session_id == "peer" {
            agent.state = crate::AgentState::Stopped;
        }
    }
    mailbox.reconcile_peer_interruptions(&agents).await.unwrap();
    assert!(client
        .collaboration_retire(&origin, &request.id, "withdrawn", "update:2", false)
        .await
        .is_err());
    let operator = CollaborationOrigin {
        console: true,
        agent_session_id: None,
        ..origin.clone()
    };
    let preview = client
        .collaboration_retire(&operator, &request.id, "withdrawn", "update:2", true)
        .await
        .unwrap();
    assert!(preview.retirement.is_some());
    assert!(!mailbox
        .list_for(&context.current, RequestMailbox::Sent, MailboxScope::Caller)
        .await
        .unwrap()[0]
        .status
        .is_terminal());
    let retired = client
        .collaboration_retire(&operator, &request.id, "withdrawn", "update:2", false)
        .await
        .unwrap();
    assert_eq!(retired.status, collaboration::RequestStatus::Cancelled);
    assert!(retired.reply.is_none());
    assert_eq!(retired.body, request.body);
    assert_eq!(
        client
            .collaboration_retire(&operator, &request.id, "withdrawn", "update:2", false)
            .await
            .unwrap(),
        retired
    );
    shutdown.send(()).unwrap();
    serving.await.unwrap();
}

#[tokio::test]
async fn collaboration_context_and_list_scan_panes_once_per_request() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("single-scan.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "caller", AgentKind::Codex).await;
    add_collaboration_agent(&store, "%2", "peer", AgentKind::ClaudeCode).await;
    let counting = CountingPaneBackend::new(
        HostKind::Tmux,
        vec![
            collaboration_test_pane("%1", "0"),
            collaboration_test_pane("%2", "1"),
        ],
    );
    let backend: SharedBackend = counting.clone();
    let server = Server::new(socket.clone(), store)
        .with_backends(vec![backend])
        .with_collaboration(CollaborationStore::in_memory(
            CollaborationOptions::default(),
        ));
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

    let context = client.collaboration_context(&origin).await.unwrap();
    assert!(!context.peers.is_empty());
    let (lists, resolves, pid_maps) = counting.counts();
    assert_eq!(
        lists, 1,
        "context: provenance and participants share one scan"
    );
    assert_eq!(
        (resolves, pid_maps),
        (0, 0),
        "tmux evidence comes from the scan"
    );

    client
        .collaboration_list(&origin, RequestMailbox::Incoming)
        .await
        .unwrap();
    assert_eq!(
        counting.counts().0,
        2,
        "list: one fresh scan for its own request"
    );

    shutdown.send(()).unwrap();
    serving.await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn worker_registration_uses_one_scan_and_keeps_worker_grants() {
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let child = Child(
        std::process::Command::new("bash")
            .args(["-c", "sleep 0.1; exec -a 'codex --no-daemon' sleep 30"])
            .spawn()
            .unwrap(),
    );
    let dir = tempdir().unwrap();
    let socket = dir.path().join("worker.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "caller", AgentKind::Codex).await;
    let mut worker = pid_pane("%2", child.0.id());
    worker.agent_program = Some("codex".into());
    let counting = CountingPaneBackend::new(
        HostKind::Tmux,
        vec![collaboration_test_pane("%1", "0"), worker],
    );
    let backend: SharedBackend = counting.clone();
    let server = Server::new(socket.clone(), store)
        .with_backends(vec![backend])
        .with_collaboration(CollaborationStore::in_memory(
            CollaborationOptions::default(),
        ));
    let (shutdown, rx) = broadcast::channel(1);
    let serving = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&socket).await;
    let client = Client::new(socket);
    let result = client
        .call(&serde_json::json!({"protocol":PROTOCOL_VERSION,
            "kind":"collaboration_worker_launch", "pane":"%2", "socket":"default",
            "agent_kind":"codex", "launch_id":uuid::Uuid::new_v4().to_string()}))
        .await
        .unwrap();
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(
        counting.counts().0,
        1,
        "registration must not rescan after its process-fenced grant"
    );
    let origin = CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: Some("default".into()),
        console: false,
        agent_session_id: None,
    };
    let context = client.collaboration_context(&origin).await.unwrap();
    let worker = context.peers.iter().find(|peer| peer.pane == "%2").unwrap();
    assert_eq!(worker.launch_purpose, collaboration::LaunchPurpose::Worker);
    shutdown.send(()).unwrap();
    serving.await.unwrap();
}

#[tokio::test]
async fn paged_list_omits_bodies_preserves_full_reads_and_unread_state() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("pages.sock");
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
    let origin = CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: Some("default".into()),
        console: false,
        agent_session_id: None,
    };
    let room = client.collaboration_context(&origin).await.unwrap();
    let sender = room.current;
    let recipient = room.peers[0].clone();
    let ids = create_private_list_history(&mailbox, &sender, &recipient).await;
    let query = collaboration::CollaborationQuery {
        limit: Some(1),
        ..Default::default()
    };
    let unread = mailbox.unread_reply_count(&sender).await;
    let page = client
        .collaboration_query(
            &origin,
            RequestMailbox::All,
            MailboxScope::Caller,
            &query,
            1,
            true,
        )
        .await
        .unwrap();
    assert_eq!(page.total, 3);
    assert!(page.has_more);
    assert_eq!(page.requests.len(), 1);
    assert_eq!(page.requests[0].id, ids[1]);
    let text = serde_json::to_string(&page).unwrap();
    assert!(!text.contains("PRIVATE"));
    assert!(
        page.requests[0].reply.is_some(),
        "receipt must retain final reply status"
    );
    assert_eq!(page.requests[0].updates.last().unwrap().sequence, 1);
    assert_eq!(mailbox.unread_reply_count(&sender).await, unread);
    let entries = audit.entries().await;
    assert!(entries.last().unwrap().response_bytes.unwrap() < 5000);
    assert_eq!(entries.last().unwrap().result_count, Some(1));
    let full = client
        .collaboration_query(
            &origin,
            RequestMailbox::All,
            MailboxScope::Caller,
            &query,
            1,
            false,
        )
        .await
        .unwrap();
    assert!(full.requests[0].body.contains("PRIVATE TASK"));
    assert!(full.requests[0]
        .reply
        .as_ref()
        .unwrap()
        .body
        .contains("PRIVATE REPLY"));
    let legacy = client
        .collaboration_list(&origin, RequestMailbox::All)
        .await
        .unwrap();
    assert_eq!(legacy.len(), 3);
    assert!(legacy
        .iter()
        .all(|request| request.body.contains("PRIVATE TASK")));
    assert_eq!(mailbox.unread_reply_count(&sender).await, unread);
    shutdown.send(()).unwrap();
    serving.await.unwrap();
    assert_legacy_list_query_fallback(legacy, &origin, &query).await;
}

#[tokio::test]
async fn auto_scope_and_read_views_keep_authority_and_unread_semantics() {
    use collaboration::CollaborationReadView as View;
    let dir = tempdir().unwrap();
    let socket = dir.path().join("views.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_collaboration_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
    let counting = CountingPaneBackend::new(
        HostKind::Tmux,
        vec![
            collaboration_test_pane("%1", "0"),
            collaboration_test_pane("%2", "1"),
            collaboration_test_pane("%9", "2"),
        ],
    );
    let backend: SharedBackend = counting.clone();
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
    let ids = create_private_list_history(&mailbox, &room.current, &room.peers[0]).await;
    let query = collaboration::CollaborationQuery {
        limit: Some(1),
        ..Default::default()
    };
    let before = counting.counts().0;
    let (page, scope) = client
        .collaboration_query_auto(&origin, RequestMailbox::All, &query, 0, View::Metadata)
        .await
        .unwrap();
    assert_eq!(scope, MailboxScope::Caller);
    assert_eq!(page.total, 3);
    assert_eq!(
        counting.counts().0 - before,
        1,
        "auto scope uses one authorized list request"
    );
    let budget_query = collaboration::CollaborationQuery {
        max_bytes: Some(64 * 1024),
        ..query.clone()
    };
    let bounded = client
        .collaboration_query_result(
            &origin,
            RequestMailbox::All,
            MailboxScope::Caller,
            &budget_query,
            0,
            View::Full,
            false,
        )
        .await
        .unwrap();
    assert!(bounded.byte_budget_supported);
    assert_eq!(bounded.page.requests.len(), 1);

    let terminal = CollaborationOrigin {
        pane: "%9".into(),
        ..origin.clone()
    };
    let (_, scope) = client
        .collaboration_query_auto(&terminal, RequestMailbox::All, &query, 0, View::Metadata)
        .await
        .unwrap();
    assert_eq!(scope, MailboxScope::Room);
    let pinned = CollaborationOrigin {
        agent_session_id: Some("missing".into()),
        ..terminal
    };
    assert!(client
        .collaboration_query_auto(&pinned, RequestMailbox::All, &query, 0, View::Metadata)
        .await
        .is_err());
    assert!(client
        .collaboration_query_view(
            &origin,
            RequestMailbox::All,
            MailboxScope::All,
            &query,
            0,
            View::Metadata
        )
        .await
        .is_err());
    assert_read_views(&client, &origin, &mailbox, &room.current, &ids).await;
    shutdown.send(()).unwrap();
    serving.await.unwrap();
}

#[test]
fn scanned_caller_evidence_matches_native_resolution_and_rejects_stale_panes() {
    let tmux: SharedBackend = CountingPaneBackend::new(HostKind::Tmux, vec![pid_pane("%3", 300)]);
    let backends = vec![tmux];
    let scan = scan_of(&backends);
    let no_parent = |_| None;

    assert_eq!(
        observed_pane_in_scan(Some("%3".into()), 1, &backends, &scan, no_parent),
        Some(("%3".into(), CollaborationPaneEvidence::ProcessEnvironment))
    );
    // A stale TMUX_PANE that no longer exists is not evidence, and an
    // unrelated process gains no pane.
    assert_eq!(
        observed_pane_in_scan(Some("%9".into()), 1, &backends, &scan, no_parent),
        None
    );
    // A stale environment falls through to the live pid inventory, which
    // names the pane the caller actually descends from now.
    let parent = |pid| (pid == 7).then_some(300);
    assert_eq!(
        observed_pane_in_scan(Some("%9".into()), 7, &backends, &scan, parent),
        Some(("%3".into(), CollaborationPaneEvidence::ProcessAncestry))
    );
    assert_eq!(
        observed_pane_in_scan(None, 300, &backends, &scan, no_parent),
        Some(("%3".into(), CollaborationPaneEvidence::ProcessAncestry))
    );
}

#[test]
fn shared_codex_app_server_caller_has_no_pane_evidence() {
    // The env pane and the ancestry both resolve, but under a shared
    // app-server they name the server's first pane, not the caller's.
    let tmux: SharedBackend = CountingPaneBackend::new(HostKind::Tmux, vec![pid_pane("%3", 300)]);
    let backends = vec![tmux];
    let scan = scan_of(&backends);
    let parent = |pid| (pid == 7).then_some(300);
    let observe = || observed_pane_in_scan(Some("%3".into()), 7, &backends, &scan, parent);
    assert_eq!(
        observed_unless_borrowed(false, observe),
        Some(("%3".into(), CollaborationPaneEvidence::ProcessEnvironment))
    );
    assert_eq!(observed_unless_borrowed(true, observe), None);
}

#[test]
fn scanned_evidence_keeps_backend_specific_resolution() {
    // rmux lists namespaced ids; a bare TMUX_PANE resolves through them.
    let rmux: SharedBackend =
        CountingPaneBackend::new(HostKind::Rmux, vec![pid_pane("rmux:%5", 0)]);
    let rmux_backends = vec![rmux];
    let scan = scan_of(&rmux_backends);
    assert_eq!(
        observed_pane_in_scan(Some("%5".into()), 1, &rmux_backends, &scan, |_| None),
        Some((
            "rmux:%5".into(),
            CollaborationPaneEvidence::ProcessEnvironment
        ))
    );
    // tmux ids are exact: the same bare id does not match a namespaced one.
    let tmux: SharedBackend =
        CountingPaneBackend::new(HostKind::Tmux, vec![pid_pane("rmux:%5", 0)]);
    let tmux_backends = vec![tmux];
    let scan = scan_of(&tmux_backends);
    assert_eq!(
        observed_pane_in_scan(Some("%5".into()), 1, &tmux_backends, &scan, |_| None),
        None
    );

    // cmux keeps its native resolver and its own (empty) pid map: a listed
    // surface with a pid is not ancestry evidence.
    let cmux = CountingPaneBackend::new(HostKind::Cmux, vec![pid_pane("cmux:s1", 400)]);
    let cmux_backends: Vec<SharedBackend> = vec![cmux.clone()];
    let scan = scan_of(&cmux_backends);
    assert_eq!(
        observed_pane_in_scan(Some("cmux:s1".into()), 400, &cmux_backends, &scan, |_| None),
        Some((
            "cmux:s1".into(),
            CollaborationPaneEvidence::ProcessEnvironment
        ))
    );
    assert_eq!(
        observed_pane_in_scan(None, 400, &cmux_backends, &scan, |_| None),
        None
    );
    assert_eq!(
        cmux.counts(),
        (1, 1, 1),
        "one scan; native resolve and pid map"
    );
}

#[test]
fn observed_environment_is_canonical_and_cross_host_or_socket_ambiguity_is_not_evidence() {
    let rmux: SharedBackend =
        CountingPaneBackend::new(HostKind::Rmux, vec![pid_pane("rmux:%5", 0)]);
    let tmux: SharedBackend = CountingPaneBackend::new(HostKind::Tmux, vec![pid_pane("%5", 0)]);
    let backends = vec![rmux, tmux];
    let scan = scan_of(&backends);
    assert_eq!(
        observed_pane_in_scan(Some("%5".into()), 1, &backends, &scan, |_| None),
        None
    );
    let mut one = pid_pane("rmux:%5", 0);
    one.socket = Some("one".into());
    let mut two = one.clone();
    two.socket = Some("two".into());
    let backends: Vec<SharedBackend> =
        vec![CountingPaneBackend::new(HostKind::Rmux, vec![one, two])];
    let scan = scan_of(&backends);
    assert!(scan.resolve_unique(&backends, "%5").is_none());
    let mut actor = CollaborationConnectionActor {
        client_kind: CollaborationClientKind::Mcp,
        caller_pid: None,
        caller_uid: None,
        caller_gid: None,
        executable: None,
        observed_pane: Some("rmux:%5".into()),
        pane_evidence: Some(CollaborationPaneEvidence::ProcessEnvironment),
        pane_observed: true,
        request_panes: None,
    };
    let origin = CollaborationOrigin {
        exact_session: false,
        pane: "rmux:%5".into(),
        socket: None,
        console: false,
        agent_session_id: None,
    };
    assert_eq!(
        actor.provenance(&origin).origin_match,
        CollaborationOriginMatch::Matched
    );
    actor.observed_pane = Some("%5".into());
    assert_eq!(
        actor.provenance(&origin).origin_match,
        CollaborationOriginMatch::Mismatched
    );
}

#[cfg(unix)]
#[tokio::test]
async fn restart_preflight_refusal_does_not_signal_shutdown_and_success_pins_target() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("muxad");
    let config = dir.path().join("cfg");
    std::fs::write(&config, "").unwrap();
    std::fs::write(&target, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (tx, mut rx) = broadcast::channel(4);
    let controller = RestartController::new(0, tx).with_upgrade_check(Some(
        crate::binary_upgrade::UpgradeCheck::new(
            target.clone(),
            vec!["--config".into(), config.into_os_string()],
        ),
    ));
    assert!(controller.request_checked_restart().await.is_err());
    assert!(!controller.restart_requested());
    assert!(rx.try_recv().is_err());
    std::fs::write(&target, "#!/bin/sh\nexit 0\n").unwrap();
    assert!(controller.request_checked_restart().await.unwrap());
    assert!(rx.try_recv().is_ok());
    assert_eq!(controller.validated_launch().await.unwrap().target, target);
    controller.stop();
    assert!(!controller.request_checked_restart().await.unwrap());
}

#[tokio::test]
async fn observed_scan_is_handed_off_once() {
    let backend: SharedBackend =
        CountingPaneBackend::new(HostKind::Tmux, vec![pid_pane("%3", 300)]);
    let mut actor = CollaborationConnectionActor {
        client_kind: CollaborationClientKind::Cli,
        caller_pid: Some(std::process::id()),
        caller_uid: None,
        caller_gid: None,
        executable: None,
        observed_pane: None,
        pane_evidence: None,
        pane_observed: false,
        request_panes: None,
    };
    actor.observe_pane(std::slice::from_ref(&backend)).await;
    assert_eq!(actor.take_request_panes().map(|panes| panes.len()), Some(1));
    assert!(actor.take_request_panes().is_none());
    // A connection observes its caller once; later requests scan afresh.
    actor.observe_pane(&[backend]).await;
    assert!(actor.take_request_panes().is_none());
}

/// Two tmux servers each have a `%1`. A caller naming its server gets
/// that server's room, and one naming a server the scan does not cover
/// gets none — not the other server's room by pane id alone.
#[test]
fn a_named_server_picks_its_own_pane_from_equal_ids() {
    let mut other = collaboration_test_pane("%1", "0");
    other.socket = Some("other".into());
    other.window_id = "@9".into();
    let mut own = collaboration_test_pane("%1", "0");
    own.socket = Some("restore".into());
    own.window_id = "@2".into();
    let mut topology = promote_topology(&[]);
    topology.panes = vec![other, own];

    let room = topology
        .room_of("%1", Some("/tmp/tmux-1000/restore"))
        .expect("the named server has a %1");
    assert_eq!(room.window_id, "@2");
    assert_eq!(room.socket.as_deref(), Some("restore"));
    assert!(topology
        .room_of("%1", Some("/tmp/tmux-1000/elsewhere"))
        .is_none());
    // Callers that name no server keep the old first-match behaviour.
    assert_eq!(topology.room_of("%1", None).unwrap().window_id, "@9");
}

#[tokio::test]
async fn collaboration_wait_falls_back_for_legacy_daemon() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("legacy-collaboration.sock");
    let listener = Listener::bind(&socket).unwrap();
    let terminal_request = serde_json::json!({
        "id": "legacy-request",
        "from": {
            "agent_kind": "codex",
            "agent_session_id": "sender",
            "pane": "%1",
            "socket": "default",
            "room": { "host": "tmux", "socket": "default", "window_id": "@1" },
            "state": "idle"
        },
        "to": {
            "agent_kind": "claude_code",
            "agent_session_id": "recipient",
            "pane": "%2",
            "socket": "default",
            "room": { "host": "tmux", "socket": "default", "window_id": "@1" },
            "state": "idle"
        },
        "kind": "question",
        "body": "legacy",
        "expects_reply": true,
        "work_mode": "read_only",
        "status": "completed",
        "created_at": "2026-08-25T00:00:00Z",
        "reply": {
            "status": "completed",
            "body": "done",
            "at": "2026-08-25T00:00:01Z"
        }
    });
    let server = tokio::spawn(async move {
        for expected_kind in ["collaboration_wait", "collaboration_get"] {
            let stream = listener.accept().await.unwrap();
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            writer.write_all(b"{\"ok\":true}\n").await.unwrap();
            writer.flush().await.unwrap();

            line.clear();
            reader.read_line(&mut line).await.unwrap();
            let request: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
            assert_eq!(request["kind"], expected_kind);
            let response = if expected_kind == "collaboration_wait" {
                serde_json::json!({
                    "ok": false,
                    "error": "bad request: unknown variant `collaboration_wait`, expected `collaboration_get`"
                })
            } else {
                serde_json::json!({
                    "ok": true,
                    "collaboration_request": terminal_request
                })
            };
            let mut bytes = serde_json::to_vec(&response).unwrap();
            bytes.push(b'\n');
            writer.write_all(&bytes).await.unwrap();
            writer.flush().await.unwrap();
        }
    });

    let client = Client::new(socket);
    let origin = CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: Some("default".into()),
        console: false,
        agent_session_id: None,
    };
    let request = client
        .collaboration_wait(&origin, "legacy-request", 2)
        .await
        .unwrap();
    assert_eq!(request.status, RequestStatus::Completed);
    assert_eq!(request.reply.unwrap().body, "done");
    server.await.unwrap();
}

#[test]
fn collaboration_wait_fallback_only_matches_unknown_request_kind() {
    assert!(collaboration_wait_is_unsupported(
        "bad request: unknown variant `collaboration_wait`, expected `collaboration_get`"
    ));
    assert!(!collaboration_wait_is_unsupported(
        "collaboration wait failed: request not found"
    ));
}

#[test]
fn caller_pane_mismatch_is_audit_evidence_not_a_refusal() {
    let actor = CollaborationConnectionActor {
        client_kind: CollaborationClientKind::Cli,
        caller_pid: Some(77),
        caller_uid: Some(1000),
        caller_gid: Some(1000),
        executable: Some("muxa".into()),
        observed_pane: Some("%9".into()),
        pane_evidence: Some(CollaborationPaneEvidence::ProcessEnvironment),
        pane_observed: true,
        request_panes: None,
    };
    let provenance = actor.provenance(&CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: Some("default".into()),
        console: false,
        agent_session_id: None,
    });
    assert_eq!(
        provenance.origin_match,
        CollaborationOriginMatch::Mismatched
    );
    assert_eq!(provenance.observed_pane.as_deref(), Some("%9"));
}

#[test]
fn a_list_request_without_a_scope_stays_caller_scoped() {
    // Every muxa built before scoped listing omits the field. Defaulting
    // it to anything wider would hand those clients other agents' mail.
    let body: RequestBody = serde_json::from_value(serde_json::json!({
        "kind": "collaboration_list",
        "origin": { "pane": "%1", "socket": "default" },
        "mailbox": "incoming",
    }))
    .expect("an old client's list request still parses");
    match body {
        RequestBody::CollaborationList { scope, .. } => {
            assert_eq!(scope, MailboxScope::Caller);
        }
        other => panic!("expected a collaboration_list request, got {other:?}"),
    }
}

#[tokio::test]
async fn request_updates_round_trip_between_fleet_console_and_recipient() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("request-updates.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "recipient", AgentKind::Codex).await;
    add_collaboration_agent(&store, "%2", "unrelated", AgentKind::ClaudeCode).await;
    let backend: SharedBackend = Arc::new(CollaborationTestBackend {
        panes: vec![
            collaboration_test_pane("%1", "0"),
            collaboration_test_pane("%2", "1"),
        ],
    });
    let audit = CollaborationAuditLog::in_memory();
    let server = Server::new(socket.clone(), store)
        .with_backends(vec![backend])
        .with_collaboration(CollaborationStore::in_memory(
            CollaborationOptions::default(),
        ))
        .with_collaboration_audit(audit.clone());
    let (shutdown, rx) = broadcast::channel(1);
    let serving = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&socket).await;
    let client = Client::new(socket);
    let origin = |pane: &str, console| CollaborationOrigin {
        exact_session: false,
        pane: pane.into(),
        socket: Some("default".into()),
        console,
        agent_session_id: None,
    };
    let request = client
        .collaboration_send(
            &origin("%1", true),
            "pane:%1",
            &NewRequest {
                task_contract: None,
                supersedes_request_id: None,
                body: "review".into(),
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();
    assert!(request.from.console);
    assert!(
        request.to_slot.is_none(),
        "a pane target pins a session, no slot"
    );
    client
        .collaboration_inbox(&origin("%1", false))
        .await
        .unwrap();
    let progress = client
        .collaboration_update(&origin("%1", false), &request.id, "Tests running")
        .await
        .unwrap();
    assert_eq!(progress.status, RequestStatus::Claimed);
    assert!(progress.reply.is_none());
    let guidance = client
        .collaboration_update(&origin("%1", true), &request.id, "Include the retry path")
        .await
        .unwrap();
    assert_eq!(guidance.updates.len(), 2);
    assert!(client
        .collaboration_update(&origin("%2", false), &request.id, "unrelated")
        .await
        .is_err());
    let completed = client
        .collaboration_reply(
            &origin("%1", false),
            &request.id,
            RequestStatus::Completed,
            "verified",
            &[],
            &[],
        )
        .await
        .unwrap();
    assert_eq!(completed.updates[0].body, "Tests running");
    assert_eq!(completed.updates[1].author.pane, "console");
    assert!(client
        .collaboration_update(&origin("%1", true), &request.id, "late")
        .await
        .is_err());
    let entries = audit.entries().await;
    assert!(entries
        .iter()
        .all(|entry| entry.response_bytes.is_some_and(|bytes| bytes > 0)
            && entry.elapsed_us.is_some()));
    assert_eq!(
        entries
            .iter()
            .filter(|e| e.operation == CollaborationAuditOperation::Update && e.ok)
            .count(),
        2
    );
    shutdown.send(()).unwrap();
    serving.await.unwrap();
}

#[tokio::test]
async fn progress_peek_over_ipc_preserves_unread_reply_and_checks_participation() {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("peek.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_collaboration_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
    add_collaboration_agent(&store, "%3", "stranger", AgentKind::GeminiCli).await;
    let backend: SharedBackend = Arc::new(CollaborationTestBackend {
        panes: vec![
            collaboration_test_pane("%1", "0"),
            collaboration_test_pane("%2", "1"),
            collaboration_test_pane("%3", "2"),
        ],
    });
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    let audit = CollaborationAuditLog::in_memory();
    let server = Server::new(socket.clone(), store)
        .with_backends(vec![backend])
        .with_collaboration(mailbox.clone())
        .with_collaboration_audit(audit.clone());
    let (shutdown, rx) = broadcast::channel(1);
    let daemon = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&socket).await;
    let client = Client::new(socket);
    let origin = |pane: &str| CollaborationOrigin {
        exact_session: false,
        pane: pane.into(),
        socket: Some("default".into()),
        console: false,
        agent_session_id: None,
    };
    let request = client
        .collaboration_send(
            &origin("%1"),
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
    client.collaboration_inbox(&origin("%2")).await.unwrap();
    client
        .collaboration_reply(
            &origin("%2"),
            &request.id,
            RequestStatus::Completed,
            "final report",
            &[],
            &[],
        )
        .await
        .unwrap();
    let peek = client
        .collaboration_peek(&origin("%1"), &request.id)
        .await
        .unwrap();
    assert!(peek.reply_read_at.is_none());
    assert!(peek.reply_notified_at.is_none());
    assert_eq!(mailbox.unread_reply_count(&request.from).await, 1);
    assert_eq!(mailbox.pending_reply_unnotified().await.len(), 1);
    assert!(client
        .collaboration_peek(&origin("%3"), &request.id)
        .await
        .is_err());
    assert!(audit
        .entries()
        .await
        .iter()
        .any(|entry| entry.operation == CollaborationAuditOperation::Peek && entry.ok));
    assert!(client
        .collaboration_get(&origin("%1"), &request.id)
        .await
        .unwrap()
        .reply_read_at
        .is_some());
    shutdown.send(()).unwrap();
    daemon.await.unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // explicit wire-compatibility request fixtures
async fn widening_a_listing_over_ipc_is_console_only() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("muxa-collaboration-scope.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_collaboration_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
    add_collaboration_agent(&store, "%3", "elsewhere", AgentKind::GeminiCli).await;
    add_collaboration_agent(&store, "%4", "elsewhere-peer", AgentKind::Codex).await;
    // %3 and %4 sit in a second window, so they form a room of their own.
    let other_window = |pane_id: &str, pane_index: &str| PaneInfo {
        window_id: "@2".into(),
        window_name: "other".into(),
        window_index: "1".into(),
        ..collaboration_test_pane(pane_id, pane_index)
    };
    let backend: SharedBackend = Arc::new(CollaborationTestBackend {
        panes: vec![
            collaboration_test_pane("%1", "0"),
            collaboration_test_pane("%2", "1"),
            other_window("%3", "0"),
            other_window("%4", "1"),
        ],
    });
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    let server = Server::new(sock.clone(), store)
        .with_backends(vec![backend])
        .with_collaboration(mailbox.clone());
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let client = Client::new(sock).with_collaboration_client_kind(CollaborationClientKind::Watch);
    let origin = |pane: &str, console: bool| CollaborationOrigin {
        exact_session: false,
        pane: pane.into(),
        socket: Some("default".into()),
        console,
        agent_session_id: None,
    };
    let ask = |from: CollaborationOrigin, to: &'static str, body: &'static str| {
        let client = client.clone();
        async move {
            client
                .collaboration_send(
                    &from,
                    to,
                    &NewRequest {
                        user_authorized: false,
                        task_contract: None,
                        supersedes_request_id: None,
                        dispatch_id: None,
                        source_node_id: None,
                        delegation_parent: None,
                        human_action: None,
                        initiator: None,
                        kind: collaboration::RequestKind::Question,
                        body: body.into(),
                        expects_reply: true,
                        notify: None,
                        work_mode: collaboration::WorkMode::ReadOnly,
                        thread_id: None,
                        parent_request_id: None,
                        workspace_id: None,
                        work_id: None,
                        run_id: None,
                        paths: Vec::new(),
                        artifacts: Vec::new(),
                        links: Vec::new(),
                        air_artifacts: Vec::new(),
                        withdraw_reply_wakes: Vec::new(),
                        recipient_slot: None,
                    },
                )
                .await
                .unwrap();
        }
    };
    ask(origin("%1", false), "%2", "inside window one").await;
    ask(origin("%3", false), "%4", "inside window two").await;

    // The console dispatched neither, so its own mailbox is empty.
    let console = origin("%1", true);
    assert!(client
        .collaboration_list(&console, RequestMailbox::All)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        client
            .collaboration_list_scoped(&console, RequestMailbox::All, MailboxScope::Room)
            .await
            .unwrap()
            .len(),
        1
    );
    let fleet = client
        .collaboration_list_scoped(&console, RequestMailbox::All, MailboxScope::All)
        .await
        .unwrap();
    assert_eq!(fleet.len(), 2);
    // Every row carries the window it happened in, which is what a
    // fleet-wide listing has to render.
    let windows: HashSet<_> = fleet
        .iter()
        .map(|request| request.from.room.window_id.clone())
        .collect();
    assert_eq!(windows.len(), 2);

    // The same call from an agent is refused, room-mate or not.
    let denied = client
        .collaboration_list_scoped(&origin("%1", false), RequestMailbox::All, MailboxScope::All)
        .await
        .expect_err("an agent may not read the fleet's mail");
    assert!(
        denied.to_string().contains("operator-console"),
        "unexpected error: {denied}"
    );

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
#[allow(clippy::too_many_lines)] // one IPC lifecycle verifies send, reply, read, and cancellation state
async fn collaboration_round_trip_over_ipc_tracks_reply_and_cancellation() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("muxa-collaboration.sock");
    let store = Store::shared();
    add_collaboration_agent(&store, "%1", "sender", AgentKind::Codex).await;
    add_collaboration_agent(&store, "%2", "recipient", AgentKind::ClaudeCode).await;
    add_collaboration_agent(&store, "%3", "verifier", AgentKind::GeminiCli).await;
    let backend: SharedBackend = Arc::new(CollaborationTestBackend {
        panes: vec![
            collaboration_test_pane("%1", "0"),
            collaboration_test_pane("%2", "1"),
            collaboration_test_pane("%3", "2"),
        ],
    });
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    let audit = CollaborationAuditLog::in_memory();
    let server = Server::new(sock.clone(), store)
        .with_backends(vec![backend])
        .with_collaboration(mailbox.clone())
        .with_collaboration_audit(audit.clone());
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;

    let client = Client::new(sock).with_collaboration_client_kind(CollaborationClientKind::Watch);
    let sender = CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: Some("default".into()),
        console: false,
        agent_session_id: None,
    };
    let recipient = CollaborationOrigin {
        exact_session: false,
        pane: "%2".into(),
        socket: Some("default".into()),
        console: false,
        agent_session_id: None,
    };
    let verifier = CollaborationOrigin {
        exact_session: false,
        pane: "%3".into(),
        socket: Some("default".into()),
        console: false,
        agent_session_id: None,
    };
    client
        .collaboration_set_identity(
            &recipient,
            Some("reviewer"),
            &["review".into(), "rust".into()],
        )
        .await
        .unwrap();
    assert!(client
        .collaboration_set_identity(&verifier, Some("reviewer"), &[])
        .await
        .is_err());
    client
        .collaboration_set_identity(&verifier, Some("verifier"), &["review".into()])
        .await
        .unwrap();
    let room = client.collaboration_context(&sender).await.unwrap();
    assert_eq!(room.peers.len(), 2);
    assert_eq!(
        room.peers
            .iter()
            .find(|peer| peer.pane == "%2")
            .unwrap()
            .alias
            .as_deref(),
        Some("reviewer")
    );
    assert!(client
        .collaboration_send(
            &sender,
            "role:review",
            &NewRequest {
                user_authorized: false,
                task_contract: None,
                supersedes_request_id: None,
                dispatch_id: None,
                source_node_id: None,
                delegation_parent: None,
                human_action: None,
                initiator: None,
                kind: collaboration::RequestKind::Question,
                body: "ambiguous".into(),
                expects_reply: true,
                notify: None,
                work_mode: collaboration::WorkMode::ReadOnly,
                thread_id: None,
                parent_request_id: None,
                workspace_id: None,
                work_id: None,
                run_id: None,
                paths: Vec::new(),
                artifacts: Vec::new(),
                links: Vec::new(),
                air_artifacts: Vec::new(),
                withdraw_reply_wakes: Vec::new(),
                recipient_slot: None,
            },
        )
        .await
        .is_err());

    let request = client
        .collaboration_send(
            &sender,
            "@reviewer",
            &NewRequest {
                user_authorized: false,
                task_contract: None,
                supersedes_request_id: None,
                dispatch_id: None,
                source_node_id: None,
                delegation_parent: None,
                human_action: None,
                initiator: None,
                kind: collaboration::RequestKind::Review,
                body: "review this".into(),
                expects_reply: true,
                notify: None,
                work_mode: collaboration::WorkMode::ReadOnly,
                thread_id: None,
                parent_request_id: None,
                workspace_id: None,
                work_id: None,
                run_id: None,
                paths: vec!["src/**".into()],
                artifacts: Vec::new(),
                links: Vec::new(),
                air_artifacts: Vec::new(),
                withdraw_reply_wakes: Vec::new(),
                recipient_slot: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(request.workspace_id.as_deref(), Some("callabo"));
    assert_eq!(request.work_id.as_deref(), Some("CAL-7345"));
    // `@reviewer` names a slot, recorded beside the session it reached.
    let slot = request
        .to_slot
        .as_ref()
        .expect("an alias target records its slot");
    assert_eq!(slot.alias.as_deref(), Some("reviewer"));
    assert_eq!(slot.room, request.to.room);
    assert_eq!(request.run_id.as_deref(), Some("tmux:default:@1"));
    let provenance = request.provenance.as_ref().unwrap();
    assert_eq!(provenance.client_kind, CollaborationClientKind::Watch);
    // Peer credentials come from `SO_PEERCRED`; a host whose transport
    // cannot report them leaves this `None` by design — see
    // `transport::PeerIdentity`. `client_kind` above is declared by the
    // client itself, so it holds either way.
    #[cfg(unix)]
    assert_eq!(provenance.caller_pid, Some(std::process::id()));
    assert_eq!(
        provenance.origin_match,
        CollaborationOriginMatch::Unverifiable
    );
    assert_eq!(
        client
            .collaboration_context(&recipient)
            .await
            .unwrap()
            .unread,
        1
    );
    assert!(
        client
            .collaboration_inbox(&recipient)
            .await
            .unwrap()
            .is_empty(),
        "primary inbox must not auto-claim peer work"
    );
    let accepted = client
        .collaboration_claim(&recipient, &request.id)
        .await
        .unwrap();
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].id, request.id);
    client
        .collaboration_purpose(&recipient, collaboration::LaunchPurpose::Worker)
        .await
        .unwrap();
    let inbox = client.collaboration_inbox(&recipient).await.unwrap();
    assert_eq!(inbox[0].status, RequestStatus::Claimed);
    let waiting_client = client.clone();
    let waiting_sender = sender.clone();
    let waiting_request_id = request.id.clone();
    let waiter = tokio::spawn(async move {
        waiting_client
            .collaboration_wait(&waiting_sender, &waiting_request_id, 2)
            .await
    });
    tokio::task::yield_now().await;
    client
        .collaboration_reply(
            &recipient,
            &request.id,
            RequestStatus::Completed,
            "looks good",
            &[],
            &[],
        )
        .await
        .unwrap();
    let observed = tokio::time::timeout(Duration::from_millis(200), waiter)
        .await
        .expect("IPC wait should resume from the mailbox revision")
        .expect("IPC wait task should not panic")
        .expect("IPC wait should succeed");
    assert_eq!(observed.status, RequestStatus::Completed);
    assert!(observed.reply_notified_at.is_some());
    assert!(observed.reply_read_at.is_some());
    let sent = client
        .collaboration_list(&sender, RequestMailbox::Sent)
        .await
        .unwrap();
    assert_eq!(sent[0].status, RequestStatus::Completed);
    // The waiting read acknowledges the result and suppresses a later
    // idle-only reply wake.
    assert!(observed.reply_notified_at.is_some());
    assert!(observed.reply_read_at.is_some());
    assert_eq!(
        client
            .collaboration_context(&sender)
            .await
            .unwrap()
            .unread_replies,
        0
    );

    let queued = client
        .collaboration_send(
            &sender,
            "role:rust",
            &NewRequest {
                user_authorized: false,
                task_contract: None,
                supersedes_request_id: None,
                dispatch_id: None,
                source_node_id: None,
                delegation_parent: None,
                human_action: None,
                initiator: None,
                kind: collaboration::RequestKind::Question,
                body: "obsolete question".into(),
                expects_reply: true,
                notify: None,
                work_mode: collaboration::WorkMode::ReadOnly,
                thread_id: None,
                parent_request_id: None,
                workspace_id: Some("explicit-workspace".into()),
                work_id: Some("explicit-work".into()),
                run_id: Some("explicit-run".into()),
                paths: Vec::new(),
                artifacts: Vec::new(),
                links: Vec::new(),
                air_artifacts: Vec::new(),
                withdraw_reply_wakes: Vec::new(),
                recipient_slot: None,
            },
        )
        .await
        .unwrap();
    let console_origin = CollaborationOrigin {
        console: true,
        ..sender.clone()
    };
    let overview = client
        .collaboration_overview(&console_origin)
        .await
        .unwrap();
    assert!(overview
        .participants
        .iter()
        .any(|peer| peer.alias.as_deref() == Some("reviewer")));
    assert!(overview
        .links
        .iter()
        .any(|link| link.request_id == queued.id && link.status == RequestStatus::Queued));
    let wire = serde_json::to_value(&overview).unwrap();
    assert!(wire["links"][0].get("body").is_none());
    assert!(client.collaboration_overview(&sender).await.is_err());
    assert_eq!(queued.workspace_id.as_deref(), Some("explicit-workspace"));
    assert_eq!(
        queued
            .to_slot
            .as_ref()
            .and_then(|slot| slot.role.as_deref()),
        Some("rust"),
        "a role target records its slot"
    );
    assert_eq!(queued.work_id.as_deref(), Some("explicit-work"));
    assert_eq!(queued.run_id.as_deref(), Some("explicit-run"));
    assert_eq!(
        client
            .collaboration_cancel(&sender, &queued.id)
            .await
            .unwrap()
            .status,
        RequestStatus::Cancelled
    );
    assert!(client
        .collaboration_inbox(&recipient)
        .await
        .unwrap()
        .is_empty());

    // Giving up on a request: the replacement withdraws its reply wake in
    // the same send, and a standalone withdrawal releases another one.
    // Only the sender may do either; neither touches the work.
    assert!(client
        .hello(Duration::from_secs(2))
        .await
        .unwrap()
        .capabilities
        .iter()
        .any(|tag| tag == collaboration::REPLY_WAKE_WITHDRAW_CAPABILITY));
    let abandoned = client
        .collaboration_send(
            &sender,
            "@reviewer",
            &NewRequest {
                task_contract: None,
                supersedes_request_id: None,
                body: "first attempt".into(),
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();
    let replacement = client
        .collaboration_send(
            &sender,
            "@reviewer",
            &NewRequest {
                task_contract: None,
                supersedes_request_id: None,
                body: "second attempt".into(),
                withdraw_reply_wakes: vec![abandoned.id.clone()],
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        replacement.withdrew_reply_wakes,
        std::slice::from_ref(&abandoned.id)
    );
    assert!(client
        .collaboration_withdraw_reply_wakes(&recipient, std::slice::from_ref(&replacement.id))
        .await
        .is_err());
    let withdrawn = client
        .collaboration_withdraw_reply_wakes(&sender, std::slice::from_ref(&replacement.id))
        .await
        .unwrap();
    assert_eq!(withdrawn.len(), 1);
    assert_eq!(withdrawn[0].status, RequestStatus::Queued);
    for request in [&abandoned, &replacement] {
        let observed = client
            .collaboration_list(&sender, RequestMailbox::Sent)
            .await
            .unwrap()
            .into_iter()
            .find(|candidate| candidate.id == request.id)
            .unwrap();
        assert!(matches!(
            observed.reply_wake,
            Some(collaboration::ReplyWake::Released {
                reason: collaboration::ReplyWakeRelease::Withdrawn,
                ..
            })
        ));
    }
    for request in [&abandoned, &replacement] {
        client
            .collaboration_cancel(&sender, &request.id)
            .await
            .unwrap();
    }

    // The console may target the pane it was opened from. That pane stays
    // audit provenance, but the represented identity must remain the
    // console rather than being inferred from the matching recipient.
    let console_request = client
        .collaboration_send(
            &CollaborationOrigin {
                exact_session: false,
                pane: "%1".into(),
                socket: Some("default".into()),
                console: true,
                agent_session_id: None,
            },
            "pane:%1",
            &NewRequest {
                user_authorized: false,
                task_contract: None,
                supersedes_request_id: None,
                dispatch_id: None,
                source_node_id: None,
                delegation_parent: None,
                human_action: None,
                initiator: None,
                kind: collaboration::RequestKind::Task,
                body: "dispatch to launch pane".into(),
                expects_reply: true,
                notify: None,
                work_mode: collaboration::WorkMode::ReadOnly,
                thread_id: None,
                parent_request_id: None,
                workspace_id: None,
                work_id: None,
                run_id: None,
                paths: Vec::new(),
                artifacts: Vec::new(),
                links: Vec::new(),
                air_artifacts: Vec::new(),
                withdraw_reply_wakes: Vec::new(),
                recipient_slot: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(console_request.workspace_id.as_deref(), Some("callabo"));
    assert_eq!(console_request.work_id.as_deref(), Some("CAL-7345"));
    assert_eq!(console_request.run_id.as_deref(), Some("tmux:default:@1"));

    let audit_entries = audit.entries().await;
    assert!(audit_entries.iter().any(|entry| {
        entry.operation == CollaborationAuditOperation::Send
            && entry.request_id.as_deref() == Some(request.id.as_str())
            && entry.actor.client_kind == CollaborationClientKind::Watch
            && entry.message_bytes == Some("review this".len())
    }));
    assert!(audit_entries
        .iter()
        .any(|entry| entry.operation == CollaborationAuditOperation::Reply));
    assert!(audit_entries.iter().any(|entry| {
        entry.operation == CollaborationAuditOperation::Wait
            && entry.request_id.as_deref() == Some(request.id.as_str())
    }));
    assert!(audit_entries.iter().any(|entry| {
        entry.operation == CollaborationAuditOperation::Send
            && entry.request_id.as_deref() == Some(console_request.id.as_str())
            && entry.represented_session_id.as_deref() == Some(collaboration::CONSOLE_SESSION_ID)
    }));

    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn forget_agent_removes_exact_cmux_record_and_notifies_watch() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("forget.sock");
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
    let client = Client::new(sock);
    let target = AgentId {
        kind: AgentKind::Codex,
        session_id: "old-cmux".into(),
        pane: Some("cmux:surface-1".into()),
        tmux_socket: Some("/tmp/cmux.sock".into()),
        surface: Some(crate::event::SurfaceRef {
            kind: crate::event::SurfaceKind::Cmux,
            id: "surface-1".into(),
            workspace: Some("workspace-1".into()),
        }),
        cwd: None,
    };
    for id in [
        target.clone(),
        AgentId {
            session_id: "old-cmux-other".into(),
            pane: Some("cmux:surface-2".into()),
            surface: None,
            ..target.clone()
        },
    ] {
        client
            .ingest(&AgentEvent::Started {
                id,
                at: OffsetDateTime::now_utc() - time::Duration::days(10),
            })
            .await
            .unwrap();
    }
    // Ordinary orphan pruning cannot remove either pane-bound record.
    assert_eq!(client.prune(std::time::Duration::ZERO).await.unwrap(), 0);
    let mut changes = store.subscribe_changes();
    let dirty = store.dirty();
    // Consume notifications from fixture setup before checking deletion.
    dirty.notified().await;
    assert!(!client.forget_agent("old").await.unwrap());
    assert!(!changes.has_changed().unwrap());
    assert!(client.forget_agent("old-cmux").await.unwrap());
    tokio::time::timeout(std::time::Duration::from_secs(1), changes.changed())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(1), dirty.notified())
        .await
        .unwrap();
    assert!(store.by_session("old-cmux").await.is_none());
    assert!(store.by_session("old-cmux-other").await.is_some());
    assert!(!client.forget_agent("old-cmux").await.unwrap());
    // Forget is cleanup, not a persistent block against a live agent.
    client
        .ingest(&AgentEvent::Started {
            id: target,
            at: OffsetDateTime::now_utc(),
        })
        .await
        .unwrap();
    assert!(store.by_session("old-cmux").await.is_some());
    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn prune_clears_missing_native_surface_without_reconciler() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("muxa-prune-native.sock");
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
    let client = Client::new(sock);
    for (sid, at) in [
        (
            "old-native",
            OffsetDateTime::now_utc() - time::Duration::days(2),
        ),
        ("fresh-native", OffsetDateTime::now_utc()),
    ] {
        client
            .ingest(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::Codex,
                    session_id: sid.into(),
                    surface: Some(crate::event::SurfaceRef {
                        kind: crate::event::SurfaceKind::Pty,
                        id: format!("pty:{sid}"),
                        workspace: None,
                    }),
                    pane: None,
                    tmux_socket: None,
                    cwd: None,
                },
                at,
            })
            .await
            .unwrap();
    }
    assert_eq!(
        client
            .prune(std::time::Duration::from_secs(3600))
            .await
            .unwrap(),
        1
    );
    assert!(store.by_session("old-native").await.is_none());
    let fresh = store.by_session("fresh-native").await.unwrap();
    assert!(fresh.surface.is_none());
    assert_eq!(fresh.state, AgentState::Stopped);
    assert_eq!(client.prune(std::time::Duration::ZERO).await.unwrap(), 1);
    tx.send(()).unwrap();
    handle.await.unwrap();
}

#[tokio::test]
async fn end_to_end_ingest_and_query() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("muxa-test.sock");
    let store = Store::shared();
    let server = Server::new(sock.clone(), store.clone());
    let (tx, rx) = broadcast::channel(1);

    let sock_for_server = sock.clone();
    let handle = tokio::spawn(async move {
        server.run(rx).await.unwrap();
        drop(sock_for_server);
    });

    // Wait for the socket to appear.
    for _ in 0..50 {
        if sock.exists() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let client = Client::new(sock.clone());
    client
        .ingest(&AgentEvent::Started {
            id: AgentId {
                tmux_socket: None,
                kind: AgentKind::ClaudeCode,
                session_id: "sess-a".into(),
                surface: None,
                pane: Some("%1".into()),
                cwd: None,
            },
            at: OffsetDateTime::now_utc(),
        })
        .await
        .unwrap();

    let agents = client.by_pane("%1").await.unwrap();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].session_id, "sess-a");

    tx.send(()).unwrap();
    handle.await.unwrap();
}
