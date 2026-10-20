//! The sender's reply-wake obligation: what the store hands the waker, and
//! what a read, a withdrawal or a restart does to an attempt in flight.

use super::*;

fn participant(pane: &str, session: &str) -> Participant {
    Participant {
        agent_kind: AgentKind::Codex,
        agent_session_id: session.into(),
        pane: pane.into(),
        socket: Some("default".into()),
        room: RoomId {
            host: "tmux".into(),
            socket: Some("default".into()),
            window_id: "@1".into(),
        },
        tmux_session_id: Some("$1".into()),
        tmux_session_name: Some("main".into()),
        window_name: Some("feature".into()),
        state: AgentState::Idle,
        cwd: Some("/repo".into()),
        alias: None,
        roles: Vec::new(),
        console: false,
    }
}

struct Pair {
    store: Arc<CollaborationStore>,
    sender: Participant,
    recipient: Participant,
}

impl Pair {
    fn new() -> Self {
        Self::with(CollaborationStore::in_memory(
            CollaborationOptions::default(),
        ))
    }

    fn with(store: Arc<CollaborationStore>) -> Self {
        Self {
            store,
            sender: participant("%1", "sender"),
            recipient: participant("%2", "recipient"),
        }
    }

    async fn ask(&self, body: &str) -> CollaborationRequest {
        self.ask_withdrawing(body, &[]).await.unwrap()
    }

    async fn ask_withdrawing(
        &self,
        body: &str,
        withdraw: &[&str],
    ) -> Result<CollaborationRequest, CollaborationError> {
        self.store
            .create(
                self.sender.clone(),
                self.recipient.clone(),
                NewRequest {
                    body: body.into(),
                    withdraw_reply_wakes: withdraw.iter().map(|id| (*id).to_string()).collect(),
                    ..NewRequest::default()
                },
            )
            .await
    }

    async fn answer(&self, id: &str, status: RequestStatus) {
        self.store
            .reply(
                &self.recipient,
                id,
                status,
                format!("answer to {id}"),
                Vec::new(),
                Vec::new(),
            )
            .await
            .unwrap();
    }

    /// One request with its terminal reply already in the mailbox.
    async fn answered(&self, body: &str) -> CollaborationRequest {
        let request = self.ask(body).await;
        self.answer(&request.id, RequestStatus::Completed).await;
        request
    }

    async fn reserve(&self) -> Option<ReplyWakeBatch> {
        self.store.reserve_reply_wakes(&self.sender).await.unwrap()
    }

    async fn wake_of(&self, id: &str) -> Option<ReplyWake> {
        self.store.requests.read().await.get(id).unwrap().reply_wake
    }

    async fn released(&self, id: &str) -> Option<ReplyWakeRelease> {
        match self.wake_of(id).await {
            Some(ReplyWake::Released { reason, .. }) => Some(reason),
            _ => None,
        }
    }
}

fn ids(batch: &ReplyWakeBatch) -> Vec<&str> {
    batch
        .requests
        .iter()
        .map(|request| request.id.as_str())
        .collect()
}

/// The reported bug's other half: the waker scans, the sender reads the
/// reply with `wait`, and only then does the waker get to it. The scan is
/// stale; the reservation must not be.
#[tokio::test]
async fn a_reply_read_after_the_scan_is_not_announced() {
    let pair = Pair::new();
    let request = pair.answered("review").await;
    let scanned = pair.store.pending_reply_unnotified().await;
    assert_eq!(scanned.len(), 1);

    pair.store
        .wait_for_terminal(&pair.sender, &request.id, Duration::from_millis(10))
        .await
        .unwrap();

    assert!(pair.reserve().await.is_none(), "read reply was reserved");
    assert_eq!(
        pair.released(&request.id).await,
        Some(ReplyWakeRelease::Read)
    );
}

