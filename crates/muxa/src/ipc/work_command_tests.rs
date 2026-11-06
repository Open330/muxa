use super::*;
use crate::fleet::HostAccessMode;
use crate::work_control::{WorkCommandError, WorkCommandOutput};
use std::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecordedRun {
    host: String,
    args: Vec<String>,
    stdin: Option<String>,
    limits: WorkCommandLimits,
}

/// Fake Fleet transport: one configured host with a fixed mode, recording
/// every argv it is asked to run.
struct FakeRunner {
    host: &'static str,
    mode: HostAccessMode,
    output: WorkCommandOutput,
    runs: Mutex<Vec<RecordedRun>>,
}

impl FakeRunner {
    fn new(mode: HostAccessMode, stdout: &str) -> Arc<Self> {
        Arc::new(Self {
            host: "dev",
            mode,
            output: WorkCommandOutput {
                exit_code: 0,
                stdout: stdout.into(),
                stderr: String::new(),
            },
            runs: Mutex::new(Vec::new()),
        })
    }

    fn runs(&self) -> Vec<RecordedRun> {
        self.runs.lock().unwrap().clone()
    }
}

impl RemoteWorkRunner for FakeRunner {
    fn host_mode<'a>(&'a self, host: &'a str) -> work_control::HostModeFuture<'a> {
        Box::pin(async move {
            if host == self.host {
                Ok(self.mode)
            } else {
                Err(WorkCommandError::Invalid(format!(
                    "fleet host '{host}' is not configured"
                )))
            }
        })
    }

    fn run<'a>(
        &'a self,
        host: &'a str,
        args: Vec<String>,
        stdin: Option<String>,
        limits: WorkCommandLimits,
    ) -> work_control::RemoteWorkFuture<'a> {
        Box::pin(async move {
            self.runs.lock().unwrap().push(RecordedRun {
                host: host.to_string(),
                args,
                stdin,
                limits,
            });
            Ok(self.output.clone())
        })
    }
}

#[tokio::test]
async fn global_ask_uses_coordinator_response_without_touching_local_history() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("entry.sock");
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        "[orchestration]\nenabled = true\ncoordinator = \"dev\"\n",
    )
    .unwrap();
    let runner = FakeRunner::new(
        HostAccessMode::Control,
        r#"{"ok":true,"protocol":1,"ask_entries":[]}"#,
    );
    let mut server =
        Server::new(socket.clone(), crate::Store::shared()).with_config_path(Some(config));
    server.work_up = WorkUpManager::with_remote(socket.clone(), Some(runner.clone()));
    let (shutdown, rx) = tokio::sync::broadcast::channel(1);
    let handle = tokio::spawn(server.run(rx));
    for _ in 0..100 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let response = Client::new(socket)
        .call(&serde_json::json!({"kind":"ask_list"}))
        .await
        .unwrap();
    assert_eq!(response["ask_entries"], serde_json::json!([]));
    let sent = runner.runs();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].host, "dev");
    assert_eq!(sent[0].args, vec!["work", "fleet-ask"]);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(sent[0].stdin.as_deref().unwrap()).unwrap()
            ["kind"],
        "ask_list"
    );
    shutdown.send(()).unwrap();
    handle.await.unwrap().unwrap();
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_string()).collect()
}

fn request(host: Option<&str>) -> WorkUpRequest {
    WorkUpRequest {
        work: "W-7".into(),
        external: None,
        pipeline: Some("solo".into()),
        workspace: None,
        cwd: Some(PathBuf::from("/srv/remote/checkout")),
        skill: None,
        body: Some("ship it".into()),
        context: None,
        no_ticket: true,
        dry_run: false,
        host: host.map(str::to_string),
    }
}

