use super::*;

/// A wake request sent from the input side must trigger a refresh on
/// the outcome channel. The fetcher counts calls and returns a marker
/// outcome so the assertion can confirm it actually came from us.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn wake_request_drives_a_refresh_outcome() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_fetch = Arc::clone(&calls);

    let (wake_tx, wake_rx) = mpsc::channel::<()>(WAKE_CAPACITY);
    let (out_tx, mut out_rx) = mpsc::channel::<RefreshOutcome>(OUTCOME_CAPACITY);

    let task = tokio::spawn(refresh_task(
        move || {
            let n = calls_for_fetch.fetch_add(1, Ordering::SeqCst);
            async move { outcome_with_marker(&format!("call-{n}")) }
        },
        || async { None },
        wake_rx,
        out_tx,
        async { None },
    ));

    // The 500 ms tick is paused; force the wake path.
    wake_tx.try_send(()).expect("wake slot empty at start");
    let outcome = out_rx.recv().await.expect("refresh outcome on wake");
    let RefreshOutcome::Full(full) = outcome else {
        panic!("wake path always sends a Full refresh");
    };
    assert_eq!(full.agents.len(), 1);
    assert_eq!(full.agents[0].session_id, "call-0");
    assert_eq!(calls.load(Ordering::SeqCst), 1);

    // Dropping the wake sender ends the task cleanly.
    drop(wake_tx);
    task.await.expect("refresh_task joins on shutdown");
}

#[cfg(unix)]
#[tokio::test]
async fn ask_progress_notifies_watch_without_an_agent_transition() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("ask-subscribe.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        for kind in ["hello", "ask_subscribe"] {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&line).unwrap()["kind"],
                kind
            );
            writer.write_all(b"{\"ok\":true}\n").await.unwrap();
        }
        writer.write_all(b"{\"revision\":7}\n").await.unwrap();
        let mut line = String::new();
        assert_eq!(reader.read_line(&mut line).await.unwrap(), 0);
    });
    let mut updates = ask_revision_updates(Client::new(socket));
    tokio::time::timeout(Duration::from_secs(2), updates.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*updates.borrow_and_update(), 8);
    drop(updates);
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}

/// On the periodic tick (no wake), the fetcher still runs and an
/// outcome lands on the channel. Validates that `MissedTickBehavior`
/// + the consume-first-tick dance still produces deliveries.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn periodic_tick_drives_a_refresh_outcome() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_fetch = Arc::clone(&calls);

    let (wake_tx, wake_rx) = mpsc::channel::<()>(WAKE_CAPACITY);
    let (out_tx, mut out_rx) = mpsc::channel::<RefreshOutcome>(OUTCOME_CAPACITY);

    let task = tokio::spawn(refresh_task(
        move || {
            calls_for_fetch.fetch_add(1, Ordering::SeqCst);
            async { outcome_with_marker("tick") }
        },
        || async { None },
        wake_rx,
        out_tx,
        async { None },
    ));

    // Time is paused; advance past one full POLL_INTERVAL so the
    // interval fires its second tick (the first is consumed inside
    // refresh_task before the loop).
    tokio::time::advance(POLL_INTERVAL + Duration::from_millis(50)).await;
    let outcome = out_rx.recv().await.expect("refresh outcome on tick");
    let RefreshOutcome::Full(full) = outcome else {
        panic!("periodic tick always sends a Full refresh");
    };
    assert_eq!(full.agents[0].session_id, "tick");
    assert!(calls.load(Ordering::SeqCst) >= 1);

    drop(wake_tx);
    task.await.expect("refresh_task joins on shutdown");
}

#[tokio::test]
async fn streaming_activity_refresh_is_event_driven() {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("subscribe.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        for expected in ["hello", "subscribe_agent_changes"] {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["kind"], expected);
            writer.write_all(b"{\"ok\":true}\n").await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(1200)).await;
        writer
            .write_all(b"{\"event\":\"agents_changed\"}\n")
            .await
            .unwrap();
        std::future::pending::<()>().await;
    });
    let stream = Client::new(socket).subscribe_agent_changes().await.unwrap();
    let full_calls = Arc::new(AtomicUsize::new(0));
    let count = full_calls.clone();
    let (wake_tx, wake_rx) = mpsc::channel(1);
    let (out_tx, mut out_rx) = mpsc::channel(1);
    let task = tokio::spawn(refresh_task(
        move || {
            count.fetch_add(1, Ordering::SeqCst);
            async { outcome_with_marker("full") }
        },
        || async {
            Some(vec![fake_agent_at(
                "activity",
                "%1",
                OffsetDateTime::now_utc(),
            )])
        },
        wake_rx,
        out_tx,
        async move { Some(stream) },
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(1100), out_rx.recv())
            .await
            .is_err(),
        "idle stream must not poll activity"
    );
    let outcome = tokio::time::timeout(Duration::from_secs(3), out_rx.recv())
        .await
        .unwrap()
        .unwrap();
    let RefreshOutcome::Agents(agents) = outcome else {
        panic!("expected lightweight activity refresh")
    };
    assert_eq!(agents[0].session_id, "activity");
    assert_eq!(full_calls.load(Ordering::SeqCst), 0);
    drop(wake_tx);
    task.await.unwrap();
    server.abort();
}

