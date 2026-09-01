//! Persistent SSH connection manager for physical Muxa Fleet hosts.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use muxa::collaboration::{CollaborationOrigin, RequestMailbox};
use muxa::config::{FleetCapturePolicy, FleetConfig, FleetConnectPolicy, FleetHostConfig};
use muxa::fleet::{
    drain_bounded, load_or_create_node_id, read_bounded_line, sanitize_capture_text,
    sanitize_raw_capture_base64, sanitize_terminal_text, validate_label_value,
    FleetCapturedWindowPane, FleetCommandEnvelope, FleetCommandReceiver, FleetCommandResult,
    FleetHostSnapshot, FleetHostState, FleetOperation, FleetRuntime, FleetStore,
    FleetWindowCapture, HostAccessMode, NodeId, RelayFrame, RelayHello, RelayRequest,
    RemoteSnapshot, FLEET_CAPABILITIES, FLEET_MAX_DIAGNOSTIC_BYTES, FLEET_MAX_FRAME_BYTES,
    FLEET_PROTOCOL_VERSION, LOCAL_HOST_ALIAS,
};
use muxa::tmux::SessionInfo;
use muxa::{HostKind, PaneKey, SharedBackend, SharedStore, WindowKey};
use time::OffsetDateTime;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{broadcast, mpsc, oneshot, RwLock, Semaphore};
use uuid::Uuid;

const MAX_BACKOFF: Duration = Duration::from_secs(30);
const MAX_PENDING_REQUESTS: usize = 64;

pub(crate) async fn start(
    cfg: &FleetConfig,
    local_agents: SharedStore,
    backends: Vec<SharedBackend>,
    local_client: muxa::ipc::Client,
    daemon_generation: u64,
    shutdown: broadcast::Receiver<()>,
) -> (FleetRuntime, tokio::task::JoinHandle<()>) {
    let store = Arc::new(FleetStore::new());
    let (runtime, receiver) = FleetRuntime::new(Arc::clone(&store));
    let local_transitions = local_agents.subscribe();
    let local_revision = Arc::new(AtomicU64::new(0));
    let (node_id, identity_error) = local_node_id();
    let local_snapshot =
        collect_local_snapshot(&local_agents, &backends, Arc::clone(&local_revision)).await;
    store
        .upsert_host(local_host(
            cfg,
            node_id.clone(),
            daemon_generation,
            local_snapshot,
            identity_error.clone(),
        ))
        .await;
    for (alias, host) in &cfg.hosts {
        store.upsert_host(base_host(alias, host, cfg.enabled)).await;
    }
    let config = cfg.clone();
    let handle = tokio::spawn(async move {
        run_manager(ManagerInput {
            cfg: config,
            store,
            commands: receiver,
            local: LocalManagerInput {
                agents: local_agents,
                backends,
                client: local_client,
                transitions: local_transitions,
                revision: local_revision,
                identity_error,
                node_id,
            },
            shutdown,
        })
        .await;
    });
    (runtime, handle)
}

fn base_host(alias: &str, cfg: &FleetHostConfig, fleet_enabled: bool) -> FleetHostSnapshot {
    FleetHostSnapshot {
        alias: alias.into(),
        local: false,
        ssh_target: cfg.ssh.clone(),
        labels: cfg.labels.clone(),
        annotations: cfg.annotations.clone(),
        mode: cfg.mode,
        state: if fleet_enabled && cfg.enabled {
            FleetHostState::Connecting
        } else {
            FleetHostState::Disabled
        },
        node_id: None,
        hostname: None,
        os: None,
        arch: None,
        muxa_version: None,
        protocol: None,
        capabilities: Vec::new(),
        daemon_generation: None,
        boot_id: None,
        latency_ms: None,
        last_seen_at: None,
        received_at: None,
        error: None,
        remote: None,
    }
}

struct ManagerInput {
    cfg: FleetConfig,
    store: Arc<FleetStore>,
    commands: FleetCommandReceiver,
    local: LocalManagerInput,
    shutdown: broadcast::Receiver<()>,
}

struct LocalManagerInput {
    agents: SharedStore,
    backends: Vec<SharedBackend>,
    client: muxa::ipc::Client,
    transitions: broadcast::Receiver<muxa::Transition>,
    revision: Arc<AtomicU64>,
    identity_error: Option<String>,
    node_id: NodeId,
}

async fn run_manager(input: ManagerInput) {
    let ManagerInput {
        cfg,
        store,
        mut commands,
        local,
        mut shutdown,
    } = input;
    let permits = Arc::new(Semaphore::new(cfg.max_parallel_connects));
    let node_registry = Arc::new(RwLock::new(BTreeMap::from([(
        local.node_id,
        LOCAL_HOST_ALIAS.to_string(),
    )])));
    let mut routes = HashMap::new();
    let mut handles = Vec::new();

    let (local_tx, local_rx) = mpsc::channel(64);
    routes.insert(LOCAL_HOST_ALIAS.to_string(), local_tx);
    handles.push(tokio::spawn(
        LocalTask {
            fleet: cfg.clone(),
            store: Arc::clone(&store),
            agents: local.agents,
            backends: local.backends,
            client: local.client,
            commands: local_rx,
            transitions: local.transitions,
            revision: local.revision,
            identity_error: local.identity_error,
            shutdown: shutdown.resubscribe(),
        }
        .run(),
    ));

    for (alias, host) in &cfg.hosts {
        if !cfg.enabled || !host.enabled {
            continue;
        }
        let (tx, rx) = mpsc::channel(64);
        routes.insert(alias.clone(), tx);
        let task = HostTask {
            alias: alias.clone(),
            config: host.clone(),
            fleet: cfg.clone(),
            store: Arc::clone(&store),
            permits: Arc::clone(&permits),
            node_registry: Arc::clone(&node_registry),
            commands: rx,
            shutdown: shutdown.resubscribe(),
        };
        handles.push(tokio::spawn(task.run()));
    }

    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            command = commands.recv() => {
                let Some(command) = command else { break; };
                if let Some(route) = routes.get(&command.host) {
                    if route.send(HostCommand::from(command)).await.is_err() {
                        // The task disappeared between lookup and send.
                        // There is no sender left to answer; the caller's
                        // oneshot observes cancellation and reports it.
                    }
                } else {
                    let _ = command.reply.send(Err(format!(
                        "fleet host '{}' is not configured or is disabled",
                        command.host
                    )));
                }
            }
        }
    }

    drop(routes);
    for handle in handles {
        let _ = tokio::time::timeout(Duration::from_secs(3), handle).await;
    }
}

fn local_node_id() -> (NodeId, Option<String>) {
    let Some(path) = muxa::paths::default_node_id_file() else {
        return (
            NodeId::generate(),
            Some("no data directory is available; local NodeId is ephemeral".into()),
        );
    };
    match load_or_create_node_id(&path) {
        Ok(node_id) => (node_id, None),
        Err(error) => (
            NodeId::generate(),
            Some(format!(
                "could not persist local NodeId at {}: {error}; identity is ephemeral",
                path.display()
            )),
        ),
    }
}