async fn wait_settled(manager: &WorkUpManager, operation_id: &str) -> WorkUpOperation {
    for _ in 0..200 {
        let operation = manager.status(operation_id).await.unwrap();
        if operation.state != WorkUpOperationState::Running {
            return operation;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("operation {operation_id} never settled");
}

#[test]
fn work_command_request_decodes_the_documented_shape() {
    let request: Request = serde_json::from_str(
            r#"{"protocol":6,"kind":"work_command","host":"dev","args":["work","options","--json"],"stdin":null}"#,
        )
        .unwrap();
    match request.body {
        RequestBody::WorkCommand { host, args, stdin } => {
            assert_eq!(host.as_deref(), Some("dev"));
            assert_eq!(args, ["work", "options", "--json"]);
            assert_eq!(stdin, None);
        }
        _ => panic!("wrong request kind"),
    }
    let request: Request = serde_json::from_str(
            r#"{"protocol":6,"kind":"work_command","args":["work","pipeline","set","--from-json","-"],"stdin":"{}"}"#,
        )
        .unwrap();
    match request.body {
        RequestBody::WorkCommand { host, args, stdin } => {
            assert_eq!(host, None);
            assert_eq!(args[1], "pipeline");
            assert_eq!(stdin.as_deref(), Some("{}"));
        }
        _ => panic!("wrong request kind"),
    }
    let request: Request = serde_json::from_str(
        r#"{"protocol":6,"kind":"work_up","request":{"work":"W-7","host":"dev","cwd":"/srv/x"}}"#,
    )
    .unwrap();
    match request.body {
        RequestBody::WorkUp { request } => assert_eq!(request.remote_host(), Some("dev")),
        _ => panic!("wrong request kind"),
    }
    assert!(CAPABILITIES.contains(&"work_command_v1"));
}

#[test]
fn work_command_response_encodes_the_documented_shape() {
    let response = Response::with_work_command(WorkCommandOutput {
        exit_code: 0,
        stdout: "{\"routes\":[]}\n".into(),
        stderr: String::new(),
    });
    let encoded = serde_json::to_value(&response).unwrap();
    assert_eq!(encoded["ok"], true);
    assert_eq!(
        encoded["work_command"],
        serde_json::json!({"exit_code": 0, "stdout": "{\"routes\":[]}\n", "stderr": ""})
    );
    let plain = serde_json::to_value(Response::ok()).unwrap();
    assert!(plain.get("work_command").is_none());
}

#[tokio::test]
async fn work_up_with_a_control_host_runs_the_same_argv_on_the_runner() {
    let runner = FakeRunner::new(HostAccessMode::Control, "{\"work\":\"W-7\",\"agents\":2}\n");
    let manager = WorkUpManager::with_remote(
        PathBuf::from("/tmp/muxa-work-remote-test.sock"),
        Some(runner.clone()),
    );
    let started = manager.start(request(Some("dev"))).await.unwrap();
    assert_eq!(started.state, WorkUpOperationState::Running);
    let settled = wait_settled(&manager, &started.operation_id).await;
    assert_eq!(settled.state, WorkUpOperationState::Succeeded);
    assert_eq!(settled.message, "Work pipeline started");
    assert_eq!(settled.result.unwrap()["agents"], 2);
    assert_eq!(
        runner.runs(),
        vec![RecordedRun {
            host: "dev".into(),
            args: request(None).arguments(),
            stdin: None,
            limits: WorkCommandLimits::WORK_UP,
        }]
    );
    // The remote cwd travelled untouched.
    assert!(runner.runs()[0]
        .args
        .windows(2)
        .any(|pair| pair == ["--cwd", "/srv/remote/checkout"]));
}

#[tokio::test]
async fn work_up_remote_failure_is_reported_from_the_last_stderr_line() {
    let runner = Arc::new(FakeRunner {
        host: "dev",
        mode: HostAccessMode::Control,
        output: WorkCommandOutput {
            exit_code: 1,
            stdout: String::new(),
            stderr: "note\nerror: no route matched W-7\n".into(),
        },
        runs: Mutex::new(Vec::new()),
    });
    let manager = WorkUpManager::with_remote(
        PathBuf::from("/tmp/muxa-work-remote-test.sock"),
        Some(runner),
    );
    let started = manager.start(request(Some("dev"))).await.unwrap();
    let settled = wait_settled(&manager, &started.operation_id).await;
    assert_eq!(settled.state, WorkUpOperationState::Failed);
    assert_eq!(
        settled.message,
        "muxa work up failed: error: no route matched W-7"
    );
}

#[tokio::test]
async fn work_up_on_an_observe_host_is_refused_before_anything_runs() {
    let runner = FakeRunner::new(HostAccessMode::Observe, "{}");
    let manager = WorkUpManager::with_remote(
        PathBuf::from("/tmp/muxa-work-remote-test.sock"),
        Some(runner.clone()),
    );
    let error = manager.start(request(Some("dev"))).await.unwrap_err();
    assert!(error.contains("observe-only"), "{error}");
    assert!(error.contains("mode = \"control\""), "{error}");
    assert!(runner.runs().is_empty());
    let error = manager.start(request(Some("nope"))).await.unwrap_err();
    assert!(error.contains("not configured"), "{error}");
}

#[tokio::test]
async fn remote_hosts_are_refused_when_fleet_is_not_installed() {
    let manager = WorkUpManager::new(PathBuf::from("/tmp/muxa-work-remote-test.sock"));
    let error = manager.start(request(Some("dev"))).await.unwrap_err();
    assert!(error.contains("fleet is not enabled"), "{error}");
    let error = manager
        .command(
            Some("dev".into()),
            argv(&["work", "options", "--json"]),
            None,
        )
        .await
        .unwrap_err();
    assert!(error.contains("fleet is not enabled"), "{error}");
}

// WS-B: new agent
fn agent_request(host: Option<&str>) -> crate::agent_control::AgentStartRequest {
    serde_json::from_value(serde_json::json!({
        "agent": "codex",
        "host": host,
        "placement": "window",
        "target": "$3",
        "cwd": "/srv/remote/checkout",
        "prompt": "fix the flaky test",
    }))
    .unwrap()
}

#[tokio::test]
async fn agent_start_on_a_control_host_runs_the_cli_argv_on_the_runner() {
    let runner = FakeRunner::new(
        HostAccessMode::Control,
        r#"{"schema_version":1,"host":"tmux","agent":"codex","placement":"window","pane":"%42","cwd":"/srv/remote/checkout","prompt_supplied":true}"#,
    );
    let manager = WorkUpManager::with_remote(
        PathBuf::from("/tmp/muxa-work-remote-test.sock"),
        Some(runner.clone()),
    );
    let result = manager
        .agent_start(agent_request(Some("dev")))
        .await
        .unwrap();
    assert_eq!(result.pane.as_deref(), Some("%42"));
    assert_eq!(result.fleet_host.as_deref(), Some("dev"));
    let runs = runner.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].args, agent_request(Some("dev")).arguments());
    assert_eq!(runs[0].limits, WorkCommandLimits::COMMAND);
    // What the runner is asked to carry is what the relay will accept.
    work_control::validate_work_command(&runs[0].args, None, WorkCommandSurface::Relay).unwrap();
}

