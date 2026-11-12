//! The per-connection IPC loop. `handle` reads one request line at a time,
//! parses it once, and routes it to the request family that owns it; each
//! family module holds the `match` arms for its requests.

use super::*;

mod agents;
mod ask_ops;
mod automation_ops;
mod config_ops;
mod daemon;
mod decisions;
mod fleet_ops;
mod mailbox;
mod pipelines;
mod sessions;
mod streams;
mod swap_jobs;
mod work_ops;
mod workers;

/// The daemon state every request handler can reach. One per connection,
/// cloned from the `Server` when the connection is accepted.
pub(super) struct ServerCtx {
    pub(super) store: SharedStore,
    pub(super) backend: SharedBackend,
    pub(super) backends: Vec<SharedBackend>,
    pub(super) sessions: SharedSessionBackend,
    pub(super) collaboration: Arc<CollaborationStore>,
    pub(super) collaboration_audit: Arc<CollaborationAuditLog>,
    pub(super) ask: Arc<AskStore>,
    pub(super) agent_directory: Arc<crate::agent_directory::AgentDirectory>,
    pub(super) automation: Arc<AutomationStore>,
    pub(super) restart: Option<Arc<RestartController>>,
    pub(super) fleet: Option<FleetRuntime>,
    pub(super) pipeline_runs: Arc<PipelineRunStore>,
    pub(super) reviews: crate::review::ReviewStore,
    pub(super) work_up: Arc<WorkUpManager>,
    pub(super) mux_snapshots: Arc<crate::mux_snapshot_control::MuxSnapshotControl>,
    pub(super) config_path: Option<PathBuf>,
    pub(super) swap: Option<Arc<dyn crate::swap_api::SwapJobs>>,
}

/// Per-connection state that outlives one request.
struct Conn {
    reader: BufReader<ReadHalf>,
    stopping: watch::Receiver<bool>,
    /// Per-connection negotiated protocol. `None` until the client sends
    /// `hello` — keeps the legacy strict-match check in force for clients
    /// that never opt into negotiation. Once set, the daemon honors the
    /// pinned version on every subsequent message on this connection,
    /// including the streaming pump.
    negotiated: Option<u32>,
    collaboration_actor: CollaborationConnectionActor,
}

/// Per-request state a handler reports back to the loop.
struct Turn {
    /// Which message kind we just dispatched, so the timing line can
    /// include it as a structured field. Initialised to a sentinel that
    /// every handler overwrites, so an added arm that forgets to label
    /// itself shows up as `dispatch_unknown` in logs rather than
    /// mis-attributing the timing.
    kind: &'static str,
    pending_audit: Option<crate::collaboration_audit::CollaborationAuditEntry>,
    response_view: collaboration::CollaborationReadView,
    /// The request's own `protocol` field.
    protocol: u32,
}

/// What a request handler asks the loop to do next.
#[allow(clippy::large_enum_variant)] // short-lived, moved straight into the write path like `Response` always was
enum Flow {
    Respond(Response),
    /// Close the connection without a response.
    Close,
}

