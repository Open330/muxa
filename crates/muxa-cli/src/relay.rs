//! SSH stdio relay used by Muxa Fleet.
//!
//! The remote shell sees only a fixed `muxa relay --stdio` command. Prompt
//! text and pane metadata travel as length-bounded JSON lines on stdin, never
//! as shell arguments. Every pane action is revalidated against its complete
//! backend/socket/session/window/pane key before it can run.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use muxa::collaboration::{CollaborationOrigin, RequestMailbox};
use muxa::fleet::{
    load_or_create_node_id, read_bounded_line, sanitize_capture_text, FleetBackendInfo,
    FleetCapturedWindowPane, FleetCommandResult, FleetWindowCapture, GlobalPaneRef, RelayFrame,
    RelayHello, RelayRequest, RemoteSnapshot, FLEET_CAPABILITIES, FLEET_MAX_FRAME_BYTES,
    FLEET_MIN_PROTOCOL_VERSION, FLEET_PROTOCOL_VERSION,
};
use muxa::ipc::Client;
use muxa::tmux::SessionInfo;
use muxa::work_control::{self, WorkCommandLimits, WorkCommandSurface};
use muxa::{HostKind, PaneKey, SharedBackend};
use time::OffsetDateTime;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, Semaphore};

const RELAY_OUTPUT_CAPACITY: usize = 256;
const RELAY_REQUEST_CONCURRENCY: usize = 32;
const RELAY_KEEPALIVE: Duration = Duration::from_secs(10);

