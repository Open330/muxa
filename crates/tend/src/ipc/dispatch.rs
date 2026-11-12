use super::*;

#[allow(clippy::too_many_arguments, clippy::too_many_lines)] // IPC dispatch table and its shared daemon state
#[tracing::instrument(
    level = "debug",
    skip(
        stream,
        store,
        backend,
        backends,
        sessions,
        collaboration,
        collaboration_audit,
        ask,
        agent_directory,
        automation,
        restart,
        fleet,
        pipeline_runs,
        reviews,
        work_up,
        mux_snapshots,
        swap
    )
)]
pub(super) async fn handle(
    stream: Stream,
    store: SharedStore,
    backend: SharedBackend,
    backends: Vec<SharedBackend>,
    sessions: SharedSessionBackend,
    collaboration: Arc<CollaborationStore>,
    collaboration_audit: Arc<CollaborationAuditLog>,
    ask: Arc<AskStore>,
    agent_directory: Arc<crate::agent_directory::AgentDirectory>,
    automation: Arc<AutomationStore>,
    restart: Option<Arc<RestartController>>,
    fleet: Option<FleetRuntime>,
    pipeline_runs: Arc<PipelineRunStore>,
    reviews: crate::review::ReviewStore,
    work_up: Arc<WorkUpManager>,
    mux_snapshots: Arc<crate::mux_snapshot_control::MuxSnapshotControl>,
    config_path: Option<PathBuf>,
    swap: Option<Arc<dyn crate::swap_service::SwapJobs>>,
    mut stopping: watch::Receiver<bool>,
) -> Result<(), RuntimeError> {
    let mut collaboration_actor = observe_collaboration_actor(&stream);
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    // Per-connection negotiated protocol. `None` until the client sends
    // `hello` — keeps the legacy strict-match check in force for clients
    // that never opt into negotiation. Once set, the daemon honors the
    // pinned version on every subsequent message on this connection,
    // including the streaming pump.
    let mut negotiated: Option<u32> = None;

    loop {
        line.clear();
        collaboration_actor.request_panes = None;
        let mut pending_audit = None;
        let mut response_view = collaboration::CollaborationReadView::Full;
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
            &mut stopping,
            tokio::time::timeout_at(deadline, async {
                reader.fill_buf().await.map(|bytes| !bytes.is_empty())
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
            tokio::time::timeout_at(deadline, read_limited_line(&mut reader, &mut line)).await
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
        // Track which message kind we just dispatched so the timing
        // line below can include it as a structured field. Initialised
        // to a sentinel that every match arm overwrites — the
        // assignment is preserved deliberately so an added arm that
        // forgets to label itself shows up as `dispatch_unknown` in
        // logs rather than mis-attributing the timing.
        #[allow(unused_assignments)]
        let mut kind: &'static str = "dispatch_unknown";
        // Global Ask has one owner. Entry hosts forward without replaying local history.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed).and_then(|value| {
            let request: Request = serde_json::from_value(value.clone())?;
            if negotiated.is_none() && request.protocol != 0 && request.protocol != PROTOCOL_VERSION
            {
                return Err(serde::de::Error::custom("protocol mismatch"));
            }
            Ok(value)
        }) {
            let ask_kind = value["kind"].as_str().unwrap_or("");
            if crate::orchestration::shared_ask_kind(ask_kind) || ask_kind == "ask_subscribe" {
                let policy = if let Ok(config) = config_path
                    .as_deref()
                    .map(|p| crate::Config::load_or_default(Some(p)))
                    .transpose()
                {
                    config.map(|c| c.orchestration)
                } else {
                    let response = Response::err("cannot load coordinator routing configuration; refusing to fall back to a local Ask conversation");
                    let bytes = encode_line(&response, negotiated.unwrap_or(PROTOCOL_VERSION))?;
                    if !write_line_or_closed(&mut writer, &bytes).await? {
                        return Ok(());
                    }
                    continue;
                };
                if let Some(host) = policy
                    .filter(|p| p.enabled)
                    .and_then(|p| p.coordinator)
                    .filter(|h| h != "local")
                {
                    let result = if ask_kind == "ask_subscribe" {
                        Err("shared Ask streaming requires a direct coordinator connection; use ask_list for snapshots".to_string())
                    } else {
                        work_up
                            .command(
                                Some(host),
                                vec!["work".into(), "fleet-ask".into()],
                                Some(trimmed.into()),
                            )
                            .await
                            .and_then(|o| {
                                if o.exit_code == 0 {
                                    serde_json::from_str::<serde_json::Value>(&o.stdout)
                                        .map_err(|e| e.to_string())
                                } else {
                                    Err(o.stderr)
                                }
                            })
                    };
                    let response = result.unwrap_or_else(|error| serde_json::json!({"ok":false,"protocol":PROTOCOL_VERSION,"error":error}));
                    let bytes = encode_line(&response, negotiated.unwrap_or(PROTOCOL_VERSION))?;
                    if !write_line_or_closed(&mut writer, &bytes).await? {
                        return Ok(());
                    }
                    continue;
                }
            }
        }
        let mut resp = match serde_json::from_str::<Request>(trimmed) {
            // Strict-match only applies in the legacy regime, and only
            // for non-`hello` kinds. Once the client has sent `hello`,
            // the negotiated version governs and per-message `protocol`
            // fields are advisory. `hello` itself carries the requested
            // version and is checked inside its own arm.
            Ok(req)
                if negotiated.is_none()
                    && !matches!(req.body, RequestBody::Hello { .. })
                    && req.protocol != 0
                    && req.protocol != PROTOCOL_VERSION =>
            {
                kind = "protocol_mismatch";
                Response::err(format!(
                    "protocol mismatch: server={PROTOCOL_VERSION} client={}",
                    req.protocol
                ))
            }
            Ok(req) => match req.body {
                RequestBody::Hello { client } => {
                    kind = "hello";
                    let requested = if req.protocol == 0 {
                        PROTOCOL_VERSION
                    } else {
                        req.protocol
                    };
                    if (MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&requested) {
                        negotiated = Some(requested);
                        if let Some(client_kind) = client
                            .as_deref()
                            .map(CollaborationClientKind::from_hello_label)
                            .filter(|kind| *kind != CollaborationClientKind::Unknown)
                        {
                            collaboration_actor.client_kind = client_kind;
                        }
                        tracing::debug!(
                            client = client.as_deref().unwrap_or("(unknown)"),
                            protocol = requested,
                            "hello"
                        );
                        let mut r = Response::hello(restart.as_deref());
                        r.collaboration_error =
                            collaboration.initialization_error().map(str::to_owned);
                        r.protocol = requested;
                        // Only a service that would take a job now is
                        // advertised; status stays served regardless.
                        if let Some(capabilities) = r.capabilities.as_mut() {
                            capabilities.push(SWAP_SERVICE_CAPABILITY);
                            capabilities.push(SWAP_HEALTH_CAPABILITY);
                            capabilities.push("swap_accounts_v1");
                            if swap.as_ref().is_some_and(|swap| swap.exists()) {
                                capabilities.push(SWAP_CANCEL_CAPABILITY);
                            }
                        }
                        if swap_healthy(swap.as_ref()).await {
                            if let Some(capabilities) = r.capabilities.as_mut() {
                                capabilities.push(SWAP_JOBS_CAPABILITY);
                                capabilities.push(SWAP_PLAN_CAPABILITY);
                                capabilities.push(SWAP_RETRY_CAPABILITY);
                            }
                        }
                        r
                    } else {
                        Response::err(format!(
                            "unsupported protocol: server supports [{MIN_PROTOCOL_VERSION},{PROTOCOL_VERSION}] client={requested}",
                        ))
                    }
                }
                RequestBody::Ingest {
                    mut event,
                    ask_entry,
                    launch_id,
                    ..
                } => {
                    if ask_entry.is_some() {
                        event.id_mut().pane = None;
                        event.id_mut().surface = None;
                    }
                    kind = "ingest";
                    // Drop only tmux events from servers outside the configured
                    // `TEND_TMUX_SOCKET` scope. Namespaced non-tmux panes have
                    // their own endpoint rules and must not be compared with a
                    // tmux socket path (notably cmux, which also retains a full
                    // Unix-socket endpoint). Ack it either way so the agent's
                    // hook never sees an error on its critical path.
                    let pane_host = event
                        .id()
                        .pane
                        .as_deref()
                        .and_then(crate::backend::pane_id_host_kind);
                    let in_scope = pane_host.is_some_and(|host| host != HostKind::Tmux)
                        || crate::tmux::scanner::event_tmux_socket_in_scope(
                            event.id().tmux_socket.as_deref(),
                        );
                    if in_scope {
                        tracing::debug!(?event, "ingest");
                        store.apply_with_ask(&event, ask_entry.as_deref()).await;
                        if ask_entry.is_none() {
                            store
                                .observe_launch_claim(&event, launch_id.as_deref())
                                .await;
                        }
                        if ask_entry.is_none() {
                            if let Err(error) = collaboration.observe_worker_hook(event.id()).await
                            {
                                tracing::warn!(%error, "worker hook binding failed; session stays protected");
                            }
                        }
                        // A Codex thread behind the shared app-server arrives
                        // paneless; bind it now, before a sibling started in
                        // the same cwd makes the evidence ambiguous.
                        // Only a session start or prompt can introduce or
                        // re-evidence a thread; tool hooks would rescan per call.
                        let id = event.id();
                        if id.kind == crate::AgentKind::Codex
                            && id.pane.is_none()
                            && matches!(
                                event,
                                AgentEvent::Started { .. } | AgentEvent::PromptSubmitted { .. }
                            )
                            && store.is_unplaced_codex(&id.session_id).await
                        {
                            crate::codex_binding::schedule_bind_paneless_codex(
                                store.clone(),
                                backends.clone(),
                            );
                        }
                    } else {
                        tracing::debug!(
                            socket = event.id().tmux_socket.as_deref(),
                            "ingest skipped: tmux socket outside TEND_TMUX_SOCKET scope",
                        );
                    }
                    Response::ok()
                }
                RequestBody::Register {
                    name,
                    pid,
                    cwd,
                    pane,
                    command,
                } => {
                    kind = "register";
                    match store.register_task(name, pid, cwd, pane, command).await {
                        Ok(session_id) => {
                            tracing::debug!(session_id, ?pid, "register task");
                            Response::ok()
                        }
                        Err(e) => Response::err(e),
                    }
                }
                RequestBody::ForgetAgent { session_id } => {
                    kind = "forget_agent";
                    Response::with_pruned(usize::from(store.forget_agent(&session_id).await))
                }
                RequestBody::Prune { max_age_secs } => {
                    kind = "prune";
                    store
                        .reconcile_native_surfaces(|id| sessions.session_liveness(id))
                        .await;
                    let cutoff = time::OffsetDateTime::now_utc()
                        - std::time::Duration::from_secs(max_age_secs);
                    let pruned = store.prune_orphans(cutoff).await;
                    tracing::debug!(pruned, max_age_secs, "prune orphans");
                    Response::with_pruned(pruned)
                }
                RequestBody::InvalidateAgentDirectory => {
                    kind = "invalidate_agent_directory";
                    agent_directory.invalidate();
                    Response::ok()
                }
                RequestBody::AgentFind {
                    query,
                    limit,
                    exclude_sessions,
                    refresh,
                } => {
                    kind = "agent_find";
                    match agent_directory
                        .find(
                            &store,
                            &ask,
                            &backends,
                            &query,
                            limit,
                            &exclude_sessions,
                            refresh,
                        )
                        .await
                    {
                        Ok(result) => {
                            let mut response = Response::ok();
                            response.agent_locations = Some(result);
                            response
                        }
                        Err(error) => Response::err(error),
                    }
                }
                RequestBody::Snapshot { launch_identity } => {
                    kind = "snapshot";
                    let agents = store.snapshot().await;
                    let identities = if launch_identity {
                        Some(Arc::new(store.launch_identity_snapshot(&agents).await))
                    } else {
                        None
                    };
                    let mut response = Response::with_agents(agents);
                    response.launch_identities = identities;
                    response
                }
                RequestBody::TurnEvidence { session_id, since } => {
                    kind = "turn_evidence";
                    // Indexing records can take a while the first time; never
                    // longer than this, and never on the async workers.
                    let read = tokio::task::spawn_blocking(move || {
                        let store = crate::agent_records::RecordStore::local(&[])?;
                        crate::agent_records::evidence::turn_evidence(&store, &session_id, &since)
                    });
                    match tokio::time::timeout(std::time::Duration::from_secs(15), read).await {
                        Ok(Ok(Ok(evidence))) => Response {
                            turn_evidence: Some(Box::new(evidence)),
                            ..Response::ok()
                        },
                        Ok(Ok(Err(error))) => {
                            Response::err(format!("could not read records: {error}"))
                        }
                        Ok(Err(error)) => Response::err(format!("record read failed: {error}")),
                        Err(_) => Response::err("reading records took too long; try again"),
                    }
                }
                body @ (RequestBody::ReviewCreate { .. }
                | RequestBody::ReviewGet { .. }
                | RequestBody::ReviewList { .. }
                | RequestBody::ReviewSummary
                | RequestBody::ReviewDispose { .. }
                | RequestBody::ReviewRecord { .. }) => {
                    // One boxed arm keeps this dispatch frame from growing.
                    let (name, response) = Box::pin(review_response(&reviews, body)).await;
                    kind = name;
                    response
                }
                RequestBody::PipelineRuns => {
                    kind = "pipeline_runs";
                    Response::with_pipeline_runs(pipeline_runs.list().await)
                }
                RequestBody::PipelineSubscribe => {
                    let changes = pipeline_runs.subscribe();
                    let stream_proto = negotiated.unwrap_or(PROTOCOL_VERSION);
                    let ack_bytes = encode_line(&Response::ok(), stream_proto)?;
                    if !write_subscription_ack(&mut writer, &ack_bytes, &mut stopping).await? {
                        return Ok(());
                    }
                    return until_server_shutdown(
                        &mut stopping,
                        stream_revision_updates(writer, changes, stream_proto),
                    )
                    .await
                    .unwrap_or(Ok(()));
                }
                RequestBody::WorkUp { request } => {
                    kind = "work_up";
                    match work_up.start(request).await {
                        Ok(operation) => Response::with_work_operation(operation),
                        Err(error) => Response::err(error),
                    }
                }
                RequestBody::WorkUpStatus { operation_id } => {
                    kind = "work_up_status";
                    match work_up.status(&operation_id).await {
                        Some(operation) => Response::with_work_operation(operation),
                        None => {
                            Response::err(format!("Work operation {operation_id:?} was not found"))
                        }
                    }
                }
                RequestBody::SwapJobStart {
                    request_key,
                    overrides,
                } => {
                    kind = "swap_job_start";
                    swap_call(swap.as_ref(), move |swap| {
                        swap.submit(&request_key, &overrides)
                            .and_then(|accepted| to_swap_value(&accepted))
                    })
                    .await
                }
                RequestBody::SwapJobRetry {
                    request_key,
                    parent,
                    overrides,
                } => {
                    kind = "swap_job_retry";
                    swap_call(swap.as_ref(), move |swap| {
                        swap.retry(&request_key, &parent, &overrides)
                            .and_then(|accepted| to_swap_value(&accepted))
                    })
                    .await
                }
                RequestBody::SwapAccounts => {
                    kind = "swap_accounts";
                    let mut response = swap_call(swap.as_ref(), move |swap| {
                        swap.accounts()
                            .and_then(|accounts| to_swap_value(&accounts))
                    })
                    .await;
                    response.swap_accounts = response.swap.take();
                    response
                }
                RequestBody::SwapHealth => {
                    kind = "swap_health";
                    swap_health_response(swap.as_ref()).await
                }
                RequestBody::SwapJobCancel { job } => {
                    kind = "swap_job_cancel";
                    swap_call(swap.as_ref(), move |swap| {
                        swap.cancel(&job).and_then(|view| to_swap_value(&view))
                    })
                    .await
                }
                RequestBody::SwapJobStatus { job } => {
                    kind = "swap_job_status";
                    swap_call(swap.as_ref(), move |swap| {
                        swap.status(&job).and_then(|view| to_swap_value(&view))
                    })
                    .await
                }
                RequestBody::SwapPlan { overrides } => {
                    kind = "swap_plan";
                    swap_call(swap.as_ref(), move |swap| {
                        swap.plan(&overrides).and_then(|plan| to_swap_value(&plan))
                    })
                    .await
                }
                RequestBody::WorkCommand { host, args, stdin } => {
                    kind = "work_command";
                    match work_up.command(host, args, stdin).await {
                        Ok(output) => Response::with_work_command(output),
                        Err(error) => Response::err(error),
                    }
                }
                // WS-B: new agent
                RequestBody::AgentStart { request } => {
                    kind = "agent_start";
                    match work_up.agent_start(request).await {
                        Ok(result) => Response::with_agent_start(result),
                        Err(error) => Response::err(error),
                    }
                }
                // WS-F: snapshot
                RequestBody::MuxSnapshotList => {
                    kind = "mux_snapshot_list";
                    mux_snapshot_response(mux_snapshots.list().await)
                }
                RequestBody::MuxSnapshotSave => {
                    kind = "mux_snapshot_save";
                    mux_snapshot_response(mux_snapshots.save().await)
                }
                RequestBody::MuxSnapshotPlan { id, only_missing } => {
                    kind = "mux_snapshot_plan";
                    mux_snapshot_response(mux_snapshots.plan(&id, only_missing).await)
                }
                RequestBody::MuxSnapshotDelete { id } => {
                    kind = "mux_snapshot_delete";
                    mux_snapshot_response(mux_snapshots.delete(&id).await)
                }
                RequestBody::MuxSnapshotRestore {
                    id,
                    layout_only,
                    only_missing,
                    continue_text,
                } => {
                    kind = "mux_snapshot_restore";
                    match mux_snapshots
                        .restore(&id, layout_only, only_missing, continue_text.as_deref())
                        .await
                    {
                        Ok(operation) => Response::with_mux_snapshot_operation(operation),
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::MuxSnapshotRestoreStatus { operation_id } => {
                    kind = "mux_snapshot_restore_status";
                    match mux_snapshots.status(&operation_id).await {
                        Some(operation) => Response::with_mux_snapshot_operation(operation),
                        None => Response::err(format!(
                            "snapshot restore {operation_id:?} was not found"
                        )),
                    }
                }
                // END WS-F: snapshot
                RequestBody::WorkCompose {
                    description,
                    agent,
                    current,
                    credential,
                } => {
                    kind = "work_compose";
                    let request = WorkComposeRequest {
                        description,
                        agent,
                        current,
                    };
                    let installed = work_compose::installed_programs();
                    // The drafting turn is read-only whatever `[ask]` says:
                    // a model describing a pipeline must not edit files.
                    let drafter = Arc::clone(&ask);
                    let agent = request.agent.clone();
                    let result = work_compose::compose(&request, &installed, move |prompt| {
                        let drafter = Arc::clone(&drafter);
                        let agent = agent.clone();
                        let credential = credential.clone();
                        async move {
                            drafter
                                .one_shot_for(
                                    agent.as_deref(),
                                    &prompt,
                                    crate::config::AskPermissionMode::Plan,
                                    credential,
                                )
                                .await
                                .map_err(|error| error.to_string())
                        }
                    })
                    .await;
                    match result {
                        Ok(output) => Response::with_work_compose(output),
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::AskOneShot {
                    prompt,
                    agent,
                    credential,
                } => {
                    kind = "ask_one_shot";
                    // Read-only whatever `[ask]` says: organizing results or
                    // drafting replies must not edit files. Bounded so one
                    // request cannot hand a provider an unbounded prompt.
                    if prompt.len() > ASK_ONE_SHOT_MAX_PROMPT_BYTES {
                        Response::err(format!(
                            "ask_one_shot prompt exceeds {ASK_ONE_SHOT_MAX_PROMPT_BYTES} bytes"
                        ))
                    } else {
                        match ask
                            .one_shot_for(
                                agent.as_deref(),
                                &prompt,
                                crate::config::AskPermissionMode::Plan,
                                credential,
                            )
                            .await
                        {
                            Ok(answer) => Response::with_ask_one_shot(answer.text),
                            Err(error) => Response::err(error.to_string()),
                        }
                    }
                }
                RequestBody::PipelineRegister { registration } => {
                    kind = "pipeline_register";
                    match pipeline_runs.register(registration).await {
                        Ok(run) => Response::with_pipeline_run(run),
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::PipelineDone {
                    identity,
                    alias,
                    generation,
                } => {
                    kind = "pipeline_done";
                    match pipeline_runs.done(&identity, &alias, generation).await {
                        Ok(run) => Response::with_pipeline_run(run),
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::PipelineInvalidate {
                    identity,
                    alias,
                    generation,
                } => {
                    kind = "pipeline_invalidate";
                    match pipeline_runs
                        .invalidate(&identity, &alias, generation)
                        .await
                    {
                        Ok(run) => Response::with_pipeline_run(run),
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::PipelineClaim {
                    identity,
                    generation,
                } => {
                    kind = "pipeline_claim";
                    match pipeline_runs.claim_ready(&identity, generation).await {
                        Ok(claims) => Response::with_pipeline_claims(claims),
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::PipelineReport {
                    identity,
                    alias,
                    generation,
                    status,
                    pane,
                    error,
                    window_id,
                } => {
                    kind = "pipeline_report";
                    match pipeline_runs
                        .report(
                            &identity, &alias, generation, status, pane, error, window_id,
                        )
                        .await
                    {
                        Ok(run) => Response::with_pipeline_run(run),
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::FleetSnapshot { selector } => {
                    kind = "fleet_snapshot";
                    match &fleet {
                        Some(fleet) => match selector
                            .as_deref()
                            .map(str::parse::<LabelSelector>)
                            .transpose()
                        {
                            Ok(selector) => Response::with_fleet(
                                fleet.store.snapshot_selected(selector.as_ref()).await,
                            ),
                            Err(error) => Response::err(format!("invalid label selector: {error}")),
                        },
                        None => Response::err("fleet is not enabled in tendd"),
                    }
                }
                RequestBody::FleetSubscribe { selector } => {
                    kind = "fleet_subscribe";
                    let Some(fleet) = &fleet else {
                        let response = Response::err("fleet is not enabled in tendd");
                        let bytes = encode_line(&response, negotiated.unwrap_or(PROTOCOL_VERSION))?;
                        let _ = write_line_or_closed(&mut writer, &bytes).await?;
                        return Ok(());
                    };
                    let selector = match selector
                        .as_deref()
                        .map(str::parse::<LabelSelector>)
                        .transpose()
                    {
                        Ok(selector) => selector,
                        Err(error) => {
                            let response =
                                Response::err(format!("invalid label selector: {error}"));
                            let bytes =
                                encode_line(&response, negotiated.unwrap_or(PROTOCOL_VERSION))?;
                            let _ = write_line_or_closed(&mut writer, &bytes).await?;
                            return Ok(());
                        }
                    };
                    // Subscribe before observing the initial membership and
                    // before ACK. The client fetches a fresh snapshot after
                    // ACK, so every mutation is represented either there or
                    // in this already-live receiver (duplicates are harmless).
                    let updates = fleet.store.subscribe();
                    let visible_hosts = fleet
                        .store
                        .snapshot_selected(selector.as_ref())
                        .await
                        .hosts
                        .into_iter()
                        .map(|host| host.alias)
                        .collect::<HashSet<_>>();
                    let stream_proto = negotiated.unwrap_or(PROTOCOL_VERSION);
                    let ack_bytes = encode_line(&Response::ok(), stream_proto)?;
                    if !write_subscription_ack(&mut writer, &ack_bytes, &mut stopping).await? {
                        return Ok(());
                    }
                    tracing::debug!(
                        elapsed_us =
                            u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                        kind,
                        "ipc.handle (fleet stream takeover)",
                    );
                    return until_server_shutdown(
                        &mut stopping,
                        stream_fleet_updates(
                            writer,
                            fleet.store.clone(),
                            updates,
                            stream_proto,
                            selector,
                            visible_hosts,
                        ),
                    )
                    .await
                    .unwrap_or(Ok(()));
                }
                RequestBody::FleetCommand { host, operation } => {
                    kind = "fleet_command";
                    let invalid = match &operation {
                        FleetOperation::CollaborationSend { request, .. } => {
                            request.validate_fleet_contract().err()
                        }
                        _ => None,
                    };
                    if let Some(error) = invalid {
                        Response::err(error)
                    } else {
                        match &fleet {
                            Some(fleet) => match fleet
                                .execute(host, operation, Duration::from_secs(20))
                                .await
                            {
                                Ok(result) => Response::with_fleet_result(result),
                                Err(error) => Response::err(error),
                            },
                            None => Response::err("fleet is not enabled in tendd"),
                        }
                    }
                }
                RequestBody::FleetWaitReply {
                    host,
                    pane,
                    request_id,
                    timeout_secs,
                    after_update,
                } => {
                    kind = "fleet_wait_reply";
                    let response = async {
                        match &fleet {
                            Some(fleet) => match fleet
                                .wait_for_reply(
                                    host,
                                    pane,
                                    request_id,
                                    Duration::from_secs(
                                        timeout_secs.clamp(1, MAX_COLLABORATION_WAIT_SECS),
                                    ),
                                    after_update,
                                )
                                .await
                            {
                                Ok(Some(request)) => Response::with_collaboration_request(request),
                                Ok(None) => Response::ok(),
                                Err(error) => Response::err(error),
                            },
                            None => Response::err("fleet is not enabled in tendd"),
                        }
                    };
                    // A disconnected waiter must release its shared subscription promptly.
                    tokio::select! {
                        response = response => response,
                        _ = reader.fill_buf() => return Ok(()),
                        _ = stopping.wait_for(|value| *value) => return Ok(()),
                    }
                }
                RequestBody::ByPane { pane } => {
                    kind = "by_pane";
                    Response::with_agents(store.by_pane(&pane).await)
                }
                RequestBody::BySession { session_id } => {
                    kind = "by_session";
                    let v = store
                        .by_session(&session_id)
                        .await
                        .into_iter()
                        .collect::<Vec<_>>();
                    Response::with_agents(v)
                }
                RequestBody::BySurface { surface_id } => {
                    kind = "by_surface";
                    Response::with_agents(store.by_surface(&surface_id).await)
                }
                RequestBody::RecentPrompts { pane, limit } => {
                    kind = "recent_prompts";
                    let prompts = store
                        .recent_prompts(pane.as_deref(), limit.unwrap_or(0))
                        .await;
                    Response::with_prompts(prompts)
                }
                RequestBody::Health => {
                    kind = "health";
                    let mut response = Response::health();
                    response.collaboration_error =
                        collaboration.initialization_error().map(str::to_owned);
                    response
                }
                RequestBody::Stop => {
                    kind = "stop";
                    match &restart {
                        Some(controller) => {
                            tracing::info!("stop requested over IPC");
                            controller.stop();
                            Response::ok()
                        }
                        None => Response::err(
                            "this server cannot stop itself (no lifecycle controller installed)",
                        ),
                    }
                }
                RequestBody::Restart => {
                    kind = "restart";
                    match match &restart {
                        Some(controller) => controller.request_checked_restart().await.map(Some),
                        None => Ok(None),
                    } {
                        Ok(Some(true)) => {
                            tracing::info!(
                                generation = restart.as_ref().unwrap().generation(),
                                "restart requested over IPC",
                            );
                            Response::ok()
                        }
                        Ok(Some(false)) => {
                            Response::err("daemon is already stopping; restart request refused")
                        }
                        Ok(None) => Response::err(
                            "this server cannot restart itself (no restart controller installed)",
                        ),
                        Err(reason) => Response::err(format!("restart refused: {reason}")),
                    }
                }
                RequestBody::BackendPaneSnapshot { panes } => {
                    kind = "backend_pane_snapshot";
                    let count = panes.len();
                    backend.ingest_pane_snapshot(panes);
                    agent_directory.invalidate();
                    tracing::debug!(panes = count, "backend_pane_snapshot");
                    Response::ok()
                }
                RequestBody::RefreshPaneLocations { pane } => {
                    kind = "refresh_pane_locations";
                    agent_directory.invalidate();
                    match resolve_backend(&backends, &pane) {
                        Err(error) => Response::err(format!("no active {error} backend")),
                        Ok(target) => {
                            let target = target.clone();
                            match tokio::task::spawn_blocking(move || target.observe_panes()).await
                            {
                                Ok(observation) => {
                                    store.refresh_pane_locations(&observation.panes).await;
                                    agent_directory.invalidate();
                                    Response::ok()
                                }
                                Err(error) => Response::err(error.to_string()),
                            }
                        }
                    }
                }
                RequestBody::SendPrompt {
                    pane,
                    socket,
                    session_id,
                    text,
                    submit,
                    origin,
                } => {
                    kind = "send_prompt";
                    match resolve_backend(&backends, &pane) {
                        // Known namespace, but no active backend observes it —
                        // refuse rather than mis-route keystrokes to another
                        // host (routing `herdr:` onto tmux would type into the
                        // wrong pane entirely).
                        Err(missing) => Response::err(format!(
                            "namespace unavailable: no active {missing} backend for pane {pane}",
                        )),
                        // Structured refusal, not a panic: the backend that
                        // owns this pane's namespace can't inject keystrokes
                        // (e.g. zellij).
                        Ok(target) if !target.caps().send_text => Response::err(format!(
                            "backend {} does not support send_text (pane {pane})",
                            target.kind(),
                        )),
                        Ok(target) => {
                            let target = target.clone();
                            // Check the registry after acquiring the input guard:
                            // waiting for an earlier action may replace the session.
                            let action_guard =
                                crate::prompt_delivery::lock_pane(&collaboration, &pane).await;
                            // Resolve endpoint/session authority before any input or
                            // expected-prompt registration can reach the backend.
                            let agents = store.by_pane(&pane).await;
                            match send_prompt_endpoint(
                                &pane,
                                target.kind(),
                                &agents,
                                socket.as_deref(),
                                session_id.as_deref(),
                            ) {
                                Err(error) => Response::err(error),
                                Ok(socket) => {
                                    use crate::prompt_delivery::{
                                        deliver_prompt_holding, DeliveryRefused, PromptDelivery,
                                    };
                                    match deliver_prompt_holding(
                                        action_guard,
                                        &collaboration,
                                        &store,
                                        &agents,
                                        target,
                                        PromptDelivery {
                                            pane: &pane,
                                            socket: socket.as_deref(),
                                            text: &text,
                                            submit,
                                            origin: Some(crate::PromptOrigin::declared(origin)),
                                        },
                                    )
                                    .await
                                    {
                                        Err(DeliveryRefused::WorkerDraining) => {
                                            Response::err(crate::prompt_delivery::WORKER_DRAINING)
                                        }
                                        // Nothing landed — safe for the caller to
                                        // retry the whole send.
                                        Err(DeliveryRefused::NotSent) => Response::err(
                                            "send_text failed: pane gone or host unreachable",
                                        ),
                                        Ok(delivered) => {
                                            let submitted = delivered.submitted;
                                            tracing::debug!(submit, submitted, "send_prompt");
                                            let mut response =
                                                Response::with_send_result(true, submitted);
                                            response.submission_confirmed =
                                                delivered.confirm().await;
                                            response
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                RequestBody::ExpectPrompt {
                    pane,
                    socket,
                    text,
                    origin,
                } => {
                    kind = "expect_prompt";
                    let token = store
                        .expect_prompt(
                            &pane,
                            socket.as_deref(),
                            &text,
                            crate::PromptOrigin::declared(Some(origin)),
                        )
                        .await;
                    Response {
                        prompt_token: token,
                        ..Response::ok()
                    }
                }
                RequestBody::ForgetExpectedPrompt { token } => {
                    kind = "forget_expected_prompt";
                    store.forget_expected_prompt(token);
                    Response::ok()
                }
                RequestBody::Capture { pane } => {
                    kind = "capture";
                    match resolve_backend(&backends, &pane) {
                        // Same structured refusal as send_prompt: capturing via
                        // the wrong backend would read a different host's screen.
                        Err(missing) => Response::err(format!(
                            "namespace unavailable: no active {missing} backend for pane {pane}",
                        )),
                        Ok(target) => {
                            let target = target.clone();
                            // Capture the RIGHT `%5` by pinning to the pane's
                            // recorded server (see send_prompt above).
                            let agents = store.by_pane(&pane).await;
                            match unique_pane_endpoint(&pane, &agents) {
                                Err(error) => Response::err(error),
                                Ok(socket) => {
                                    let text = tokio::task::spawn_blocking(move || {
                                        target.capture_pane_on(socket.as_deref(), &pane)
                                    })
                                    .await
                                    .unwrap_or(None);
                                    Response::with_capture(text)
                                }
                            }
                        }
                    }
                }
                RequestBody::CollaborationContext { origin } => {
                    kind = "collaboration_context";
                    collaboration_actor.observe_pane(&backends).await;
                    let audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::Context,
                        origin.clone(),
                    );
                    let response = if collaboration.enabled() {
                        let topology = collaboration_participants(
                            &store,
                            &backends,
                            &collaboration,
                            origin.agent_session_id.as_deref(),
                            collaboration_actor.take_request_panes(),
                        )
                        .await;
                        match topology.resolve_origin(&origin) {
                            Ok(current) => Response::with_room(
                                collaboration::room_context(
                                    collaboration.as_ref(),
                                    current,
                                    &topology.participants,
                                )
                                .await,
                            ),
                            Err(error) => Response::err(error.to_string()),
                        }
                    } else {
                        Response::err(
                            "agent collaboration is disabled; enable [collaboration].enabled",
                        )
                    };
                    pending_audit = Some(collaboration_audit_entry(
                        &collaboration_actor,
                        audit_context,
                        &response,
                    ));
                    response
                }
                RequestBody::CollaborationIssueHandle {
                    pane,
                    socket,
                    request,
                } => {
                    kind = "collaboration_issue_handle";
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        None,
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    match topology.room_of(&pane, socket.as_deref()) {
                        Some(room) => match collaboration
                            .issue_handle(&room, &pane, &topology.participants, request)
                            .await
                        {
                            Ok(handle) => Response::with_handle(handle),
                            Err(error) => Response::err(error.to_string()),
                        },
                        // A pane the scan cannot place has no room, so it has
                        // no namespace to allocate from. The caller falls back
                        // to leaving it unnamed rather than guessing.
                        None => Response::with_handle(None),
                    }
                }
                RequestBody::CollaborationSetIdentity {
                    origin,
                    alias,
                    roles,
                } => {
                    kind = "collaboration_set_identity";
                    collaboration_actor.observe_pane(&backends).await;
                    let audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::SetIdentity,
                        origin.clone(),
                    );
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        origin.agent_session_id.as_deref(),
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    let response = match topology.resolve_origin(&origin) {
                        Ok(current) => match collaboration
                            .set_identity(&current, &topology.participants, alias, roles)
                            .await
                        {
                            Ok(current) => Response::with_room(
                                collaboration::room_context(
                                    collaboration.as_ref(),
                                    current,
                                    &topology.participants,
                                )
                                .await,
                            ),
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    };
                    pending_audit = Some(collaboration_audit_entry(
                        &collaboration_actor,
                        audit_context,
                        &response,
                    ));
                    response
                }
                RequestBody::AskSend { prompt, credential } => {
                    kind = "ask_send";
                    match ask.ask_with_credential(&prompt, credential).await {
                        Ok(entry) => Response::with_ask_entry(entry),
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::AskSendNew { prompt, credential } => {
                    kind = "ask_send_new";
                    match ask
                        .ask_in_new_conversation_with_credential(&prompt, credential)
                        .await
                    {
                        Ok(entry) => Response::with_ask_entry(entry),
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::AskSubscribe => {
                    let changes = ask.subscribe();
                    let stream_proto = negotiated.unwrap_or(PROTOCOL_VERSION);
                    let ack_bytes = encode_line(&Response::ok(), stream_proto)?;
                    if !write_subscription_ack(&mut writer, &ack_bytes, &mut stopping).await? {
                        return Ok(());
                    }
                    return until_server_shutdown(
                        &mut stopping,
                        stream_revision_updates(writer, changes, stream_proto),
                    )
                    .await
                    .unwrap_or(Ok(()));
                }
                RequestBody::AskStatus {} => {
                    kind = "ask_status";
                    Response::with_ask_status(ask.enabled())
                }
                RequestBody::AskList {
                    id,
                    conversation_id,
                } => {
                    kind = "ask_list";
                    Response::with_ask_entries(
                        ask.list_filtered(id.as_deref(), conversation_id.as_deref())
                            .await,
                    )
                }
                RequestBody::AskConversationList {} => {
                    kind = "ask_conversation_list";
                    Response::with_ask_conversations(
                        ask.list_conversations().await,
                        ask.active_conversation().await,
                    )
                }
                RequestBody::AskConversationSelect { conversation_id } => {
                    kind = "ask_conversation_select";
                    match ask.select_conversation(&conversation_id).await {
                        Ok(conversation) => Response::with_ask_conversation(conversation),
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::AskAgent { agent } => {
                    kind = "ask_agent";
                    match agent {
                        Some(name) => match ask.set_agent(&name).await {
                            Ok(label) => Response::with_ask_agent(label),
                            Err(error) => Response::err(error.to_string()),
                        },
                        None => Response::with_ask_agent(ask.agent().await),
                    }
                }
                RequestBody::AskReset {} => {
                    kind = "ask_reset";
                    Response::with_ask_conversation(ask.reset_thread().await)
                }
                RequestBody::AskClear {} => {
                    kind = "ask_clear";
                    Response::with_pruned(ask.clear_history().await)
                }
                RequestBody::AskDelete { id } => {
                    kind = "ask_delete";
                    Response::with_pruned(usize::from(ask.delete_history_entry(&id).await))
                }
                RequestBody::ConfigRead {} => {
                    kind = "config_read";
                    match config_path.as_deref() {
                        Some(path) => match crate::config_file::read(path) {
                            Ok(document) => Response::with_config(document),
                            Err(error) => Response::err(error.to_string()),
                        },
                        None => Response::err(NO_CONFIG_PATH.to_string()),
                    }
                }
                RequestBody::ConfigOrchestrationRead {} => {
                    kind = "config_orchestration_read";
                    orchestration_config_response(config_path.as_deref(), None)
                }
                RequestBody::ConfigOrchestrationWrite {
                    expected_text,
                    settings,
                } => {
                    kind = "config_orchestration_write";
                    orchestration_config_response(
                        config_path.as_deref(),
                        Some((&expected_text, &settings)),
                    )
                }
                RequestBody::ConfigLaunchRead {} => {
                    kind = "config_launch_read";
                    match config_path.as_deref() {
                        Some(path) => match crate::config_file::read_launch(path) {
                            Ok(document) => {
                                let mut response = Response::with_config(document.config);
                                response.launch = Some(document.launch);
                                response
                            }
                            Err(error) => Response::err(error.to_string()),
                        },
                        None => Response::err(NO_CONFIG_PATH.to_string()),
                    }
                }
                RequestBody::ConfigLaunchWrite {
                    expected_text,
                    edits,
                } => {
                    kind = "config_launch_write";
                    match config_path.as_deref() {
                        Some(path) => {
                            match crate::config_file::write_launch(path, &expected_text, &edits) {
                                Ok(document) => {
                                    let mut response = Response::with_config(document.config);
                                    response.launch = Some(document.launch);
                                    response
                                }
                                Err(crate::config_file::ConfigFileError::Conflict { current }) => {
                                    let mut response = Response::err(
                                        "config.toml changed; reload before saving launch options",
                                    );
                                    response.config = Some(crate::config_file::ConfigDocument {
                                        path: path.to_path_buf(),
                                        exists: true,
                                        text: current,
                                    });
                                    response
                                }
                                Err(error) => Response::err(error.to_string()),
                            }
                        }
                        None => Response::err(NO_CONFIG_PATH.to_string()),
                    }
                }
                RequestBody::ConfigWrite {
                    text,
                    expected_text,
                } => {
                    kind = "config_write";
                    match config_path.as_deref() {
                        Some(path) => {
                            match crate::config_file::write(path, &text, expected_text.as_deref()) {
                                Ok(document) => Response::with_config(document),
                                Err(crate::config_file::ConfigFileError::Conflict { current }) => {
                                    // Hand the current text back with the
                                    // refusal so the editor can merge instead
                                    // of asking for it again.
                                    let mut response = Response::err(
                                        crate::config_file::ConfigFileError::Conflict {
                                            current: current.clone(),
                                        }
                                        .to_string(),
                                    );
                                    response.config = Some(crate::config_file::ConfigDocument {
                                        path: path.to_path_buf(),
                                        exists: true,
                                        text: current,
                                    });
                                    response
                                }
                                Err(error) => Response::err(error.to_string()),
                            }
                        }
                        None => Response::err(NO_CONFIG_PATH.to_string()),
                    }
                }
                RequestBody::AskProviders {} => {
                    kind = "ask_providers";
                    Response::with_ask_providers(ask.providers().await)
                }
                RequestBody::AskProviderConfigure {
                    provider,
                    title,
                    model,
                    api_key_env,
                    executable,
                } => {
                    kind = "ask_provider_configure";
                    let edit = AskProviderEdit {
                        title,
                        model,
                        api_key_env,
                        executable,
                    };
                    match ask.configure_provider(&provider, edit).await {
                        Ok(providers) => Response::with_ask_providers(providers),
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::AskProviderAdd {
                    id,
                    engine,
                    title,
                    model,
                    api_key_env,
                    executable,
                } => {
                    kind = "ask_provider_add";
                    let request = AskProviderAdd {
                        id,
                        engine,
                        title,
                        model,
                        api_key_env,
                        executable,
                    };
                    match ask.add_provider(request).await {
                        Ok(providers) => Response::with_ask_providers(providers),
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::AskProviderDefault { provider } => {
                    kind = "ask_provider_default";
                    match ask.set_default_agent(&provider).await {
                        Ok(providers) => Response::with_ask_providers(providers),
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::AskProviderRemove { id } => {
                    kind = "ask_provider_remove";
                    match ask.remove_provider(&id).await {
                        Ok(providers) => Response::with_ask_providers(providers),
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::AutomationList {} => {
                    kind = "automation_list";
                    Response::with_automation_rules(
                        automation.views(time::OffsetDateTime::now_utc()).await,
                    )
                }
                RequestBody::AutomationLog { limit } => {
                    kind = "automation_log";
                    Response::with_automation_log(
                        automation
                            .ledger()
                            .recent(limit.unwrap_or(crate::automation::MAX_LEDGER_ENTRIES))
                            .await,
                    )
                }
                RequestBody::AutomationSetEnabled { name, enabled } => {
                    kind = "automation_set_enabled";
                    // No name is the master switch: the whole engine, rather
                    // than one rule.
                    let applied = match name.as_deref() {
                        Some(name) => automation.set_rule_enabled(name, enabled).await,
                        None => automation.set_master_enabled(enabled).await,
                    };
                    match applied {
                        Ok(()) => Response::with_automation_rules(
                            automation.views(time::OffsetDateTime::now_utc()).await,
                        ),
                        Err(error) => Response::err(error),
                    }
                }
                RequestBody::AutomationPause { until } => {
                    kind = "automation_pause";
                    match automation.set_paused_until(until).await {
                        Ok(()) => Response::with_automation_rules(
                            automation.views(time::OffsetDateTime::now_utc()).await,
                        ),
                        Err(error) => Response::err(error),
                    }
                }
                RequestBody::AutomationSetRule { rule } => {
                    kind = "automation_set_rule";
                    match automation.upsert_rule(rule).await {
                        Ok(()) => Response::with_automation_rules(
                            automation.views(time::OffsetDateTime::now_utc()).await,
                        ),
                        Err(error) => Response::err(error),
                    }
                }
                RequestBody::AutomationRemoveRule { name } => {
                    kind = "automation_remove_rule";
                    match automation.remove_rule(&name).await {
                        Ok(()) => Response::with_automation_rules(
                            automation.views(time::OffsetDateTime::now_utc()).await,
                        ),
                        Err(error) => Response::err(error),
                    }
                }
                RequestBody::AutomationTest { name } => {
                    kind = "automation_test";
                    let subjects = automation_subjects(&store, &backends).await;
                    match automation
                        .test_rule(&name, &subjects, time::OffsetDateTime::now_utc())
                        .await
                    {
                        Ok(report) => Response::with_automation_test(report),
                        Err(error) => Response::err(error),
                    }
                }
                RequestBody::AutomationJudgeTest { rule, pane } => {
                    kind = "automation_judge_test";
                    match automation_judge_test(&rule, &pane, &store, &backends, &automation, &ask)
                        .await
                    {
                        Ok(judgment) => {
                            let mut response = Response::ok();
                            response.automation_judgment = Some(judgment);
                            response
                        }
                        Err(error) => Response::err(error),
                    }
                }
                RequestBody::CollaborationSend {
                    origin,
                    target,
                    mut request,
                    view,
                } => {
                    response_view = view;
                    kind = "collaboration_send";
                    collaboration_actor.observe_pane(&backends).await;
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::Send,
                        origin.clone(),
                    );
                    audit_context.target = Some(target.clone());
                    audit_context.message_bytes = Some(request.body.len());
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        origin.agent_session_id.as_deref(),
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    let result = topology.resolve_origin(&origin).and_then(|sender| {
                        topology
                            .resolve_target_with_slot(&sender, &target, collaboration.scope())
                            .map(|(recipient, slot)| (sender, recipient, slot))
                    });
                    let response = match result {
                        Ok((sender, recipient, slot)) => {
                            request.recipient_slot = slot;
                            // A console represents the operator, so stamp the
                            // pane it dispatches to. Agent-originated sends are
                            // stamped from the sending execution surface.
                            let anchor = if origin.console { &recipient } else { &sender };
                            topology.stamp_request_metadata(anchor, &mut request);
                            let provenance = collaboration_actor.provenance(&origin);
                            match collaboration
                                .create_with_provenance(
                                    sender,
                                    recipient,
                                    *request,
                                    Some(provenance),
                                )
                                .await
                            {
                                Ok(request) => Response::with_collaboration_request(request),
                                Err(error) => Response::err(error.to_string()),
                            }
                        }
                        Err(error) => Response::err(error.to_string()),
                    };
                    pending_audit = Some(collaboration_audit_entry(
                        &collaboration_actor,
                        audit_context,
                        &response,
                    ));
                    response
                }
                RequestBody::CollaborationTurnBoundaryOffer {
                    origin,
                    event,
                    max_requests,
                    max_bytes,
                } => {
                    kind = "collaboration_turn_boundary_offer";
                    collaboration_actor.observe_pane(&backends).await;
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        origin.agent_session_id.as_deref(),
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    // Exact-session origins (a paneless Codex app-server hook)
                    // resolve only to a live binding; no pane fallback.
                    match topology.resolve_origin(&origin) {
                        Ok(current) => match collaboration
                            .offer_turn_boundary(&current, event, max_requests, max_bytes)
                            .await
                        {
                            Ok(offers) => Response::with_turn_boundary_offers(offers),
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::CollaborationInbox {
                    origin,
                    request_id,
                    limit,
                } => {
                    kind = "collaboration_inbox";
                    collaboration_actor.observe_pane(&backends).await;
                    let audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::Inbox,
                        origin.clone(),
                    );
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        origin.agent_session_id.as_deref(),
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    let response = match topology.resolve_origin(&origin) {
                        Ok(current) => match if let Some(id) = request_id.as_deref() {
                            collaboration
                                .claim_by_id(&current, id)
                                .await
                                .map(|request| vec![request])
                        } else if let Some(limit) = limit {
                            collaboration.claim_for_limit(&current, limit).await
                        } else {
                            collaboration.claim_for(&current).await
                        } {
                            Ok(requests) => Response::with_collaboration_requests(requests),
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    };
                    pending_audit = Some(collaboration_audit_entry(
                        &collaboration_actor,
                        audit_context,
                        &response,
                    ));
                    response
                }
                RequestBody::CollaborationPurpose { origin, purpose } => {
                    kind = "collaboration_purpose";
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        origin.agent_session_id.as_deref(),
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    match topology.resolve_origin(&origin) {
                        Ok(current) => match collaboration.set_purpose(&current, purpose).await {
                            Ok(()) => Response::ok(),
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::CollaborationPromotePurpose {
                    session_ids,
                    purpose,
                } => {
                    kind = "collaboration_promote_purpose";
                    collaboration_actor.observe_pane(&backends).await;
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        None,
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    let origin = CollaborationOrigin {
                        exact_session: false,
                        pane: collaboration_actor
                            .observed_pane
                            .clone()
                            .unwrap_or_default(),
                        socket: None,
                        console: true,
                        agent_session_id: None,
                    };
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::PromotePurpose,
                        origin,
                    );
                    audit_context.target_sessions.clone_from(&session_ids);
                    audit_context.purpose = Some(purpose);
                    let response = match promote_purpose_targets(&topology, &session_ids) {
                        Ok(targets) => match collaboration.set_purposes(&targets, purpose).await {
                            Ok(()) => Response::ok(),
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error),
                    };
                    pending_audit = Some(collaboration_audit_entry(
                        &collaboration_actor,
                        audit_context,
                        &response,
                    ));
                    response
                }
                drain_request @ (RequestBody::WorkerDrainPlan { .. }
                | RequestBody::WorkerDrainAcquire { .. }
                | RequestBody::WorkerDrainRenew { .. }
                | RequestBody::WorkerDrainStop { .. }
                | RequestBody::WorkerDrainFinish { .. }
                | RequestBody::WorkerDrains { .. }) => {
                    let (origin, operation, target, guard_pane) = match &drain_request {
                        RequestBody::WorkerDrainPlan { origin, .. } => (
                            origin,
                            CollaborationAuditOperation::WorkerDrainPlan,
                            None,
                            None,
                        ),
                        RequestBody::WorkerDrainAcquire { origin, target, .. } => (
                            origin,
                            CollaborationAuditOperation::WorkerDrainAcquire,
                            Some(serde_json::to_string(target).unwrap_or_default()),
                            Some(target.pane.clone()),
                        ),
                        RequestBody::WorkerDrainRenew { origin, token } => (
                            origin,
                            CollaborationAuditOperation::WorkerDrainRenew,
                            Some(serde_json::to_string(token).unwrap_or_default()),
                            None,
                        ),
                        RequestBody::WorkerDrainStop {
                            origin,
                            token,
                            operation_id,
                        } => (
                            origin,
                            CollaborationAuditOperation::WorkerDrainStop,
                            Some(
                                serde_json::json!({"token":token,"operation_id":operation_id})
                                    .to_string(),
                            ),
                            None,
                        ),
                        RequestBody::WorkerDrainFinish { origin, token, .. } => (
                            origin,
                            CollaborationAuditOperation::WorkerDrainFinish,
                            Some(serde_json::to_string(token).unwrap_or_default()),
                            Some(token.target.pane.clone()),
                        ),
                        RequestBody::WorkerDrains { origin } => (
                            origin,
                            CollaborationAuditOperation::WorkerDrains,
                            None,
                            None,
                        ),
                        _ => unreachable!(),
                    };
                    kind = match operation {
                        CollaborationAuditOperation::WorkerDrainPlan => "worker_drain_plan",
                        CollaborationAuditOperation::WorkerDrainAcquire => "worker_drain_acquire",
                        CollaborationAuditOperation::WorkerDrainRenew => "worker_drain_renew",
                        CollaborationAuditOperation::WorkerDrainStop => "worker_drain_stop",
                        CollaborationAuditOperation::WorkerDrainFinish => "worker_drain_finish",
                        _ => "worker_drains",
                    };
                    let origin = origin.clone();
                    collaboration_actor.observe_pane(&backends).await;
                    let mut audit_context =
                        CollaborationAuditContext::new(operation, origin.clone());
                    audit_context.target = target;
                    let _gate = if let Some(pane) = guard_pane {
                        Some(collaboration.worker_action_guard(&pane).await)
                    } else {
                        None
                    };
                    // The acquire/finish gate precedes this scan; cached caller
                    // provenance must never become stop authority.
                    let topology =
                        collaboration_participants(&store, &backends, &collaboration, None, None)
                            .await;
                    let response = match topology.resolve_origin(&origin) {
                        Ok(caller) if caller.console => {
                            let result: Result<Response, String> = async {
                                let mut response = Response::ok();
                                match drain_request {
                                    RequestBody::WorkerDrainPlan { policy, .. } => {
                                        response.worker_drain_plan = Some(
                                            collaboration
                                                .worker_drain_plan(
                                                    &topology.participants,
                                                    &topology.agents,
                                                    &policy,
                                                )
                                                .await,
                                        );
                                    }
                                    RequestBody::WorkerDrainAcquire { target, policy, .. } => {
                                        response.worker_drain_acquire = Some(
                                            collaboration
                                                .worker_drain_acquire(
                                                    &target,
                                                    policy,
                                                    &topology.participants,
                                                    &topology.agents,
                                                    &topology.panes,
                                                )
                                                .await
                                                .map_err(|e| e.to_string())?,
                                        );
                                    }
                                    RequestBody::WorkerDrainRenew { token, .. } => {
                                        response.worker_drain_token = Some(
                                            collaboration
                                                .worker_drain_renew(&token)
                                                .await
                                                .map_err(|e| e.to_string())?,
                                        );
                                    }
                                    RequestBody::WorkerDrainStop {
                                        token,
                                        operation_id,
                                        ..
                                    } => {
                                        response.worker_drain_stop = Some(
                                            worker_drain_control::stop(
                                                store.clone(),
                                                collaboration.clone(),
                                                backends.clone(),
                                                token,
                                                operation_id,
                                            )
                                            .await?,
                                        );
                                    }
                                    RequestBody::WorkerDrainFinish {
                                        token,
                                        outcome,
                                        detail,
                                        ..
                                    } => {
                                        let view = collaboration
                                            .worker_drain_finish(&token, outcome, &detail)
                                            .await
                                            .map_err(|e| e.to_string())?;
                                        if view.phase == collaboration::WorkerDrainPhase::Exited {
                                            worker_drain_control::settle_row(
                                                &store, &backends, &token,
                                            )
                                            .await;
                                        }
                                        response.worker_drain = Some(view);
                                    }
                                    RequestBody::WorkerDrains { .. } => {
                                        response.worker_drains =
                                            Some(collaboration.worker_drains().await);
                                    }
                                    _ => unreachable!(),
                                }
                                Ok(response)
                            }
                            .await;
                            result.unwrap_or_else(Response::err)
                        }
                        Ok(_) => Response::err("worker drain requires a console origin"),
                        Err(error) => Response::err(error.to_string()),
                    };
                    audit_context.release_reason = response
                        .worker_drain_stop
                        .as_ref()
                        .map(|s| s.drain.detail.clone())
                        .or_else(|| {
                            response
                                .worker_drain_acquire
                                .as_ref()
                                .and_then(|a| match a {
                                    collaboration::WorkerDrainAcquire::Skipped { reason } => {
                                        Some(reason.clone())
                                    }
                                    collaboration::WorkerDrainAcquire::Acquired { .. } => None,
                                })
                        });
                    if let Some(view) = &response.worker_drain {
                        audit_context.release_reason = Some(view.detail.clone());
                    }
                    pending_audit = Some(collaboration_audit_entry(
                        &collaboration_actor,
                        audit_context,
                        &response,
                    ));
                    response
                }
                RequestBody::CollaborationWorkerSpawns => {
                    kind = "collaboration_worker_spawns";
                    let mut response = Response::ok();
                    response.worker_spawns = Some(collaboration.worker_spawn_records().await);
                    response
                }
                RequestBody::CollaborationWorkerSpawnRelease {
                    origin,
                    launch_id,
                    reason,
                } => {
                    kind = "collaboration_worker_spawn_release";
                    collaboration_actor.observe_pane(&backends).await;
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::WorkerSpawnRelease,
                        origin.clone(),
                    );
                    audit_context.target = Some(launch_id.clone());
                    audit_context.release_reason = Some(reason.chars().take(512).collect());
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        None,
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    let response = match topology.resolve_origin(&origin) {
                        Ok(caller) if caller.console => match collaboration.release_worker_spawn(&caller, &launch_id, &reason, &topology.panes).await {
                            Ok(()) => Response::ok(),
                            Err(error) => Response::err(error.to_string()),
                        },
                        Ok(_) => Response::err("operator release requires a console origin; owners use the Release action"),
                        Err(error) => Response::err(error.to_string()),
                    };
                    pending_audit = Some(collaboration_audit_entry(
                        &collaboration_actor,
                        audit_context,
                        &response,
                    ));
                    response
                }
                RequestBody::CollaborationWorkerAcquire { origin, spec } => {
                    kind = "collaboration_worker_acquire";
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        origin.agent_session_id.as_deref(),
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    match topology.resolve_origin(&origin) {
                        Ok(caller) => match collaboration
                            .acquire_worker_spawn(
                                &caller,
                                origin,
                                spec,
                                &topology.participants,
                                &topology.panes,
                            )
                            .await
                        {
                            Ok(acquisition) => {
                                let mut response = Response::ok();
                                response.worker_spawn = Some(acquisition);
                                response
                            }
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::CollaborationWorkerSpawn { ticket, action } => {
                    kind = "collaboration_worker_spawn";
                    let audit_context =
                        if let collaboration::WorkerSpawnAction::Release { reason } = &action {
                            collaboration_actor.observe_pane(&backends).await;
                            let mut context = CollaborationAuditContext::new(
                                CollaborationAuditOperation::WorkerSpawnRelease,
                                ticket.origin.clone(),
                            );
                            context.target = Some(ticket.launch_id.clone());
                            context.release_reason = Some(reason.chars().take(512).collect());
                            Some(context)
                        } else {
                            None
                        };
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        ticket.origin.agent_session_id.as_deref(),
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    let response = match topology.resolve_origin(&ticket.origin) {
                        Ok(caller) => match collaboration
                            .update_worker_spawn(
                                &caller,
                                &ticket.launch_id,
                                action,
                                &topology.panes,
                            )
                            .await
                        {
                            Ok(()) => Response::ok(),
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    };
                    if let Some(context) = audit_context {
                        pending_audit = Some(collaboration_audit_entry(
                            &collaboration_actor,
                            context,
                            &response,
                        ));
                    }
                    response
                }
                RequestBody::CollaborationWorkerLaunch {
                    pane,
                    socket,
                    agent_kind,
                    launch_id,
                } => {
                    kind = "collaboration_worker_launch";
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        None,
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    let candidates: Vec<_> = topology
                        .panes
                        .iter()
                        .filter(|p| p.pane_id == pane && p.socket == socket)
                        .collect();
                    match candidates.first() {
                        Some(first)
                            if candidates.iter().all(|p| {
                                p.pane_pid == first.pane_pid && p.window_id == first.window_id
                            }) =>
                        {
                            match collaboration
                                .register_worker_launch(launch_id, (*first).clone(), agent_kind)
                                .await
                            {
                                Ok(()) => {
                                    let mut participants = topology.participants;
                                    collaboration
                                        .enrich_launch_purposes(&mut participants, &topology.panes)
                                        .await;
                                    Response::ok()
                                }
                                Err(error) => Response::err(error.to_string()),
                            }
                        }
                        _ => Response::err("worker pane endpoint is absent or ambiguous"),
                    }
                }
                RequestBody::CollaborationSubscribe { requests_only } => {
                    kind = "collaboration_subscribe";
                    let changes = collaboration.subscribe();
                    // Capture before ACK: a mutation after this baseline stays dirty,
                    // even when it races the caller's first history snapshot.
                    let mut request_revision = collaboration.request_revision();
                    let request_store = collaboration.clone();
                    let stream_proto = negotiated.unwrap_or(PROTOCOL_VERSION);
                    let ack_bytes = encode_line(&Response::ok(), stream_proto)?;
                    if !write_subscription_ack(&mut writer, &ack_bytes, &mut stopping).await? {
                        return Ok(());
                    }
                    tracing::debug!(
                        elapsed_us =
                            u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                        kind,
                        "ipc.handle (collaboration stream takeover)",
                    );
                    return until_server_shutdown(
                        &mut stopping,
                        stream_revision_updates_filtered(
                            writer,
                            changes,
                            stream_proto,
                            move || {
                                let current = request_store.request_revision();
                                let dirty = current != request_revision;
                                request_revision = current;
                                !requests_only || dirty
                            },
                        ),
                    )
                    .await
                    .unwrap_or(Ok(()));
                }
                RequestBody::CollaborationWorkers {
                    idle_threshold_secs,
                } => {
                    kind = "collaboration_workers";
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        None,
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    let mut response = Response::ok();
                    response.worker_inventory = Some(
                        collaboration
                            .worker_inventory(
                                &topology.participants,
                                &topology.agents,
                                idle_threshold_secs,
                            )
                            .await,
                    );
                    response
                }
                RequestBody::CollaborationOverview { origin } => {
                    kind = "collaboration_overview";
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        None,
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    match topology.resolve_origin(&origin) {
                        Ok(current) => match collaboration
                            .overview_for(&current, &topology.participants)
                            .await
                        {
                            Ok(overview) => {
                                let mut response = Response::ok();
                                response.collaboration_overview = Some(overview);
                                response
                            }
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::CollaborationOpenCounts { origin, scope } => {
                    kind = "collaboration_open_counts";
                    collaboration_actor.observe_pane(&backends).await;
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::Counts,
                        origin.clone(),
                    );
                    audit_context.scope = Some(scope);
                    audit_context.read_view = Some(collaboration::CollaborationReadView::Metadata);
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        origin.agent_session_id.as_deref(),
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    let response = match topology.resolve_origin(&origin) {
                        Ok(current) => match collaboration.open_counts_for(&current, scope).await {
                            Ok(counts) => {
                                let mut response = Response::ok();
                                response.collaboration_open_counts = Some(counts);
                                response.collaboration_scope = Some(scope);
                                response
                            }
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    };
                    pending_audit = Some(collaboration_audit_entry(
                        &collaboration_actor,
                        audit_context,
                        &response,
                    ));
                    response
                }
                RequestBody::CollaborationMailboxSnapshot {
                    origin,
                    operator_only,
                    if_unchanged,
                } => {
                    kind = "collaboration_mailbox_snapshot";
                    collaboration_actor.observe_pane(&backends).await;
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::List,
                        origin.clone(),
                    );
                    audit_context.mailbox = Some(RequestMailbox::All);
                    audit_context.read_view = Some(collaboration::CollaborationReadView::Full);
                    audit_context.query_limit = Some(crate::fleet::FLEET_MAILBOX_REQUEST_LIMIT);
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        origin.agent_session_id.as_deref(),
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    let response = async {
                        if !origin.console {
                            return Err(collaboration::CollaborationError::ScopeDenied);
                        }
                        let console = topology.resolve_origin(&origin)?;
                        let agent = if operator_only {
                            None
                        } else {
                            Some(topology.resolve_origin(&CollaborationOrigin {
                                console: false,
                                ..origin.clone()
                            })?)
                        };
                        let snapshot = collaboration
                            .mailbox_snapshot(&console, agent.as_ref(), if_unchanged.as_deref())
                            .await?;
                        let mut response = Response::ok();
                        response.collaboration_mailbox_snapshot = Some(snapshot);
                        Ok::<_, collaboration::CollaborationError>(response)
                    }
                    .await;
                    let response = match response {
                        Ok(response) => response,
                        Err(error) => Response::err(error.to_string()),
                    };
                    pending_audit = Some(collaboration_audit_entry(
                        &collaboration_actor,
                        audit_context,
                        &response,
                    ));
                    response
                }
                RequestBody::CollaborationList {
                    mut origin,
                    mailbox,
                    mut scope,
                    query,
                    offset,
                    metadata_only,
                    view,
                    auto_scope,
                } => {
                    kind = "collaboration_list";
                    collaboration_actor.observe_pane(&backends).await;
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::List,
                        origin.clone(),
                    );
                    audit_context.mailbox = Some(mailbox);
                    audit_context.read_view = Some(if query.is_none() {
                        collaboration::CollaborationReadView::Full
                    } else if metadata_only {
                        collaboration::CollaborationReadView::Metadata
                    } else {
                        view
                    });
                    audit_context.query_limit = query.as_ref().and_then(|q| q.limit);
                    // Only the widened listings are worth a ledger line of
                    // their own — a caller-scoped list is the norm and says
                    // nothing about reach.
                    audit_context.scope = (!matches!(scope, MailboxScope::Caller)).then_some(scope);
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        origin.agent_session_id.as_deref(),
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    let mut current = topology.resolve_origin(&origin);
                    if auto_scope
                        && matches!(scope, MailboxScope::Caller)
                        && origin.agent_session_id.is_none()
                        && matches!(
                            current,
                            Err(collaboration::CollaborationError::UnknownOrigin(_))
                        )
                    {
                        origin.console = true;
                        scope = MailboxScope::Room;
                        current = topology.resolve_origin(&origin);
                    }
                    audit_context.represented_origin = origin.clone();
                    audit_context.scope = (!matches!(scope, MailboxScope::Caller)).then_some(scope);
                    let response = match current {
                        Ok(current) if query.is_some() => {
                            match collaboration
                                .query_for_offset(
                                    &current,
                                    mailbox,
                                    scope,
                                    query.as_ref().expect("query present"),
                                    offset,
                                )
                                .await
                            {
                                Ok(mut page) => {
                                    let view = if metadata_only {
                                        collaboration::CollaborationReadView::Metadata
                                    } else {
                                        view
                                    };
                                    for request in &mut page.requests {
                                        view.apply(request);
                                    }
                                    let mut response = Response::ok();
                                    response.collaboration_scope = Some(scope);
                                    response.collaboration_byte_budget = query
                                        .as_ref()
                                        .and_then(collaboration::CollaborationQuery::byte_budget);
                                    response.collaboration_page = Some(page);
                                    response
                                }
                                Err(error) => Response::err(error.to_string()),
                            }
                        }
                        Ok(current) => match collaboration.list_for(&current, mailbox, scope).await
                        {
                            Ok(requests) => {
                                Response::with_scoped_collaboration_requests(requests, scope)
                            }
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    };
                    pending_audit = Some(collaboration_audit_entry(
                        &collaboration_actor,
                        audit_context,
                        &response,
                    ));
                    response
                }
                RequestBody::CollaborationUpdate {
                    origin,
                    request_id,
                    body,
                    view,
                } => {
                    response_view = view;
                    kind = "collaboration_update";
                    collaboration_actor.observe_pane(&backends).await;
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::Update,
                        origin.clone(),
                    );
                    audit_context.request_id = Some(request_id.clone());
                    audit_context.message_bytes = Some(body.len());
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        origin.agent_session_id.as_deref(),
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    let response = match topology.resolve_origin(&origin) {
                        Ok(current) => match collaboration
                            .update_request(&current, &request_id, body)
                            .await
                        {
                            Ok(request) => Response::with_collaboration_request(request),
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    };
                    pending_audit = Some(collaboration_audit_entry(
                        &collaboration_actor,
                        audit_context,
                        &response,
                    ));
                    response
                }
                RequestBody::CollaborationReply {
                    origin,
                    request_id,
                    status,
                    body,
                    artifacts,
                    view,
                    report,
                } => {
                    response_view = view;
                    kind = "collaboration_reply";
                    collaboration_actor.observe_pane(&backends).await;
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::Reply,
                        origin.clone(),
                    );
                    audit_context.request_id = Some(request_id.clone());
                    audit_context.status = Some(status);
                    audit_context.message_bytes = Some(body.len());
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        origin.agent_session_id.as_deref(),
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    let response = match topology.resolve_origin(&origin) {
                        Ok(current) => match collaboration
                            .reply_with_report(
                                &current,
                                &request_id,
                                status,
                                body,
                                artifacts,
                                report,
                            )
                            .await
                        {
                            Ok(request) => Response::with_collaboration_request(request),
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    };
                    pending_audit = Some(collaboration_audit_entry(
                        &collaboration_actor,
                        audit_context,
                        &response,
                    ));
                    response
                }
                RequestBody::CollaborationGet {
                    origin,
                    request_id,
                    view,
                } => {
                    response_view = view;
                    kind = "collaboration_get";
                    collaboration_actor.observe_pane(&backends).await;
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::Get,
                        origin.clone(),
                    );
                    audit_context.request_id = Some(request_id.clone());
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        origin.agent_session_id.as_deref(),
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    let response = match topology.resolve_origin(&origin) {
                        Ok(current) => match collaboration.get_for(&current, &request_id).await {
                            Ok(request) => Response::with_collaboration_request(request),
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    };
                    pending_audit = Some(collaboration_audit_entry(
                        &collaboration_actor,
                        audit_context,
                        &response,
                    ));
                    response
                }
                RequestBody::CollaborationPeek {
                    origin,
                    request_id,
                    view,
                } => {
                    response_view = view;
                    kind = "collaboration_peek";
                    collaboration_actor.observe_pane(&backends).await;
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::Peek,
                        origin.clone(),
                    );
                    audit_context.request_id = Some(request_id.clone());
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        origin.agent_session_id.as_deref(),
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    let response = match topology.resolve_origin(&origin) {
                        Ok(current) => match collaboration.peek_for(&current, &request_id).await {
                            Ok(request) => Response::with_collaboration_request(request),
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    };
                    pending_audit = Some(collaboration_audit_entry(
                        &collaboration_actor,
                        audit_context,
                        &response,
                    ));
                    response
                }
                RequestBody::CollaborationWait {
                    origin,
                    request_id,
                    timeout_secs,
                    view,
                } => {
                    response_view = view;
                    kind = "collaboration_wait";
                    collaboration_actor.observe_pane(&backends).await;
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::Wait,
                        origin.clone(),
                    );
                    audit_context.request_id = Some(request_id.clone());
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        origin.agent_session_id.as_deref(),
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    let response = match topology.resolve_origin(&origin) {
                        // A read-only long poll may be cut short by shutdown,
                        // but it is still answered before the drain deadline:
                        // aborting it closed the socket with no reply, which
                        // clients read as a broken response.
                        Ok(current) => match Box::pin(until_server_shutdown(
                            &mut stopping,
                            collaboration.wait_for_terminal(
                                &current,
                                &request_id,
                                Duration::from_secs(
                                    timeout_secs.clamp(1, MAX_COLLABORATION_WAIT_SECS),
                                ),
                            ),
                        ))
                        .await
                        {
                            Some(Ok(request)) => Response::with_collaboration_request(request),
                            Some(Err(error)) => Response::err(error.to_string()),
                            None => Response::restarting(),
                        },
                        Err(error) => Response::err(error.to_string()),
                    };
                    pending_audit = Some(collaboration_audit_entry(
                        &collaboration_actor,
                        audit_context,
                        &response,
                    ));
                    response
                }
                RequestBody::CollaborationHealth => {
                    kind = "collaboration_health";
                    match collaboration.health().await {
                        Ok(mut health) => {
                            let configured = backends.iter().map(|b| b.kind()).collect::<Vec<_>>();
                            health.synthetic_agents =
                                store.synthetic_health(&agent_directory, &configured).await;
                            let mut response = Response::ok();
                            response.collaboration_health = Some(health);
                            response
                        }
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::CollaborationExpire {
                    older_than_secs,
                    dry_run,
                    quiet_only,
                    request_id,
                }
                | RequestBody::CollaborationExpireFiltered {
                    older_than_secs,
                    dry_run,
                    quiet_only,
                    request_id,
                } => {
                    kind = "collaboration_expire";
                    let age =
                        time::Duration::seconds(i64::try_from(older_than_secs).unwrap_or(i64::MAX));
                    let cutoff = time::OffsetDateTime::now_utc()
                        .checked_sub(age)
                        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
                    match collaboration
                        .expire_queued_filtered(cutoff, dry_run, quiet_only, request_id.as_deref())
                        .await
                    {
                        Ok(requests) => Response::with_collaboration_requests(requests),
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::CollaborationExpireSummary {
                    older_than_secs,
                    dry_run,
                    quiet_only,
                    request_id,
                } => {
                    kind = "collaboration_expire_summary";
                    let age =
                        time::Duration::seconds(i64::try_from(older_than_secs).unwrap_or(i64::MAX));
                    let cutoff = time::OffsetDateTime::now_utc()
                        .checked_sub(age)
                        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
                    match collaboration
                        .expire_queued_filtered(cutoff, dry_run, quiet_only, request_id.as_deref())
                        .await
                    {
                        Ok(requests) => {
                            let mut response = Response::ok();
                            response.collaboration_expire_summary =
                                Some(collaboration::ExpireSummary::from_requests(&requests));
                            response
                        }
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::CollaborationRetire {
                    origin,
                    request_id,
                    reason,
                    evidence,
                    dry_run,
                } => {
                    kind = "collaboration_retire";
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::Retire,
                        origin.clone(),
                    );
                    audit_context.request_id = Some(request_id.clone());
                    let response = if !origin.console
                        || origin.agent_session_id.is_some()
                        || origin.exact_session
                    {
                        Response::err("retirement requires an explicit operator console origin")
                    } else {
                        match collaboration
                            .retire_request(&request_id, &reason, &evidence, dry_run)
                            .await
                        {
                            Ok(request) => Response::with_collaboration_request(request),
                            Err(error) => Response::err(error.to_string()),
                        }
                    };
                    pending_audit = Some(collaboration_audit_entry(
                        &collaboration_actor,
                        audit_context,
                        &response,
                    ));
                    response
                }
                RequestBody::CollaborationCancel {
                    origin,
                    request_id,
                    view,
                } => {
                    response_view = view;
                    kind = "collaboration_cancel";
                    collaboration_actor.observe_pane(&backends).await;
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::Cancel,
                        origin.clone(),
                    );
                    audit_context.request_id = Some(request_id.clone());
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        origin.agent_session_id.as_deref(),
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    let response = match topology.resolve_origin(&origin) {
                        Ok(current) => {
                            match collaboration.cancel_for(&current, &request_id).await {
                                Ok(request) => Response::with_collaboration_request(request),
                                Err(error) => Response::err(error.to_string()),
                            }
                        }
                        Err(error) => Response::err(error.to_string()),
                    };
                    pending_audit = Some(collaboration_audit_entry(
                        &collaboration_actor,
                        audit_context,
                        &response,
                    ));
                    response
                }
                RequestBody::CollaborationWithdrawReplyWakes {
                    origin,
                    request_ids,
                } => {
                    kind = "collaboration_withdraw_reply_wakes";
                    collaboration_actor.observe_pane(&backends).await;
                    let mut audit_context = CollaborationAuditContext::new(
                        CollaborationAuditOperation::WithdrawReplyWakes,
                        origin.clone(),
                    );
                    audit_context.request_id = request_ids.first().cloned();
                    let topology = collaboration_participants(
                        &store,
                        &backends,
                        &collaboration,
                        origin.agent_session_id.as_deref(),
                        collaboration_actor.take_request_panes(),
                    )
                    .await;
                    let response = match topology.resolve_origin(&origin) {
                        Ok(current) => match collaboration
                            .withdraw_reply_wakes(&current, &request_ids)
                            .await
                        {
                            Ok(requests) => Response::with_collaboration_requests(requests),
                            Err(error) => Response::err(error.to_string()),
                        },
                        Err(error) => Response::err(error.to_string()),
                    };
                    pending_audit = Some(collaboration_audit_entry(
                        &collaboration_actor,
                        audit_context,
                        &response,
                    ));
                    response
                }
                RequestBody::SpawnSession {
                    command,
                    args,
                    env,
                    cwd,
                    name,
                    cols,
                    rows,
                } => {
                    kind = "spawn_session";
                    match sessions.spawn_session(SpawnSession {
                        command,
                        args,
                        env,
                        cwd,
                        name,
                        cols,
                        rows,
                    }) {
                        Ok(session) => {
                            // Surface the PTY child as a pid-tracked Task row
                            // so `tend run` processes appear in `tend status`.
                            // Best-effort: a name collision with a real agent
                            // just skips the task row, the session still runs.
                            let _ = store
                                .register_surface_task(
                                    session
                                        .display_name
                                        .clone()
                                        .unwrap_or_else(|| session.id.clone()),
                                    session.pid,
                                    session.cwd.clone(),
                                    session.surface(),
                                    None,
                                )
                                .await;
                            agent_directory.invalidate();
                            Response::with_session(session)
                        }
                        Err(e) => Response::err(e.to_string()),
                    }
                }
                RequestBody::ListSessions => {
                    kind = "list_sessions";
                    match sessions.list_sessions_checked() {
                        Ok(list) => Response::with_sessions(list),
                        Err(error) => Response::err(error.to_string()),
                    }
                }
                RequestBody::CaptureSession { session_id } => {
                    kind = "capture_session";
                    match sessions.capture(&session_id) {
                        Ok(snapshot) => Response::with_terminal(snapshot),
                        Err(e) => Response::err(e.to_string()),
                    }
                }
                RequestBody::ReadSession { session_id, offset } => {
                    kind = "read_session";
                    match sessions.read_output(&session_id, offset) {
                        Ok(output) => Response::with_output(output),
                        Err(e) => Response::err(e.to_string()),
                    }
                }
                RequestBody::ReadSessionWait {
                    session_id,
                    offset,
                    timeout_ms,
                } => {
                    kind = "read_session_wait";
                    let sessions = Arc::clone(&sessions);
                    let timeout = Duration::from_millis(timeout_ms.clamp(1, 30_000));
                    let wait = tokio::task::spawn_blocking(move || {
                        sessions.read_output_wait(&session_id, offset, timeout)
                    });
                    // A read-only long poll, so shutdown may abandon it. It
                    // has to be abandoned explicitly: `spawn_blocking` cannot
                    // be cancelled, and `JoinSet::abort_all` only drops the
                    // join handle, so a handler that kept awaiting this would
                    // sit in the drain until the client's own timeout expired
                    // — up to 30s, which made every restart burn the whole
                    // `HANDLER_DRAIN_TIMEOUT` before re-exec. Stop awaiting
                    // instead: the blocking thread expires on its own (and is
                    // replaced outright by the re-exec), and no mutation is
                    // lost because this request performs none.
                    let Some(joined) = until_server_shutdown(&mut stopping, wait).await else {
                        return Ok(());
                    };
                    match joined {
                        Ok(Ok(output)) => Response::with_output(output),
                        Ok(Err(e)) => Response::err(e.to_string()),
                        Err(e) => Response::err(format!("session wait task failed: {e}")),
                    }
                }
                RequestBody::WriteSession { session_id, data } => {
                    kind = "write_session";
                    match sessions.send_input(&session_id, data.as_bytes()) {
                        Ok(()) => Response::ok(),
                        Err(e) => Response::err(e.to_string()),
                    }
                }
                RequestBody::WriteSessionBytes {
                    session_id,
                    data_base64,
                } => {
                    kind = "write_session_bytes";
                    match BASE64_STANDARD.decode(data_base64) {
                        Ok(data) => match sessions.send_input(&session_id, &data) {
                            Ok(()) => Response::ok(),
                            Err(e) => Response::err(e.to_string()),
                        },
                        Err(e) => Response::err(format!("invalid base64 session input: {e}")),
                    }
                }
                RequestBody::ResizeSession {
                    session_id,
                    cols,
                    rows,
                } => {
                    kind = "resize_session";
                    match sessions.resize(&session_id, cols, rows) {
                        Ok(()) => Response::ok(),
                        Err(e) => Response::err(e.to_string()),
                    }
                }
                RequestBody::SetSessionAttached {
                    session_id,
                    client_id,
                    attached,
                } => {
                    kind = "set_session_attached";
                    match sessions.set_attached(&session_id, client_id.as_deref(), attached) {
                        Ok(()) => Response::ok(),
                        Err(e) => Response::err(e.to_string()),
                    }
                }
                RequestBody::TerminateSession { session_id } => {
                    kind = "terminate_session";
                    match sessions.terminate(&session_id) {
                        Ok(()) => {
                            agent_directory.invalidate();
                            Response::ok()
                        }
                        Err(e) => Response::err(e.to_string()),
                    }
                }
                RequestBody::SubscribeAgentChanges => {
                    let protocol = negotiated.unwrap_or(PROTOCOL_VERSION);
                    let changes = store.subscribe_changes();
                    let ack_line = encode_line(&Response::ok(), protocol)?;
                    if !write_subscription_ack(&mut writer, &ack_line, &mut stopping).await? {
                        return Ok(());
                    }
                    return until_server_shutdown(
                        &mut stopping,
                        stream_agent_changes(writer, changes, protocol),
                    )
                    .await
                    .unwrap_or(Ok(()));
                }
                RequestBody::Subscribe { lagged_markers } => {
                    kind = "subscribe";
                    let stream_proto = negotiated.unwrap_or(PROTOCOL_VERSION);
                    // Arm the receiver before acknowledging the subscription.
                    // Once the client sees the ack, every later transition is
                    // therefore either queued here or already being written.
                    let transitions = store.subscribe();
                    // Stream takeover. Send ack, then write transitions
                    // until the client disconnects or tendd shuts down.
                    // We deliberately do NOT return to the request loop
                    // — this connection is now owned by the streaming
                    // pump.
                    let ack_bytes = encode_line(&Response::ok(), stream_proto)?;
                    if !write_subscription_ack(&mut writer, &ack_bytes, &mut stopping).await? {
                        return Ok(());
                    }
                    tracing::debug!(
                        elapsed_us =
                            u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
                        kind,
                        "ipc.handle (stream takeover)",
                    );
                    return until_server_shutdown(
                        &mut stopping,
                        stream_transitions(writer, transitions, stream_proto, lagged_markers),
                    )
                    .await
                    .unwrap_or(Ok(()));
                }
            },
            Err(e) => {
                kind = "parse_error";
                Response::err(format!("bad request: {e}"))
            }
        };

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
            collaboration_audit.append(entry).await;
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

/// The largest prompt `ask_one_shot` accepts.
const ASK_ONE_SHOT_MAX_PROMPT_BYTES: usize = 256 * 1024;
