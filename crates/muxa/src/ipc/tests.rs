use super::*;
use crate::ipc::framing::downgrade_wire;
use crate::ipc::framing::lagged_marker_bytes;
use crate::ipc::identity::observed_pane_in_scan;
use crate::ipc::identity::RequestPaneScan;

fn promote_topology(panes: &[(&str, &str)]) -> CollaborationTopology {
    CollaborationTopology {
        participants: panes
            .iter()
            .map(|(pane, session)| collaboration::Participant {
                agent_kind: crate::event::AgentKind::Codex,
                agent_session_id: (*session).into(),
                pane: (*pane).into(),
                socket: Some("default".into()),
                room: collaboration::RoomId {
                    host: "rmux".into(),
                    socket: Some("default".into()),
                    window_id: "@1".into(),
                },
                tmux_session_id: None,
                tmux_session_name: None,
                window_name: None,
                state: crate::AgentState::Idle,
                cwd: None,
                alias: None,
                roles: Vec::new(),
                console: false,
                launch_purpose: collaboration::LaunchPurpose::Primary,
                draining: false,
            })
            .collect(),
        panes: Vec::new(),
        agents: Vec::new(),
    }
}
use crate::event::{AgentEvent, AgentId, AgentKind, AgentState};
use crate::state::Store;
use std::collections::HashMap;
use tempfile::tempdir;
use time::OffsetDateTime;

struct CollaborationTestBackend {
    panes: Vec<PaneInfo>,
}

impl PaneBackend for CollaborationTestBackend {
    fn kind(&self) -> HostKind {
        HostKind::Tmux
    }

    fn list_panes(&self) -> Vec<PaneInfo> {
        self.panes.clone()
    }

    fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo> {
        self.panes
            .iter()
            .find(|pane| pane.pane_id == pane_id)
            .cloned()
    }

    fn capture_pane(&self, _pane_id: &str) -> Option<String> {
        None
    }

    fn pane_pid_map(&self) -> HashMap<u32, String> {
        HashMap::new()
    }

    fn current_pane(&self) -> Option<String> {
        self.panes.first().map(|pane| pane.pane_id.clone())
    }

    fn focus_pane(&self, _pane_id: &str) -> bool {
        true
    }

    fn caps(&self) -> BackendCaps {
        BackendCaps::default()
    }
}

/// Counts every inventory/resolve/pid-map call so tests can prove how
/// many scans one collaboration request performs.
struct CountingPaneBackend {
    kind: HostKind,
    panes: Vec<PaneInfo>,
    native_pid_map: HashMap<u32, String>,
    lists: std::sync::atomic::AtomicUsize,
    resolves: std::sync::atomic::AtomicUsize,
    pid_maps: std::sync::atomic::AtomicUsize,
}

impl CountingPaneBackend {
    fn new(kind: HostKind, panes: Vec<PaneInfo>) -> Arc<Self> {
        Arc::new(Self {
            kind,
            panes,
            native_pid_map: HashMap::new(),
            lists: 0.into(),
            resolves: 0.into(),
            pid_maps: 0.into(),
        })
    }

    fn counts(&self) -> (usize, usize, usize) {
        use std::sync::atomic::Ordering::SeqCst;
        (
            self.lists.load(SeqCst),
            self.resolves.load(SeqCst),
            self.pid_maps.load(SeqCst),
        )
    }
}

impl PaneBackend for CountingPaneBackend {
    fn kind(&self) -> HostKind {
        self.kind
    }

    fn list_panes(&self) -> Vec<PaneInfo> {
        self.lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.panes.clone()
    }

    fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo> {
        self.resolves
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.panes
            .iter()
            .find(|pane| pane.pane_id == pane_id)
            .cloned()
    }

    fn capture_pane(&self, _pane_id: &str) -> Option<String> {
        None
    }

    fn pane_pid_map(&self) -> HashMap<u32, String> {
        self.pid_maps
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.native_pid_map.clone()
    }

    fn current_pane(&self) -> Option<String> {
        None
    }

    fn focus_pane(&self, _pane_id: &str) -> bool {
        false
    }

    fn caps(&self) -> BackendCaps {
        BackendCaps::default()
    }
}

fn pid_pane(pane_id: &str, shell_pid: u32) -> PaneInfo {
    PaneInfo {
        pane_pid: shell_pid,
        ..collaboration_test_pane(pane_id, "0")
    }
}