#[allow(clippy::too_many_lines)] // relay lifecycle and its owned tasks are one protocol boundary
pub(crate) async fn run(client: Client) -> Result<()> {
    let id_path = muxa::paths::default_node_id_file()
        .context("no data directory is available for the fleet host id")?;
    let node_id = load_or_create_node_id(&id_path)
        .with_context(|| format!("loading fleet host identity from {}", id_path.display()))?;
    let daemon = client
        .hello(Duration::from_secs(3))
        .await
        .context("local muxad is unavailable")?;
    let backends = muxa::active_backends();
    let backend_info = backend_info(&backends);
    let mut transitions = client
        .subscribe()
        .await
        .context("subscribing to local muxad")?;
    let mut mailbox_updates = if daemon
        .capabilities
        .iter()
        .any(|capability| capability == "collaboration_subscribe")
    {
        Some(
            client
                .collaboration_subscribe()
                .await
                .context("subscribing to local collaboration changes")?,
        )
    } else {
        None
    };
    let revision = Arc::new(AtomicU64::new(0));
    let (output_tx, mut output_rx) = mpsc::channel::<RelayFrame>(RELAY_OUTPUT_CAPACITY);

    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(frame) = output_rx.recv().await {
            let mut encoded = serde_json::to_vec(&frame)?;
            if encoded.len() > FLEET_MAX_FRAME_BYTES {
                bail!("relay response exceeds {FLEET_MAX_FRAME_BYTES} bytes");
            }
            encoded.push(b'\n');
            stdout.write_all(&encoded).await?;
            stdout.flush().await?;
        }
        Ok::<(), anyhow::Error>(())
    });

    output_tx
        .send(RelayFrame::Hello {
            hello: RelayHello {
                fleet_protocol: FLEET_PROTOCOL_VERSION,
                min_fleet_protocol: FLEET_MIN_PROTOCOL_VERSION,
                node_id,
                hostname: hostname(),
                os: std::env::consts::OS.into(),
                arch: std::env::consts::ARCH.into(),
                muxa_version: env!("CARGO_PKG_VERSION").into(),
                capabilities: FLEET_CAPABILITIES
                    .iter()
                    .map(|capability| (*capability).to_string())
                    .collect(),
                daemon_generation: daemon.generation,
                boot_id: boot_id(),
                backends: backend_info,
                server_time: OffsetDateTime::now_utc(),
            },
        })
        .await
        .context("relay output closed")?;

    let transition_tx = output_tx.clone();
    let transition_revision = Arc::clone(&revision);
    let transition_task = tokio::spawn(async move {
        loop {
            match transitions.recv().await {
                Ok(Some(transition)) => {
                    let next = transition_revision.fetch_add(1, Ordering::SeqCst) + 1;
                    if transition_tx
                        .send(RelayFrame::Transition {
                            revision: next,
                            transition,
                        })
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
                Ok(None) => {
                    let _ = transition_tx
                        .send(RelayFrame::ResyncRequired {
                            reason: "local muxad subscription closed".into(),
                        })
                        .await;
                    break;
                }
                Err(error) => {
                    let _ = transition_tx
                        .send(RelayFrame::ResyncRequired {
                            reason: format!("local muxad subscription failed: {error}"),
                        })
                        .await;
                    break;
                }
            }
        }
    });

    let mailbox_task = mailbox_updates.take().map(|mut mailbox_updates| {
        let mailbox_tx = output_tx.clone();
        let mailbox_topology_revision = Arc::clone(&revision);
        tokio::spawn(async move {
            while let Ok(Some(mailbox_revision)) = mailbox_updates.recv().await {
                if mailbox_tx
                    .send(RelayFrame::Keepalive {
                        revision: mailbox_topology_revision.load(Ordering::SeqCst),
                        observed_at: OffsetDateTime::now_utc(),
                        mailbox_revision: Some(mailbox_revision),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        })
    });

    let keepalive_tx = output_tx.clone();
    let keepalive_revision = Arc::clone(&revision);
    let keepalive_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(RELAY_KEEPALIVE);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            if keepalive_tx
                .send(RelayFrame::Keepalive {
                    revision: keepalive_revision.load(Ordering::SeqCst),
                    observed_at: OffsetDateTime::now_utc(),
                    mailbox_revision: None,
                })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut line = String::new();
    let request_permits = Arc::new(Semaphore::new(RELAY_REQUEST_CONCURRENCY));
    loop {
        line.clear();
        let read = read_bounded_line(&mut stdin, &mut line, FLEET_MAX_FRAME_BYTES).await?;
        if read == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let request = match serde_json::from_str::<RelayRequest>(trimmed) {
            Ok(request) => request,
            Err(error) => {
                output_tx
                    .send(RelayFrame::Error {
                        request_id: String::new(),
                        code: "bad_request".into(),
                        message: error.to_string(),
                    })
                    .await
                    .ok();
                continue;
            }
        };
        let tx = output_tx.clone();
        let request_client = client.clone();
        let request_backends = backends.clone();
        let request_revision = Arc::clone(&revision);
        let permit = Arc::clone(&request_permits)
            .acquire_owned()
            .await
            .context("relay request semaphore closed")?;
        tokio::spawn(async move {
            let _permit = permit;
            let request_id = request.request_id().to_string();
            let frame =
                match handle_request(request, request_client, request_backends, request_revision)
                    .await
                {
                    Ok(frame) => frame,
                    Err(error) => RelayFrame::Error {
                        request_id,
                        code: "request_failed".into(),
                        message: error.to_string(),
                    },
                };
            let _ = tx.send(frame).await;
        });
    }

    drop(output_tx);
    transition_task.abort();
    if let Some(mailbox_task) = mailbox_task {
        mailbox_task.abort();
    }
    keepalive_task.abort();
    writer.await.context("joining relay writer")??;
    Ok(())
}

#[allow(clippy::too_many_lines)] // exhaustive relay request dispatch
async fn handle_request(
    request: RelayRequest,
    client: Client,
    backends: Vec<SharedBackend>,
    revision: Arc<AtomicU64>,
) -> Result<RelayFrame> {
    match request {
        RelayRequest::Snapshot { request_id } => {
            let snapshot = collect_snapshot(&client, &backends, revision).await?;
            Ok(RelayFrame::Snapshot {
                request_id,
                snapshot,
            })
        }
        RelayRequest::Ping { request_id } => Ok(RelayFrame::Result {
            request_id,
            result: FleetCommandResult::accepted("pong"),
        }),
        RelayRequest::Capture { request_id, pane } => {
            let backend = exact_backend(&backends, &pane).await?;
            let socket = pane.window.session.endpoint.socket.clone();
            let pane_id = pane.pane_id.clone();
            let capture = tokio::task::spawn_blocking(move || {
                backend.capture_pane_on(Some(&socket), &pane_id)
            })
            .await
            .context("capture task panicked")?;
            Ok(RelayFrame::Result {
                request_id,
                result: FleetCommandResult::capture_with_raw(capture),
            })
        }
        RelayRequest::CaptureWindow { request_id, window } => {
            if window.session.endpoint.host != HostKind::Tmux {
                bail!("window geometry is currently available only for tmux backends");
            }
            let backend = backends
                .iter()
                .find(|backend| backend.kind() == HostKind::Tmux)
                .cloned()
                .context("tmux backend is unavailable")?;
            let verify_backend = backend.clone();
            let verify_window = window.clone();
            let exists = tokio::task::spawn_blocking(move || {
                verify_backend.list_panes().iter().any(|pane| {
                    PaneKey::from_pane(verify_backend.kind(), pane).window == verify_window
                })
            })
            .await
            .context("window verification task panicked")?;
            if !exists {
                bail!("exact window target is stale or no longer exists");
            }
            let capture_window = window.clone();
            let capture = tokio::task::spawn_blocking(move || {
                let socket = capture_window.session.endpoint.socket.clone();
                let (geometries, zoomed) =
                    muxa::tmux::layout::window_panes_on(Some(&socket), &capture_window.window_id);
                let visible = geometries
                    .into_iter()
                    .filter(|geometry| !zoomed || geometry.active)
                    .collect::<Vec<_>>();
                let mut panes = Vec::with_capacity(visible.len());
                for batch in visible.chunks(8) {
                    panes.extend(std::thread::scope(|scope| {
                        let handles = batch
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
                            .collect::<Vec<_>>();
                        handles
                            .into_iter()
                            .filter_map(|handle| handle.join().ok())
                            .collect::<Vec<_>>()
                    }));
                }
                FleetWindowCapture {
                    window: capture_window,
                    panes,
                    zoomed,
                    observed_at: OffsetDateTime::now_utc(),
                }
            })
            .await
            .context("window capture task panicked")?;
            Ok(RelayFrame::Result {
                request_id,
                result: FleetCommandResult::window_capture(capture),
            })
        }
        RelayRequest::SendPrompt {
            request_id,
            pane,
            text,
            submit,
        } => {
            let backend = exact_backend(&backends, &pane).await?;
            if !backend.caps().send_text {
                bail!("{} backend does not support text injection", backend.kind());
            }
            let socket = pane.window.session.endpoint.socket.clone();
            let pane_id = pane.pane_id.clone();
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
            .context("send task panicked")?
            .context("backend refused text injection")?;
            Ok(RelayFrame::Result {
                request_id,
                result: FleetCommandResult::sent(outcome),
            })
        }
        RelayRequest::WorkCommand {
            request_id,
            args,
            stdin,
        } => {
            // The controller already applied this allowlist; a relay never
            // trusts that and refuses anything else before a process exists.
            work_control::validate_work_command(
                &args,
                stdin.as_deref(),
                WorkCommandSurface::Relay,
            )?;
            let binary = std::env::current_exe().context("locating this muxa binary")?;
            let limits = WorkCommandLimits::for_args(&args);
            let output = work_control::execute_work_command(
                &binary,
                &args,
                stdin.as_deref(),
                Some(client.socket()),
                limits,
            )
            .await?;
            Ok(RelayFrame::Result {
                request_id,
                result: FleetCommandResult::command(output),
            })
        }
        RelayRequest::CollaborationSend {
            request_id,
            pane,
            request,
        } => {
            exact_backend(&backends, &pane).await?;
            let origin = collaboration_origin(&pane, true);
            let target = format!("pane:{}", pane.pane_id);
            let request = client
                .collaboration_send(&origin, &target, &request)
                .await
                .context("sending collaboration request")?;
            Ok(RelayFrame::Result {
                request_id,
                result: FleetCommandResult::collaboration_request(request),
            })
        }
        RelayRequest::CollaborationMailbox { request_id, pane } => {
            exact_backend(&backends, &pane).await?;
            let agent = collaboration_origin(&pane, false);
            let console = collaboration_origin(&pane, true);
            let (incoming, sent) = tokio::try_join!(
                client.collaboration_list(&agent, RequestMailbox::Incoming),
                client.collaboration_list(&console, RequestMailbox::Sent),
            )
            .context("reading collaboration mailbox")?;
            Ok(RelayFrame::Result {
                request_id,
                result: FleetCommandResult::collaboration_mailbox(incoming, sent),
            })
        }
        RelayRequest::CollaborationGet {
            request_id,
            pane,
            collaboration_request_id,
        } => {
            let origin = collaboration_origin(&pane, true);
            let request = client
                .collaboration_get(&origin, &collaboration_request_id)
                .await
                .context("reading collaboration request")?;
            Ok(RelayFrame::Result {
                request_id,
                result: FleetCommandResult::collaboration_request(request),
            })
        }
        RelayRequest::CollaborationClaim { request_id, pane } => {
            exact_backend(&backends, &pane).await?;
            let agent = collaboration_origin(&pane, false);
            let incoming = client
                .collaboration_inbox(&agent)
                .await
                .context("claiming collaboration inbox")?;
            Ok(RelayFrame::Result {
                request_id,
                result: FleetCommandResult::collaboration_mailbox(incoming, Vec::new()),
            })
        }
        RelayRequest::CollaborationReply {
            request_id,
            pane,
            collaboration_request_id,
            status,
            body,
        } => {
            exact_backend(&backends, &pane).await?;
            let agent = collaboration_origin(&pane, false);
            let request = client
                .collaboration_reply(&agent, &collaboration_request_id, status, &body, &[], &[])
                .await
                .context("replying to collaboration request")?;
            Ok(RelayFrame::Result {
                request_id,
                result: FleetCommandResult::collaboration_request(request),
            })
        }
    }
}

fn collaboration_origin(pane: &PaneKey, console: bool) -> CollaborationOrigin {
    let endpoint = &pane.window.session.endpoint;
    CollaborationOrigin {
        pane: pane.pane_id.clone(),
        socket: matches!(endpoint.host, HostKind::Tmux | HostKind::Rmux)
            .then(|| endpoint.socket.clone()),
        console,
    }
}

async fn exact_backend(backends: &[SharedBackend], key: &PaneKey) -> Result<SharedBackend> {
    let backend = backends
        .iter()
        .find(|backend| backend.kind() == key.window.session.endpoint.host)
        .cloned()
        .with_context(|| {
            format!(
                "{} backend is unavailable",
                key.window.session.endpoint.host
            )
        })?;
    let scan_backend = backend.clone();
    let panes = tokio::task::spawn_blocking(move || scan_backend.list_panes())
        .await
        .context("pane scan task panicked")?;
    if !panes
        .iter()
        .any(|pane| PaneKey::from_pane(backend.kind(), pane) == *key)
    {
        bail!("exact pane target is stale or no longer exists");
    }
    Ok(backend)
}

async fn collect_snapshot(
    client: &Client,
    backends: &[SharedBackend],
    revision: Arc<AtomicU64>,
) -> Result<RemoteSnapshot> {
    // Pin before any scans. Transitions that happen during collection receive
    // a strictly newer revision and therefore remain applicable regardless of
    // whether their frame is queued before or after this snapshot frame.
    let snapshot_revision = revision.load(Ordering::SeqCst);
    let pane_tasks: Vec<_> = backends
        .iter()
        .map(|backend| {
            let backend = backend.clone();
            tokio::task::spawn_blocking(move || backend.list_panes())
        })
        .collect();
    let session_tasks: Vec<_> = backends
        .iter()
        .map(|backend| {
            let kind = backend.kind();
            tokio::task::spawn_blocking(move || sessions_for_backend(kind))
        })
        .collect();
    let agents = client
        .snapshot()
        .await
        .context("reading local agent snapshot")?;
    let mut panes = Vec::new();
    for task in pane_tasks {
        panes.extend(task.await.context("pane scan task panicked")?);
    }
    let mut sessions = Vec::new();
    for task in session_tasks {
        sessions.extend(task.await.context("session scan task panicked")?);
    }
    Ok(RemoteSnapshot {
        revision: snapshot_revision,
        observed_at: OffsetDateTime::now_utc(),
        agents,
        panes,
        sessions,
        backends: backend_info(backends),
    })
}

fn sessions_for_backend(kind: HostKind) -> Vec<SessionInfo> {
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

fn backend_info(backends: &[SharedBackend]) -> Vec<FleetBackendInfo> {
    backends
        .iter()
        .map(|backend| FleetBackendInfo::new(backend.kind(), backend.caps()))
        .collect()
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".into())
}

fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| format!("relay-{}", muxa::NodeId::generate()))
}

/// Encode an exact target into a shell-token-safe hexadecimal argument for a
/// separate `ssh -t` attach channel. The target remains validated remotely.
pub(crate) fn encode_attach_token(target: &GlobalPaneRef) -> Result<String> {
    let bytes = serde_json::to_vec(target)?;
    let mut token = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut token, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(token)
}

fn decode_attach_token(token: &str) -> Result<GlobalPaneRef> {
    if token.is_empty() || !token.len().is_multiple_of(2) || token.len() > 64 * 1024 {
        bail!("invalid remote attach token length");
    }
    let bytes = token
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let pair = std::str::from_utf8(pair)?;
            u8::from_str_radix(pair, 16).map_err(anyhow::Error::from)
        })
        .collect::<Result<Vec<_>>>()?;
    serde_json::from_slice(&bytes).context("decoding remote attach target")
}