// ---- detail row -------------------------------------------------------

/// `apply_outcome` is the only path data flows back into `App`. It
/// must mirror what the old inline `refresh` helper did: stash the
/// error and feed agents+panes through `set_data`.
#[test]
fn apply_outcome_mirrors_old_refresh_helper() {
    let mut app = App::new();
    let outcome = RefreshOutcome::Full(FullRefresh {
        agents: vec![fake_agent(
            "s1",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Idle,
            None,
            None,
            None,
            None,
        )],
        panes: vec![fake_pane("%99", "side", 0, 0, "vim")],
        sessions: vec![],
        session_activity: vec![],
        pipeline_runs: vec![],
        error: Some(DaemonError {
            self_describing: false,
            message: "boom".into(),
        }),
    });
    apply_outcome(&mut app, outcome);
    assert_eq!(app.rows.len(), 2);
    assert!(app.last_error.is_some());
    assert_eq!(app.last_error.as_ref().unwrap().message, "boom");
}

/// Regression for the "all rows flicker through Starting" report on
/// v0.5.0. The push-based `Subscribe` stream triggers a fresh
/// snapshot per transition; if any of those snapshots momentarily
/// returns a row in `Starting` (e.g. because a new entry was just
/// inserted by an event that didn't carry an explicit transition),
/// the row would visibly flicker cyan for one tick before settling
/// back. `apply_outcome` MUST keep the previously-known steady
/// state for an `(kind, session_id)` already in `app.rows`.
#[test]
fn apply_outcome_preserves_state_on_unchanged_rows() {
    let mut app = App::new();

    // First refresh: row %1 lands in `Working`.
    apply_outcome(
        &mut app,
        RefreshOutcome::Full(FullRefresh {
            agents: vec![fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Working,
                None,
                None,
                None,
                None,
            )],
            panes: vec![fake_pane("%1", "main", 0, 0, "claude")],
            sessions: vec![],
            session_activity: vec![],
            pipeline_runs: vec![],
            error: None,
        }),
    );
    assert_eq!(app.rows.len(), 1);
    let WatchRow::Agent(a) = &app.rows[0] else {
        panic!("expected agent row");
    };
    assert_eq!(a.state, AgentState::Working);

    // Second refresh: same `(kind, session_id)`, but state has
    // regressed to `Starting` — the daemon-side bug we're papering
    // over here. `apply_outcome` must NOT propagate that to the
    // table; the user sees `Working` continuously.
    apply_outcome(
        &mut app,
        RefreshOutcome::Full(FullRefresh {
            agents: vec![fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Starting,
                None,
                None,
                None,
                None,
            )],
            panes: vec![fake_pane("%1", "main", 0, 0, "claude")],
            sessions: vec![],
            session_activity: vec![],
            pipeline_runs: vec![],
            error: None,
        }),
    );
    let WatchRow::Agent(a) = &app.rows[0] else {
        panic!("expected agent row");
    };
    assert_eq!(
        a.state,
        AgentState::Working,
        "row that was Working must not flicker through Starting on a transient placeholder snapshot",
    );

    // Third refresh: legitimate transition Working → Idle. The
    // merge MUST let real state changes through — the invariant
    // is "Starting placeholder doesn't override steady state",
    // not "state never changes".
    apply_outcome(
        &mut app,
        RefreshOutcome::Full(FullRefresh {
            agents: vec![fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Idle,
                None,
                None,
                None,
                None,
            )],
            panes: vec![fake_pane("%1", "main", 0, 0, "claude")],
            sessions: vec![],
            session_activity: vec![],
            pipeline_runs: vec![],
            error: None,
        }),
    );
    let WatchRow::Agent(a) = &app.rows[0] else {
        panic!("expected agent row");
    };
    assert_eq!(a.state, AgentState::Idle);
}

/// A genuinely fresh agent (no prior row at the same
/// `(kind, session_id)`) is allowed to appear as `Starting` — the
/// merge only suppresses the `Starting` placeholder when we have
/// a steady state to fall back on.
#[test]
fn apply_outcome_lets_starting_through_for_brand_new_rows() {
    let mut app = App::new();
    apply_outcome(
        &mut app,
        RefreshOutcome::Full(FullRefresh {
            agents: vec![fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Starting,
                None,
                None,
                None,
                None,
            )],
            panes: vec![fake_pane("%1", "main", 0, 0, "claude")],
            sessions: vec![],
            session_activity: vec![],
            pipeline_runs: vec![],
            error: None,
        }),
    );
    let WatchRow::Agent(a) = &app.rows[0] else {
        panic!("expected agent row");
    };
    assert_eq!(a.state, AgentState::Starting);
}