#[allow(clippy::too_many_lines)] // the read / route / audit / write loop
#[tracing::instrument(level = "debug", skip(stream, ctx), fields(config_path = ?ctx.config_path))]
pub(super) async fn handle(
    stream: Stream,
    ctx: ServerCtx,
    stopping: watch::Receiver<bool>,
) -> Result<(), RuntimeError> {
    let collaboration_actor = observe_collaboration_actor(&stream);
    let (reader, mut writer) = stream.into_split();
    let mut conn = Conn {
        reader: BufReader::new(reader),
        stopping,
        negotiated: None,
        collaboration_actor,
    };
    let mut line = String::new();

    loop {
        line.clear();
        conn.collaboration_actor.request_panes = None;
        // Bound the wait for the *next* request line. A client that connects
        // and then neither sends a complete request nor closes its half would
        // otherwise park this handler — and pin its fd — forever. A live
        // persistent client simply reconnects; a half-open one can't leak.
        // (A `Subscribe` connection never reaches a second iteration: it hands
        // off to `stream_transitions` and returns, so streams are unaffected.)
        let deadline = tokio::time::Instant::now() + IDLE_CONN_TIMEOUT;
        // fill_buf is cancellation-safe. Once any bytes of a request arrive,
        // finish reading/processing it under the normal drain budget instead
        // of dropping a partially received ingest on shutdown.
        let Some(ready) = until_server_shutdown(
            &mut conn.stopping,
            tokio::time::timeout_at(deadline, async {
                conn.reader.fill_buf().await.map(|bytes| !bytes.is_empty())
            }),
        )
        .await
        else {
            return Ok(());
        };
        match ready {
            Ok(Ok(true)) => {}
            Ok(Err(error)) if !is_client_disconnect(&error) => return Err(error.into()),
            _ => return Ok(()),
        }
        let Ok(read_result) =
            tokio::time::timeout_at(deadline, read_limited_line(&mut conn.reader, &mut line)).await
        else {
            tracing::debug!("idle connection timed out; closing");
            return Ok(());
        };
        let n = match read_result {
            Ok(n) => n,
            Err(e) if e.is_client_disconnect() => return Ok(()),
            Err(e) => return Err(e),
        };
        if n == 0 {
            return Ok(());
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Per-message timer. Start after the read so we don't bake
        // client-side blocking time into the handler latency we're
        // trying to measure. `Instant::now()` is a vDSO call on Linux
        // — effectively free.
        let started = Instant::now();
        let mut turn = Turn {
            kind: "dispatch_unknown",
            pending_audit: None,
            response_view: collaboration::CollaborationReadView::Full,
            protocol: 0,
        };
        let mut resp = match serde_json::from_str::<Request>(trimmed) {
            // Strict-match only applies in the legacy regime, and only
            // for non-`hello` kinds. Once the client has sent `hello`,
            // the negotiated version governs and per-message `protocol`
            // fields are advisory. `hello` itself carries the requested
            // version and is checked inside its own arm.
            Ok(req)
                if conn.negotiated.is_none()
                    && !matches!(req.body, RequestBody::Hello { .. })
                    && req.protocol != 0
                    && req.protocol != PROTOCOL_VERSION =>
            {
                turn.kind = "protocol_mismatch";
                Response::err(format!(
                    "protocol mismatch: server={PROTOCOL_VERSION} client={}",
                    req.protocol
                ))
            }
            Ok(req) => {
                turn.protocol = req.protocol;
                // Global Ask has one owner. Entry hosts forward without
                // replaying local history.
                if let Some(ask_kind) = ask_ops::shared_kind(&req.body) {
                    if let Some(bytes) =
                        ask_ops::forward_shared(&ctx, conn.negotiated, ask_kind, trimmed).await?
                    {
                        if !write_line_or_closed(&mut writer, &bytes).await? {
                            return Ok(());
                        }
                        continue;
                    }
                }
                if streams::takes_over(&req.body) {
                    return streams::takeover(&ctx, conn, writer, started, req.body).await;
                }
                match route(&ctx, &mut conn, &mut turn, req.body).await? {
                    Flow::Respond(response) => response,
                    Flow::Close => return Ok(()),
                }
            }
            Err(e) => {
                turn.kind = "parse_error";
                Response::err(format!("bad request: {e}"))
            }
        };
        let Turn {
            kind,
            pending_audit,
            response_view,
            ..
        } = turn;
        let negotiated = conn.negotiated;

        if let Some(request) = &mut resp.collaboration_request {
            response_view.apply(request);
        }
        let bytes = encode_line(&resp, negotiated.unwrap_or(PROTOCOL_VERSION))?;
        if let Some(mut entry) = pending_audit {
            entry.response_bytes = Some(bytes.len());
            entry.request_bytes = Some(n);
            if negotiated.unwrap_or(PROTOCOL_VERSION) == PROTOCOL_VERSION {
                let body = response_cost::measure(&resp);
                entry.response_body_bytes = Some(body.raw);
                entry.response_body_json_bytes = Some(body.json);
                entry.response_unique_body_bytes = Some(body.unique);
                entry.response_duplicate_body_bytes = Some(body.raw.saturating_sub(body.unique));
                entry.response_extra_unique_bytes = Some(bytes.len().saturating_sub(body.unique));
                entry.response_extra_bytes = Some(bytes.len().saturating_sub(body.raw));
                entry.response_structure_bytes = Some(bytes.len().saturating_sub(body.json));
                entry.response_escape_bytes = Some(body.json.saturating_sub(body.raw));
            }
            entry.conditional_unchanged = resp
                .collaboration_mailbox_snapshot
                .as_ref()
                .map(|s| s.unchanged);
            if entry.read_view.is_none() {
                entry.read_view = Some(response_view);
            }
            entry.elapsed_us =
                Some(u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX));
            ctx.collaboration_audit.append(entry).await;
        }
        if !write_line_or_closed(&mut writer, &bytes).await? {
            return Ok(());
        }

        // Per-message timing. `debug!` so it's filtered out by default
        // (production: `info`); the field-style call defers any
        // formatting until the subscriber actually wants the line.
        tracing::debug!(
            elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
            kind,
            ok = resp.ok,
            "ipc.handle",
        );
    }
}