struct TmuxFitGuard {
    socket: String,
    window_target: String,
    pane: String,
    previous_pane: String,
    previous_dimensions: Option<(String, String)>,
    previous_window_size: Option<String>,
    zoomed_by_us: bool,
}

impl TmuxFitGuard {
    fn begin(pane: &PaneKey) -> Result<Option<Self>> {
        if pane.window.session.endpoint.host != HostKind::Tmux {
            return Ok(None);
        }
        let socket = pane.window.session.endpoint.socket.clone();
        let window_target = format!(
            "{}:{}",
            pane.window.session.session_id, pane.window.window_id
        );
        let previous_window_size = muxa::tmux::capture_control_on(
            Some(&socket),
            &[
                "show-options",
                "-w",
                "-q",
                "-v",
                "-t",
                &window_target,
                "window-size",
            ],
        )?
        .trim()
        .to_string();
        let previous_window_size =
            (!previous_window_size.is_empty()).then_some(previous_window_size);
        let previous_pane = muxa::tmux::capture_control_on(
            Some(&socket),
            &["display-message", "-p", "-t", &window_target, "#{pane_id}"],
        )?
        .trim()
        .to_string();
        let previous_dimensions = muxa::tmux::capture_control_on(
            Some(&socket),
            &[
                "display-message",
                "-p",
                "-t",
                &window_target,
                "#{window_width} #{window_height}",
            ],
        )?;
        let mut dimensions = previous_dimensions.split_whitespace();
        let previous_dimensions = dimensions
            .next()
            .zip(dimensions.next())
            .map(|(width, height)| (width.to_string(), height.to_string()));
        let was_zoomed = muxa::tmux::capture_control_on(
            Some(&socket),
            &[
                "display-message",
                "-p",
                "-t",
                &pane.pane_id,
                "#{window_zoomed_flag}",
            ],
        )?
        .trim()
            == "1";
        let mut guard = Self {
            socket,
            window_target,
            pane: pane.pane_id.clone(),
            previous_pane,
            previous_dimensions,
            previous_window_size,
            zoomed_by_us: false,
        };
        muxa::tmux::run_control_on(
            Some(&guard.socket),
            &[
                "set-option",
                "-w",
                "-t",
                &guard.window_target,
                "window-size",
                "latest",
            ],
        )?;
        muxa::tmux::run_control_on(Some(&guard.socket), &["select-pane", "-t", &guard.pane])?;
        if !was_zoomed {
            muxa::tmux::run_control_on(
                Some(&guard.socket),
                &["resize-pane", "-Z", "-t", &guard.pane],
            )?;
            guard.zoomed_by_us = true;
        }
        Ok(Some(guard))
    }
}