/// `SingleAgent` push only touches the matching `(kind, session_id)`
/// row. Other rows keep their existing values byte-for-byte —
/// this is what stops the UI from "redrawing every row on every
/// tick".
#[test]
fn single_agent_outcome_only_updates_matching_row() {
    let mut app = App::new();

    // Seed two agent rows.
    apply_outcome(
        &mut app,
        RefreshOutcome::Full(FullRefresh {
            agents: vec![
                fake_agent(
                    "s1",
                    Some("%1"),
                    AgentKind::ClaudeCode,
                    AgentState::Working,
                    Some("first prompt"),
                    None,
                    None,
                    None,
                ),
                fake_agent(
                    "s2",
                    Some("%2"),
                    AgentKind::ClaudeCode,
                    AgentState::Idle,
                    Some("second prompt"),
                    None,
                    None,
                    None,
                ),
            ],
            panes: vec![
                fake_pane("%1", "main", 0, 0, "claude"),
                fake_pane("%2", "side", 1, 0, "claude"),
            ],
            sessions: vec![],
            session_activity: vec![],
            pipeline_runs: vec![],
            error: None,
        }),
    );
    assert_eq!(app.rows.len(), 2);

    // SingleAgent push for s2 only — last_prompt and state change.
    apply_outcome(
        &mut app,
        RefreshOutcome::SingleAgent(fake_agent(
            "s2",
            Some("%2"),
            AgentKind::ClaudeCode,
            AgentState::Working,
            Some("second prompt UPDATED"),
            None,
            None,
            None,
        )),
    );

    // s1 must be untouched.
    let WatchRow::Agent(a1) = &app.rows[0] else {
        panic!("row 0 should be agent s1");
    };
    assert_eq!(a1.session_id, "s1");
    assert_eq!(a1.state, AgentState::Working);
    assert_eq!(a1.last_prompt.as_deref(), Some("first prompt"));

    // s2 reflects the push.
    let WatchRow::Agent(a2) = &app.rows[1] else {
        panic!("row 1 should be agent s2");
    };
    assert_eq!(a2.session_id, "s2");
    assert_eq!(a2.state, AgentState::Working);
    assert_eq!(a2.last_prompt.as_deref(), Some("second prompt UPDATED"));
}

/// `SingleAgent` push for an unknown `(kind, session_id)` appends.
/// The next periodic Full refresh handles sort order.
#[test]
fn single_agent_outcome_appends_unknown_row() {
    let mut app = App::new();
    apply_outcome(
        &mut app,
        RefreshOutcome::SingleAgent(fake_agent(
            "fresh",
            Some("%9"),
            AgentKind::ClaudeCode,
            AgentState::Idle,
            None,
            None,
            None,
            None,
        )),
    );
    assert_eq!(app.rows.len(), 1);
    let WatchRow::Agent(a) = &app.rows[0] else {
        panic!("expected agent row");
    };
    assert_eq!(a.session_id, "fresh");
}

/// A `SingleAgent` push whose payload carries `last_prompt: None`
/// must not wipe the prompt the user already sees. This happens
/// when the daemon's broadcast captures a transition that didn't
/// touch the prompt field (e.g. `ToolStarted` → `Working`).
#[test]
fn single_agent_preserves_last_prompt_when_none() {
    let mut app = App::new();
    // Seed with a row that has a prompt.
    apply_outcome(
        &mut app,
        RefreshOutcome::Full(FullRefresh {
            agents: vec![fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Working,
                Some("original prompt"),
                Some("Opus"),
                Some(34.0),
                Some(0.12),
            )],
            panes: vec![fake_pane("%1", "main", 0, 0, "claude")],
            sessions: vec![],
            session_activity: vec![],
            pipeline_runs: vec![],
            error: None,
        }),
    );

    // Push a state-only update (no prompt).
    apply_outcome(
        &mut app,
        RefreshOutcome::SingleAgent(fake_agent(
            "s1",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Idle,
            None, // last_prompt cleared in payload
            None,
            None,
            None,
        )),
    );

    let WatchRow::Agent(a) = &app.rows[0] else {
        panic!("expected agent row");
    };
    assert_eq!(a.state, AgentState::Idle);
    assert_eq!(a.last_prompt.as_deref(), Some("original prompt"));
    // Other optional fields should also survive.
    assert_eq!(a.model.as_deref(), Some("Opus"));
    assert_eq!(a.context_used_pct, Some(34.0));
    assert_eq!(a.cost_usd, Some(0.12));
}