/// Replies that piled up while the sender was busy go out as one batch,
/// oldest reply first, instead of one wake per idle turn.
#[tokio::test]
async fn owed_replies_are_reserved_together_oldest_reply_first() {
    let pair = Pair::new();
    let first = pair.ask("first").await;
    let second = pair.ask("second").await;
    pair.answer(&second.id, RequestStatus::Blocked).await;
    pair.answer(&first.id, RequestStatus::Completed).await;

    let batch = pair.reserve().await.unwrap();
    assert_eq!(ids(&batch), [second.id.as_str(), first.id.as_str()]);
    assert_eq!(batch.remaining, 0);

    // A reply landing while that prompt is being typed waits for the next
    // batch rather than starting a second prompt into the same pane.
    let late = pair.answered("late").await;
    assert!(
        pair.reserve().await.is_none(),
        "a second batch for a sender with one in flight"
    );

    pair.store.complete_reply_wakes(&batch, true).await.unwrap();
    for id in [&first.id, &second.id] {
        assert_eq!(pair.released(id).await, Some(ReplyWakeRelease::Delivered));
    }
    assert_eq!(ids(&pair.reserve().await.unwrap()), [late.id.as_str()]);
    assert_eq!(
        pair.store.unread_reply_count(&pair.sender).await,
        3,
        "a delivered wake is not a read"
    );
}