impl Drop for TmuxFitGuard {
    fn drop(&mut self) {
        if self.zoomed_by_us {
            let _ = muxa::tmux::run_control_on(
                Some(&self.socket),
                &["resize-pane", "-Z", "-t", &self.pane],
            );
        }
        if !self.previous_pane.is_empty() && self.previous_pane != self.pane {
            let _ = muxa::tmux::run_control_on(
                Some(&self.socket),
                &["select-pane", "-t", &self.previous_pane],
            );
        }
        if let Some((width, height)) = self.previous_dimensions.as_ref() {
            let _ = muxa::tmux::run_control_on(
                Some(&self.socket),
                &[
                    "resize-window",
                    "-t",
                    &self.window_target,
                    "-x",
                    width,
                    "-y",
                    height,
                ],
            );
        }
        if let Some(previous) = self.previous_window_size.as_deref() {
            let _ = muxa::tmux::run_control_on(
                Some(&self.socket),
                &[
                    "set-option",
                    "-w",
                    "-t",
                    &self.window_target,
                    "window-size",
                    previous,
                ],
            );
        } else {
            let _ = muxa::tmux::run_control_on(
                Some(&self.socket),
                &[
                    "set-option",
                    "-w",
                    "-u",
                    "-t",
                    &self.window_target,
                    "window-size",
                ],
            );
        }
    }
}