/// A `Full` refresh that carries a `Starting` placeholder with
/// `last_prompt: None` must not blank the prompt we already know.
/// This is the fallback-tick path (5 s interval) hitting a daemon
/// that has a fresh store entry for a pane we've been tracking.
#[test]
fn apply_full_preserves_last_prompt_on_starting_placeholder() {
    let mut app = App::new();
    // First refresh: row is Working with a prompt.
    apply_outcome(
        &mut app,
        RefreshOutcome::Full(FullRefresh {
            agents: vec![fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Working,
                Some("original prompt"),
                Some("Sonnet"),
                Some(50.0),
                Some(0.05),
            )],
            panes: vec![fake_pane("%1", "main", 0, 0, "claude")],
            sessions: vec![],
            session_activity: vec![],
            pipeline_runs: vec![],
            error: None,
        }),
    );

    // Second refresh: snapshot regressed to Starting + blank optional fields.
    apply_outcome(
        &mut app,
        RefreshOutcome::Full(FullRefresh {
            agents: vec![fake_agent(
                "s1",
                Some("%1"),
                AgentKind::ClaudeCode,
                AgentState::Starting,
                None,
                None,
                None,
                None,
            )],
            panes: vec![fake_pane("%1", "main", 0, 0, "claude")],
            sessions: vec![],
            session_activity: vec![],
            pipeline_runs: vec![],
            error: None,
        }),
    );

    let WatchRow::Agent(a) = &app.rows[0] else {
        panic!("expected agent row");
    };
    // Anti-flicker: state should stay Working.
    assert_eq!(a.state, AgentState::Working);
    // Anti-data-loss: prompt and metadata must survive.
    assert_eq!(a.last_prompt.as_deref(), Some("original prompt"));
    assert_eq!(a.model.as_deref(), Some("Sonnet"));
    assert_eq!(a.context_used_pct, Some(50.0));
    assert_eq!(a.cost_usd, Some(0.05));
}

/// A `SingleAgent` push whose payload carries cleared
/// rate-limit fields (`rate_limited_until`, `rate_limit_scope`,
/// `rate_limit_source`) must propagate the clear to the UI. These
/// three fields are NOT merge-preserved because events like
/// `Started` and `TurnStopped` legitimately clear them — a new
/// session or a successful turn means the cap has been lifted.
#[test]
fn single_agent_propagates_rate_limit_clear() {
    use muxa::event::{RateLimitScope, RateLimitSource};
    let mut app = App::new();

    // Seed a row that is currently rate-limited.
    let mut limited = fake_agent(
        "s1",
        Some("%1"),
        AgentKind::ClaudeCode,
        AgentState::Error,
        Some("prompt"),
        Some("Opus"),
        None,
        None,
    );
    limited.rate_limited_until = Some(time::macros::datetime!(2026-05-06 15:00:00 UTC));
    limited.rate_limit_scope = Some(RateLimitScope::FiveHour);
    limited.rate_limit_source = Some(RateLimitSource::StopFailure);
    limited.rate_limit_5h_pct = Some(100.0);

    apply_outcome(
        &mut app,
        RefreshOutcome::Full(FullRefresh {
            agents: vec![limited],
            panes: vec![fake_pane("%1", "main", 0, 0, "claude")],
            sessions: vec![],
            session_activity: vec![],
            pipeline_runs: vec![],
            error: None,
        }),
    );
    assert_eq!(app.rows.len(), 1);

    // Push a transition that clears the cap (e.g. TurnStopped after
    // a successful turn, or Started for a fresh session).
    apply_outcome(
        &mut app,
        RefreshOutcome::SingleAgent(fake_agent(
            "s1",
            Some("%1"),
            AgentKind::ClaudeCode,
            AgentState::Idle,
            None, // last_prompt should be preserved
            None, // model should be preserved
            None,
            None,
        )),
    );

    let WatchRow::Agent(a) = &app.rows[0] else {
        panic!("expected agent row");
    };
    // Rate-limit markers must be gone.
    assert!(
        a.rate_limited_until.is_none(),
        "rate_limited_until must be cleared"
    );
    assert!(
        a.rate_limit_scope.is_none(),
        "rate_limit_scope must be cleared"
    );
    assert!(
        a.rate_limit_source.is_none(),
        "rate_limit_source must be cleared"
    );
    // But last_prompt and model should survive (None means "don't touch").
    assert_eq!(a.last_prompt.as_deref(), Some("prompt"));
    assert_eq!(a.model.as_deref(), Some("Opus"));
    // Rolling percentages should also survive (they're not cleared by events).
    assert_eq!(a.rate_limit_5h_pct, Some(100.0));
}

// ---- detail row: precise visual layout --------------------------------