fn local_host(
    cfg: &FleetConfig,
    node_id: NodeId,
    daemon_generation: u64,
    remote: RemoteSnapshot,
    identity_error: Option<String>,
) -> FleetHostSnapshot {
    let hostname = local_hostname();
    let mut labels = cfg.local.labels.clone();
    labels.insert("muxa.io/local".into(), "true".into());
    labels.insert("muxa.io/transport".into(), "local".into());
    labels.insert("kubernetes.io/os".into(), std::env::consts::OS.into());
    labels.insert("kubernetes.io/arch".into(), std::env::consts::ARCH.into());
    if validate_label_value(&hostname).is_ok() {
        labels.insert("kubernetes.io/hostname".into(), hostname.clone());
    }
    let now = OffsetDateTime::now_utc();
    FleetHostSnapshot {
        alias: LOCAL_HOST_ALIAS.into(),
        local: true,
        ssh_target: "local://".into(),
        labels,
        annotations: cfg.local.annotations.clone(),
        mode: HostAccessMode::Control,
        state: if identity_error.is_some() {
            FleetHostState::Degraded
        } else {
            FleetHostState::Online
        },
        node_id: Some(node_id),
        hostname: Some(hostname),
        os: Some(std::env::consts::OS.into()),
        arch: Some(std::env::consts::ARCH.into()),
        muxa_version: Some(env!("CARGO_PKG_VERSION").into()),
        protocol: Some(FLEET_PROTOCOL_VERSION),
        capabilities: FLEET_CAPABILITIES
            .iter()
            .map(|capability| (*capability).to_string())
            .collect(),
        daemon_generation: Some(daemon_generation),
        boot_id: Some(local_boot_id()),
        latency_ms: Some(0),
        last_seen_at: Some(remote.observed_at),
        received_at: Some(now),
        error: identity_error,
        remote: Some(remote),
    }
}

struct LocalTask {
    fleet: FleetConfig,
    store: Arc<FleetStore>,
    agents: SharedStore,
    backends: Vec<SharedBackend>,
    client: muxa::ipc::Client,
    commands: mpsc::Receiver<HostCommand>,
    transitions: broadcast::Receiver<muxa::Transition>,
    revision: Arc<AtomicU64>,
    identity_error: Option<String>,
    shutdown: broadcast::Receiver<()>,
}

impl LocalTask {
    async fn run(mut self) {
        let mut refresh = tokio::time::interval(Duration::from_secs(self.fleet.refresh_secs));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut mailbox_retry = tokio::time::interval(Duration::from_secs(2));
        mailbox_retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut mailbox_updates: Option<muxa::ipc::CollaborationUpdateStream> = None;
        // The initial snapshot was collected before the manager was exposed.
        refresh.tick().await;
        loop {
            tokio::select! {
                _ = self.shutdown.recv() => break,
                _ = refresh.tick() => self.refresh().await,
                _ = mailbox_retry.tick(), if mailbox_updates.is_none() => {
                    mailbox_updates = self.client.collaboration_subscribe().await.ok();
                }
                mailbox = next_mailbox_update(&mut mailbox_updates), if mailbox_updates.is_some() => {
                    match mailbox {
                        Ok(Some(revision)) => {
                            self.store.notify_mailbox(LOCAL_HOST_ALIAS, revision).await;
                        }
                        Ok(None) | Err(_) => mailbox_updates = None,
                    }
                }
                transition = self.transitions.recv() => {
                    match transition {
                        Ok(transition) => {
                            let revision = self.revision.fetch_add(1, Ordering::SeqCst) + 1;
                            self.store.apply_transition(LOCAL_HOST_ALIAS, revision, transition).await;
                        }
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            self.revision.fetch_add(skipped, Ordering::SeqCst);
                            self.refresh().await;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            self.store.mutate_host(LOCAL_HOST_ALIAS, |host| {
                                host.state = FleetHostState::Degraded;
                                host.error = Some("local agent transition stream closed".into());
                            }).await;
                            break;
                        }
                    }
                }
                command = self.commands.recv() => {
                    let Some(command) = command else { break; };
                    let result = self.execute(command.operation).await;
                    let _ = command.reply.send(result);
                }
            }
        }
    }

    async fn refresh(&self) {
        let snapshot =
            collect_local_snapshot(&self.agents, &self.backends, Arc::clone(&self.revision)).await;
        let observed = snapshot.observed_at;
        let identity_error = self.identity_error.clone();
        let current = self
            .store
            .snapshot()
            .await
            .hosts
            .into_iter()
            .find(|host| host.local);
        let changed = current
            .as_ref()
            .and_then(|host| host.remote.as_ref())
            .is_none_or(|remote| remote_payload_changed(remote, &snapshot));
        let update = |host: &mut FleetHostSnapshot| {
            host.remote = Some(merge_remote_snapshot(host.remote.take(), snapshot));
            host.state = if identity_error.is_some() {
                FleetHostState::Degraded
            } else {
                FleetHostState::Online
            };
            host.last_seen_at = Some(observed);
            host.received_at = Some(OffsetDateTime::now_utc());
            host.error = identity_error;
        };
        if changed {
            self.store.mutate_host(LOCAL_HOST_ALIAS, update).await;
        } else {
            self.store
                .mutate_host_silent(LOCAL_HOST_ALIAS, update)
                .await;
        }
    }

    #[allow(clippy::too_many_lines)] // one exhaustive local Fleet operation table
    async fn execute(&self, operation: FleetOperation) -> Result<FleetCommandResult, String> {
        match operation {
            FleetOperation::Connect => Ok(FleetCommandResult::accepted(
                "local host is always connected",
            )),
            FleetOperation::Disconnect => {
                Err("local host cannot be disconnected from its own muxad".into())
            }
            FleetOperation::Refresh => {
                self.refresh().await;
                let revision = self.revision.load(Ordering::SeqCst);
                Ok(FleetCommandResult::accepted(format!(
                    "refreshed local revision {revision}"
                )))
            }
            FleetOperation::Capture { pane } => {
                if self.fleet.capture_policy == FleetCapturePolicy::Never {
                    return Err("fleet capture policy is 'never'".into());
                }
                audit_operation(LOCAL_HOST_ALIAS, "capture", 0);
                local_capture(&self.backends, pane).await
            }
            FleetOperation::CaptureWindow { window } => {
                if self.fleet.capture_policy == FleetCapturePolicy::Never {
                    return Err("fleet capture policy is 'never'".into());
                }
                audit_operation(LOCAL_HOST_ALIAS, "capture_window", 0);
                local_capture_window(&self.backends, window).await
            }
            FleetOperation::SendPrompt { pane, text, submit } => {
                if text.len() > FLEET_MAX_FRAME_BYTES {
                    return Err(format!(
                        "local prompt exceeds {FLEET_MAX_FRAME_BYTES} bytes"
                    ));
                }
                audit_operation(LOCAL_HOST_ALIAS, "send_prompt", text.len());
                local_send_prompt(&self.backends, pane, text, submit).await
            }
            FleetOperation::CollaborationSend { pane, request } => {
                exact_local_backend(&self.backends, &pane).await?;
                audit_operation(LOCAL_HOST_ALIAS, "collaboration_send", request.body.len());
                let origin = fleet_collaboration_origin(&pane, true);
                let target = format!("pane:{}", pane.pane_id);
                self.client
                    .collaboration_send(&origin, &target, &request)
                    .await
                    .map(FleetCommandResult::collaboration_request)
                    .map_err(|error| error.to_string())
            }
            FleetOperation::CollaborationMailbox { pane } => {
                exact_local_backend(&self.backends, &pane).await?;
                audit_operation(LOCAL_HOST_ALIAS, "collaboration_mailbox", 0);
                let agent = fleet_collaboration_origin(&pane, false);
                let console = fleet_collaboration_origin(&pane, true);
                let (incoming, sent) = tokio::try_join!(
                    self.client
                        .collaboration_list(&agent, RequestMailbox::Incoming),
                    self.client
                        .collaboration_list(&console, RequestMailbox::Sent),
                )
                .map_err(|error| error.to_string())?;
                Ok(FleetCommandResult::collaboration_mailbox(incoming, sent))
            }
            FleetOperation::CollaborationGet { pane, request_id } => {
                audit_operation(LOCAL_HOST_ALIAS, "collaboration_get", 0);
                self.client
                    .collaboration_get(&fleet_collaboration_origin(&pane, true), &request_id)
                    .await
                    .map(FleetCommandResult::collaboration_request)
                    .map_err(|error| error.to_string())
            }
            FleetOperation::CollaborationClaim { pane } => {
                exact_local_backend(&self.backends, &pane).await?;
                audit_operation(LOCAL_HOST_ALIAS, "collaboration_claim", 0);
                self.client
                    .collaboration_inbox(&fleet_collaboration_origin(&pane, false))
                    .await
                    .map(|incoming| FleetCommandResult::collaboration_mailbox(incoming, Vec::new()))
                    .map_err(|error| error.to_string())
            }
            FleetOperation::CollaborationReply {
                pane,
                request_id,
                status,
                body,
            } => {
                exact_local_backend(&self.backends, &pane).await?;
                audit_operation(LOCAL_HOST_ALIAS, "collaboration_reply", body.len());
                self.client
                    .collaboration_reply(
                        &fleet_collaboration_origin(&pane, false),
                        &request_id,
                        status,
                        &body,
                        &[],
                        &[],
                    )
                    .await
                    .map(FleetCommandResult::collaboration_request)
                    .map_err(|error| error.to_string())
            }
        }
    }
}