fn scan_of(backends: &[SharedBackend]) -> RequestPaneScan {
    RequestPaneScan::take(backends)
}

async fn assert_read_views(
    client: &Client,
    origin: &CollaborationOrigin,
    mailbox: &CollaborationStore,
    sender: &Participant,
    ids: &[String],
) {
    use collaboration::CollaborationReadView as View;
    for view in [
        View::Metadata,
        View::Preview,
        View::Outcome,
        View::Progress { after_update: 1 },
    ] {
        let request = client
            .collaboration_peek_view(origin, &ids[0], view)
            .await
            .unwrap();
        assert_eq!(request.status, RequestStatus::Completed);
        assert_eq!(request.updates.last().unwrap().sequence, 1);
        if view == View::Preview {
            assert_eq!(request.body.chars().count(), 256);
        } else {
            assert!(request.body.is_empty());
        }
        if view == View::Outcome {
            assert!(request
                .reply
                .as_ref()
                .unwrap()
                .body
                .contains("PRIVATE REPLY"));
        }
        if matches!(view, View::Progress { .. }) {
            assert!(request.updates[0].body.is_empty());
        }
    }
    assert_eq!(mailbox.unread_reply_count(sender).await, 3);
    let progress = client
        .collaboration_peek_view(origin, &ids[0], View::Progress { after_update: 0 })
        .await
        .unwrap();
    assert_eq!(progress.updates[0].body, "PRIVATE UPDATE");
    let full = client.collaboration_peek(origin, &ids[0]).await.unwrap();
    assert!(full.body.contains("PRIVATE TASK"));
    assert_eq!(full.updates[0].body, "PRIVATE UPDATE");
    client
        .collaboration_get_view(origin, &ids[0], View::Outcome)
        .await
        .unwrap();
    assert_eq!(mailbox.unread_reply_count(sender).await, 2);
    let reply = client
        .collaboration_wait_view(origin, &ids[1], 1, View::Outcome)
        .await
        .unwrap();
    assert!(reply.body.is_empty());
    assert!(reply.reply.as_ref().unwrap().body.contains("PRIVATE REPLY"));
    assert_eq!(mailbox.unread_reply_count(sender).await, 1);
}

async fn create_private_list_history(
    mailbox: &CollaborationStore,
    sender: &Participant,
    recipient: &Participant,
) -> Vec<String> {
    let mut ids = Vec::new();
    for _ in 0..3 {
        let request = mailbox
            .create(
                sender.clone(),
                recipient.clone(),
                NewRequest {
                    task_contract: None,
                    supersedes_request_id: None,
                    body: "PRIVATE TASK ".repeat(900),
                    expects_reply: true,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        mailbox
            .update_request(sender, &request.id, "PRIVATE UPDATE".into())
            .await
            .unwrap();
        mailbox
            .reply(
                recipient,
                &request.id,
                RequestStatus::Completed,
                "PRIVATE REPLY".repeat(300),
                vec![],
                vec![],
            )
            .await
            .unwrap();
        ids.push(request.id);
    }
    ids
}

async fn assert_legacy_list_query_fallback(
    requests: Vec<CollaborationRequest>,
    origin: &CollaborationOrigin,
    query: &collaboration::CollaborationQuery,
) {
    let dir = tempdir().unwrap();
    let socket = dir.path().join("legacy-list.sock");
    let listener = Listener::bind(&socket).unwrap();
    let bytes = encode_line(
        &serde_json::json!({"ok":true,"protocol":PROTOCOL_VERSION,
            "collaboration_requests":requests}),
        PROTOCOL_VERSION,
    )
    .unwrap();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let stream = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut line = String::new();
            let mut reader = BufReader::new(read);
            reader.read_line(&mut line).await.unwrap();
            write.write_all(b"{\"ok\":true}\n").await.unwrap();
            line.clear();
            reader.read_line(&mut line).await.unwrap();
            assert!(line.contains("collaboration_list"));
            write.write_all(&bytes).await.unwrap();
        }
    });
    let client = Client::new(socket);
    let page = client
        .collaboration_query(
            origin,
            RequestMailbox::All,
            MailboxScope::Caller,
            query,
            1,
            true,
        )
        .await
        .unwrap();
    assert_eq!(page.total, 3);
    assert_eq!(page.requests.len(), 1);
    assert!(!serde_json::to_string(&page).unwrap().contains("PRIVATE"));
    let error = client
        .collaboration_query(
            origin,
            RequestMailbox::All,
            MailboxScope::Room,
            query,
            0,
            true,
        )
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("predates scoped"),
        "old daemon must not silently weaken the requested scope"
    );
    server.await.unwrap();
}