#[cfg(unix)]
#[tokio::test]
async fn watch_previews_keep_full_search_and_fetch_details_only_on_change() {
    use muxa::collaboration::CollaborationReadView as View;
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("watch-history.sock");
    let mut full = collab_request("%2");
    full.body = format!("{}needle_tail", "body ".repeat(1000));
    let mut preview = full.clone();
    View::Preview.apply(&mut preview);
    let page = |request: &CollaborationRequest| json!({"ok":true,"collaboration_scope":"all", "collaboration_page":{"requests":[request],"total":1,"has_more":false}});
    let mut changed = full.clone();
    changed
        .updates
        .push(muxa::collaboration::CollaborationUpdate {
            sequence: 1,
            author: full.from.clone(),
            body: "new progress".into(),
            at: full.created_at,
        });
    let server = crate::collaboration_output::mock_collaboration_responses(
        &socket,
        vec![
            (
                json!({"/view":"preview","/query/limit":500}),
                page(&preview),
            ),
            (json!({"/view":"preview"}), page(&preview)),
            (json!({"/view":"full","/query/limit":8}), page(&full)),
            (json!({"/view":"preview"}), page(&preview)),
            (
                json!({"/kind":"collaboration_peek"}),
                json!({"ok":true,"collaboration_request":full}),
            ),
            (
                json!({"/kind":"collaboration_peek"}),
                json!({"ok":true,"collaboration_request":changed}),
            ),
        ],
    );
    let client = Client::new(socket);
    let mut app = collab_app("%2");
    let (_mailbox_sender, mailbox_rx) = tokio::sync::watch::channel(None);
    app.collaboration_history.changes = Some(mailbox_rx);
    let origin = CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
    };
    app.collaboration.origin = Some(origin.clone());
    let rows = watch_history(&client, &mut app, &origin).await.unwrap();
    assert_eq!(rows[0].body.chars().count(), 256);
    app.search_query = "needle_tail".into();
    let searched = watch_history(&client, &mut app, &origin).await.unwrap();
    assert!(searched[0].body.contains("needle_tail"));
    let cached = watch_history(&client, &mut app, &origin).await.unwrap();
    assert!(
        cached[0].body.contains("needle_tail"),
        "unchanged summaries reuse full search content"
    );
    app.search_query.clear();
    app.collaboration_history.search_full = None;
    app.collab.set_requests(rows);
    refresh_selected_collaboration_detail(&client, &mut app).await;
    refresh_selected_collaboration_detail(&client, &mut app).await;
    assert!(app
        .collaboration_history
        .detail
        .as_ref()
        .unwrap()
        .body
        .contains("needle_tail"));
    let mut summary = changed.clone();
    View::Preview.apply(&mut summary);
    app.collab.set_requests(vec![summary]);
    refresh_selected_collaboration_detail(&client, &mut app).await;
    assert_eq!(
        app.collaboration_history.detail.as_ref().unwrap().updates[0].body,
        "new progress"
    );
    server.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn full_body_search_refreshes_only_changed_requests() {
    use muxa::collaboration::CollaborationReadView as View;
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("changed-search.sock");
    let first = collab_request("%2");
    let mut second = first.clone();
    second.id = "unchanged".into();
    let mut changed = first.clone();
    changed
        .updates
        .push(muxa::collaboration::CollaborationUpdate {
            sequence: 1,
            author: first.from.clone(),
            body: "new update".into(),
            at: first.created_at,
        });
    let mut previews = vec![changed.clone(), second.clone()];
    for request in &mut previews {
        View::Preview.apply(request);
    }
    let server = crate::collaboration_output::mock_collaboration_responses(
        &socket,
        vec![(
            json!({"/kind":"collaboration_peek","/request_id":first.id}),
            json!({"ok":true,"collaboration_request":changed}),
        )],
    );
    let origin = CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
    };
    let full = reconcile_search_history(&Client::new(socket), &origin, &previews, &[first, second])
        .await
        .unwrap();
    assert_eq!(full.len(), 2);
    assert_eq!(full[0].updates[0].body, "new update");
    assert_eq!(full[1].body, COLLAB_FIXTURE_BODY);
    server.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn watch_startup_negotiates_before_the_first_badge_and_board_reads() {
    use serde_json::json;
    for supported in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("startup.sock");
        let mut human = collab_request("%2");
        human.to = Participant::console(human.from.room.clone());
        human.omit_list_bodies();
        let room = RoomContext {
            current: human.to.clone(),
            peers: vec![human.from.clone()],
            unread: 1,
            unread_replies: 0,
        };
        let page = |rows: Vec<CollaborationRequest>| {
            json!({
                "ok":true,"collaboration_scope":"all",
                "collaboration_page":{"total":rows.len(),"requests":rows,"has_more":false}
            })
        };
        let capabilities = if supported {
            vec![
                muxa::collaboration::HUMAN_QUERY_CAPABILITY,
                muxa::collaboration::OPEN_COUNTS_CAPABILITY,
            ]
        } else {
            vec![]
        };
        let mut calls = vec![
            (
                json!({"/kind":"hello"}),
                json!({"ok":true,"protocol":muxa::event::PROTOCOL_VERSION,
                "version":env!("CARGO_PKG_VERSION"),"capabilities":capabilities}),
            ),
            (
                json!({"/kind":"collaboration_context"}),
                json!({"ok":true,"room":room}),
            ),
        ];
        if supported {
            calls.push((
                json!({"/kind":"collaboration_list","/query/human_only":true,"/view":"metadata"}),
                page(vec![human.clone(), collab_request("%3")]),
            ));
            calls.push((json!({"/kind":"collaboration_open_counts","/scope":"all"}),json!({
                "ok":true,"collaboration_open_counts":muxa::collaboration::CollaborationOpenCounts::from_requests([&human])
            })));
        } else {
            for _ in 0..2 {
                calls.push((
                    json!({"/kind":"collaboration_list","/query/status":"queued"}),
                    page(vec![human.clone()]),
                ));
                calls.push((
                    json!({"/kind":"collaboration_list","/query/status":"claimed"}),
                    page(vec![]),
                ));
            }
        }
        let server = crate::collaboration_output::mock_collaboration_responses(&socket, calls);
        let client = Client::new(socket);
        let mut app = collab_app("%2");
        app.apply_screen(WatchScreen::Board);
        app.collaboration.origin = Some(CollaborationOrigin {
            exact_session: false,
            pane: "%1".into(),
            socket: Some("default".into()),
            console: true,
            agent_session_id: None,
        });
        // Exercise production initialization, without setting capability flags in the test.
        initialize_watch_collaboration(&client, &mut app).await;
        assert_eq!(app.human_query_supported, supported);
        assert_eq!(app.open_counts_supported, supported);
        assert_eq!(app.collaboration.human, vec![human.clone()]);
        assert!(app.collaboration.unavailable.is_none());
        assert!(app.version_skew.is_none());
        let (_sender, changes) = tokio::sync::watch::channel(Some(1));
        app.board_collab_cache.changes = Some(changes);
        refresh_board_collaboration(&client, &mut app);
        assert_eq!(
            app.board_collab
                .in_flight
                .take()
                .unwrap()
                .await
                .unwrap()
                .unwrap(),
            muxa::collaboration::CollaborationOpenCounts::from_requests([&human])
        );
        server.await.unwrap();
    }
}