#[tokio::test]
async fn agent_start_is_refused_on_observe_hosts_and_bad_requests() {
    let runner = FakeRunner::new(HostAccessMode::Observe, "{}");
    let manager = WorkUpManager::with_remote(
        PathBuf::from("/tmp/muxa-work-remote-test.sock"),
        Some(runner.clone()),
    );
    let error = manager
        .agent_start(agent_request(Some("dev")))
        .await
        .unwrap_err();
    assert!(error.contains("observe-only"), "{error}");
    let mut native = agent_request(Some("dev"));
    native.placement = crate::agent_control::AgentPlacement::Native;
    native.target = None;
    let error = manager.agent_start(native).await.unwrap_err();
    assert!(error.contains("this Mac"), "{error}");
    assert!(runner.runs().is_empty());
}

#[test]
fn agent_start_request_decodes_and_response_encodes() {
    let request: Request = serde_json::from_str(
            r#"{"protocol":6,"kind":"agent_start","request":{"agent":"claude","placement":"session","cwd":"/srv/app"}}"#,
        )
        .unwrap();
    assert!(matches!(request.body, RequestBody::AgentStart { .. }));
    assert!(CAPABILITIES.contains(&"agent_start_v1"));
    let plain = serde_json::to_value(Response::ok()).unwrap();
    assert!(plain.get("agent_start").is_none());
}