async fn next_mailbox_update(
    updates: &mut Option<muxa::ipc::CollaborationUpdateStream>,
) -> Result<Option<u64>, muxa::ipc::RuntimeError> {
    match updates {
        Some(updates) => updates.recv().await,
        None => std::future::pending().await,
    }
}

/// Ignore observation timestamps/revision churn when deciding whether a
/// periodic authoritative scan needs to invalidate native clients.
fn remote_payload_changed(left: &RemoteSnapshot, right: &RemoteSnapshot) -> bool {
    let left = serde_json::to_vec(&(&left.agents, &left.panes, &left.sessions, &left.backends));
    let right = serde_json::to_vec(&(
        &right.agents,
        &right.panes,
        &right.sessions,
        &right.backends,
    ));
    match (left, right) {
        (Ok(left), Ok(right)) => left != right,
        _ => true,
    }
}

fn fleet_collaboration_origin(pane: &PaneKey, console: bool) -> CollaborationOrigin {
    let endpoint = &pane.window.session.endpoint;
    CollaborationOrigin {
        pane: pane.pane_id.clone(),
        socket: matches!(endpoint.host, HostKind::Tmux | HostKind::Rmux)
            .then(|| endpoint.socket.clone()),
        console,
    }
}

async fn collect_local_snapshot(
    agents: &SharedStore,
    backends: &[SharedBackend],
    revision: Arc<AtomicU64>,
) -> RemoteSnapshot {
    // Pin before scans so transitions received while a backend is enumerating
    // have a newer revision and are overlaid by `merge_remote_snapshot`.
    let snapshot_revision = revision.load(Ordering::SeqCst);
    let pane_tasks = backends
        .iter()
        .map(|backend| {
            let backend = backend.clone();
            tokio::task::spawn_blocking(move || backend.list_panes())
        })
        .collect::<Vec<_>>();
    let session_tasks = backends
        .iter()
        .map(|backend| {
            let kind = backend.kind();
            tokio::task::spawn_blocking(move || local_sessions_for_backend(kind))
        })
        .collect::<Vec<_>>();
    let agents = agents.snapshot().await;
    let mut panes = Vec::new();
    for task in pane_tasks {
        match task.await {
            Ok(observed) => panes.extend(observed),
            Err(error) => tracing::warn!(%error, "local Fleet pane scan panicked"),
        }
    }
    let mut sessions = Vec::new();
    for task in session_tasks {
        match task.await {
            Ok(observed) => sessions.extend(observed),
            Err(error) => tracing::warn!(%error, "local Fleet session scan panicked"),
        }
    }
    RemoteSnapshot {
        revision: snapshot_revision,
        observed_at: OffsetDateTime::now_utc(),
        agents,
        panes,
        sessions,
        backends: local_backend_info(backends),
    }
}

fn local_sessions_for_backend(kind: HostKind) -> Vec<SessionInfo> {
    match kind {
        HostKind::Tmux => muxa::tmux::list_sessions().unwrap_or_default(),
        HostKind::Herdr => {
            let socket = muxa::backend::herdr::default_socket_path();
            muxa::backend::herdr::herdr_list_workspaces(&socket)
                .into_iter()
                .map(|workspace| SessionInfo {
                    group: None,
                    session_id: workspace.id,
                    name: workspace.label,
                    attached_clients: 0,
                })
                .collect()
        }
        HostKind::Cmux | HostKind::Rmux | HostKind::Zellij => Vec::new(),
    }
}

fn local_backend_info(backends: &[SharedBackend]) -> Vec<muxa::fleet::FleetBackendInfo> {
    backends
        .iter()
        .map(|backend| muxa::fleet::FleetBackendInfo::new(backend.kind(), backend.caps()))
        .collect()
}

async fn exact_local_backend(
    backends: &[SharedBackend],
    key: &PaneKey,
) -> Result<SharedBackend, String> {
    let backend = backends
        .iter()
        .find(|backend| backend.kind() == key.window.session.endpoint.host)
        .cloned()
        .ok_or_else(|| {
            format!(
                "{} backend is unavailable",
                key.window.session.endpoint.host
            )
        })?;
    let scanner = backend.clone();
    let panes = tokio::task::spawn_blocking(move || scanner.list_panes())
        .await
        .map_err(|error| format!("local pane scan panicked: {error}"))?;
    if !panes
        .iter()
        .any(|pane| PaneKey::from_pane(backend.kind(), pane) == *key)
    {
        return Err("exact local pane target is stale or no longer exists".into());
    }
    Ok(backend)
}

async fn local_capture(
    backends: &[SharedBackend],
    pane: PaneKey,
) -> Result<FleetCommandResult, String> {
    let backend = exact_local_backend(backends, &pane).await?;
    if !backend.caps().capture_pane {
        return Err(format!(
            "{} backend does not support pane capture",
            backend.kind()
        ));
    }
    let socket = pane.window.session.endpoint.socket;
    let pane_id = pane.pane_id;
    let capture =
        tokio::task::spawn_blocking(move || backend.capture_pane_on(Some(&socket), &pane_id))
            .await
            .map_err(|error| format!("local capture task panicked: {error}"))?;
    Ok(FleetCommandResult::capture_with_raw(capture))
}

