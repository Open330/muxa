//! Operator promotion of exact sessions, as used for muxa-launched workers
//! that predate session purposes.

use super::*;

fn agent(pane: &str, session: &str) -> Participant {
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
        launch_purpose: LaunchPurpose::Primary,
        draining: false,
    }
}

async fn purposes_of(
    store: &CollaborationStore,
    participants: &[Participant],
) -> Vec<LaunchPurpose> {
    let mut participants = participants.to_vec();
    store.enrich_launch_purposes(&mut participants, &[]).await;
    participants.iter().map(|p| p.launch_purpose).collect()
}

/// Promoting named sessions makes exactly those accept peer work, in one
/// write, and leaves everyone else protected.
#[tokio::test]
async fn promoted_sessions_accept_peer_work_and_others_stay_protected() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let first = agent("%1", "worker-a");
    let second = agent("%2", "worker-b");
    let human = agent("%3", "human");
    let everyone = [first.clone(), second.clone(), human.clone()];
    assert_eq!(
        purposes_of(&store, &everyone).await,
        [LaunchPurpose::Primary; 3]
    );

    store
        .set_purposes(&[first, second], LaunchPurpose::Worker)
        .await
        .unwrap();

    assert_eq!(
        purposes_of(&store, &everyone).await,
        [
            LaunchPurpose::Worker,
            LaunchPurpose::Worker,
            LaunchPurpose::Primary
        ]
    );
}

#[tokio::test]
async fn promotion_refuses_console_and_pending_sessions_whole() {
    let store = CollaborationStore::in_memory(CollaborationOptions::default());
    let worker = agent("%1", "worker-a");
    for odd in [
        Participant::console(worker.room.clone()),
        agent("%2", "pending-pane:default:%2"),
    ] {
        assert!(store
            .set_purposes(&[worker.clone(), odd], LaunchPurpose::Worker)
            .await
            .is_err());
    }
    assert_eq!(
        purposes_of(&store, std::slice::from_ref(&worker)).await,
        [LaunchPurpose::Primary],
        "a refused batch changes nothing"
    );
}