fn collaboration_test_pane(pane_id: &str, pane_index: &str) -> PaneInfo {
    PaneInfo {
        session_group: None,
        agent_program: None,
        worker_launch_id: None,
        agent_role: None,
        agent_alias: None,
        workspace_id: Some("callabo".into()),
        work_id: Some("CAL-7345".into()),
        pane_id: pane_id.into(),
        session_id: "$1".into(),
        session: "collaboration".into(),
        window_id: "@1".into(),
        window_name: "agents".into(),
        window_index: "0".into(),
        pane_index: pane_index.into(),
        tty: String::new(),
        current_command: "agent".into(),
        title: String::new(),
        current_path: "/repo".into(),
        pane_pid: 0,
        socket: Some("default".into()),
    }
}

async fn add_collaboration_agent(
    store: &SharedStore,
    pane: &str,
    session_id: &str,
    kind: AgentKind,
) {
    store
        .apply(&AgentEvent::Started {
            id: AgentId {
                kind,
                session_id: session_id.into(),
                surface: None,
                pane: Some(pane.into()),
                tmux_socket: Some("default".into()),
                cwd: Some("/repo".into()),
            },
            at: OffsetDateTime::now_utc(),
        })
        .await;
}

/// Server must wait for the socket to appear before tests dial in.
/// A server over `mailbox` with a sender (%1) and recipient (%2).
async fn wait_test_server(
    socket: &Path,
    store: &SharedStore,
    mailbox: &Arc<collaboration::CollaborationStore>,
) -> (broadcast::Sender<()>, tokio::task::JoinHandle<()>) {
    let backend: SharedBackend = Arc::new(CollaborationTestBackend {
        panes: vec![
            collaboration_test_pane("%1", "0"),
            collaboration_test_pane("%2", "1"),
        ],
    });
    let server = Server::new(socket.to_path_buf(), store.clone())
        .with_backends(vec![backend])
        .with_collaboration(mailbox.clone());
    let (stop, rx) = broadcast::channel(1);
    let serving = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(socket).await;
    (stop, serving)
}

fn pane_origin(pane: &str) -> CollaborationOrigin {
    CollaborationOrigin {
        exact_session: false,
        pane: pane.into(),
        socket: Some("default".into()),
        console: false,
        agent_session_id: None,
    }
}