// WS-F: snapshot
#[test]
fn mux_snapshot_requests_decode_the_documented_shapes() {
    let decode = |json: &str| serde_json::from_str::<Request>(json).unwrap().body;
    assert!(matches!(
        decode(r#"{"protocol":6,"kind":"mux_snapshot_list"}"#),
        RequestBody::MuxSnapshotList
    ));
    assert!(matches!(
        decode(r#"{"protocol":6,"kind":"mux_snapshot_save"}"#),
        RequestBody::MuxSnapshotSave
    ));
    assert!(matches!(
        decode(r#"{"protocol":6,"kind":"mux_snapshot_plan","id":"1790000000"}"#),
        RequestBody::MuxSnapshotPlan { id, only_missing: true } if id == "1790000000"
    ));
    assert!(matches!(
        decode(r#"{"protocol":6,"kind":"mux_snapshot_plan","id":"1","only_missing":false}"#),
        RequestBody::MuxSnapshotPlan {
            only_missing: false,
            ..
        }
    ));
    match decode(r#"{"protocol":6,"kind":"mux_snapshot_restore","id":"1790000000"}"#) {
        RequestBody::MuxSnapshotRestore {
            id,
            layout_only,
            only_missing,
            continue_text,
        } => {
            assert_eq!(id, "1790000000");
            assert!(!layout_only, "restores the commands unless asked not to");
            assert!(
                only_missing,
                "a mux_snapshot_v1 client never fills live sessions"
            );
            assert_eq!(continue_text, None, "nothing is typed unless asked");
        }
        other => panic!("unexpected {other:?}"),
    }
    assert!(matches!(
        decode(r#"{"protocol":6,"kind":"mux_snapshot_restore","id":"1","continue":"go on"}"#),
        RequestBody::MuxSnapshotRestore {
            continue_text: Some(text),
            ..
        } if text == "go on"
    ));
    assert!(matches!(
        decode(r#"{"protocol":6,"kind":"mux_snapshot_restore","id":"1","only_missing":false}"#),
        RequestBody::MuxSnapshotRestore {
            only_missing: false,
            ..
        }
    ));
    assert!(matches!(
        decode(
            r#"{"protocol":6,"kind":"mux_snapshot_restore_status","operation_id":"snapshot-restore-1"}"#
        ),
        RequestBody::MuxSnapshotRestoreStatus { .. }
    ));
    assert!(matches!(
        decode(r#"{"protocol":6,"kind":"mux_snapshot_delete","id":"1790000000"}"#),
        RequestBody::MuxSnapshotDelete { .. }
    ));
    assert!(CAPABILITIES.contains(&"mux_snapshot_v1"));
    assert!(CAPABILITIES.contains(&"mux_snapshot_plan_v1"));
    assert!(CAPABILITIES.contains(&"mux_snapshot_continue_v1"));
    let plain = serde_json::to_value(Response::ok()).unwrap();
    assert!(plain.get("mux_snapshot").is_none());
    assert!(plain.get("mux_snapshot_operation").is_none());
    let listed = serde_json::to_value(Response::with_mux_snapshot(
        serde_json::json!({"snapshots": []}),
    ))
    .unwrap();
    assert_eq!(listed["mux_snapshot"]["snapshots"], serde_json::json!([]));
}

#[tokio::test]
async fn work_command_on_a_remote_host_forwards_argv_and_stdin() {
    let runner = FakeRunner::new(HostAccessMode::Control, "{\"pipeline\":\"solo\"}\n");
    let manager = WorkUpManager::with_remote(
        PathBuf::from("/tmp/muxa-work-remote-test.sock"),
        Some(runner.clone()),
    );
    let output = manager
        .command(
            Some("dev".into()),
            argv(&["work", "pipeline", "set", "--from-json", "-"]),
            Some("{\"name\":\"solo\"}".into()),
        )
        .await
        .unwrap();
    assert_eq!(output.exit_code, 0);
    assert_eq!(output.stdout, "{\"pipeline\":\"solo\"}\n");
    assert_eq!(
        runner.runs(),
        vec![RecordedRun {
            host: "dev".into(),
            args: argv(&["work", "pipeline", "set", "--from-json", "-"]),
            stdin: Some("{\"name\":\"solo\"}".into()),
            limits: WorkCommandLimits::COMMAND,
        }]
    );
}

#[tokio::test]
async fn work_command_on_an_observe_host_may_only_read_options() {
    let runner = FakeRunner::new(HostAccessMode::Observe, "{}");
    let manager = WorkUpManager::with_remote(
        PathBuf::from("/tmp/muxa-work-remote-test.sock"),
        Some(runner.clone()),
    );
    manager
        .command(
            Some("dev".into()),
            argv(&["work", "options", "--json"]),
            None,
        )
        .await
        .unwrap();
    for args in [
        argv(&["work", "preset", "apply", "solo"]),
        argv(&["work", "pipeline", "set", "--from-json", "-"]),
        argv(&["work", "route", "remove", "ENG-.*"]),
    ] {
        let error = manager
            .command(Some("dev".into()), args.clone(), None)
            .await
            .unwrap_err();
        assert!(error.contains("observe-only"), "{args:?}: {error}");
    }
    assert_eq!(runner.runs().len(), 1);
}

#[tokio::test]
async fn work_command_rejects_non_allowlisted_argv_before_dispatch() {
    let runner = FakeRunner::new(HostAccessMode::Control, "{}");
    let manager = WorkUpManager::with_remote(
        PathBuf::from("/tmp/muxa-work-remote-test.sock"),
        Some(runner.clone()),
    );
    for args in [
        argv(&["work", "up", "W-7", "--json", "--yes"]),
        argv(&["fleet", "status"]),
        argv(&["--socket", "/tmp/x.sock", "work", "options"]),
        argv(&["work", "options", "--config", "/tmp/other.toml"]),
        argv(&[]),
    ] {
        let error = manager
            .command(Some("dev".into()), args.clone(), None)
            .await
            .unwrap_err();
        assert!(
            error.starts_with("invalid work command"),
            "{args:?}: {error}"
        );
        let error = manager.command(None, args.clone(), None).await.unwrap_err();
        assert!(
            error.starts_with("invalid work command"),
            "{args:?}: {error}"
        );
    }
    let error = manager
        .command(Some("   ".into()), argv(&["work", "options"]), None)
        .await
        .unwrap_err();
    assert_eq!(error, "host alias is empty");
    assert!(runner.runs().is_empty());
}
