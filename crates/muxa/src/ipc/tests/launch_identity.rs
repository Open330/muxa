use super::*;

#[test]
fn legacy_launch_payloads_decode_without_provenance_or_receipt() {
    let snapshot: Request =
        serde_json::from_value(serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"snapshot"}))
            .unwrap();
    assert!(matches!(
        snapshot.body,
        RequestBody::Snapshot {
            launch_identity: false
        }
    ));
    let event = crate::event::AgentEvent::Started {
        id: crate::event::AgentId {
            kind: AgentKind::ClaudeCode,
            session_id: "legacy".into(),
            pane: Some("%1".into()),
            tmux_socket: None,
            surface: None,
            cwd: None,
        },
        at: time::OffsetDateTime::now_utc(),
    };
    let ingest: Request = serde_json::from_value(
        serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"ingest","event":event}),
    )
    .unwrap();
    assert!(matches!(
        ingest.body,
        RequestBody::Ingest {
            launch_id: None,
            ..
        }
    ));
    let value = serde_json::to_value(Response::with_agents(Vec::new())).unwrap();
    assert!(value.get("launch_identities").is_none());
}
