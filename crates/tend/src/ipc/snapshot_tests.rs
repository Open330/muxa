use super::*;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

fn origin() -> CollaborationOrigin {
    CollaborationOrigin {
        pane: "%1".into(),
        socket: Some("default".into()),
        console: true,
        agent_session_id: None,
        exact_session: false,
    }
}

async fn read_request(
    stream: tokio::net::UnixStream,
) -> (
    tokio::io::BufReader<tokio::net::UnixStream>,
    serde_json::Value,
) {
    let mut reader = tokio::io::BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    reader
        .get_mut()
        .write_all(b"{\"ok\":true,\"protocol\":6}\n")
        .await
        .unwrap();
    line.clear();
    reader.read_line(&mut line).await.unwrap();
    let request = serde_json::from_str(&line).unwrap();
    (reader, request)
}

#[tokio::test]
async fn snapshot_legacy_fallback_keeps_bounded_full_requests() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("legacy-mailbox.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let old = CollaborationStore::in_memory(CollaborationOptions::default());
    let console = collaboration::Participant::console(collaboration::RoomId {
        host: "tmux".into(),
        socket: Some("default".into()),
        window_id: "@1".into(),
    });
    let mut sender = console.clone();
    sender.console = false;
    sender.agent_session_id = "sender".into();
    let request = old
        .create(
            sender,
            console,
            collaboration::NewRequest {
                body: "private legacy question".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let serving = tokio::spawn(async move {
        for _ in 0..3 {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut reader, input) = read_request(stream).await;
            let response = if input["kind"] == "collaboration_mailbox_snapshot" {
                serde_json::json!({"ok":false,"error":"bad request: unknown variant collaboration_mailbox_snapshot"})
            } else {
                assert_eq!(input["kind"], "collaboration_list");
                assert_eq!(
                    input["query"]["limit"],
                    crate::fleet::FLEET_MAILBOX_REQUEST_LIMIT
                );
                serde_json::json!({"ok":true,"collaboration_scope":"caller","collaboration_requests":if input["mailbox"]=="incoming" {vec![request.clone()]} else {vec![]}})
            };
            reader
                .get_mut()
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        }
    });
    let client = Client::new(socket).with_mailbox_compat_cache();
    let snapshot = client
        .collaboration_mailbox_snapshot(&origin(), true, Some("old-token"))
        .await
        .unwrap();
    assert!(snapshot.token.is_none() && !snapshot.unchanged);
    assert_eq!(snapshot.incoming[0].body, "private legacy question");
    serving.await.unwrap();
}

#[tokio::test]
async fn snapshot_corrupt_conditional_hit_is_rejected_and_invalidates_compatibility_cache() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("corrupt-mailbox.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let serving = tokio::spawn(async move {
        for n in 0..3 {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut reader, input) = read_request(stream).await;
            assert_eq!(
                input["if_unchanged"],
                if n == 1 {
                    serde_json::json!("first")
                } else {
                    serde_json::Value::Null
                }
            );
            let response = serde_json::json!({"ok":true,"collaboration_mailbox_snapshot":{"token":if n==0 {"first"} else {"different"},"unchanged":n==1,"incoming":[],"sent":[]}});
            reader
                .get_mut()
                .write_all(format!("{response}\n").as_bytes())
                .await
                .unwrap();
        }
    });
    let client = Client::new(socket).with_mailbox_compat_cache();
    client
        .collaboration_mailbox_snapshot(&origin(), true, None)
        .await
        .unwrap();
    assert!(client
        .collaboration_mailbox_snapshot(&origin(), true, None)
        .await
        .is_err());
    client
        .collaboration_mailbox_snapshot(&origin(), true, None)
        .await
        .unwrap();
    serving.await.unwrap();
}

#[tokio::test]
async fn snapshot_authorization_failure_never_falls_back_to_a_legacy_listing() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("denied-mailbox.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let serving = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (mut reader, input) = read_request(stream).await;
        assert_eq!(input["kind"], "collaboration_mailbox_snapshot");
        reader
            .get_mut()
            .write_all(b"{\"ok\":false,\"error\":\"origin denied\"}\n")
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
    });
    let client = Client::new(socket).with_mailbox_compat_cache();
    let error = client
        .collaboration_mailbox_snapshot(&origin(), true, Some("prior-token"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("origin denied"));
    serving.await.unwrap();
}