#[cfg(unix)]
#[tokio::test]
async fn watch_startup_with_unreachable_daemon_stays_unavailable() {
    let dir = tempfile::tempdir().unwrap();
    let client = Client::new(dir.path().join("absent.sock"));
    let mut app = collab_app("%2");
    app.collaboration.origin = Some(CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
    });
    tokio::time::timeout(
        Duration::from_secs(3),
        initialize_watch_collaboration(&client, &mut app),
    )
    .await
    .expect("startup must return when the daemon is unreachable");
    assert!(!app.human_query_supported);
    assert!(!app.open_counts_supported);
    assert!(app.collaboration.unavailable.is_some());
}

#[cfg(unix)]
#[tokio::test]
async fn hidden_mailbox_reads_open_badges_then_loads_history_only_when_opened() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("hidden-mailbox.sock");
    let mut human = collab_request("%2");
    human.body = "Human needs a decision".into();
    human.to = Participant::console(human.from.room.clone());
    let room = RoomContext {
        current: human.to.clone(),
        peers: vec![human.from.clone()],
        unread: 1,
        unread_replies: 0,
    };
    let page = |requests: Vec<CollaborationRequest>| {
        json!({"ok":true,"collaboration_scope":"all",
        "collaboration_page":{"total":requests.len(),"requests":requests,"has_more":false}})
    };
    let server = crate::collaboration_output::mock_collaboration_responses(
        &socket,
        vec![
            (
                json!({"/kind":"collaboration_context"}),
                json!({"ok":true,"room":room}),
            ),
            (
                json!({"/view":"metadata","/query/status":"queued"}),
                page(vec![human.clone()]),
            ),
            (
                json!({"/view":"metadata","/query/status":"claimed"}),
                page(vec![]),
            ),
            (
                json!({"/kind":"collaboration_context"}),
                json!({"ok":true,"room":room}),
            ),
            (json!({"/view":"preview"}), page(vec![human.clone()])),
        ],
    );
    let client = Client::new(socket);
    let mut app = collab_app("%2");
    app.apply_screen(WatchScreen::Board);
    app.collaboration.origin = Some(CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
    });
    let (_sender, changes) = tokio::sync::watch::channel(Some(9));
    app.collaboration_history.changes = Some(changes);
    refresh_watch_collaboration(&client, &mut app).await;
    assert!(
        app.collaboration.room.is_some(),
        "quick-send routing must remain available"
    );
    assert_eq!(
        app.collaboration.human.len(),
        1,
        "human-action badges must be preserved"
    );
    assert!(app.collaboration.human[0].body.is_empty());
    assert!(app.collaboration_history.previews.is_empty());
    open_watch_collaboration_mailbox(&client, &mut app).await;
    assert_eq!(app.collaboration.human[0].body, human.body);
    assert_eq!(app.collaboration_history.previews.len(), 1);
    server.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn board_reuses_unchanged_revision_and_queries_only_open_metadata() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("board-open.sock");
    let queued = collab_request("%2");
    let mut claimed = queued.clone();
    claimed.status = RequestStatus::Claimed;
    let response = |row: &CollaborationRequest| {
        json!({"ok":true,"collaboration_scope":"all",
        "collaboration_page":{"requests":[row],"total":1,"has_more":false}})
    };
    let mut calls = Vec::new();
    for _ in 0..2 {
        calls.push((
            json!({"/kind":"collaboration_list","/view":"metadata","/query/status":"queued"}),
            response(&queued),
        ));
        calls.push((
            json!({"/kind":"collaboration_list","/view":"metadata","/query/status":"claimed"}),
            response(&claimed),
        ));
    }
    let server = crate::collaboration_output::mock_collaboration_responses(&socket, calls);
    let client = Client::new(socket);
    let mut app = collab_app("%2");
    let (sender, changes) = tokio::sync::watch::channel(Some(7));
    app.board_collab_cache.changes = Some(changes);
    app.collaboration.origin = Some(CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
    });
    refresh_board_collaboration(&client, &mut app);
    let rows = app
        .board_collab
        .in_flight
        .take()
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        rows,
        muxa::collaboration::CollaborationOpenCounts::from_requests([&claimed])
    );
    app.board_collab.value = Some(Ok(rows));
    refresh_board_collaboration(&client, &mut app);
    assert!(
        app.board_collab.in_flight.is_none(),
        "idle mailbox must do no list RPCs"
    );
    sender.send_replace(Some(8));
    refresh_board_collaboration(&client, &mut app);
    let rows = app
        .board_collab
        .in_flight
        .take()
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        rows,
        muxa::collaboration::CollaborationOpenCounts::from_requests([&claimed])
    );
    server.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn board_summary_uses_one_aggregate_and_skips_idle_reads() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("board-counts.sock");
    let counts =
        muxa::collaboration::CollaborationOpenCounts::from_requests([&collab_request("%2")]);
    let response = json!({"ok":true,"collaboration_open_counts":counts});
    let server = crate::collaboration_output::mock_collaboration_responses(
        &socket,
        vec![
            (
                json!({"/kind":"collaboration_open_counts","/scope":"all"}),
                response.clone(),
            ),
            (
                json!({"/kind":"collaboration_open_counts","/scope":"all"}),
                response,
            ),
        ],
    );
    let client = Client::new(socket);
    let mut app = collab_app("%2");
    app.open_counts_supported = true;
    app.collaboration.origin = Some(CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
    });
    let (sender, changes) = tokio::sync::watch::channel(Some(7));
    app.board_collab_cache.changes = Some(changes);
    refresh_board_collaboration(&client, &mut app);
    let first = app
        .board_collab
        .in_flight
        .take()
        .unwrap()
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first, counts);
    app.board_collab.value = Some(Ok(first));
    refresh_board_collaboration(&client, &mut app);
    assert!(app.board_collab.in_flight.is_none());
    sender.send_replace(Some(8));
    refresh_board_collaboration(&client, &mut app);
    assert_eq!(
        app.board_collab
            .in_flight
            .take()
            .unwrap()
            .await
            .unwrap()
            .unwrap(),
        counts
    );
    server.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn human_badges_push_the_filter_before_pagination() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("human-query.sock");
    let mut human = collab_request("%2");
    human.to = Participant::console(human.from.room.clone());
    human.omit_list_bodies();
    let server = crate::collaboration_output::mock_collaboration_responses(
        &socket,
        vec![(
            json!({"/kind":"collaboration_list","/view":"metadata","/query/human_only":true}),
            json!({"ok":true,"collaboration_scope":"all","collaboration_page":{"requests":[human.clone()],"total":1,"has_more":false}}),
        )],
    );
    let origin = CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
    };
    assert_eq!(
        read_human_requests(&Client::new(socket), &origin, true)
            .await
            .unwrap(),
        vec![human]
    );
    server.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn board_legacy_listing_filters_terminal_bodies_in_one_read() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("board-legacy.sock");
    let queued = collab_request("%2");
    let mut terminal = queued.clone();
    terminal.id = "old".into();
    terminal.status = RequestStatus::Completed;
    let server = crate::collaboration_output::mock_collaboration_responses(
        &socket,
        vec![(
            json!({"/kind":"collaboration_list","/view":"metadata"}),
            json!({"ok":true,"collaboration_scope":"all","collaboration_requests":[queued,terminal]}),
        )],
    );
    let origin = CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
    };
    let rows = read_board_open_requests(&Client::new(socket), &origin)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert!(rows[0].body.is_empty());
    server.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn unchanged_mailbox_revision_does_not_reload_history() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("revision-history.sock");
    let request = collab_request("%2");
    let response = json!({"ok":true,"collaboration_scope":"all","collaboration_page":{"requests":[request],"total":1,"has_more":false}});
    let server = crate::collaboration_output::mock_collaboration_responses(
        &socket,
        vec![
            (json!({"/view":"preview"}), response.clone()),
            (json!({"/view":"preview"}), response),
        ],
    );
    let client = Client::new(socket);
    let mut app = collab_app("%2");
    let (sender, changes) = tokio::sync::watch::channel(Some(7));
    app.collaboration_history.changes = Some(changes);
    let origin = CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
    };
    let first = watch_history(&client, &mut app, &origin).await.unwrap();
    let idle = watch_history(&client, &mut app, &origin).await.unwrap();
    assert_eq!(first, idle, "no additional RPC for an unchanged revision");
    sender.send_replace(Some(8));
    let updated = watch_history(&client, &mut app, &origin).await.unwrap();
    assert_eq!(first, updated);
    server.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn first_history_snapshot_waits_for_subscription_and_keeps_racing_updates() {
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("first-history.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (race, race_rx) = tokio::sync::oneshot::channel();
    let (finish, finish_rx) = tokio::sync::oneshot::channel();
    let row = collab_request("%2");
    let server = tokio::spawn(async move {
        let mut race_rx = Some(race_rx);
        // The subscription must be acknowledged before the first list.
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut subscription) = stream.into_split();
        let mut reader = BufReader::new(read);
        for kind in ["hello", "collaboration_subscribe"] {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["kind"], kind);
            if kind == "collaboration_subscribe" {
                assert_eq!(request["requests_only"], true);
            }
            subscription.write_all(b"{\"ok\":true}\n").await.unwrap();
        }
        // First snapshot, then a refresh whose read races an invalidation,
        // then the necessary refresh of that dirty snapshot.
        for index in 0..3 {
            if index == 1 {
                race_rx.take().unwrap().await.unwrap();
                subscription.write_all(b"{\"revision\":1}\n").await.unwrap();
            }
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut reader = BufReader::new(read);
            for kind in ["hello", "collaboration_list"] {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&line).unwrap()["kind"],
                    kind
                );
                if kind == "hello" {
                    write.write_all(b"{\"ok\":true}\n").await.unwrap();
                }
            }
            if index == 1 {
                subscription.write_all(b"{\"revision\":2}\n").await.unwrap();
            }
            let response = json!({"ok":true,"collaboration_scope":"all",
                "collaboration_page":{"requests":[row],"total":1,"has_more":false}});
            write
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        }
        tokio::select! {
            extra = listener.accept() => panic!("unchanged history opened another connection: {extra:?}"),
            _ = finish_rx => (),
        }
    });
    let client = Client::new(socket);
    let mut app = collab_app("%2");
    let origin = CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
    };
    let first = watch_history(&client, &mut app, &origin).await.unwrap();
    assert_eq!(app.collaboration_history.revision, Some(0));
    assert_eq!(
        watch_history(&client, &mut app, &origin).await.unwrap(),
        first
    );
    race.send(()).unwrap();
    let changes = app.collaboration_history.changes.as_mut().unwrap();
    changes.changed().await.unwrap();
    watch_history(&client, &mut app, &origin).await.unwrap();
    let changes = app.collaboration_history.changes.as_mut().unwrap();
    if *changes.borrow() != Some(2) {
        changes.changed().await.unwrap();
    }
    watch_history(&client, &mut app, &origin).await.unwrap();
    assert_eq!(app.collaboration_history.revision, Some(2));
    assert_eq!(
        watch_history(&client, &mut app, &origin).await.unwrap(),
        first
    );
    finish.send(()).unwrap();
    server.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn mailbox_subscription_ack_establishes_idle_baseline_without_a_revision_event() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("idle-subscription.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);
        let mut line = String::new();
        for kind in ["hello", "collaboration_subscribe"] {
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&line).unwrap()["kind"],
                kind
            );
            write.write_all(b"{\"ok\":true}\n").await.unwrap();
        }
        assert_eq!(
            reader.read_line(&mut line).await.unwrap(),
            0,
            "dropping watch must close its subscription"
        );
    });
    let mut changes = collaboration_revision_updates(Client::new(socket));
    tokio::time::timeout(Duration::from_secs(2), changes.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*changes.borrow_and_update(), Some(0));
    drop(changes);
    server.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_watch_history_is_received_once_not_once_per_page() {
    use serde_json::json;
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("legacy-watch-history.sock");
    let records: Vec<_> = (0..501)
        .map(|index| {
            let mut request = collab_request("%2");
            request.id = format!("row-{index}");
            request
        })
        .collect();
    let server = crate::collaboration_output::mock_collaboration_responses(
        &socket,
        vec![(
            json!({"/kind":"collaboration_list"}),
            json!({"ok":true,"collaboration_scope":"all","collaboration_requests":records}),
        )],
    );
    let origin = CollaborationOrigin {
        exact_session: false,
        pane: "%1".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
    };
    let rows = read_watch_history(&Client::new(socket), &origin, false)
        .await
        .unwrap();
    assert_eq!(rows.len(), 501);
    assert_eq!(rows.last().unwrap().id, "row-500");
    server.await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn full_history_widens_pages_only_after_budget_acknowledgement() {
    use serde_json::json;
    for acknowledged in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("page-budget.sock");
        let first = collab_request("%2");
        let mut second = first.clone();
        second.id = "second".into();
        let cursor = muxa::collaboration::CollaborationCursor {
            created_at: first.created_at,
            id: first.id.clone(),
        };
        let mut response = json!({"ok":true,"collaboration_scope":"all","collaboration_page":{"requests":[first],"total":2,"has_more":true,"next_cursor":cursor}});
        if acknowledged {
            response["collaboration_byte_budget"] = json!(4 * 1024 * 1024);
        }
        let next_limit = if acknowledged { 500 } else { 8 };
        let server = crate::collaboration_output::mock_collaboration_responses(
            &socket,
            vec![
                (
                    json!({"/query/limit":8,"/query/max_bytes":4*1024*1024}),
                    response,
                ),
                (
                    json!({"/query/limit":next_limit}),
                    json!({"ok":true,"collaboration_scope":"all","collaboration_page":{"requests":[second],"total":2,"has_more":false}}),
                ),
            ],
        );
        let origin = CollaborationOrigin {
            exact_session: false,
            pane: "%1".into(),
            socket: Some("default".into()),
            console: true,
            agent_session_id: None,
        };
        let rows = read_watch_history(&Client::new(socket), &origin, true)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].body, COLLAB_FIXTURE_BODY);
        server.await.unwrap();
    }
}
