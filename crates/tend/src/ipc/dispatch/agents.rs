//! Agent rows and pane control: ingest, lookups, prompts and captures.

#![allow(clippy::unused_async)] // every request handler has the same async shape
#![allow(clippy::wildcard_imports)] // the arms name the IPC module's items as `dispatch.rs` does

use super::*;

pub(super) async fn handle(
    ctx: &ServerCtx,
    conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    match body {
        body @ RequestBody::Ingest { .. } => ingest(ctx, conn, turn, body).await,
        body @ RequestBody::Register { .. } => register(ctx, conn, turn, body).await,
        body @ RequestBody::ForgetAgent { .. } => forget_agent(ctx, conn, turn, body).await,
        body @ RequestBody::Prune { .. } => prune(ctx, conn, turn, body).await,
        body @ RequestBody::InvalidateAgentDirectory => {
            invalidate_agent_directory(ctx, conn, turn, body).await
        }
        body @ RequestBody::AgentFind { .. } => agent_find(ctx, conn, turn, body).await,
        body @ RequestBody::Snapshot { .. } => snapshot(ctx, conn, turn, body).await,
        body @ RequestBody::TurnEvidence { .. } => turn_evidence(ctx, conn, turn, body).await,
        body @ RequestBody::ByPane { .. } => by_pane(ctx, conn, turn, body).await,
        body @ RequestBody::BySession { .. } => by_session(ctx, conn, turn, body).await,
        body @ RequestBody::BySurface { .. } => by_surface(ctx, conn, turn, body).await,
        body @ RequestBody::RecentPrompts { .. } => recent_prompts(ctx, conn, turn, body).await,
        body @ RequestBody::BackendPaneSnapshot { .. } => {
            backend_pane_snapshot(ctx, conn, turn, body).await
        }
        body @ RequestBody::RefreshPaneLocations { .. } => {
            refresh_pane_locations(ctx, conn, turn, body).await
        }
        body @ RequestBody::SendPrompt { .. } => send_prompt(ctx, conn, turn, body).await,
        body @ RequestBody::ExpectPrompt { .. } => expect_prompt(ctx, conn, turn, body).await,
        body @ RequestBody::ForgetExpectedPrompt { .. } => {
            forget_expected_prompt(ctx, conn, turn, body).await
        }
        body @ RequestBody::Capture { .. } => capture(ctx, conn, turn, body).await,
        other => unreachable!("dispatch routed {other:?} to agents"),
    }
}

