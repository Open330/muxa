//! Daemon-owned stop driver. The action gate lives inside the blocking task,
//! so disconnecting or canceling the IPC waiter never releases it during IO.
use super::{
    collaboration, collaboration_participants, resolve_backend, Arc, CollaborationStore, HostKind,
    PaneInfo, SharedBackend, SharedStore,
};
use crate::backend::PaneBackend;
use crate::collaboration::{WorkerDrainStop, WorkerDrainStopOutcome as Outcome, WorkerDrainToken};
use std::sync::atomic::{AtomicBool, Ordering};

struct GuardedBackend {
    inner: SharedBackend,
    socket: Option<String>,
    safe: Box<dyn Fn() -> Option<bool> + Send + Sync>,
    attempted: AtomicBool,
    captured: std::sync::Mutex<Option<String>>,
}
impl GuardedBackend {
    fn allowed(&self) -> bool {
        (self.safe)() == Some(false)
    }
}
impl PaneBackend for GuardedBackend {
    fn kind(&self) -> HostKind {
        self.inner.kind()
    }
    fn list_panes(&self) -> Vec<PaneInfo> {
        self.inner.list_panes()
    }
    fn resolve_pane(&self, p: &str) -> Option<PaneInfo> {
        self.inner.resolve_pane(p)
    }
    fn capture_pane(&self, p: &str) -> Option<String> {
        let screen = self.inner.capture_pane_on(self.socket.as_deref(), p);
        self.captured.lock().ok()?.clone_from(&screen);
        screen
    }
    fn capture_pane_on(&self, _: Option<&str>, p: &str) -> Option<String> {
        self.capture_pane(p)
    }
    fn pane_pid_map(&self) -> std::collections::HashMap<u32, String> {
        self.inner.pane_pid_map()
    }
    fn current_pane(&self) -> Option<String> {
        self.inner.current_pane()
    }
    fn focus_pane(&self, _: &str) -> bool {
        false
    }
    fn send_text_on(&self, s: Option<&str>, p: &str, t: &str) -> bool {
        if !self.allowed() {
            return false;
        }
        self.attempted.store(true, Ordering::SeqCst);
        self.inner.send_text_on(s, p, t)
    }
    fn send_prompt_text_on(&self, s: Option<&str>, p: &str, t: &str) -> bool {
        if !self.allowed() {
            return false;
        }
        // Re-read after the state/process probe: even an otherwise idle
        // human draft must veto quit if the screen changed during that probe.
        let Some(previous) = self.captured.lock().ok().and_then(|s| s.clone()) else {
            return false;
        };
        if self.inner.capture_pane_on(self.socket.as_deref(), p) != Some(previous) {
            return false;
        }
        self.attempted.store(true, Ordering::SeqCst);
        self.inner.send_prompt_text_on(s, p, t)
    }
}