pub(crate) fn remote_attach(token: &str, fit: bool) -> Result<()> {
    let target = decode_attach_token(token)?;
    let id_path = muxa::paths::default_node_id_file()
        .context("no data directory is available for the fleet host id")?;
    let local = load_or_create_node_id(&id_path)?;
    if target.node_id != local {
        bail!(
            "attach target belongs to node {}, not this node {local}",
            target.node_id
        );
    }
    let backend = muxa::active_backends()
        .into_iter()
        .find(|backend| backend.kind() == target.pane.window.session.endpoint.host)
        .context("target backend is unavailable")?;
    if !backend
        .list_panes()
        .iter()
        .any(|pane| PaneKey::from_pane(backend.kind(), pane) == target.pane)
    {
        bail!("exact pane target is stale or no longer exists");
    }
    let _fit_guard = fit
        .then(|| TmuxFitGuard::begin(&target.pane))
        .transpose()?
        .flatten();
    crate::jump_to_topology_pane(&target.pane);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::{BackendEndpoint, SessionKey, WindowKey};

    #[test]
    fn attach_token_is_shell_safe_and_round_trips() {
        let target = GlobalPaneRef {
            node_id: muxa::NodeId::generate(),
            pane: PaneKey {
                window: WindowKey {
                    session: SessionKey {
                        endpoint: BackendEndpoint {
                            host: HostKind::Tmux,
                            socket: "default".into(),
                        },
                        session_id: "$1".into(),
                    },
                    window_id: "@2".into(),
                },
                pane_id: "%3".into(),
            },
            agent_session_id: Some("agent;$(bad)".into()),
        };
        let token = encode_attach_token(&target).unwrap();
        assert!(token.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_eq!(decode_attach_token(&token).unwrap(), target);
    }
}
