//! Golden replies for the Mac app's hand-written IPC types.
//!
//! Each case sends a request the way `apps/tend-macos/Sources/TendIPC*.swift`
//! does and records the daemon's real reply in
//! `apps/tend-macos/Tests/Fixtures/IPC/<name>.json`. The Swift tests decode
//! every fixture with the app's own types (`TendIPCFixtureTests.swift`), so a
//! change here that the app cannot read fails the macOS job.
//!
//! This test fails when a reply's *shape* (its keys and value types, not its
//! ids or timestamps) no longer matches the checked-in fixture. Regenerate
//! with `TEND_UPDATE_IPC_FIXTURES=1 cargo test -p tend --lib swift_fixtures`
//! and commit the result, which runs the Swift decoders on it.
use super::*;
use serde_json::{json, Value};

fn fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/tend-macos/Tests/Fixtures/IPC")
}

/// The JSON's structure with every scalar replaced by its type, so two
/// replies with different ids, times or counts compare equal. An array is
/// the set of its element shapes, so a longer list (another capability, a
/// second agent) is the same shape.
fn shape(value: &Value) -> Value {
    match value {
        Value::Null => json!("null"),
        Value::Bool(_) => json!("bool"),
        Value::Number(_) => json!("number"),
        Value::String(_) => json!("string"),
        Value::Array(items) => {
            let mut shapes: Vec<Value> = Vec::new();
            for item in items.iter().map(shape) {
                if !shapes.contains(&item) {
                    shapes.push(item);
                }
            }
            shapes.sort_by_key(ToString::to_string);
            Value::Array(shapes)
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), shape(value)))
                .collect(),
        ),
    }
}

fn request(kind: &str, fields: Value) -> Value {
    let mut request = json!({ "protocol": PROTOCOL_VERSION, "kind": kind });
    if let (Some(request), Value::Object(fields)) = (request.as_object_mut(), fields) {
        request.extend(fields);
    }
    request
}

async fn call_ok(socket: &Path, request: Value) -> Value {
    let reply = raw_call(socket, &request).await;
    assert_eq!(reply["ok"], true, "{request} failed: {reply}");
    reply
}

/// State the replies describe: one agent with a prompt, an open and a
/// decided review, one planned Work.
async fn seed(socket: &Path) {
    for event in [
        json!({"type": "started", "id": {"kind": "claude_code", "session_id": "s1",
               "pane": "%1", "cwd": "/repo"}, "at": "2026-11-12T00:00:00Z"}),
        json!({"type": "prompt_submitted", "id": {"kind": "claude_code", "session_id": "s1",
               "pane": "%1", "cwd": "/repo"}, "at": "2026-11-12T00:00:01Z",
               "prompt": "fix the flaky test"}),
    ] {
        call_ok(socket, request("ingest", json!({ "event": event }))).await;
    }
    call_ok(
        socket,
        request(
            "review_create",
            json!({"review": {
                "target": {"kind": "session", "provider": "claude", "session_id": "s1"},
                "summary": "check the migration",
                "idempotency_key": "fixture:1",
                "requested_by": {"kind": "agent", "label": "impl"},
            }}),
        ),
    )
    .await;
    call_ok(
        socket,
        request(
            "review_record",
            json!({
                "review": {
                    "target": {"kind": "session", "provider": "claude", "session_id": "s1"},
                    "summary": "Reviewed in Home",
                    "idempotency_key": "fixture:2",
                    "requested_by": {"kind": "user", "label": "Tend.app"},
                },
                "disposition": "accepted",
            }),
        ),
    )
    .await;
    call_ok(
        socket,
        request(
            "work_edit",
            json!({
                "identity": {"workspace_id": "tend", "work_id": "ENG-1"},
                "edit": {"title": "Fix the flaky test", "goal": "CI is green", "stage": "queued"},
            }),
        ),
    )
    .await;
}

#[tokio::test]
async fn swift_ipc_fixtures_match_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("tendd.sock");
    let store = Store::shared();
    let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
    let (stop, serving) = wait_test_server(&socket, &store, &mailbox).await;

    seed(&socket).await;

    // The requests as the Mac app sends them.
    let console = json!({"pane": "", "console": true});
    let cases: Vec<(&str, Value)> = vec![
        ("hello", request("hello", json!({"client": "tend-macos"}))),
        ("snapshot", request("snapshot", json!({}))),
        ("list_sessions", request("list_sessions", json!({}))),
        ("pipeline_runs", request("pipeline_runs", json!({}))),
        ("ask_status", request("ask_status", json!({}))),
        ("ask_list", request("ask_list", json!({}))),
        ("ask_providers", request("ask_providers", json!({}))),
        (
            "review_list",
            request(
                "review_list",
                json!({"open_only": true, "oldest_first": true, "limit": 50, "offset": 0}),
            ),
        ),
        (
            "decision_queue",
            request("decision_queue", json!({"limit": 50, "origin": console})),
        ),
        (
            "decision_history",
            request("decision_history", json!({"limit": 50, "origin": console})),
        ),
        (
            "work_backlog",
            request("work_backlog", json!({"stage": "queued"})),
        ),
    ];

    let update = std::env::var_os("TEND_UPDATE_IPC_FIXTURES").is_some();
    let root = fixture_dir();
    if update {
        std::fs::create_dir_all(&root).unwrap();
    }
    let mut stale = Vec::new();
    for (name, request) in cases {
        let reply = call_ok(&socket, request).await;
        let path = root.join(format!("{name}.json"));
        if update {
            let mut text = serde_json::to_string_pretty(&reply).unwrap();
            text.push('\n');
            std::fs::write(&path, text).unwrap();
            continue;
        }
        let checked_in: Value = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(Value::Null);
        if shape(&checked_in) != shape(&reply) {
            stale.push(name);
        }
    }
    let _ = stop.send(());
    serving.await.unwrap();
    assert!(
        stale.is_empty(),
        "IPC replies changed shape for {stale:?}. If the change is intended, run \
         `TEND_UPDATE_IPC_FIXTURES=1 cargo test -p tend --lib swift_fixtures` and commit \
         apps/tend-macos/Tests/Fixtures/IPC; the macOS job then checks that the app still \
         decodes them."
    );
}