async fn ingest(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let backends = &ctx.backends;
    let collaboration = &ctx.collaboration;
    let kind = &mut turn.kind;
    let RequestBody::Ingest {
        mut event,
        ask_entry,
        launch_id,
        ..
    } = body
    else {
        unreachable!("dispatch routed another request to ingest")
    };
    let response = {
        if ask_entry.is_some() {
            event.id_mut().pane = None;
            event.id_mut().surface = None;
        }
        *kind = "ingest";
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
            || crate::tmux::scanner::event_tmux_socket_in_scope(event.id().tmux_socket.as_deref());
        if in_scope {
            tracing::debug!(?event, "ingest");
            store.apply_with_ask(&event, ask_entry.as_deref()).await;
            if ask_entry.is_none() {
                store
                    .observe_launch_claim(&event, launch_id.as_deref())
                    .await;
            }
            if ask_entry.is_none() {
                if let Err(error) = collaboration.observe_worker_hook(event.id()).await {
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
                crate::codex_binding::schedule_bind_paneless_codex(store.clone(), backends.clone());
            }
        } else {
            tracing::debug!(
                socket = event.id().tmux_socket.as_deref(),
                "ingest skipped: tmux socket outside TEND_TMUX_SOCKET scope",
            );
        }
        Response::ok()
    };
    Ok(Flow::Respond(response))
}

async fn register(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let kind = &mut turn.kind;
    let RequestBody::Register {
        name,
        pid,
        cwd,
        pane,
        command,
    } = body
    else {
        unreachable!("dispatch routed another request to register")
    };
    let response = {
        *kind = "register";
        match store.register_task(name, pid, cwd, pane, command).await {
            Ok(session_id) => {
                tracing::debug!(session_id, ?pid, "register task");
                Response::ok()
            }
            Err(e) => Response::err(e),
        }
    };
    Ok(Flow::Respond(response))
}

async fn forget_agent(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let kind = &mut turn.kind;
    let RequestBody::ForgetAgent { session_id } = body else {
        unreachable!("dispatch routed another request to forget_agent")
    };
    let response = {
        *kind = "forget_agent";
        Response::with_pruned(usize::from(store.forget_agent(&session_id).await))
    };
    Ok(Flow::Respond(response))
}

async fn prune(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let sessions = &ctx.sessions;
    let kind = &mut turn.kind;
    let RequestBody::Prune { max_age_secs } = body else {
        unreachable!("dispatch routed another request to prune")
    };
    let response = {
        *kind = "prune";
        store
            .reconcile_native_surfaces(|id| sessions.session_liveness(id))
            .await;
        let cutoff = time::OffsetDateTime::now_utc() - std::time::Duration::from_secs(max_age_secs);
        let pruned = store.prune_orphans(cutoff).await;
        tracing::debug!(pruned, max_age_secs, "prune orphans");
        Response::with_pruned(pruned)
    };
    Ok(Flow::Respond(response))
}

async fn invalidate_agent_directory(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let agent_directory = &ctx.agent_directory;
    let kind = &mut turn.kind;
    let RequestBody::InvalidateAgentDirectory = body else {
        unreachable!("dispatch routed another request to invalidate_agent_directory")
    };
    let response = {
        *kind = "invalidate_agent_directory";
        agent_directory.invalidate();
        Response::ok()
    };
    Ok(Flow::Respond(response))
}

async fn agent_find(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let backends = &ctx.backends;
    let ask = &ctx.ask;
    let agent_directory = &ctx.agent_directory;
    let kind = &mut turn.kind;
    let RequestBody::AgentFind {
        query,
        limit,
        exclude_sessions,
        refresh,
    } = body
    else {
        unreachable!("dispatch routed another request to agent_find")
    };
    let response = {
        *kind = "agent_find";
        match agent_directory
            .find(
                store,
                ask,
                backends,
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
    };
    Ok(Flow::Respond(response))
}

async fn snapshot(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let kind = &mut turn.kind;
    let RequestBody::Snapshot { launch_identity } = body else {
        unreachable!("dispatch routed another request to snapshot")
    };
    let response = {
        *kind = "snapshot";
        let agents = store.snapshot().await;
        let identities = if launch_identity {
            Some(Arc::new(store.launch_identity_snapshot(&agents).await))
        } else {
            None
        };
        let mut response = Response::with_agents(agents);
        response.launch_identities = identities;
        response
    };
    Ok(Flow::Respond(response))
}

async fn turn_evidence(
    _ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let kind = &mut turn.kind;
    let RequestBody::TurnEvidence { session_id, since } = body else {
        unreachable!("dispatch routed another request to turn_evidence")
    };
    let response = {
        *kind = "turn_evidence";
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
            Ok(Ok(Err(error))) => Response::err(format!("could not read records: {error}")),
            Ok(Err(error)) => Response::err(format!("record read failed: {error}")),
            Err(_) => Response::err("reading records took too long; try again"),
        }
    };
    Ok(Flow::Respond(response))
}

async fn by_pane(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let kind = &mut turn.kind;
    let RequestBody::ByPane { pane } = body else {
        unreachable!("dispatch routed another request to by_pane")
    };
    let response = {
        *kind = "by_pane";
        Response::with_agents(store.by_pane(&pane).await)
    };
    Ok(Flow::Respond(response))
}

async fn by_session(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let kind = &mut turn.kind;
    let RequestBody::BySession { session_id } = body else {
        unreachable!("dispatch routed another request to by_session")
    };
    let response = {
        *kind = "by_session";
        let v = store
            .by_session(&session_id)
            .await
            .into_iter()
            .collect::<Vec<_>>();
        Response::with_agents(v)
    };
    Ok(Flow::Respond(response))
}

async fn by_surface(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let kind = &mut turn.kind;
    let RequestBody::BySurface { surface_id } = body else {
        unreachable!("dispatch routed another request to by_surface")
    };
    let response = {
        *kind = "by_surface";
        Response::with_agents(store.by_surface(&surface_id).await)
    };
    Ok(Flow::Respond(response))
}

async fn recent_prompts(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let kind = &mut turn.kind;
    let RequestBody::RecentPrompts { pane, limit } = body else {
        unreachable!("dispatch routed another request to recent_prompts")
    };
    let response = {
        *kind = "recent_prompts";
        let prompts = store
            .recent_prompts(pane.as_deref(), limit.unwrap_or(0))
            .await;
        Response::with_prompts(prompts)
    };
    Ok(Flow::Respond(response))
}

async fn backend_pane_snapshot(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let backend = &ctx.backend;
    let agent_directory = &ctx.agent_directory;
    let kind = &mut turn.kind;
    let RequestBody::BackendPaneSnapshot { panes } = body else {
        unreachable!("dispatch routed another request to backend_pane_snapshot")
    };
    let response = {
        *kind = "backend_pane_snapshot";
        let count = panes.len();
        backend.ingest_pane_snapshot(panes);
        agent_directory.invalidate();
        tracing::debug!(panes = count, "backend_pane_snapshot");
        Response::ok()
    };
    Ok(Flow::Respond(response))
}

async fn refresh_pane_locations(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let backends = &ctx.backends;
    let agent_directory = &ctx.agent_directory;
    let kind = &mut turn.kind;
    let RequestBody::RefreshPaneLocations { pane } = body else {
        unreachable!("dispatch routed another request to refresh_pane_locations")
    };
    let response = {
        *kind = "refresh_pane_locations";
        agent_directory.invalidate();
        match resolve_backend(backends, &pane) {
            Err(error) => Response::err(format!("no active {error} backend")),
            Ok(target) => {
                let target = target.clone();
                match tokio::task::spawn_blocking(move || target.observe_panes()).await {
                    Ok(observation) => {
                        store.refresh_pane_locations(&observation.panes).await;
                        agent_directory.invalidate();
                        Response::ok()
                    }
                    Err(error) => Response::err(error.to_string()),
                }
            }
        }
    };
    Ok(Flow::Respond(response))
}

async fn send_prompt(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let backends = &ctx.backends;
    let collaboration = &ctx.collaboration;
    let kind = &mut turn.kind;
    let RequestBody::SendPrompt {
        pane,
        socket,
        session_id,
        text,
        submit,
        origin,
        force,
    } = body
    else {
        unreachable!("dispatch routed another request to send_prompt")
    };
    let response = {
        *kind = "send_prompt";
        match resolve_backend(backends, &pane) {
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
                let action_guard = crate::prompt_delivery::lock_pane(collaboration, &pane).await;
                // Resolve endpoint/session authority before any input or
                // expected-prompt registration can reach the backend.
                let agents = store.by_pane(&pane).await;
                match send_prompt_endpoint(
                    &pane,
                    target.kind(),
                    &agents,
                    socket.as_deref(),
                    session_id.as_deref(),
                )
                .and_then(|socket| {
                    // Judged under the input guard, on the
                    // resolved endpoint, before anything is typed.
                    if force == Some(false) {
                        crate::prompt_delivery::idle_gate(
                            &pane,
                            &agents,
                            socket.as_deref(),
                            session_id.as_deref(),
                        )?;
                    }
                    Ok(socket)
                }) {
                    Err(error) => Response::err(error),
                    Ok(socket) => {
                        use crate::prompt_delivery::{
                            deliver_prompt_holding, DeliveryRefused, PromptDelivery,
                        };
                        match deliver_prompt_holding(
                            action_guard,
                            collaboration,
                            store,
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
                            Err(DeliveryRefused::NotSent) => {
                                Response::err("send_text failed: pane gone or host unreachable")
                            }
                            Ok(delivered) => {
                                let submitted = delivered.submitted;
                                tracing::debug!(submit, submitted, "send_prompt");
                                let mut response = Response::with_send_result(true, submitted);
                                response.submission_confirmed = delivered.confirm().await;
                                response
                            }
                        }
                    }
                }
            }
        }
    };
    Ok(Flow::Respond(response))
}

async fn expect_prompt(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let kind = &mut turn.kind;
    let RequestBody::ExpectPrompt {
        pane,
        socket,
        text,
        origin,
    } = body
    else {
        unreachable!("dispatch routed another request to expect_prompt")
    };
    let response = {
        *kind = "expect_prompt";
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
    };
    Ok(Flow::Respond(response))
}

async fn forget_expected_prompt(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let kind = &mut turn.kind;
    let RequestBody::ForgetExpectedPrompt { token } = body else {
        unreachable!("dispatch routed another request to forget_expected_prompt")
    };
    let response = {
        *kind = "forget_expected_prompt";
        store.forget_expected_prompt(token);
        Response::ok()
    };
    Ok(Flow::Respond(response))
}

async fn capture(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let backends = &ctx.backends;
    let kind = &mut turn.kind;
    let RequestBody::Capture { pane } = body else {
        unreachable!("dispatch routed another request to capture")
    };
    let response = {
        *kind = "capture";
        match resolve_backend(backends, &pane) {
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
    };
    Ok(Flow::Respond(response))
}
