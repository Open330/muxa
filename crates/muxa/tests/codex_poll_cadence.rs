use std::time::Duration;

use muxa::backend::PaneObservation;
use muxa::{AgentEvent, AgentId, AgentKind, AgentState, LivenessSource, Reconciler, Store};
use time::OffsetDateTime;

struct NoPanes;

impl LivenessSource for NoPanes {
    fn observe_panes(&self) -> PaneObservation {
        PaneObservation::complete(Vec::new())
    }
}

#[tokio::test]
async fn codex_status_publishes_before_slow_inventory_tick() {
    let now = OffsetDateTime::now_utc();
    let store = Store::shared();
    let id = AgentId {
        kind: AgentKind::Codex,
        session_id: "cadence".into(),
        surface: None,
        pane: None,
        tmux_socket: None,
        cwd: None,
    };
    store
        .apply(&AgentEvent::Started {
            id,
            at: now - time::Duration::seconds(10),
        })
        .await;
    let root = tempfile::tempdir().unwrap();
    let day = root.path().join(format!(
        "{:04}/{:02}/{:02}",
        now.year(),
        u8::from(now.month()),
        now.day()
    ));
    std::fs::create_dir_all(&day).unwrap();
    std::fs::write(
        day.join("rollout-current-cadence.jsonl"),
        serde_json::json!({
            "timestamp": now.format(&time::format_description::well_known::Rfc3339).unwrap(),
            "type": "response_item", "payload": {"type": "reasoning"}
        })
        .to_string(),
    )
    .unwrap();

    let mut changes = store.subscribe_changes();
    let reconciler = Reconciler::new(store.clone(), NoPanes, Duration::from_secs(60))
        .with_codex_sessions_root(Some(root.path().to_path_buf()));
    let (shutdown_tx, shutdown_rx) = tokio::sync::broadcast::channel(1);
    let task = tokio::spawn(reconciler.run(shutdown_rx));
    let result = tokio::time::timeout(Duration::from_secs(6), changes.changed()).await;
    shutdown_tx.send(()).unwrap();
    task.await.unwrap();
    result
        .expect("Codex status waited for the 60-second inventory tick")
        .unwrap();
    let agent = store.by_session("cadence").await.unwrap();
    assert_eq!(agent.state, AgentState::Working);
    assert_eq!(agent.last_activity_at, now);
}