async fn wait_for_socket(sock: &Path) {
    for _ in 0..50 {
        if sock.exists() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Raw single-line request/response on a fresh connection. Bypasses
/// `Client::call`'s built-in `hello` handshake so tests can exercise
/// the legacy strict-match path and the negotiated downgrade path
/// in isolation.
async fn raw_call(sock: &Path, req: &serde_json::Value) -> serde_json::Value {
    let mut stream = Stream::connect(sock).await.unwrap();
    let mut bytes = serde_json::to_vec(req).unwrap();
    bytes.push(b'\n');
    stream.write_all(&bytes).await.unwrap();
    stream.flush().await.unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    serde_json::from_str(line.trim()).unwrap()
}

/// A stand-in swap service: answers from memory, and records which
/// thread each call ran on.
struct FakeSwap {
    healthy: std::sync::atomic::AtomicBool,
    slow_health: bool,
    jobs: std::sync::Mutex<Vec<(String, String)>>,
}

impl FakeSwap {
    fn new(healthy: bool) -> Arc<Self> {
        Arc::new(Self {
            healthy: std::sync::atomic::AtomicBool::new(healthy),
            slow_health: false,
            jobs: std::sync::Mutex::new(Vec::new()),
        })
    }

    fn view(job: &str) -> crate::swap_service::JobView {
        crate::swap_service::JobView {
            job_id: job.to_owned(),
            node_id: "node".into(),
            state: "running".into(),
            revision: 1,
            reason: None,
            entries: Vec::new(),
            fault: None,
            service_fault: None,
            cancel_requested: false,
            evidence_pruned_at: None,
        }
    }

    /// Fails if called on a runtime worker: blocking here would.
    fn on_blocking_thread() {
        tokio::runtime::Handle::current().block_on(async {});
    }
}

impl crate::swap_service::SwapJobs for FakeSwap {
    fn health(&self) -> crate::swap_service::SwapHealth {
        if self.slow_health {
            std::thread::sleep(Duration::from_secs(3));
        }
        let ready = self.healthy.load(std::sync::atomic::Ordering::SeqCst);
        crate::swap_service::SwapHealth {
            readiness: if ready {
                crate::swap_service::SwapReadiness::Ready
            } else {
                crate::swap_service::SwapReadiness::Halted
            },
            reason: (!ready).then(|| "halted".to_owned()),
            schema: Some(2),
            expected_schema: 2,
            fault: (!ready).then(|| "halted".to_owned()),
            jobs_running: 0,
            maintenance: None,
        }
    }

    fn cancel(
        &self,
        job: &crate::swap_service::JobRef,
    ) -> Result<crate::swap_service::JobView, String> {
        self.status(job)
    }

    fn retry(
        &self,
        key: &str,
        _: &crate::swap_service::JobRef,
        overrides: &crate::swap_service::RetryOverrides,
    ) -> Result<crate::swap_service::Accepted, String> {
        self.submit(
            key,
            &crate::swap_service::SwapOverrides {
                force: overrides.force,
                ..crate::swap_service::SwapOverrides::default()
            },
        )
    }

    fn active(&self) -> Result<(), String> {
        if self.slow_health {
            std::thread::sleep(Duration::from_secs(2));
        }
        if self.healthy.load(std::sync::atomic::Ordering::SeqCst) {
            Ok(())
        } else {
            Err("swap jobs are halted".into())
        }
    }

    fn plan(
        &self,
        _: &crate::swap_service::SwapOverrides,
    ) -> Result<crate::swap_service::PlanView, String> {
        Self::on_blocking_thread();
        Ok(crate::swap_service::PlanView {
            outcomes: Vec::new(),
            direct_targets: Vec::new(),
            executable: self.active().is_ok(),
            unavailable: self.active().err(),
        })
    }

    fn submit(
        &self,
        key: &str,
        _: &crate::swap_service::SwapOverrides,
    ) -> Result<crate::swap_service::Accepted, String> {
        Self::on_blocking_thread();
        self.active()?;
        let mut jobs = self.jobs.lock().unwrap();
        if let Some((_, job)) = jobs.iter().find(|(k, _)| k == key) {
            return Ok(crate::swap_service::Accepted {
                job_id: job.clone(),
                existing: true,
                view: Self::view(job),
            });
        }
        let job = format!("swap-{}", jobs.len());
        jobs.push((key.to_owned(), job.clone()));
        Ok(crate::swap_service::Accepted {
            job_id: job.clone(),
            existing: false,
            view: Self::view(&job),
        })
    }

    fn status(
        &self,
        job: &crate::swap_service::JobRef,
    ) -> Result<crate::swap_service::JobView, String> {
        Self::on_blocking_thread();
        let jobs = self.jobs.lock().unwrap();
        let found = jobs.iter().find(|(key, id)| match job {
            crate::swap_service::JobRef::Id(wanted) => id == wanted,
            crate::swap_service::JobRef::Key(wanted) => key == wanted,
        });
        found
            .map(|(_, id)| Self::view(id))
            .ok_or_else(|| "swap job not found".into())
    }
}

async fn swap_server(
    swap: Option<Arc<dyn crate::swap_service::SwapJobs>>,
) -> (
    tempfile::TempDir,
    Client,
    broadcast::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("muxa-swap.sock");
    let server = Server::new(sock.clone(), Store::shared()).with_swap(swap);
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(&sock).await;
    (dir, Client::new(sock), tx, handle)
}

/// Whether swap jobs are advertised. Every daemon of this build marks
/// that it runs swaps as jobs, healthy or not, so a client can tell an
/// unavailable service from a daemon that predates it.
fn advertises_swap(hello: &Hello) -> bool {
    assert!(
        hello
            .capabilities
            .iter()
            .any(|c| c == SWAP_SERVICE_CAPABILITY),
        "{:?}",
        hello.capabilities
    );
    hello.capabilities.iter().any(|c| c == SWAP_JOBS_CAPABILITY)
        && hello.capabilities.iter().any(|c| c == SWAP_PLAN_CAPABILITY)
}

// --- Control-plane routing tests (send_prompt / capture) -------------

// `HostKind` comes in via `use super::*` (it's imported at module top).
use crate::backend::{BackendCaps, PaneBackend};
use std::sync::Arc;
use std::sync::Mutex;

/// Recorded `(pane_id, text)` injections from a `RecordingBackend`.
type SendLog = Arc<Mutex<Vec<(String, String)>>>;
/// Recorded per-injection `socket` argument threaded into `send_text_on`
/// (the pane row's recorded server, or `None`).
type SocketLog = Arc<Mutex<Vec<Option<String>>>>;

/// A fake backend that records every `send_text_on` call — both the
/// `(pane_id, text)` and the pinned `socket` — and answers `capture_pane`
/// with a canned string. `has_cap` (advertised `caps().send_text`) and
/// `send_ok` (the runtime injection result) are decoupled so we can model
/// "backend can't inject" (refusal) separately from "backend accepted the
/// call but the pane is gone" (runtime failure). `fail_on_cr` makes just
/// the submit CR (`"\r"`) fail while the text send still succeeds, to
/// exercise the partial-failure signal.
struct RecordingBackend {
    kind: HostKind,
    has_cap: bool,
    send_ok: bool,
    fail_on_cr: bool,
    sends: SendLog,
    sockets: SocketLog,
}

impl RecordingBackend {
    /// The common case: a backend whose `send_text` capability and runtime
    /// result are the same bool (`true` = injects fine; `false` = no cap,
    /// so it's refused before any injection is attempted).
    fn new(kind: HostKind, can_send: bool) -> (Arc<Self>, SendLog) {
        let (backend, sends, _sockets) = Self::new_full(kind, can_send, can_send, false);
        (backend, sends)
    }

    /// Full constructor exposing the socket log and decoupled cap / runtime
    /// / CR-failure toggles.
    fn new_full(
        kind: HostKind,
        has_cap: bool,
        send_ok: bool,
        fail_on_cr: bool,
    ) -> (Arc<Self>, SendLog, SocketLog) {
        let sends = Arc::new(Mutex::new(Vec::new()));
        let sockets = Arc::new(Mutex::new(Vec::new()));
        let backend = Arc::new(Self {
            kind,
            has_cap,
            send_ok,
            fail_on_cr,
            sends: sends.clone(),
            sockets: sockets.clone(),
        });
        (backend, sends, sockets)
    }
}

impl PaneBackend for RecordingBackend {
    fn kind(&self) -> HostKind {
        self.kind
    }
    fn list_panes(&self) -> Vec<PaneInfo> {
        Vec::new()
    }
    fn resolve_pane(&self, _: &str) -> Option<PaneInfo> {
        None
    }
    fn capture_pane(&self, pane_id: &str) -> Option<String> {
        Some(format!("captured:{pane_id}"))
    }
    fn pane_pid_map(&self) -> std::collections::HashMap<u32, String> {
        std::collections::HashMap::new()
    }
    fn current_pane(&self) -> Option<String> {
        None
    }
    fn focus_pane(&self, _: &str) -> bool {
        false
    }
    fn send_text(&self, pane_id: &str, text: &str) -> bool {
        self.sends
            .lock()
            .unwrap()
            .push((pane_id.to_string(), text.to_string()));
        if self.fail_on_cr && text == "\r" {
            return false;
        }
        self.send_ok
    }
    fn send_text_on(&self, socket: Option<&str>, pane_id: &str, text: &str) -> bool {
        self.sockets.lock().unwrap().push(socket.map(str::to_owned));
        self.send_text(pane_id, text)
    }
    fn caps(&self) -> BackendCaps {
        BackendCaps {
            send_text: self.has_cap,
            ..BackendCaps::default()
        }
    }
}

async fn serve<B: PaneBackend>(
    sock: &Path,
    store: SharedStore,
    backends: Vec<Arc<B>>,
) -> (broadcast::Sender<()>, tokio::task::JoinHandle<()>) {
    let backends: Vec<SharedBackend> = backends.into_iter().map(|b| b as SharedBackend).collect();
    let server = Server::new(sock.to_path_buf(), store).with_backends(backends);
    let (tx, rx) = broadcast::channel(1);
    let handle = tokio::spawn(async move { server.run(rx).await.unwrap() });
    wait_for_socket(sock).await;
    (tx, handle)
}
mod agents_and_collaboration;
mod protocol_and_pane_control;
mod server_lifecycle_and_services;