#[allow(clippy::too_many_lines)] // One guarded lifecycle keeps the IO ownership visible.
pub(super) async fn stop(
    store: SharedStore,
    collaboration: Arc<CollaborationStore>,
    backends: Vec<SharedBackend>,
    token: WorkerDrainToken,
    operation_id: String,
) -> Result<WorkerDrainStop, String> {
    // Spawn before awaiting the pane gate: the entire operation belongs to the
    // daemon even if its socket closes at any later await point.
    tokio::spawn(async move {
        let gate = collaboration.worker_action_guard(&token.target.pane).await;
        let topology =
            collaboration_participants(&store, &backends, &collaboration, None, None).await;
        let (replay, dispatch) = collaboration
            .worker_drain_begin_stop(
                &token,
                &operation_id,
                &topology.participants,
                &topology.agents,
                &topology.panes,
            )
            .await
            .map_err(|e| e.to_string())?;
        if !dispatch {
            return Ok(replay);
        }
        let backend = match resolve_backend(&backends, &token.target.pane) {
            Ok(b) if b.caps().send_text => b.clone(),
            Ok(_) => {
                return collaboration
                    .worker_drain_complete_stop(
                        &token,
                        Outcome::Refused,
                        "pane backend cannot send input",
                    )
                    .await
                    .map_err(|e| e.to_string())
            }
            Err(_) => {
                return collaboration
                    .worker_drain_complete_stop(
                        &token,
                        Outcome::Refused,
                        "pane backend unavailable",
                    )
                    .await
                    .map_err(|e| e.to_string())
            }
        };
        let handle = tokio::runtime::Handle::current();
        let guarded_store = store.clone();
        let guarded_collaboration = collaboration.clone();
        let guarded_backends = backends.clone();
        let guarded_token = token.clone();
        let safe = move || {
            match guarded_token.process_fence.exited_status() {
                Some(true) => return Some(true),
                Some(false) => {}
                None => return None,
            }
            handle.block_on(async {
                let fresh = collaboration_participants(
                    &guarded_store,
                    &guarded_backends,
                    &guarded_collaboration,
                    None,
                    None,
                )
                .await;
                if !guarded_collaboration
                    .worker_drain_stop_safe(&guarded_token, &fresh.participants, &fresh.agents)
                    .await
                {
                    return None;
                }
                let pane = collaboration::unique_pane(
                    &guarded_token.target.pane,
                    guarded_token.target.socket.as_deref(),
                    &fresh.panes,
                )?;
                if collaboration::ProcessFence::observe(pane, guarded_token.target.agent_kind)
                    .as_ref()
                    != Some(&guarded_token.process_fence)
                {
                    return None;
                }
                let workload = crate::process_tree::scan_pane_workload(pane);
                if workload.process_count > 0 || workload.subagent_count > 0 {
                    return None;
                }
                Some(false)
            })
        };
        let stop_token = token.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _gate = gate;
            if stop_token.process_fence.definitely_exited() {
                return (Outcome::Exited, "fenced process exited before input".into());
            }
            let guarded = GuardedBackend {
                inner: backend.clone(),
                socket: stop_token.target.socket.clone(),
                safe: Box::new(safe),
                attempted: AtomicBool::new(false),
                captured: std::sync::Mutex::new(None),
            };
            let endpoint = stop_token.target.socket.as_deref();
            let pane = &stop_token.target.pane;
            let target = backend.list_panes();
            let Some(info) = collaboration::unique_pane(pane, endpoint, &target) else {
                return (Outcome::Refused, "pane endpoint unobservable".into());
            };
            let shell =
                crate::account_swap::pane_has_shell(info.pane_pid, stop_token.process_fence.pid);
            let mut original = None;
            match shell {
                None => return (Outcome::Refused, "pane shell ancestry unobservable".into()),
                Some(false) => {
                    let Ok(value) = backend.remain_on_exit_override(endpoint, pane) else {
                        return (Outcome::Refused, "cannot preserve direct-exec pane".into());
                    };
                    original = Some(value);
                    if !guarded.allowed() {
                        return (
                            Outcome::Refused,
                            "stop guard changed before preserving pane".into(),
                        );
                    }
                    if !backend.set_remain_on_exit_on(endpoint, pane, true) {
                        // The failed host response might follow a partial option write.
                        if guarded.allowed() {
                            backend.restore_remain_on_exit(
                                endpoint,
                                pane,
                                original.as_ref().and_then(|v| v.as_deref()),
                            );
                        }
                        return (Outcome::Refused, "cannot preserve direct-exec pane".into());
                    }
                }
                Some(true) => {}
            }
            let outcome = crate::worker_stop::graceful_stop(&crate::worker_stop::StopRequest {
                backend: &guarded,
                socket: endpoint,
                pane,
                kind: stop_token.target.agent_kind,
                timeout: std::time::Duration::from_secs(10),
                exited: guarded.safe.as_ref(),
            });
            let attempted = guarded.attempted.load(Ordering::SeqCst);
            let (outcome, mut detail) = match outcome {
                crate::worker_stop::StopOutcome::Exited => {
                    (Outcome::Exited, "fenced process exited".into())
                }
                crate::worker_stop::StopOutcome::Refused(s) => (Outcome::Refused, s),
                crate::worker_stop::StopOutcome::LeftRunning(s) => (Outcome::LeftRunning, s),
                crate::worker_stop::StopOutcome::Uncertain(s) if !attempted => {
                    (Outcome::Refused, format!("guard refused before input: {s}"))
                }
                crate::worker_stop::StopOutcome::Uncertain(s) => (Outcome::Uncertain, s),
            };
            if outcome == Outcome::Refused
                && stop_token.process_fence.exited_status() == Some(false)
            {
                // Only restore while the original endpoint still owns this process.
                let fresh = backend.list_panes();
                if collaboration::unique_pane(pane, endpoint, &fresh)
                    .and_then(|p| {
                        collaboration::ProcessFence::observe(p, stop_token.target.agent_kind)
                    })
                    .as_ref()
                    == Some(&stop_token.process_fence)
                {
                    if let Some(value) = original {
                        if !backend.restore_remain_on_exit(endpoint, pane, value.as_deref()) {
                            detail.push_str("; restoring remain-on-exit failed");
                        }
                    }
                }
            }
            (outcome, detail)
        })
        .await
        .unwrap_or((
            Outcome::Uncertain,
            "stop task panicked; no retry permitted".into(),
        ));
        let result = collaboration
            .worker_drain_complete_stop(&token, result.0, &result.1)
            .await
            .map_err(|e| e.to_string())?;
        if result.outcome == Outcome::Exited {
            settle_row(&store, &backends, &token).await;
        }
        Ok(result)
    })
    .await
    .map_err(|e| format!("stop task lost: {e}"))?
}