async fn local_capture_window(
    backends: &[SharedBackend],
    window: WindowKey,
) -> Result<FleetCommandResult, String> {
    if window.session.endpoint.host != HostKind::Tmux {
        return Err("window geometry is currently available only for tmux backends".into());
    }
    let backend = backends
        .iter()
        .find(|backend| backend.kind() == HostKind::Tmux)
        .cloned()
        .ok_or_else(|| "tmux backend is unavailable".to_string())?;
    let verify_backend = backend.clone();
    let verify_window = window.clone();
    let exists = tokio::task::spawn_blocking(move || {
        verify_backend
            .list_panes()
            .iter()
            .any(|pane| PaneKey::from_pane(verify_backend.kind(), pane).window == verify_window)
    })
    .await
    .map_err(|error| format!("local window verification panicked: {error}"))?;
    if !exists {
        return Err("exact local window target is stale or no longer exists".into());
    }
    let capture = tokio::task::spawn_blocking(move || {
        let socket = window.session.endpoint.socket.clone();
        let (geometries, zoomed) =
            muxa::tmux::layout::window_panes_on(Some(&socket), &window.window_id);
        let visible = geometries
            .into_iter()
            .filter(|geometry| !zoomed || geometry.active)
            .collect::<Vec<_>>();
        let mut panes = Vec::with_capacity(visible.len());
        for batch in visible.chunks(8) {
            panes.extend(std::thread::scope(|scope| {
                batch
                    .iter()
                    .map(|geometry| {
                        let backend = backend.clone();
                        let socket = socket.clone();
                        let geometry = geometry.clone();
                        scope.spawn(move || {
                            let text = backend
                                .capture_pane_on(Some(&socket), &geometry.pane_id)
                                .map(sanitize_capture_text);
                            FleetCapturedWindowPane { geometry, text }
                        })
                    })
                    .collect::<Vec<_>>()
                    .into_iter()
                    .filter_map(|handle| handle.join().ok())
                    .collect::<Vec<_>>()
            }));
        }
        FleetWindowCapture {
            window,
            panes,
            zoomed,
            observed_at: OffsetDateTime::now_utc(),
        }
    })
    .await
    .map_err(|error| format!("local window capture panicked: {error}"))?;
    let result = FleetCommandResult::window_capture(capture);
    let encoded = serde_json::to_vec(&result)
        .map_err(|error| format!("encoding local window capture: {error}"))?;
    if encoded.len() > FLEET_MAX_FRAME_BYTES {
        return Err(format!(
            "local window capture exceeds {FLEET_MAX_FRAME_BYTES} bytes"
        ));
    }
    Ok(result)
}

async fn local_send_prompt(
    backends: &[SharedBackend],
    pane: PaneKey,
    text: String,
    submit: bool,
) -> Result<FleetCommandResult, String> {
    let backend = exact_local_backend(backends, &pane).await?;
    if !backend.caps().send_text {
        return Err(format!(
            "{} backend does not support text injection",
            backend.kind()
        ));
    }
    let socket = pane.window.session.endpoint.socket;
    let pane_id = pane.pane_id;
    let outcome = tokio::task::spawn_blocking(move || {
        if !backend.send_text_on(Some(&socket), &pane_id, &text) {
            return None;
        }
        let submitted = if submit {
            std::thread::sleep(muxa::backend::PROMPT_SUBMIT_GRACE);
            backend.send_text_on(Some(&socket), &pane_id, "\r")
        } else {
            false
        };
        Some(muxa::ipc::SendPromptOutcome {
            sent: true,
            submitted,
        })
    })
    .await
    .map_err(|error| format!("local send task panicked: {error}"))?
    .ok_or_else(|| "backend refused text injection".to_string())?;
    Ok(FleetCommandResult::sent(outcome))
}

fn local_hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".into())
}

fn local_boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("process-{}", std::process::id()))
}

struct HostCommand {
    operation: FleetOperation,
    reply: oneshot::Sender<Result<FleetCommandResult, String>>,
}

impl From<FleetCommandEnvelope> for HostCommand {
    fn from(value: FleetCommandEnvelope) -> Self {
        Self {
            operation: value.operation,
            reply: value.reply,
        }
    }
}

struct HostTask {
    alias: String,
    config: FleetHostConfig,
    fleet: FleetConfig,
    store: Arc<FleetStore>,
    permits: Arc<Semaphore>,
    node_registry: Arc<RwLock<BTreeMap<NodeId, String>>>,
    commands: mpsc::Receiver<HostCommand>,
    shutdown: broadcast::Receiver<()>,
}

enum PendingReply {
    Command(oneshot::Sender<Result<FleetCommandResult, String>>),
    Refresh(oneshot::Sender<Result<FleetCommandResult, String>>),
}

struct PendingRequest {
    reply: PendingReply,
    deadline: Instant,
}

impl PendingRequest {
    fn new(reply: PendingReply, timeout: Duration) -> Self {
        Self {
            reply,
            deadline: Instant::now() + timeout,
        }
    }
}