#[tokio::test]
async fn a_batch_is_bounded_and_the_overflow_stays_owed() {
    let pair = Pair::new();
    let mut requests = Vec::new();
    for index in 0..REPLY_WAKE_BATCH_LIMIT + 2 {
        requests.push(pair.answered(&format!("question {index}")).await);
    }
    let batch = pair.reserve().await.unwrap();
    assert_eq!(batch.requests.len(), REPLY_WAKE_BATCH_LIMIT);
    assert_eq!(batch.remaining, 2);
    pair.store.complete_reply_wakes(&batch, true).await.unwrap();

    let rest = pair.reserve().await.unwrap();
    assert_eq!(
        ids(&rest),
        requests[REPLY_WAKE_BATCH_LIMIT..]
            .iter()
            .map(|request| request.id.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(rest.remaining, 0);
}

#[tokio::test]
async fn a_failed_attempt_leaves_the_wake_owed() {
    let pair = Pair::new();
    let request = pair.answered("review").await;
    let batch = pair.reserve().await.unwrap();
    pair.store
        .complete_reply_wakes(&batch, false)
        .await
        .unwrap();
    assert_eq!(pair.wake_of(&request.id).await, None);
    assert_eq!(ids(&pair.reserve().await.unwrap()), [request.id.as_str()]);
}

/// A read or withdrawal that lands while an attempt is in flight decides
/// the outcome. Neither a failed attempt (which would re-owe the wake) nor a
/// successful one (which would relabel it delivered) may overwrite it.
#[tokio::test]
async fn an_attempt_cannot_undo_a_read_or_withdrawal_made_meanwhile() {
    for sent in [false, true] {
        let pair = Pair::new();
        let read = pair.answered("read meanwhile").await;
        let withdrawn = pair.answered("withdrawn meanwhile").await;
        let batch = pair.reserve().await.unwrap();
        assert_eq!(batch.requests.len(), 2);

        pair.store.get_for(&pair.sender, &read.id).await.unwrap();
        pair.store
            .withdraw_reply_wakes(&pair.sender, std::slice::from_ref(&withdrawn.id))
            .await
            .unwrap();
        pair.store.complete_reply_wakes(&batch, sent).await.unwrap();

        assert_eq!(pair.released(&read.id).await, Some(ReplyWakeRelease::Read));
        assert_eq!(
            pair.released(&withdrawn.id).await,
            Some(ReplyWakeRelease::Withdrawn)
        );
        assert!(pair.reserve().await.is_none(), "sent={sent}");
    }
}

/// The reported case: a sender gives up on a request before it is answered.
/// The late reply must not wake it, yet stays a readable, unread result.
#[tokio::test]
async fn withdrawing_before_the_reply_outlives_the_reply() {
    let pair = Pair::new();
    let request = pair.ask("draft chapter").await;
    pair.store.claim_for(&pair.recipient).await.unwrap();
    let withdrawn = pair
        .store
        .withdraw_reply_wakes(&pair.sender, std::slice::from_ref(&request.id))
        .await
        .unwrap();
    assert_eq!(
        withdrawn[0].status,
        RequestStatus::Claimed,
        "work untouched"
    );

    pair.answer(&request.id, RequestStatus::Completed).await;

    assert!(pair.store.pending_reply_unnotified().await.is_empty());
    assert!(pair.reserve().await.is_none());
    assert_eq!(pair.store.unread_reply_count(&pair.sender).await, 1);
    let result = pair
        .store
        .wait_for_terminal(&pair.sender, &request.id, Duration::from_millis(10))
        .await
        .unwrap();
    assert_eq!(
        result.reply.unwrap().body,
        format!("answer to {}", request.id)
    );
    assert_eq!(
        pair.released(&request.id).await,
        Some(ReplyWakeRelease::Withdrawn),
        "reading keeps the first release reason"
    );
}

#[tokio::test]
async fn sending_a_replacement_withdraws_atomically() {
    let pair = Pair::new();
    let old = pair.answered("first attempt").await;
    let other = pair.answered("unrelated question").await;

    let replacement = pair
        .ask_withdrawing("second attempt", &[&old.id, &old.id])
        .await
        .unwrap();
    assert_eq!(
        replacement.withdrew_reply_wakes,
        std::slice::from_ref(&old.id)
    );
    assert_eq!(
        pair.released(&old.id).await,
        Some(ReplyWakeRelease::Withdrawn)
    );
    assert_eq!(
        ids(&pair.reserve().await.unwrap()),
        [other.id.as_str()],
        "only the named request is withdrawn"
    );

    // One bad reference rejects the whole send: no request, no withdrawal.
    let before = pair.store.requests.read().await.len();
    let untouched = pair.answered("keep me").await;
    assert!(matches!(
        pair.ask_withdrawing("third attempt", &[&untouched.id, "req_missing"])
            .await,
        Err(CollaborationError::NotFound(_))
    ));
    assert_eq!(pair.store.requests.read().await.len(), before + 1);
    assert_eq!(pair.wake_of(&untouched.id).await, None);
}

#[tokio::test]
async fn only_the_sender_may_withdraw_and_not_human_decisions() {
    let pair = Pair::new();
    let request = pair.answered("review").await;
    for caller in [pair.recipient.clone(), participant("%1", "a-later-session")] {
        assert!(matches!(
            pair.store
                .withdraw_reply_wakes(&caller, std::slice::from_ref(&request.id))
                .await,
            Err(CollaborationError::NotParticipant(_))
        ));
    }
    assert_eq!(pair.wake_of(&request.id).await, None);

    let human = Participant::console(pair.sender.room.clone());
    let decision = pair
        .store
        .create(
            pair.sender.clone(),
            human,
            NewRequest {
                body: "approve?".into(),
                human_action: Some(HumanAction::Approval),
                ..NewRequest::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        pair.store
            .withdraw_reply_wakes(&pair.sender, std::slice::from_ref(&decision.id))
            .await,
        Err(CollaborationError::ReplyWakeNotWithdrawable(_))
    ));
    assert!(matches!(
        pair.ask_withdrawing("next", &[&decision.id]).await,
        Err(CollaborationError::ReplyWakeNotWithdrawable(_))
    ));
}

#[tokio::test]
async fn a_dispatch_retry_must_name_the_same_withdrawals() {
    let pair = Pair::new();
    let old = pair.answered("first attempt").await;
    let other = pair.answered("other").await;
    let input = NewRequest {
        dispatch_id: Some(uuid::Uuid::new_v4().to_string()),
        body: "second attempt".into(),
        withdraw_reply_wakes: vec![old.id.clone()],
        ..NewRequest::default()
    };
    let sent = pair
        .store
        .create(pair.sender.clone(), pair.recipient.clone(), input.clone())
        .await
        .unwrap();
    assert_eq!(
        pair.store
            .create(pair.sender.clone(), pair.recipient.clone(), input.clone())
            .await
            .unwrap()
            .id,
        sent.id
    );
    let mut changed = input;
    changed.withdraw_reply_wakes = vec![other.id.clone()];
    assert!(matches!(
        pair.store
            .create(pair.sender.clone(), pair.recipient.clone(), changed)
            .await,
        Err(CollaborationError::DispatchConflict(_))
    ));
    assert_eq!(
        pair.wake_of(&other.id).await,
        None,
        "conflict withdrew nothing"
    );
}

/// A reservation belongs to the daemon that made it. After a restart its
/// outcome is unknown, so the wake is owed again; released states survive.
#[tokio::test]
async fn restart_voids_reservations_and_keeps_releases() {
    let dir = tempfile::tempdir().unwrap();
    let options = CollaborationOptions {
        path: Some(dir.path().join("mailbox.json")),
        ..Default::default()
    };
    let pair = Pair::with(CollaborationStore::load(options.clone()).await.unwrap());
    let reserved = pair.answered("in flight at crash").await;
    let batch = pair.reserve().await.unwrap();
    assert_eq!(ids(&batch), [reserved.id.as_str()]);
    let delivered = pair.answered("delivered").await;
    let withdrawn = pair.answered("withdrawn").await;
    pair.store
        .withdraw_reply_wakes(&pair.sender, std::slice::from_ref(&withdrawn.id))
        .await
        .unwrap();
    let delivered_batch = {
        // The in-flight batch blocks a second one for this sender; settle a
        // copy of it holding only `delivered` by reserving after a restart.
        let reloaded = Pair::with(CollaborationStore::load(options.clone()).await.unwrap());
        let batch = reloaded.reserve().await.unwrap();
        assert_eq!(ids(&batch), [reserved.id.as_str(), delivered.id.as_str()]);
        let mut only = batch.clone();
        only.requests.retain(|request| request.id == delivered.id);
        reloaded
            .store
            .complete_reply_wakes(&only, true)
            .await
            .unwrap();
        only
    };
    assert_eq!(delivered_batch.requests.len(), 1);

    let restarted = Pair::with(CollaborationStore::load(options).await.unwrap());
    assert_eq!(restarted.wake_of(&reserved.id).await, None);
    assert_eq!(
        restarted.released(&delivered.id).await,
        Some(ReplyWakeRelease::Delivered)
    );
    assert_eq!(
        restarted.released(&withdrawn.id).await,
        Some(ReplyWakeRelease::Withdrawn)
    );
    assert_eq!(
        ids(&restarted.reserve().await.unwrap()),
        [reserved.id.as_str()]
    );
}

/// Rows written before `reply_wake` existed carry only the two timestamps.
#[test]
fn legacy_rows_derive_their_release_from_timestamps() {
    let pair = Pair::new();
    let now = OffsetDateTime::now_utc();
    let mut request = CollaborationRequest {
        reply: Some(CollaborationReply {
            status: RequestStatus::Completed,
            body: "done".into(),
            artifacts: Vec::new(),
            air_artifacts: Vec::new(),
            at: now,
        }),
        status: RequestStatus::Completed,
        ..legacy_request(&pair)
    };
    let mut json = serde_json::to_value(&request).unwrap();
    json.as_object_mut().unwrap().remove("reply_wake");

    let mut unseen: CollaborationRequest = serde_json::from_value(json.clone()).unwrap();
    unseen.normalize_loaded_reply_wake();
    assert!(unseen.reply_wake_owed());

    request.reply_notified_at = Some(now);
    let mut notified: CollaborationRequest =
        serde_json::from_value(serde_json::to_value(&request).unwrap()).unwrap();
    notified.reply_wake = None;
    notified.normalize_loaded_reply_wake();
    assert!(matches!(
        notified.reply_wake,
        Some(ReplyWake::Released { reason: ReplyWakeRelease::Delivered, at }) if at == now
    ));

    request.reply_read_at = Some(now);
    request.reply_wake = None;
    request.normalize_loaded_reply_wake();
    assert!(matches!(
        request.reply_wake,
        Some(ReplyWake::Released {
            reason: ReplyWakeRelease::Read,
            ..
        })
    ));
}

fn legacy_request(pair: &Pair) -> CollaborationRequest {
    CollaborationRequest {
        dispatch_id: None,
        source_node_id: None,
        delegation_parent: None,
        interruption: None,
        human_action: None,
        id: "req_legacy".into(),
        from: pair.sender.clone(),
        to: pair.recipient.clone(),
        initiator: None,
        updates: Vec::new(),
        provenance: None,
        kind: RequestKind::Question,
        body: "old".into(),
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
        air_artifacts: Vec::new(),
        withdrew_reply_wakes: Vec::new(),
        status: RequestStatus::Queued,
        created_at: OffsetDateTime::now_utc(),
        claimed_at: None,
        wake_delivery: None,
        notified_at: None,
        reply_notified_at: None,
        reply_read_at: None,
        reply_wake: None,
        reply: None,
    }
}

/// A failed attempt whose release also fails to persist must stay
/// retryable by this daemon. Restoring the reservation instead would leave
/// it with no attempt to settle it, blocking the sender until a restart.
#[tokio::test]
async fn a_failed_attempt_stays_retryable_when_its_release_cannot_persist() {
    let dir = tempfile::tempdir().unwrap();
    let options = CollaborationOptions {
        path: Some(dir.path().join("mailbox.json")),
        ..Default::default()
    };
    let pair = Pair::with(CollaborationStore::load(options).await.unwrap());
    let request = pair.answered("review").await;
    let batch = pair.reserve().await.unwrap();

    // A directory where the database file was makes every write fail, for
    // any user including root (permission bits would not stop root).
    let database = pair.store.database_path.clone().unwrap();
    let saved = dir.path().join("saved.sqlite3");
    std::fs::rename(&database, &saved).unwrap();
    std::fs::create_dir(&database).unwrap();
    let outcome = pair.store.complete_reply_wakes(&batch, false).await;
    std::fs::remove_dir(&database).unwrap();
    std::fs::rename(&saved, &database).unwrap();
    assert!(outcome.is_err(), "the fault must actually hit persistence");

    let retry = pair.reserve().await.expect("still owed, without a reload");
    assert_eq!(ids(&retry), [request.id.as_str()]);
}

#[tokio::test]
async fn blank_withdrawal_ids_reject_the_whole_operation() {
    let pair = Pair::new();
    let owned = pair.answered("owned").await;
    let before = pair.store.requests.read().await.len();
    for ids in [vec![owned.id.as_str(), " "], vec![""]] {
        assert!(matches!(
            pair.ask_withdrawing("replacement", &ids).await,
            Err(CollaborationError::InvalidWithdrawal(_))
        ));
        let owned_ids: Vec<String> = ids.iter().map(|id| (*id).to_string()).collect();
        assert!(matches!(
            pair.store
                .withdraw_reply_wakes(&pair.sender, &owned_ids)
                .await,
            Err(CollaborationError::InvalidWithdrawal(_))
        ));
    }
    assert!(matches!(
        pair.store.withdraw_reply_wakes(&pair.sender, &[]).await,
        Err(CollaborationError::InvalidWithdrawal(_))
    ));
    assert_eq!(pair.store.requests.read().await.len(), before);
    assert_eq!(pair.wake_of(&owned.id).await, None);
}

async fn agent_on(pane: &str, session: &str) -> Agent {
    let store = crate::Store::shared();
    store
        .apply(&crate::event::AgentEvent::Started {
            id: crate::event::AgentId {
                kind: AgentKind::Codex,
                session_id: session.into(),
                surface: None,
                pane: Some(pane.into()),
                tmux_socket: Some("default".into()),
                cwd: Some("/repo".into()),
            },
            at: OffsetDateTime::now_utc(),
        })
        .await;
    let mut agent = store.snapshot().await.remove(0);
    agent.state = AgentState::Idle;
    agent
}

/// The waker's participant list is a scan; this is the check it makes right
/// before typing.
#[tokio::test]
async fn still_idle_owner_requires_the_same_session_idle_now() {
    let sender = participant("%1", "sender");
    let idle = agent_on("%1", "sender").await;
    assert!(still_idle_owner(std::slice::from_ref(&idle), &sender));

    let mut working = idle.clone();
    working.state = AgentState::Working;
    assert!(!still_idle_owner(&[working], &sender));

    // `/new` in the same pane: a newer session now owns it.
    let mut replacement = agent_on("%1", "fresh-session").await;
    replacement.last_activity_at = idle.last_activity_at + time::Duration::seconds(1);
    assert!(!still_idle_owner(&[idle.clone(), replacement], &sender));

    let mut stopped = idle;
    stopped.state = AgentState::Stopped;
    assert!(!still_idle_owner(&[stopped], &sender));
}
