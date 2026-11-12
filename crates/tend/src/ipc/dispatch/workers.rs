//! Worker drains and the collaboration worker pool.

#![allow(clippy::wildcard_imports)] // the arms name the IPC module's items as `dispatch.rs` does

use super::*;

pub(super) async fn handle(
    ctx: &ServerCtx,
    conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    match body {
        body @ (RequestBody::WorkerDrainPlan { .. }
        | RequestBody::WorkerDrainAcquire { .. }
        | RequestBody::WorkerDrainRenew { .. }
        | RequestBody::WorkerDrainStop { .. }
        | RequestBody::WorkerDrainFinish { .. }
        | RequestBody::WorkerDrains { .. }) => worker_drain(ctx, conn, turn, body).await,
        body @ RequestBody::CollaborationWorkerSpawns => {
            collaboration_worker_spawns(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationWorkerSpawnRelease { .. } => {
            collaboration_worker_spawn_release(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationWorkerAcquire { .. } => {
            collaboration_worker_acquire(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationWorkerSpawn { .. } => {
            collaboration_worker_spawn(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationWorkerLaunch { .. } => {
            collaboration_worker_launch(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationWorkers { .. } => {
            collaboration_workers(ctx, conn, turn, body).await
        }
        other => unreachable!("dispatch routed {other:?} to workers"),
    }
}

#[allow(clippy::too_many_lines)] // one request, moved whole from the family match
async fn worker_drain(
    ctx: &ServerCtx,
    conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let backends = &ctx.backends;
    let collaboration = &ctx.collaboration;
    let collaboration_actor = &mut conn.collaboration_actor;
    let kind = &mut turn.kind;
    let pending_audit = &mut turn.pending_audit;
    let drain_request @ (RequestBody::WorkerDrainPlan { .. }
    | RequestBody::WorkerDrainAcquire { .. }
    | RequestBody::WorkerDrainRenew { .. }
    | RequestBody::WorkerDrainStop { .. }
    | RequestBody::WorkerDrainFinish { .. }
    | RequestBody::WorkerDrains { .. }) = body
    else {
        unreachable!("dispatch routed another request to worker_drain")
    };
    let response = {
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
                Some(serde_json::json!({"token":token,"operation_id":operation_id}).to_string()),
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
        *kind = match operation {
            CollaborationAuditOperation::WorkerDrainPlan => "worker_drain_plan",
            CollaborationAuditOperation::WorkerDrainAcquire => "worker_drain_acquire",
            CollaborationAuditOperation::WorkerDrainRenew => "worker_drain_renew",
            CollaborationAuditOperation::WorkerDrainStop => "worker_drain_stop",
            CollaborationAuditOperation::WorkerDrainFinish => "worker_drain_finish",
            _ => "worker_drains",
        };
        let origin = origin.clone();
        collaboration_actor.observe_pane(backends).await;
        let mut audit_context = CollaborationAuditContext::new(operation, origin.clone());
        audit_context.target = target;
        let _gate = if let Some(pane) = guard_pane {
            Some(collaboration.worker_action_guard(&pane).await)
        } else {
            None
        };
        // The acquire/finish gate precedes this scan; cached caller
        // provenance must never become stop authority.
        let topology = collaboration_participants(store, backends, collaboration, None, None).await;
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
                                worker_drain_control::settle_row(store, backends, &token).await;
                            }
                            response.worker_drain = Some(view);
                        }
                        RequestBody::WorkerDrains { .. } => {
                            response.worker_drains = Some(collaboration.worker_drains().await);
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
        *pending_audit = Some(collaboration_audit_entry(
            collaboration_actor,
            audit_context,
            &response,
        ));
        response
    };
    Ok(Flow::Respond(response))
}

async fn collaboration_worker_spawns(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let collaboration = &ctx.collaboration;
    let kind = &mut turn.kind;
    let RequestBody::CollaborationWorkerSpawns = body else {
        unreachable!("dispatch routed another request to collaboration_worker_spawns")
    };
    let response = {
        *kind = "collaboration_worker_spawns";
        let mut response = Response::ok();
        response.worker_spawns = Some(collaboration.worker_spawn_records().await);
        response
    };
    Ok(Flow::Respond(response))
}

async fn collaboration_worker_spawn_release(
    ctx: &ServerCtx,
    conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let backends = &ctx.backends;
    let collaboration = &ctx.collaboration;
    let collaboration_actor = &mut conn.collaboration_actor;
    let kind = &mut turn.kind;
    let pending_audit = &mut turn.pending_audit;
    let RequestBody::CollaborationWorkerSpawnRelease {
        origin,
        launch_id,
        reason,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_worker_spawn_release")
    };
    let response = {
        *kind = "collaboration_worker_spawn_release";
        collaboration_actor.observe_pane(backends).await;
        let mut audit_context = CollaborationAuditContext::new(
            CollaborationAuditOperation::WorkerSpawnRelease,
            origin.clone(),
        );
        audit_context.target = Some(launch_id.clone());
        audit_context.release_reason = Some(reason.chars().take(512).collect());
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            None,
            collaboration_actor.take_request_panes(),
        )
        .await;
        let response = match topology.resolve_origin(&origin) {
            Ok(caller) if caller.console => match collaboration
                .release_worker_spawn(&caller, &launch_id, &reason, &topology.panes)
                .await
            {
                Ok(()) => Response::ok(),
                Err(error) => Response::err(error.to_string()),
            },
            Ok(_) => Response::err(
                "operator release requires a console origin; owners use the Release action",
            ),
            Err(error) => Response::err(error.to_string()),
        };
        *pending_audit = Some(collaboration_audit_entry(
            collaboration_actor,
            audit_context,
            &response,
        ));
        response
    };
    Ok(Flow::Respond(response))
}

async fn collaboration_worker_acquire(
    ctx: &ServerCtx,
    conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let backends = &ctx.backends;
    let collaboration = &ctx.collaboration;
    let collaboration_actor = &mut conn.collaboration_actor;
    let kind = &mut turn.kind;
    let RequestBody::CollaborationWorkerAcquire { origin, spec } = body else {
        unreachable!("dispatch routed another request to collaboration_worker_acquire")
    };
    let response = {
        *kind = "collaboration_worker_acquire";
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
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
    };
    Ok(Flow::Respond(response))
}

async fn collaboration_worker_spawn(
    ctx: &ServerCtx,
    conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let backends = &ctx.backends;
    let collaboration = &ctx.collaboration;
    let collaboration_actor = &mut conn.collaboration_actor;
    let kind = &mut turn.kind;
    let pending_audit = &mut turn.pending_audit;
    let RequestBody::CollaborationWorkerSpawn { ticket, action } = body else {
        unreachable!("dispatch routed another request to collaboration_worker_spawn")
    };
    let response = {
        *kind = "collaboration_worker_spawn";
        let audit_context = if let collaboration::WorkerSpawnAction::Release { reason } = &action {
            collaboration_actor.observe_pane(backends).await;
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
            store,
            backends,
            collaboration,
            ticket.origin.agent_session_id.as_deref(),
            collaboration_actor.take_request_panes(),
        )
        .await;
        let response = match topology.resolve_origin(&ticket.origin) {
            Ok(caller) => match collaboration
                .update_worker_spawn(&caller, &ticket.launch_id, action, &topology.panes)
                .await
            {
                Ok(()) => Response::ok(),
                Err(error) => Response::err(error.to_string()),
            },
            Err(error) => Response::err(error.to_string()),
        };
        if let Some(context) = audit_context {
            *pending_audit = Some(collaboration_audit_entry(
                collaboration_actor,
                context,
                &response,
            ));
        }
        response
    };
    Ok(Flow::Respond(response))
}

async fn collaboration_worker_launch(
    ctx: &ServerCtx,
    conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let backends = &ctx.backends;
    let collaboration = &ctx.collaboration;
    let collaboration_actor = &mut conn.collaboration_actor;
    let kind = &mut turn.kind;
    let RequestBody::CollaborationWorkerLaunch {
        pane,
        socket,
        agent_kind,
        launch_id,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_worker_launch")
    };
    let response = {
        *kind = "collaboration_worker_launch";
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
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
                if candidates
                    .iter()
                    .all(|p| p.pane_pid == first.pane_pid && p.window_id == first.window_id) =>
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
    };
    Ok(Flow::Respond(response))
}

async fn collaboration_workers(
    ctx: &ServerCtx,
    conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let backends = &ctx.backends;
    let collaboration = &ctx.collaboration;
    let collaboration_actor = &mut conn.collaboration_actor;
    let kind = &mut turn.kind;
    let RequestBody::CollaborationWorkers {
        idle_threshold_secs,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_workers")
    };
    let response = {
        *kind = "collaboration_workers";
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
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
    };
    Ok(Flow::Respond(response))
}