impl HostTask {
    async fn run(mut self) {
        let mut desired = self.config.connect == FleetConnectPolicy::Auto;
        let mut backoff = Duration::from_secs(1);
        loop {
            if !desired {
                self.store
                    .mutate_host(&self.alias, |host| {
                        host.state = FleetHostState::Offline;
                        host.error = Some("connection is on demand".into());
                    })
                    .await;
                tokio::select! {
                    _ = self.shutdown.recv() => break,
                    command = self.commands.recv() => {
                        let Some(command) = command else { break; };
                        match command.operation {
                            FleetOperation::Connect => {
                                desired = true;
                                let _ = command.reply.send(Ok(FleetCommandResult::accepted("connecting")));
                            }
                            _ => {
                                let _ = command.reply.send(Err(format!(
                                    "host '{}' is disconnected; run `muxa fleet connect {}` first",
                                    self.alias, self.alias
                                )));
                            }
                        }
                    }
                }
                continue;
            }

            self.store
                .mutate_host(&self.alias, |host| {
                    host.state = FleetHostState::Connecting;
                    host.error = None;
                })
                .await;

            let permit = tokio::select! {
                _ = self.shutdown.recv() => break,
                permit = Arc::clone(&self.permits).acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(_) => break,
                },
            };
            let connection = self.connect().await;
            drop(permit);

            match connection {
                Ok(connected) => {
                    backoff = Duration::from_secs(1);
                    let result = self.serve_connection(connected, &mut desired).await;
                    if let Err(error) = result {
                        self.record_disconnect(&error).await;
                    }
                }
                Err(error) => self.record_disconnect(&error).await,
            }

            if !desired {
                continue;
            }
            let delay = backoff + deterministic_jitter(&self.alias, backoff);
            tokio::select! {
                _ = self.shutdown.recv() => break,
                () = tokio::time::sleep(delay) => {},
                command = self.commands.recv() => {
                    let Some(command) = command else { break; };
                    match command.operation {
                        FleetOperation::Disconnect => {
                            desired = false;
                            let _ = command.reply.send(Ok(FleetCommandResult::accepted("disconnected")));
                        }
                        FleetOperation::Connect | FleetOperation::Refresh => {
                            let _ = command.reply.send(Ok(FleetCommandResult::accepted("reconnecting")));
                        }
                        _ => {
                            let _ = command.reply.send(Err("host is offline; command was not sent".into()));
                        }
                    }
                }
            }
            backoff = (backoff * 2).min(MAX_BACKOFF);
        }
    }

    async fn connect(&self) -> Result<Connected, String> {
        let started = Instant::now();
        let mut command = Command::new("ssh");
        command
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                &format!("ConnectTimeout={}", self.fleet.connect_timeout_secs),
                "--",
                &self.config.ssh,
                &self.config.muxa_path,
            ]);
        if let Some(socket) = &self.config.remote_socket {
            command.arg("--socket").arg(socket);
        }
        command.args(["relay", "--stdio"]);

        let mut child = command
            .spawn()
            .map_err(|error| format!("could not start OpenSSH: {error}"))?;
        let stdin = child.stdin.take().ok_or("OpenSSH stdin is unavailable")?;
        let stdout = child.stdout.take().ok_or("OpenSSH stdout is unavailable")?;
        let stderr = child.stderr.take().ok_or("OpenSSH stderr is unavailable")?;
        let stderr_task = tokio::spawn(async move {
            let mut stderr = stderr;
            let bytes = drain_bounded(&mut stderr, FLEET_MAX_DIAGNOSTIC_BYTES)
                .await
                .unwrap_or_default();
            sanitize_remote_error(&String::from_utf8_lossy(&bytes))
        });
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let read = tokio::time::timeout(
            Duration::from_secs(self.fleet.connect_timeout_secs),
            read_relay_line(&mut reader, &mut line),
        )
        .await
        .map_err(|_| "timed out waiting for the remote relay handshake".to_string())?
        .map_err(|error| error.to_string())?;
        if read == 0 {
            let _ = child.wait().await;
            let stderr = stderr_task.await.unwrap_or_default();
            return Err(if stderr.is_empty() {
                "remote relay exited before its handshake".into()
            } else {
                stderr
            });
        }
        let frame: RelayFrame = serde_json::from_str(line.trim())
            .map_err(|error| format!("invalid remote relay handshake: {error}"))?;
        let RelayFrame::Hello { hello } = frame else {
            return Err("remote relay did not send hello first".into());
        };
        if hello.fleet_protocol < FLEET_PROTOCOL_VERSION
            || hello.min_fleet_protocol > FLEET_PROTOCOL_VERSION
        {
            return Err(format!(
                "fleet protocol mismatch: local={FLEET_PROTOCOL_VERSION}, remote=[{},{}]",
                hello.min_fleet_protocol, hello.fleet_protocol
            ));
        }
        {
            let mut registry = self.node_registry.write().await;
            if let Some(other) = registry.get(&hello.node_id) {
                if other != &self.alias {
                    return Err(format!(
                        "node id {} is already connected as host '{other}'",
                        hello.node_id
                    ));
                }
            }
            registry.insert(hello.node_id.clone(), self.alias.clone());
        }
        let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.apply_hello(&hello, latency_ms).await;
        Ok(Connected {
            child,
            stdin,
            reader,
            stderr_task,
            hello,
            last_frame: Instant::now(),
        })
    }

    async fn apply_hello(&self, hello: &RelayHello, latency_ms: u64) {
        self.store
            .mutate_host(&self.alias, |host| {
                host.node_id = Some(hello.node_id.clone());
                host.hostname = Some(hello.hostname.clone());
                host.os = Some(hello.os.clone());
                host.arch = Some(hello.arch.clone());
                host.muxa_version = Some(hello.muxa_version.clone());
                host.protocol = Some(hello.fleet_protocol);
                host.capabilities.clone_from(&hello.capabilities);
                host.daemon_generation = hello.daemon_generation;
                host.boot_id = Some(hello.boot_id.clone());
                host.latency_ms = Some(latency_ms);
                host.last_seen_at = Some(hello.server_time);
                host.received_at = Some(OffsetDateTime::now_utc());
                host.error = None;
                // Relay revisions are scoped to one SSH/relay stream and
                // restart from zero after reconnect. Drop the stale revision
                // here so a new revision 1 cannot be mistaken for an old
                // duplicate. Offline state retained the snapshot until this
                // authenticated hello completed.
                host.remote = Some(muxa::fleet::RemoteSnapshot {
                    revision: 0,
                    observed_at: hello.server_time,
                    agents: Vec::new(),
                    panes: Vec::new(),
                    sessions: Vec::new(),
                    backends: hello.backends.clone(),
                });
            })
            .await;
    }

    #[allow(clippy::too_many_lines)] // connection loop owns ordering, deadlines, and pending replies
    async fn serve_connection(
        &mut self,
        mut connected: Connected,
        desired: &mut bool,
    ) -> Result<(), String> {
        let mut pending = HashMap::<String, PendingRequest>::new();
        if let Err(error) = send_request(
            &mut connected.stdin,
            &RelayRequest::Snapshot {
                request_id: request_id(),
            },
        )
        .await
        {
            let mut registry = self.node_registry.write().await;
            if registry.get(&connected.hello.node_id) == Some(&self.alias) {
                registry.remove(&connected.hello.node_id);
            }
            drop(registry);
            let _ = connected.child.kill().await;
            let _ = connected.child.wait().await;
            let stderr = connected.stderr_task.await.unwrap_or_default();
            return Err(if stderr.is_empty() {
                error
            } else {
                format!("{error}: {stderr}")
            });
        }
        let mut refresh = tokio::time::interval(Duration::from_secs(self.fleet.refresh_secs));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        refresh.tick().await;
        let mut keepalive = tokio::time::interval(Duration::from_secs(self.fleet.keepalive_secs));
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        keepalive.tick().await;
        let mut health = tokio::time::interval(Duration::from_secs(1));
        health.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut line = String::new();

        let outcome: Result<(), String> = loop {
            line.clear();
            tokio::select! {
                _ = self.shutdown.recv() => {
                    *desired = false;
                    break Ok(());
                }
                command = self.commands.recv() => {
                    let Some(command) = command else {
                        *desired = false;
                        break Ok(());
                    };
                    if let FleetOperation::Disconnect = command.operation {
                        *desired = false;
                        let _ = command.reply.send(Ok(FleetCommandResult::accepted("disconnected")));
                        break Ok(());
                    }
                    if let FleetOperation::Connect = command.operation {
                        let _ = command.reply.send(Ok(FleetCommandResult::accepted("already connected")));
                        continue;
                    }
                    if matches!(
                        command.operation,
                        FleetOperation::SendPrompt { .. }
                            | FleetOperation::CollaborationSend { .. }
                            | FleetOperation::CollaborationGet { .. }
                            | FleetOperation::CollaborationClaim { .. }
                            | FleetOperation::CollaborationReply { .. }
                    )
                        && self.config.mode != HostAccessMode::Control
                    {
                        let _ = command.reply.send(Err(format!(
                            "host '{}' is observe-only; set mode = 'control' to perform control actions",
                            self.alias
                        )));
                        continue;
                    }
                    if matches!(
                        command.operation,
                        FleetOperation::CollaborationSend { .. }
                            | FleetOperation::CollaborationMailbox { .. }
                            | FleetOperation::CollaborationGet { .. }
                            | FleetOperation::CollaborationClaim { .. }
                            | FleetOperation::CollaborationReply { .. }
                    ) && !connected.hello.capabilities.iter().any(|capability| capability == "collaboration")
                    {
                        let _ = command.reply.send(Err(format!(
                            "host '{}' does not support Fleet collaboration; upgrade muxa on that host",
                            self.alias
                        )));
                        continue;
                    }
                    if matches!(command.operation, FleetOperation::CollaborationGet { .. })
                        && !connected
                            .hello
                            .capabilities
                            .iter()
                            .any(|capability| capability == "collaboration_get")
                    {
                        let _ = command.reply.send(Err(format!(
                            "host '{}' does not support exact Fleet collaboration replies; upgrade muxa on that host",
                            self.alias
                        )));
                        continue;
                    }
                    if matches!(
                        command.operation,
                        FleetOperation::Capture { .. } | FleetOperation::CaptureWindow { .. }
                    )
                        && self.fleet.capture_policy == FleetCapturePolicy::Never
                    {
                        let _ = command.reply.send(Err("fleet capture_policy is 'never'".into()));
                        continue;
                    }
                    if pending.len() >= MAX_PENDING_REQUESTS {
                        let _ = command.reply.send(Err(format!(
                            "host '{}' already has {MAX_PENDING_REQUESTS} commands awaiting acknowledgement",
                            self.alias
                        )));
                        continue;
                    }
                    let id = request_id();
                    let command_timeout = Duration::from_secs(self.fleet.command_timeout_secs);
                    let request = match command.operation {
                        FleetOperation::Refresh => {
                            pending.insert(
                                id.clone(),
                                PendingRequest::new(PendingReply::Refresh(command.reply), command_timeout),
                            );
                            RelayRequest::Snapshot { request_id: id }
                        }
                        FleetOperation::Capture { pane } => {
                            audit_operation(&self.alias, "capture", 0);
                            pending.insert(
                                id.clone(),
                                PendingRequest::new(PendingReply::Command(command.reply), command_timeout),
                            );
                            RelayRequest::Capture { request_id: id, pane }
                        }
                        FleetOperation::CaptureWindow { window } => {
                            audit_operation(&self.alias, "capture_window", 0);
                            pending.insert(
                                id.clone(),
                                PendingRequest::new(PendingReply::Command(command.reply), command_timeout),
                            );
                            RelayRequest::CaptureWindow { request_id: id, window }
                        }
                        FleetOperation::SendPrompt { pane, text, submit } => {
                            audit_operation(&self.alias, "send_prompt", text.len());
                            pending.insert(
                                id.clone(),
                                PendingRequest::new(PendingReply::Command(command.reply), command_timeout),
                            );
                            RelayRequest::SendPrompt { request_id: id, pane, text, submit }
                        }
                        FleetOperation::CollaborationSend { pane, request } => {
                            audit_operation(&self.alias, "collaboration_send", request.body.len());
                            pending.insert(
                                id.clone(),
                                PendingRequest::new(PendingReply::Command(command.reply), command_timeout),
                            );
                            RelayRequest::CollaborationSend { request_id: id, pane, request }
                        }
                        FleetOperation::CollaborationMailbox { pane } => {
                            audit_operation(&self.alias, "collaboration_mailbox", 0);
                            pending.insert(
                                id.clone(),
                                PendingRequest::new(PendingReply::Command(command.reply), command_timeout),
                            );
                            RelayRequest::CollaborationMailbox { request_id: id, pane }
                        }
                        FleetOperation::CollaborationGet { pane, request_id } => {
                            audit_operation(&self.alias, "collaboration_get", 0);
                            pending.insert(
                                id.clone(),
                                PendingRequest::new(PendingReply::Command(command.reply), command_timeout),
                            );
                            RelayRequest::CollaborationGet {
                                request_id: id,
                                pane,
                                collaboration_request_id: request_id,
                            }
                        }
                        FleetOperation::CollaborationClaim { pane } => {
                            audit_operation(&self.alias, "collaboration_claim", 0);
                            pending.insert(
                                id.clone(),
                                PendingRequest::new(PendingReply::Command(command.reply), command_timeout),
                            );
                            RelayRequest::CollaborationClaim { request_id: id, pane }
                        }
                        FleetOperation::CollaborationReply { pane, request_id, status, body } => {
                            audit_operation(&self.alias, "collaboration_reply", body.len());
                            pending.insert(
                                id.clone(),
                                PendingRequest::new(PendingReply::Command(command.reply), command_timeout),
                            );
                            RelayRequest::CollaborationReply {
                                request_id: id,
                                pane,
                                collaboration_request_id: request_id,
                                status,
                                body,
                            }
                        }
                        FleetOperation::Connect | FleetOperation::Disconnect => unreachable!(),
                    };
                    if let Err(error) = send_request(&mut connected.stdin, &request).await {
                        if let Some(reply) = pending.remove(request.request_id()) {
                            fail_pending(reply.reply, error.clone());
                        }
                        break Err(error);
                    }
                }
                _ = refresh.tick() => {
                    if let Err(error) = send_request(
                        &mut connected.stdin,
                        &RelayRequest::Snapshot { request_id: request_id() },
                    ).await {
                        break Err(error);
                    }
                }
                _ = keepalive.tick() => {
                    if let Err(error) = send_request(
                        &mut connected.stdin,
                        &RelayRequest::Ping { request_id: request_id() },
                    ).await {
                        break Err(error);
                    }
                }
                _ = health.tick() => {
                    expire_pending(
                        &mut pending,
                        Instant::now(),
                        self.fleet.command_timeout_secs,
                    );
                    if connected.last_frame.elapsed() > Duration::from_secs(self.fleet.offline_after_secs) {
                        break Err(format!(
                            "no relay frame received for {} seconds",
                            self.fleet.offline_after_secs
                        ));
                    }
                    match connected.child.try_wait() {
                        Ok(Some(status)) => break Err(format!("OpenSSH exited with {status}")),
                        Ok(None) => {}
                        Err(error) => break Err(error.to_string()),
                    }
                }
                read = read_relay_line(&mut connected.reader, &mut line) => {
                    let read = match read {
                        Ok(read) => read,
                        Err(error) => break Err(error.to_string()),
                    };
                    if read == 0 {
                        break Err("SSH relay closed its output".into());
                    }
                    connected.last_frame = Instant::now();
                    let frame: RelayFrame = match serde_json::from_str(line.trim()) {
                        Ok(frame) => frame,
                        Err(error) => break Err(format!("invalid relay frame: {error}")),
                    };
                    if let Err(error) = self.handle_frame(frame, &mut connected, &mut pending).await {
                        break Err(error);
                    }
                }
            }
        };

        for (_, request) in pending {
            fail_pending(
                request.reply,
                "SSH connection closed before command acknowledgement".into(),
            );
        }
        let mut registry = self.node_registry.write().await;
        if registry.get(&connected.hello.node_id) == Some(&self.alias) {
            registry.remove(&connected.hello.node_id);
        }
        drop(registry);
        let _ = connected.child.kill().await;
        let _ = connected.child.wait().await;
        let stderr = connected.stderr_task.await.unwrap_or_default();
        if !*desired {
            self.store
                .mutate_host(&self.alias, |host| {
                    host.state = FleetHostState::Offline;
                    host.error = Some("disconnected by operator".into());
                })
                .await;
            return Ok(());
        }
        match outcome {
            Ok(()) if stderr.is_empty() => Err("SSH relay disconnected".into()),
            Ok(()) => Err(stderr),
            Err(error) if stderr.is_empty() => Err(error),
            Err(error) => Err(format!("{error}: {stderr}")),
        }
    }

    #[allow(clippy::too_many_lines)] // exhaustive wire-frame state transition table
    async fn handle_frame(
        &self,
        frame: RelayFrame,
        connected: &mut Connected,
        pending: &mut HashMap<String, PendingRequest>,
    ) -> Result<(), String> {
        match frame {
            RelayFrame::Snapshot {
                request_id,
                snapshot,
            } => {
                let observed = snapshot.observed_at;
                let revision = snapshot.revision;
                let current = self
                    .store
                    .snapshot()
                    .await
                    .hosts
                    .into_iter()
                    .find(|host| host.alias == self.alias);
                let changed = current
                    .as_ref()
                    .and_then(|host| host.remote.as_ref())
                    .is_none_or(|remote| remote_payload_changed(remote, &snapshot));
                let update = |host: &mut FleetHostSnapshot| {
                    // Never regress a newer transition that raced a slow
                    // full scan. Pane/session inventories are still fresh,
                    // but agent state and revision remain monotonic.
                    let snapshot = merge_remote_snapshot(host.remote.take(), snapshot);
                    host.remote = Some(snapshot);
                    host.state = FleetHostState::Online;
                    host.last_seen_at = Some(observed);
                    host.received_at = Some(OffsetDateTime::now_utc());
                    host.error = None;
                };
                if changed {
                    self.store.mutate_host(&self.alias, update).await;
                } else {
                    self.store.mutate_host_silent(&self.alias, update).await;
                }
                if let Some(request) = pending.remove(&request_id) {
                    match request.reply {
                        PendingReply::Refresh(reply) | PendingReply::Command(reply) => {
                            let _ = reply.send(Ok(FleetCommandResult::accepted(format!(
                                "refreshed revision {revision}"
                            ))));
                        }
                    }
                }
            }
            RelayFrame::Transition {
                revision,
                transition,
            } => {
                self.store
                    .apply_transition(&self.alias, revision, transition)
                    .await;
            }
            RelayFrame::Keepalive {
                revision,
                observed_at,
                mailbox_revision,
            } => {
                let mut needs_resync = false;
                self.store
                    .mutate_host_silent(&self.alias, |host| {
                        host.last_seen_at = Some(observed_at);
                        host.received_at = Some(OffsetDateTime::now_utc());
                        if host
                            .remote
                            .as_ref()
                            .is_some_and(|remote| remote.revision < revision)
                        {
                            host.state = FleetHostState::Degraded;
                            host.error = Some("remote revision is ahead; resynchronizing".into());
                            needs_resync = true;
                        }
                    })
                    .await;
                if let Some(mailbox_revision) = mailbox_revision {
                    self.store
                        .notify_mailbox(&self.alias, mailbox_revision)
                        .await;
                }
                if needs_resync {
                    self.store.mutate_host(&self.alias, |_| {}).await;
                    send_request(
                        &mut connected.stdin,
                        &RelayRequest::Snapshot {
                            request_id: request_id(),
                        },
                    )
                    .await?;
                }
            }
            RelayFrame::Result { request_id, result } => {
                let result = sanitize_command_result(result);
                if let Some(request) = pending.remove(&request_id) {
                    match request.reply {
                        PendingReply::Command(reply) | PendingReply::Refresh(reply) => {
                            let _ = reply.send(Ok(result));
                        }
                    }
                }
            }
            RelayFrame::Error {
                request_id,
                code,
                message,
            } => {
                let code = sanitize_remote_error(&code);
                let message = sanitize_remote_error(&message);
                if let Some(request) = pending.remove(&request_id) {
                    fail_pending(request.reply, format!("remote {code}: {message}"));
                } else {
                    tracing::warn!(host = %self.alias, %code, %message, "unmatched relay error");
                }
            }
            RelayFrame::ResyncRequired { reason } => {
                let reason = sanitize_remote_error(&reason);
                let display_reason = reason.clone();
                self.store
                    .mutate_host(&self.alias, |host| {
                        host.state = FleetHostState::Degraded;
                        host.error = Some(reason);
                    })
                    .await;
                // The relay's local transition subscription is gone. A full
                // snapshot can repair current state but cannot restore future
                // push events; reconnect the relay process instead.
                return Err(format!("relay stream requires reconnect: {display_reason}"));
            }
            RelayFrame::Hello { .. } => return Err("relay sent a duplicate hello frame".into()),
        }
        Ok(())
    }

    async fn record_disconnect(&self, error: &str) {
        let error = sanitize_remote_error(error);
        let state = classify_error(&error);
        self.store
            .mutate_host(&self.alias, |host| {
                host.state = state;
                host.error = Some(error);
                // Deliberately retain `remote`: callers see the last-known
                // snapshot with stale/offline metadata instead of a false
                // empty fleet.
            })
            .await;
    }
}