pub(super) async fn settle_row(
    store: &SharedStore,
    backends: &[SharedBackend],
    token: &WorkerDrainToken,
) {
    if !token.process_fence.definitely_exited() {
        return;
    }
    let rows = store.snapshot().await;
    let Some(row) = rows.iter().find(|a| {
        a.session_id == token.target.session_id
            && a.kind == token.target.agent_kind
            && a.pane.as_deref() == Some(&token.target.pane)
            && a.tmux_socket == token.target.socket
    }) else {
        return;
    };
    let Ok(backend) = resolve_backend(backends, &token.target.pane) else {
        return;
    };
    let listed = backend.clone();
    let target = token.target.clone();
    let empty = tokio::task::spawn_blocking(move || {
        let panes = listed.list_panes();
        collaboration::unique_pane(&target.pane, target.socket.as_deref(), &panes)
            .is_some_and(|p| crate::account_swap::pane_has_agent_process(p.pane_pid) == Some(false))
    })
    .await
    .unwrap_or(false);
    if empty {
        store.mark_drained_if_unchanged(row).await;
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::collaboration::{
        CollaborationOptions, LaunchPurpose, WorkerDrainAcquire, WorkerDrainPhase,
        WorkerDrainPolicy,
    };
    use crate::ipc::{
        Client, CollaborationAuditLog, CollaborationAuditOperation, CollaborationOrigin, Duration,
        Server, PROTOCOL_VERSION,
    };
    use crate::state::Store;
    use crate::{AgentKind, AgentState};
    use std::sync::Mutex;
    use time::OffsetDateTime;
    use tokio::sync::broadcast;

    struct FakePane {
        child: Mutex<std::process::Child>,
        screen: Mutex<String>,
        typed: Mutex<Vec<String>>,
        local: Mutex<Option<String>>,
        on_capture: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }
    impl Drop for FakePane {
        fn drop(&mut self) {
            let child = self.child.get_mut().unwrap();
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    impl PaneBackend for FakePane {
        fn kind(&self) -> HostKind {
            HostKind::Tmux
        }
        fn list_panes(&self) -> Vec<PaneInfo> {
            vec![serde_json::from_value(serde_json::json!({"pane_id":"%2","session":"main","window_id":"@1","window_index":"0","pane_index":"1","tty":"","current_command":"codex","title":"","pane_pid":self.child.lock().unwrap().id(),"socket":"default","agent_program":"codex"})).unwrap()]
        }
        fn resolve_pane(&self, _: &str) -> Option<PaneInfo> {
            self.list_panes().pop()
        }
        fn capture_pane(&self, _: &str) -> Option<String> {
            panic!("capture must use the exact socket")
        }
        fn capture_pane_on(&self, s: Option<&str>, _: &str) -> Option<String> {
            assert_eq!(s, Some("default"));
            if let Some(f) = self.on_capture.lock().unwrap().take() {
                f();
            }
            Some(self.screen.lock().unwrap().clone())
        }
        fn pane_pid_map(&self) -> std::collections::HashMap<u32, String> {
            std::collections::HashMap::default()
        }
        fn current_pane(&self) -> Option<String> {
            None
        }
        fn focus_pane(&self, _: &str) -> bool {
            false
        }
        fn remain_on_exit_override(
            &self,
            _: Option<&str>,
            _: &str,
        ) -> Result<Option<String>, String> {
            Ok(self.local.lock().unwrap().clone())
        }
        fn restore_remain_on_exit(&self, _: Option<&str>, _: &str, v: Option<&str>) -> bool {
            *self.local.lock().unwrap() = v.map(str::to_string);
            true
        }
        fn set_remain_on_exit_on(&self, _: Option<&str>, _: &str, on: bool) -> bool {
            *self.local.lock().unwrap() = on.then(|| "on".into());
            true
        }
        fn send_prompt_text_on(&self, s: Option<&str>, _: &str, t: &str) -> bool {
            assert_eq!(s, Some("default"));
            self.typed.lock().unwrap().push(t.into());
            true
        }
        fn send_text_on(&self, s: Option<&str>, _: &str, t: &str) -> bool {
            assert_eq!(s, Some("default"));
            self.typed.lock().unwrap().push(t.into());
            let mut child = self.child.lock().unwrap();
            child.kill().unwrap();
            child.wait().unwrap();
            true
        }
    }
    async fn setup(
        screen: &str,
    ) -> (
        SharedStore,
        Arc<CollaborationStore>,
        Arc<FakePane>,
        WorkerDrainToken,
    ) {
        let child = std::process::Command::new("bash")
            .args(["-c", "exec -a 'codex --no-daemon' sleep 60"])
            .spawn()
            .unwrap();
        let backend = Arc::new(FakePane {
            child: Mutex::new(child),
            screen: Mutex::new(screen.into()),
            typed: Mutex::new(Vec::new()),
            local: Mutex::new(Some("off".into())),
            on_capture: Mutex::new(None),
        });
        let pane = backend.list_panes().pop().unwrap();
        for _ in 0..300 {
            if collaboration::ProcessFence::observe(&pane, AgentKind::Codex).is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(collaboration::ProcessFence::observe(&pane, AgentKind::Codex).is_some());
        let store = Store::shared();
        let at = OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        for event in [
            serde_json::json!({"type":"started","id":{"kind":"codex","session_id":"worker","pane":"%2","tmux_socket":"default"},"at":at}),
            serde_json::json!({"type":"turn_stopped","id":{"kind":"codex","session_id":"worker","pane":"%2","tmux_socket":"default"},"response":"ready","at":at}),
        ] {
            store.apply(&serde_json::from_value(event).unwrap()).await;
        }
        let collaboration = CollaborationStore::in_memory(CollaborationOptions::default());
        let backends: Vec<SharedBackend> = vec![backend.clone()];
        let topology =
            collaboration_participants(&store, &backends, &collaboration, None, None).await;
        let p = topology
            .participants
            .iter()
            .find(|p| p.agent_session_id == "worker")
            .unwrap();
        collaboration
            .set_purpose(p, LaunchPurpose::Worker)
            .await
            .unwrap();
        let topology =
            collaboration_participants(&store, &backends, &collaboration, None, None).await;
        let p = topology
            .participants
            .iter()
            .find(|p| p.agent_session_id == "worker")
            .unwrap();
        let guard = collaboration.worker_action_guard("%2").await;
        let WorkerDrainAcquire::Acquired { token } = collaboration
            .worker_drain_acquire(
                &collaboration::WorkerDrainTarget::from(p),
                WorkerDrainPolicy {
                    min_observed_idle_secs: 0,
                    allow_error: false,
                },
                &topology.participants,
                &topology.agents,
                &topology.panes,
            )
            .await
            .unwrap()
        else {
            panic!("fixture was not acquired")
        };
        drop(guard);
        (store, collaboration, backend, token)
    }

    #[tokio::test]
    async fn worker_drain_stop_is_single_use_and_marks_only_the_exited_row() {
        let (store, c, b, t) = setup("› ").await;
        let op = uuid::Uuid::new_v4().to_string();
        let (first, replay) = tokio::join!(
            stop(
                store.clone(),
                c.clone(),
                vec![b.clone()],
                t.clone(),
                op.clone()
            ),
            stop(
                store.clone(),
                c.clone(),
                vec![b.clone()],
                t.clone(),
                op.clone()
            )
        );
        assert_eq!(first.unwrap().outcome, Outcome::Exited);
        assert!(matches!(
            replay.unwrap().outcome,
            Outcome::Exited | Outcome::Uncertain
        ));
        assert_eq!(*b.typed.lock().unwrap(), ["/quit", "\r"]);
        assert_eq!(*b.local.lock().unwrap(), Some("on".into()));
        assert_eq!(store.snapshot().await[0].state, AgentState::Stopped);
        assert_eq!(
            stop(store, c, vec![b.clone()], t, op)
                .await
                .unwrap()
                .outcome,
            Outcome::Exited
        );
        assert_eq!(b.typed.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn worker_drain_draft_refusal_preserves_the_pane_and_restores_exact_override() {
        for original in [
            None,
            Some("off".into()),
            Some("failed".into()),
            Some("on".into()),
        ] {
            let (store, c, b, t) = setup("› human draft").await;
            *b.local.lock().unwrap() = original.clone();
            let result = stop(
                store,
                c,
                vec![b.clone()],
                t,
                uuid::Uuid::new_v4().to_string(),
            )
            .await
            .unwrap();
            assert_eq!(result.outcome, Outcome::Refused);
            assert_eq!(result.drain.phase, WorkerDrainPhase::Aborted);
            assert!(b.typed.lock().unwrap().is_empty());
            assert_eq!(*b.local.lock().unwrap(), original);
            assert!(b.child.lock().unwrap().try_wait().unwrap().is_none());
        }
    }

    #[tokio::test]
    async fn worker_drain_new_turn_after_capture_prevents_all_terminal_input() {
        let (store, c, b, t) = setup("› ").await;
        let changed = store.clone();
        let handle = tokio::runtime::Handle::current();
        *b.on_capture.lock().unwrap() = Some(Box::new(move || {
            handle.block_on(async move {
            let event=serde_json::from_value(serde_json::json!({"type":"prompt_submitted","id":{"kind":"codex","session_id":"worker","pane":"%2","tmux_socket":"default"},"prompt":"new human turn","at":OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap()})).unwrap();changed.apply(&event).await;
        });
        }));
        let result = stop(
            store,
            c,
            vec![b.clone()],
            t,
            uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap();
        assert_eq!(result.outcome, Outcome::Refused);
        assert!(b.typed.lock().unwrap().is_empty());
        assert!(b.child.lock().unwrap().try_wait().unwrap().is_none());
    }

    #[tokio::test]
    async fn worker_drain_disconnected_waiter_does_not_release_the_physical_action_gate() {
        let (store, c, b, t) = setup("› ").await;
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *b.on_capture.lock().unwrap() = Some(Box::new(move || {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }));
        let task = tokio::spawn(stop(
            store,
            c.clone(),
            vec![b.clone()],
            t,
            uuid::Uuid::new_v4().to_string(),
        ));
        entered_rx.await.unwrap();
        task.abort();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), c.worker_action_guard("%2"))
                .await
                .is_err()
        );
        release_tx.send(()).unwrap();
        for _ in 0..300 {
            if c.worker_drains().await[0].phase == WorkerDrainPhase::Exited {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(c.worker_drains().await[0].phase, WorkerDrainPhase::Exited);
        assert_eq!(*b.typed.lock().unwrap(), ["/quit", "\r"]);
    }
    #[tokio::test]
    async fn worker_drain_ipc_capability_console_audit_and_direct_prompt_gates() {
        let (store, c, b, t) = setup("› human draft").await;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("drain.sock");
        let audit = CollaborationAuditLog::in_memory();
        let server = Server::new(socket.clone(), store)
            .with_backends(vec![b.clone()])
            .with_collaboration(c.clone())
            .with_collaboration_audit(audit.clone());
        let (shutdown, rx) = broadcast::channel(1);
        let serving = tokio::spawn(server.run(rx));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let client = Client::new(socket);
        let console = CollaborationOrigin {
            pane: "%2".into(),
            socket: Some("default".into()),
            console: true,
            agent_session_id: None,
        };
        let policy = WorkerDrainPolicy {
            min_observed_idle_secs: 0,
            allow_error: false,
        };
        let hello = client
            .call(&serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"hello"}))
            .await
            .unwrap();
        assert!(hello["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == collaboration::WORKER_DRAIN_CAPABILITY));
        let mut agent = console.clone();
        agent.console = false;
        agent.agent_session_id = Some("worker".into());
        assert!(client.worker_drain_plan(&agent, &policy).await.is_err());
        assert!(client.send_prompt("%2", "do work", true).await.is_err());
        let plan = client.worker_drain_plan(&console, &policy).await.unwrap();
        assert!(plan
            .candidates
            .iter()
            .any(|c| c.reason == "already_draining"));
        assert!(matches!(
            client
                .worker_drain_acquire(&console, &t.target, &policy)
                .await
                .unwrap(),
            WorkerDrainAcquire::Skipped { .. }
        ));
        let token = client.worker_drain_renew(&console, &t).await.unwrap();
        assert!(token.expires_at >= t.expires_at);
        assert_eq!(client.worker_drains(&console).await.unwrap().len(), 1);
        assert_eq!(
            client
                .worker_drain_stop(&console, &token, &uuid::Uuid::new_v4().to_string())
                .await
                .unwrap()
                .outcome,
            Outcome::Refused
        );
        client
            .worker_drain_finish(
                &console,
                &token,
                collaboration::WorkerDrainFinish::Aborted,
                "aborted",
            )
            .await
            .unwrap();
        let entries = audit.entries().await;
        for operation in [
            CollaborationAuditOperation::WorkerDrainPlan,
            CollaborationAuditOperation::WorkerDrainAcquire,
            CollaborationAuditOperation::WorkerDrainRenew,
            CollaborationAuditOperation::WorkerDrainStop,
            CollaborationAuditOperation::WorkerDrainFinish,
            CollaborationAuditOperation::WorkerDrains,
        ] {
            assert!(
                entries.iter().any(|e| e.operation == operation && e.ok),
                "missing audit {operation:?}"
            );
        }
        assert!(entries
            .iter()
            .any(|e| e.operation == CollaborationAuditOperation::WorkerDrainPlan && !e.ok));
        assert!(b.typed.lock().unwrap().is_empty());
        shutdown.send(()).unwrap();
        serving.await.unwrap().unwrap();
    }
    #[tokio::test]
    async fn worker_drain_primary_promotion_and_replaced_process_veto_a_stale_stop() {
        let (store, c, b, t) = setup("› ").await;
        let topology =
            collaboration_participants(&store, &[b.clone() as SharedBackend], &c, None, None).await;
        let p = topology
            .participants
            .iter()
            .find(|p| p.agent_session_id == "worker")
            .unwrap();
        c.set_purpose(p, LaunchPurpose::Primary).await.unwrap();
        let result = stop(
            store,
            c,
            vec![b.clone()],
            t,
            uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap();
        assert_eq!(result.outcome, Outcome::Skipped);
        assert!(b.typed.lock().unwrap().is_empty());
        let (store, c, b, t) = setup("› ").await;
        {
            let mut child = b.child.lock().unwrap();
            child.kill().unwrap();
            child.wait().unwrap();
            *child = std::process::Command::new("bash")
                .args(["-c", "exec -a 'codex --no-daemon' sleep 60"])
                .spawn()
                .unwrap();
        }
        let result = stop(
            store.clone(),
            c,
            vec![b.clone()],
            t,
            uuid::Uuid::new_v4().to_string(),
        )
        .await
        .unwrap();
        assert_eq!(result.outcome, Outcome::Skipped);
        assert!(b.typed.lock().unwrap().is_empty());
        assert_eq!(store.snapshot().await[0].state, AgentState::Idle);
    }

    #[tokio::test]
    async fn worker_drain_pending_automation_and_new_requests_exclude_acquisition() {
        let (store, c, b, t) = setup("› ").await;
        let reservation = c
            .reserve_worker_automation("worker", "%2", Some("default"))
            .await
            .unwrap();
        assert_eq!(c.worker_drains().await[0].phase, WorkerDrainPhase::Aborted);
        let topology =
            collaboration_participants(&store, &[b.clone() as SharedBackend], &c, None, None).await;
        let policy = WorkerDrainPolicy {
            min_observed_idle_secs: 0,
            allow_error: false,
        };
        assert!(
            matches!(c.worker_drain_acquire(&t.target,policy.clone(),&topology.participants,&topology.agents,&topology.panes).await.unwrap(),WorkerDrainAcquire::Skipped{reason} if reason=="active_automation")
        );
        drop(reservation);
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        let mut sender = topology.participants[0].clone();
        sender.agent_session_id = "sender".into();
        sender.pane = "%1".into();
        c.create(
            sender,
            topology.participants[0].clone(),
            collaboration::NewRequest {
                body: "new work".into(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(
            matches!(c.worker_drain_acquire(&t.target,policy,&topology.participants,&topology.agents,&topology.panes).await.unwrap(),WorkerDrainAcquire::Skipped{reason} if reason=="open_requests_or_reply_wakes")
        );
    }
    #[tokio::test]
    async fn worker_drain_changed_composer_during_final_probe_refuses_quit() {
        let (_store, _c, b, _t) = setup("› ").await;
        let changed = b.clone();
        let guarded = GuardedBackend {
            inner: b.clone(),
            socket: Some("default".into()),
            safe: Box::new(move || {
                *changed.screen.lock().unwrap() = "› human draft".into();
                Some(false)
            }),
            attempted: AtomicBool::new(false),
            captured: Mutex::new(None),
        };
        assert_eq!(guarded.capture_pane("%2"), Some("› ".into()));
        assert!(!guarded.send_prompt_text_on(Some("default"), "%2", "/quit"));
        assert!(!guarded.attempted.load(Ordering::SeqCst));
        assert!(b.typed.lock().unwrap().is_empty());
    }
    #[tokio::test]
    async fn worker_drain_workers_own_pending_launches_veto_retirement() {
        let (store, c, b, t) = setup("› ").await;
        let topology =
            collaboration_participants(&store, &[b.clone() as SharedBackend], &c, None, None).await;
        let worker = topology
            .participants
            .iter()
            .find(|p| p.agent_session_id == "worker")
            .unwrap();
        let origin = CollaborationOrigin {
            pane: worker.pane.clone(),
            socket: worker.socket.clone(),
            console: false,
            agent_session_id: Some(worker.agent_session_id.clone()),
        };
        let launch = c
            .acquire_worker_spawn(
                worker,
                origin,
                collaboration::WorkerSpawnSpec {
                    agent_kind: AgentKind::ClaudeCode,
                    role: None,
                    launch_role: "reviewer".into(),
                },
                &topology.participants,
                &topology.panes,
            )
            .await
            .unwrap();
        assert!(matches!(
            launch,
            collaboration::WorkerSpawnAcquisition::LaunchToken { .. }
        ));
        assert_eq!(c.worker_drains().await[0].phase, WorkerDrainPhase::Aborted);
        let topology =
            collaboration_participants(&store, &[b.clone() as SharedBackend], &c, None, None).await;
        let policy = WorkerDrainPolicy {
            min_observed_idle_secs: 0,
            allow_error: false,
        };
        assert!(
            matches!(c.worker_drain_acquire(&t.target,policy,&topology.participants,&topology.agents,&topology.panes).await.unwrap(),WorkerDrainAcquire::Skipped{reason} if reason=="unfinished_launch")
        );
    }
    #[tokio::test]
    async fn worker_drain_plan_explains_policy_exclusions_without_acquiring() {
        let (store, c, b, t) = setup("› ").await;
        c.worker_drain_finish(&t, collaboration::WorkerDrainFinish::Aborted, "inspect")
            .await
            .unwrap();
        let topology =
            collaboration_participants(&store, &[b.clone() as SharedBackend], &c, None, None).await;
        let worker = topology
            .participants
            .iter()
            .find(|p| p.agent_session_id == "worker")
            .unwrap();
        let policy = WorkerDrainPolicy {
            min_observed_idle_secs: 0,
            allow_error: false,
        };
        for (change, reason) in [
            (0, "primary"),
            (1, "unsupported_provider"),
            (2, "pending"),
            (3, "not_idle"),
        ] {
            let mut p = worker.clone();
            match change {
                0 => p.launch_purpose = LaunchPurpose::Primary,
                1 => p.agent_kind = AgentKind::GeminiCli,
                2 => p.agent_session_id = "pending-pane:default:%2".into(),
                _ => p.state = AgentState::Working,
            }
            let plan = c.worker_drain_plan(&[p], &topology.agents, &policy).await;
            assert!(!plan.candidates[0].eligible);
            assert_eq!(plan.candidates[0].reason, reason);
        }
        let plan = c
            .worker_drain_plan(
                std::slice::from_ref(worker),
                &topology.agents,
                &WorkerDrainPolicy {
                    min_observed_idle_secs: 86400,
                    allow_error: false,
                },
            )
            .await;
        assert_eq!(plan.candidates[0].reason, "too_fresh");
        assert_eq!(c.worker_drains().await.len(), 1);
    }
}
