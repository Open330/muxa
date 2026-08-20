//! Persistent SSH connection manager for physical Muxa Fleet hosts.

use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use muxa::config::{FleetCapturePolicy, FleetConfig, FleetConnectPolicy, FleetHostConfig};
use muxa::fleet::{
    drain_bounded, read_bounded_line, sanitize_capture_text, sanitize_terminal_text,
    FleetCommandEnvelope, FleetCommandReceiver, FleetCommandResult, FleetHostSnapshot,
    FleetHostState, FleetOperation, FleetRuntime, FleetStore, HostAccessMode, NodeId, RelayFrame,
    RelayHello, RelayRequest, FLEET_MAX_DIAGNOSTIC_BYTES, FLEET_MAX_FRAME_BYTES,
    FLEET_PROTOCOL_VERSION,
};
use time::OffsetDateTime;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{broadcast, mpsc, oneshot, RwLock, Semaphore};
use uuid::Uuid;

const MAX_BACKOFF: Duration = Duration::from_secs(30);
const MAX_PENDING_REQUESTS: usize = 64;

pub(crate) async fn start(
    cfg: &FleetConfig,
    shutdown: broadcast::Receiver<()>,
) -> (Option<FleetRuntime>, Option<tokio::task::JoinHandle<()>>) {
    if !cfg.enabled {
        return (None, None);
    }

    let store = Arc::new(FleetStore::new());
    let (runtime, receiver) = FleetRuntime::new(Arc::clone(&store));
    for (alias, host) in &cfg.hosts {
        store.upsert_host(base_host(alias, host)).await;
    }
    let config = cfg.clone();
    let handle = tokio::spawn(async move {
        run_manager(config, store, receiver, shutdown).await;
    });
    (Some(runtime), Some(handle))
}

fn base_host(alias: &str, cfg: &FleetHostConfig) -> FleetHostSnapshot {
    FleetHostSnapshot {
        alias: alias.into(),
        ssh_target: cfg.ssh.clone(),
        labels: cfg.labels.clone(),
        annotations: cfg.annotations.clone(),
        mode: cfg.mode,
        state: if cfg.enabled {
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

async fn run_manager(
    cfg: FleetConfig,
    store: Arc<FleetStore>,
    mut commands: FleetCommandReceiver,
    mut shutdown: broadcast::Receiver<()>,
) {
    let permits = Arc::new(Semaphore::new(cfg.max_parallel_connects));
    let node_registry = Arc::new(RwLock::new(BTreeMap::<NodeId, String>::new()));
    let mut routes = HashMap::new();
    let mut handles = Vec::new();

    for (alias, host) in &cfg.hosts {
        if !host.enabled {
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
                    if matches!(command.operation, FleetOperation::SendPrompt { .. })
                        && self.config.mode != HostAccessMode::Control
                    {
                        let _ = command.reply.send(Err(format!(
                            "host '{}' is observe-only; set mode = 'control' to send prompts",
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
                self.store
                    .mutate_host(&self.alias, |host| {
                        // Never regress a newer transition that raced a slow
                        // full scan. Pane/session inventories are still fresh,
                        // but agent state and revision remain monotonic.
                        let snapshot = merge_remote_snapshot(host.remote.take(), snapshot);
                        host.remote = Some(snapshot);
                        host.state = FleetHostState::Online;
                        host.last_seen_at = Some(observed);
                        host.received_at = Some(OffsetDateTime::now_utc());
                        host.error = None;
                    })
                    .await;
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
            } => {
                let mut needs_resync = false;
                self.store
                    .mutate_host(&self.alias, |host| {
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
                if needs_resync {
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