struct Connected {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    stderr_task: tokio::task::JoinHandle<String>,
    hello: RelayHello,
    last_frame: Instant,
}

async fn read_relay_line(
    reader: &mut BufReader<tokio::process::ChildStdout>,
    line: &mut String,
) -> std::io::Result<usize> {
    read_bounded_line(reader, line, FLEET_MAX_FRAME_BYTES).await
}

async fn send_request(stdin: &mut ChildStdin, request: &RelayRequest) -> Result<(), String> {
    let mut bytes = serde_json::to_vec(request).map_err(|error| error.to_string())?;
    if bytes.len() > FLEET_MAX_FRAME_BYTES {
        return Err(format!(
            "relay request exceeds {FLEET_MAX_FRAME_BYTES} bytes"
        ));
    }
    bytes.push(b'\n');
    stdin
        .write_all(&bytes)
        .await
        .map_err(|error| error.to_string())?;
    stdin.flush().await.map_err(|error| error.to_string())
}

fn request_id() -> String {
    Uuid::new_v4().to_string()
}

fn fail_pending(reply: PendingReply, error: String) {
    match reply {
        PendingReply::Command(reply) | PendingReply::Refresh(reply) => {
            let _ = reply.send(Err(error));
        }
    }
}

fn expire_pending(pending: &mut HashMap<String, PendingRequest>, now: Instant, timeout_secs: u64) {
    let expired = pending
        .iter()
        .filter_map(|(id, request)| (request.deadline <= now).then_some(id.clone()))
        .collect::<Vec<_>>();
    for id in expired {
        if let Some(request) = pending.remove(&id) {
            fail_pending(
                request.reply,
                format!("remote command timed out after {timeout_secs} seconds"),
            );
        }
    }
}