/// Hand a request to the family that owns it.
#[allow(clippy::too_many_lines)] // the routing table
async fn route(
    ctx: &ServerCtx,
    conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    match body {
        body @ (RequestBody::Hello { .. }
        | RequestBody::Health
        | RequestBody::Stop
        | RequestBody::Restart) => Box::pin(daemon::handle(ctx, conn, turn, body)).await,
        body @ (RequestBody::Ingest { .. }
        | RequestBody::Register { .. }
        | RequestBody::ForgetAgent { .. }
        | RequestBody::Prune { .. }
        | RequestBody::InvalidateAgentDirectory
        | RequestBody::AgentFind { .. }
        | RequestBody::Snapshot { .. }
        | RequestBody::TurnEvidence { .. }
        | RequestBody::ByPane { .. }
        | RequestBody::BySession { .. }
        | RequestBody::BySurface { .. }
        | RequestBody::RecentPrompts { .. }
        | RequestBody::BackendPaneSnapshot { .. }
        | RequestBody::RefreshPaneLocations { .. }
        | RequestBody::SendPrompt { .. }
        | RequestBody::ExpectPrompt { .. }
        | RequestBody::ForgetExpectedPrompt { .. }
        | RequestBody::Capture { .. }) => Box::pin(agents::handle(ctx, conn, turn, body)).await,
        body @ (RequestBody::ReviewCreate { .. }
        | RequestBody::ReviewGet { .. }
        | RequestBody::ReviewList { .. }
        | RequestBody::ReviewSummary
        | RequestBody::ReviewDispose { .. }
        | RequestBody::ReviewRecord { .. }
        | RequestBody::DecisionQueue { .. }
        | RequestBody::DecisionHistory { .. }) => {
            Box::pin(decisions::handle(ctx, conn, turn, body)).await
        }
        body @ (RequestBody::PipelineRuns
        | RequestBody::PipelineRegister { .. }
        | RequestBody::PipelineDone { .. }
        | RequestBody::PipelineInvalidate { .. }
        | RequestBody::PipelineClaim { .. }
        | RequestBody::PipelineReport { .. }) => {
            Box::pin(pipelines::handle(ctx, conn, turn, body)).await
        }
        body @ (RequestBody::WorkUp { .. }
        | RequestBody::WorkUpStatus { .. }
        | RequestBody::WorkCommand { .. }
        | RequestBody::AgentStart { .. }
        | RequestBody::MuxSnapshotList
        | RequestBody::MuxSnapshotSave
        | RequestBody::MuxSnapshotPlan { .. }
        | RequestBody::MuxSnapshotDelete { .. }
        | RequestBody::MuxSnapshotRestore { .. }
        | RequestBody::MuxSnapshotRestoreStatus { .. }
        | RequestBody::WorkCompose { .. }) => {
            Box::pin(work_ops::handle(ctx, conn, turn, body)).await
        }
        body @ (RequestBody::SwapJobStart { .. }
        | RequestBody::SwapJobRetry { .. }
        | RequestBody::SwapAccounts
        | RequestBody::SwapHealth
        | RequestBody::SwapJobCancel { .. }
        | RequestBody::SwapJobStatus { .. }
        | RequestBody::SwapPlan { .. }) => Box::pin(swap_jobs::handle(ctx, conn, turn, body)).await,
        body @ (RequestBody::AskOneShot { .. }
        | RequestBody::AskSend { .. }
        | RequestBody::AskSendNew { .. }
        | RequestBody::AskStatus { .. }
        | RequestBody::AskList { .. }
        | RequestBody::AskConversationList { .. }
        | RequestBody::AskConversationSelect { .. }
        | RequestBody::AskAgent { .. }
        | RequestBody::AskReset { .. }
        | RequestBody::AskClear { .. }
        | RequestBody::AskDelete { .. }
        | RequestBody::AskProviders { .. }
        | RequestBody::AskProviderConfigure { .. }
        | RequestBody::AskProviderAdd { .. }
        | RequestBody::AskProviderDefault { .. }
        | RequestBody::AskProviderRemove { .. }) => {
            Box::pin(ask_ops::handle(ctx, conn, turn, body)).await
        }
        body @ (RequestBody::FleetSnapshot { .. }
        | RequestBody::FleetCommand { .. }
        | RequestBody::FleetWaitReply { .. }) => {
            Box::pin(fleet_ops::handle(ctx, conn, turn, body)).await
        }
        body @ (RequestBody::CollaborationContext { .. }
        | RequestBody::CollaborationIssueHandle { .. }
        | RequestBody::CollaborationSetIdentity { .. }
        | RequestBody::CollaborationSend { .. }
        | RequestBody::CollaborationTurnBoundaryOffer { .. }
        | RequestBody::CollaborationInbox { .. }
        | RequestBody::CollaborationPurpose { .. }
        | RequestBody::CollaborationPromotePurpose { .. }
        | RequestBody::CollaborationOverview { .. }
        | RequestBody::CollaborationOpenCounts { .. }
        | RequestBody::CollaborationMailboxSnapshot { .. }
        | RequestBody::CollaborationList { .. }
        | RequestBody::CollaborationUpdate { .. }
        | RequestBody::CollaborationReply { .. }
        | RequestBody::CollaborationGet { .. }
        | RequestBody::CollaborationPeek { .. }
        | RequestBody::CollaborationWait { .. }
        | RequestBody::CollaborationHealth
        | RequestBody::CollaborationExpire { .. }
        | RequestBody::CollaborationExpireFiltered { .. }
        | RequestBody::CollaborationExpireSummary { .. }
        | RequestBody::CollaborationRetire { .. }
        | RequestBody::CollaborationCancel { .. }
        | RequestBody::CollaborationWithdrawReplyWakes { .. }) => {
            Box::pin(mailbox::handle(ctx, conn, turn, body)).await
        }
        body @ (RequestBody::ConfigRead { .. }
        | RequestBody::ConfigOrchestrationRead { .. }
        | RequestBody::ConfigOrchestrationWrite { .. }
        | RequestBody::ConfigLaunchRead { .. }
        | RequestBody::ConfigLaunchWrite { .. }
        | RequestBody::ConfigWrite { .. }) => {
            Box::pin(config_ops::handle(ctx, conn, turn, body)).await
        }
        body @ (RequestBody::AutomationList { .. }
        | RequestBody::AutomationLog { .. }
        | RequestBody::AutomationSetEnabled { .. }
        | RequestBody::AutomationPause { .. }
        | RequestBody::AutomationSetRule { .. }
        | RequestBody::AutomationRemoveRule { .. }
        | RequestBody::AutomationTest { .. }
        | RequestBody::AutomationJudgeTest { .. }) => {
            Box::pin(automation_ops::handle(ctx, conn, turn, body)).await
        }
        body @ (RequestBody::WorkerDrainPlan { .. }
        | RequestBody::WorkerDrainAcquire { .. }
        | RequestBody::WorkerDrainRenew { .. }
        | RequestBody::WorkerDrainStop { .. }
        | RequestBody::WorkerDrainFinish { .. }
        | RequestBody::WorkerDrains { .. }
        | RequestBody::CollaborationWorkerSpawns
        | RequestBody::CollaborationWorkerSpawnRelease { .. }
        | RequestBody::CollaborationWorkerAcquire { .. }
        | RequestBody::CollaborationWorkerSpawn { .. }
        | RequestBody::CollaborationWorkerLaunch { .. }
        | RequestBody::CollaborationWorkers { .. }) => {
            Box::pin(workers::handle(ctx, conn, turn, body)).await
        }
        body @ (RequestBody::SpawnSession { .. }
        | RequestBody::ListSessions
        | RequestBody::CaptureSession { .. }
        | RequestBody::ReadSession { .. }
        | RequestBody::ReadSessionWait { .. }
        | RequestBody::WriteSession { .. }
        | RequestBody::WriteSessionBytes { .. }
        | RequestBody::ResizeSession { .. }
        | RequestBody::SetSessionAttached { .. }
        | RequestBody::TerminateSession { .. }) => {
            Box::pin(sessions::handle(ctx, conn, turn, body)).await
        }
        body @ (RequestBody::PipelineSubscribe
        | RequestBody::FleetSubscribe { .. }
        | RequestBody::AskSubscribe
        | RequestBody::CollaborationSubscribe { .. }
        | RequestBody::SubscribeAgentChanges
        | RequestBody::Subscribe { .. }) => {
            unreachable!("dispatch routed stream request {body:?} as a single request")
        }
    }
}