fn classify_error(error: &str) -> FleetHostState {
    let lower = error.to_ascii_lowercase();
    if lower.contains("permission denied")
        || lower.contains("host key verification failed")
        || lower.contains("remote host identification has changed")
    {
        FleetHostState::AuthFailed
    } else if lower.contains("protocol mismatch") {
        FleetHostState::VersionSkew
    } else {
        FleetHostState::Offline
    }
}

fn sanitize_remote_error(error: &str) -> String {
    let mut clean = sanitize_terminal_text(error);
    if clean.len() > FLEET_MAX_DIAGNOSTIC_BYTES {
        let mut boundary = FLEET_MAX_DIAGNOSTIC_BYTES;
        while !clean.is_char_boundary(boundary) {
            boundary -= 1;
        }
        clean.truncate(boundary);
        clean.push('…');
    }
    clean.trim().to_string()
}

fn sanitize_command_result(mut result: FleetCommandResult) -> FleetCommandResult {
    result.message = result
        .message
        .map(|message| sanitize_remote_error(&message));
    result.capture = result.capture.map(sanitize_capture_text);
    result.capture_raw_base64 = result
        .capture_raw_base64
        .and_then(sanitize_raw_capture_base64);
    if let Some(window) = &mut result.window_capture {
        for pane in &mut window.panes {
            pane.text = pane.text.take().map(sanitize_capture_text);
        }
    }
    result
}

fn merge_remote_snapshot(
    current: Option<muxa::fleet::RemoteSnapshot>,
    mut snapshot: muxa::fleet::RemoteSnapshot,
) -> muxa::fleet::RemoteSnapshot {
    let Some(current) = current.filter(|current| current.revision > snapshot.revision) else {
        return snapshot;
    };
    for current_agent in current.agents {
        if let Some(agent) = snapshot.agents.iter_mut().find(|agent| {
            agent.kind == current_agent.kind && agent.session_id == current_agent.session_id
        }) {
            *agent = current_agent;
        } else {
            snapshot.agents.push(current_agent);
        }
    }
    snapshot.revision = current.revision;
    snapshot.observed_at = snapshot.observed_at.max(current.observed_at);
    snapshot
}

fn deterministic_jitter(alias: &str, base: Duration) -> Duration {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    alias.hash(&mut hasher);
    let max_ms = u64::try_from(base.as_millis()).unwrap_or(u64::MAX) / 4 + 1;
    Duration::from_millis(hasher.finish() % max_ms)
}

fn audit_operation(host: &str, operation: &str, body_bytes: usize) {
    tracing::info!(
        fleet_host = host,
        fleet_operation = operation,
        body_bytes,
        "fleet control request"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::event::{AgentEvent, AgentId, AgentKind, AgentState};
    use muxa::state::Store;

    #[test]
    fn local_host_is_first_class_with_truthful_managed_labels() {
        let mut cfg = FleetConfig::default();
        cfg.local
            .labels
            .insert("environment".into(), "development".into());
        cfg.local
            .annotations
            .insert("example.com/owner".into(), "June".into());
        let observed_at = OffsetDateTime::now_utc();
        let remote = RemoteSnapshot {
            revision: 4,
            observed_at,
            agents: Vec::new(),
            panes: Vec::new(),
            sessions: Vec::new(),
            backends: Vec::new(),
        };
        let host = local_host(&cfg, NodeId::generate(), 9, remote, None);
        assert!(host.local);
        assert_eq!(host.alias, LOCAL_HOST_ALIAS);
        assert_eq!(host.ssh_target, "local://");
        assert_eq!(host.mode, HostAccessMode::Control);
        assert_eq!(host.state, FleetHostState::Online);
        assert_eq!(host.labels["muxa.io/local"], "true");
        assert_eq!(host.labels["muxa.io/transport"], "local");
        assert_eq!(host.labels["environment"], "development");
        assert_eq!(host.annotations["example.com/owner"], "June");
        assert_eq!(host.daemon_generation, Some(9));
        assert_eq!(host.remote.unwrap().revision, 4);
    }

    #[test]
    fn ssh_errors_are_classified_without_ansi_controls() {
        assert_eq!(
            classify_error("Permission denied (publickey)"),
            FleetHostState::AuthFailed
        );
        assert_eq!(
            classify_error("fleet protocol mismatch"),
            FleetHostState::VersionSkew
        );
        assert_eq!(classify_error("connection reset"), FleetHostState::Offline);
        assert_eq!(sanitize_remote_error("\u{1b}[31mboom\u{1b}[0m"), "boom");
        let unicode = sanitize_remote_error(&"가".repeat(400));
        assert!(unicode.ends_with('…'));
        assert!(unicode.len() <= FLEET_MAX_DIAGNOSTIC_BYTES + '…'.len_utf8());
    }

    #[test]
    fn reconnect_jitter_is_stable_and_bounded() {
        let base = Duration::from_secs(4);
        let first = deterministic_jitter("dev", base);
        assert_eq!(first, deterministic_jitter("dev", base));
        assert!(first <= Duration::from_secs(1));
    }

    #[tokio::test]
    async fn newer_transition_state_overlays_a_slow_full_snapshot() {
        let store = Store::shared();
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::Codex,
                    session_id: "agent-1".into(),
                    surface: None,
                    pane: Some("%1".into()),
                    tmux_socket: Some("default".into()),
                    cwd: Some("/tmp/project".into()),
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;
        let original = store.snapshot().await.pop().unwrap();
        let mut transitioned = original.clone();
        transitioned.state = AgentState::Working;
        let mut other = original.clone();
        other.session_id = "agent-2".into();
        let now = OffsetDateTime::now_utc();
        let current = muxa::fleet::RemoteSnapshot {
            revision: 2,
            observed_at: now,
            agents: vec![transitioned],
            panes: Vec::new(),
            sessions: Vec::new(),
            backends: Vec::new(),
        };
        let stale_scan = muxa::fleet::RemoteSnapshot {
            revision: 1,
            observed_at: now - time::Duration::SECOND,
            agents: vec![original, other],
            panes: Vec::new(),
            sessions: Vec::new(),
            backends: Vec::new(),
        };

        let merged = merge_remote_snapshot(Some(current), stale_scan);
        assert_eq!(merged.revision, 2);
        assert_eq!(merged.agents.len(), 2);
        assert_eq!(
            merged
                .agents
                .iter()
                .find(|agent| agent.session_id == "agent-1")
                .unwrap()
                .state,
            AgentState::Working
        );
    }

    #[test]
    fn command_deadlines_release_pending_callers() {
        let (reply, mut response) = oneshot::channel();
        let now = Instant::now();
        let mut pending = HashMap::from([(
            "request".into(),
            PendingRequest {
                reply: PendingReply::Command(reply),
                deadline: now,
            },
        )]);
        expire_pending(&mut pending, now, 7);
        assert!(pending.is_empty());
        let error = response.try_recv().unwrap().unwrap_err();
        assert_eq!(error, "remote command timed out after 7 seconds");
    }
}
