//! Work launch, work commands, agent start, mux snapshots and work compose.

#![allow(clippy::wildcard_imports)] // the arms name the IPC module's items as `dispatch.rs` does

use super::*;

pub(super) async fn handle(
    ctx: &ServerCtx,
    conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    match body {
        body @ RequestBody::WorkUp { .. } => work_up(ctx, conn, turn, body).await,
        body @ RequestBody::WorkUpStatus { .. } => work_up_status(ctx, conn, turn, body).await,
        body @ RequestBody::WorkCommand { .. } => work_command(ctx, conn, turn, body).await,
        body @ RequestBody::AgentStart { .. } => agent_start(ctx, conn, turn, body).await,
        body @ RequestBody::MuxSnapshotList => mux_snapshot_list(ctx, conn, turn, body).await,
        body @ RequestBody::MuxSnapshotSave => mux_snapshot_save(ctx, conn, turn, body).await,
        body @ RequestBody::MuxSnapshotPlan { .. } => {
            mux_snapshot_plan(ctx, conn, turn, body).await
        }
        body @ RequestBody::MuxSnapshotDelete { .. } => {
            mux_snapshot_delete(ctx, conn, turn, body).await
        }
        body @ RequestBody::MuxSnapshotRestore { .. } => {
            mux_snapshot_restore(ctx, conn, turn, body).await
        }
        body @ RequestBody::MuxSnapshotRestoreStatus { .. } => {
            mux_snapshot_restore_status(ctx, conn, turn, body).await
        }
        body @ RequestBody::WorkCompose { .. } => work_compose(ctx, conn, turn, body).await,
        other => unreachable!("dispatch routed {other:?} to work_ops"),
    }
}

async fn work_up(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let work_up = &ctx.work_up;
    let kind = &mut turn.kind;
    let RequestBody::WorkUp { request } = body else {
        unreachable!("dispatch routed another request to work_up")
    };
    let response = {
        *kind = "work_up";
        match work_up.start(request).await {
            Ok(operation) => Response::with_work_operation(operation),
            Err(error) => Response::err(error),
        }
    };
    Ok(Flow::Respond(response))
}

async fn work_up_status(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let work_up = &ctx.work_up;
    let kind = &mut turn.kind;
    let RequestBody::WorkUpStatus { operation_id } = body else {
        unreachable!("dispatch routed another request to work_up_status")
    };
    let response = {
        *kind = "work_up_status";
        match work_up.status(&operation_id).await {
            Some(operation) => Response::with_work_operation(operation),
            None => Response::err(format!("Work operation {operation_id:?} was not found")),
        }
    };
    Ok(Flow::Respond(response))
}

async fn work_command(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let work_up = &ctx.work_up;
    let kind = &mut turn.kind;
    let RequestBody::WorkCommand { host, args, stdin } = body else {
        unreachable!("dispatch routed another request to work_command")
    };
    let response = {
        *kind = "work_command";
        match work_up.command(host, args, stdin).await {
            Ok(output) => Response::with_work_command(output),
            Err(error) => Response::err(error),
        }
    };
    Ok(Flow::Respond(response))
}

// WS-B: new agent
async fn agent_start(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let work_up = &ctx.work_up;
    let kind = &mut turn.kind;
    let RequestBody::AgentStart { request } = body else {
        unreachable!("dispatch routed another request to agent_start")
    };
    let response = {
        *kind = "agent_start";
        match work_up.agent_start(request).await {
            Ok(result) => Response::with_agent_start(result),
            Err(error) => Response::err(error),
        }
    };
    Ok(Flow::Respond(response))
}

// WS-F: snapshot
async fn mux_snapshot_list(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let mux_snapshots = &ctx.mux_snapshots;
    let kind = &mut turn.kind;
    let RequestBody::MuxSnapshotList = body else {
        unreachable!("dispatch routed another request to mux_snapshot_list")
    };
    let response = {
        *kind = "mux_snapshot_list";
        mux_snapshot_response(mux_snapshots.list().await)
    };
    Ok(Flow::Respond(response))
}

async fn mux_snapshot_save(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let mux_snapshots = &ctx.mux_snapshots;
    let kind = &mut turn.kind;
    let RequestBody::MuxSnapshotSave = body else {
        unreachable!("dispatch routed another request to mux_snapshot_save")
    };
    let response = {
        *kind = "mux_snapshot_save";
        mux_snapshot_response(mux_snapshots.save().await)
    };
    Ok(Flow::Respond(response))
}

async fn mux_snapshot_plan(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let mux_snapshots = &ctx.mux_snapshots;
    let kind = &mut turn.kind;
    let RequestBody::MuxSnapshotPlan { id, only_missing } = body else {
        unreachable!("dispatch routed another request to mux_snapshot_plan")
    };
    let response = {
        *kind = "mux_snapshot_plan";
        mux_snapshot_response(mux_snapshots.plan(&id, only_missing).await)
    };
    Ok(Flow::Respond(response))
}

async fn mux_snapshot_delete(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let mux_snapshots = &ctx.mux_snapshots;
    let kind = &mut turn.kind;
    let RequestBody::MuxSnapshotDelete { id } = body else {
        unreachable!("dispatch routed another request to mux_snapshot_delete")
    };
    let response = {
        *kind = "mux_snapshot_delete";
        mux_snapshot_response(mux_snapshots.delete(&id).await)
    };
    Ok(Flow::Respond(response))
}

async fn mux_snapshot_restore(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let mux_snapshots = &ctx.mux_snapshots;
    let kind = &mut turn.kind;
    let RequestBody::MuxSnapshotRestore {
        id,
        layout_only,
        only_missing,
        continue_text,
    } = body
    else {
        unreachable!("dispatch routed another request to mux_snapshot_restore")
    };
    let response = {
        *kind = "mux_snapshot_restore";
        match mux_snapshots
            .restore(&id, layout_only, only_missing, continue_text.as_deref())
            .await
        {
            Ok(operation) => Response::with_mux_snapshot_operation(operation),
            Err(error) => Response::err(error.to_string()),
        }
    };
    Ok(Flow::Respond(response))
}

async fn mux_snapshot_restore_status(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let mux_snapshots = &ctx.mux_snapshots;
    let kind = &mut turn.kind;
    let RequestBody::MuxSnapshotRestoreStatus { operation_id } = body else {
        unreachable!("dispatch routed another request to mux_snapshot_restore_status")
    };
    let response = {
        *kind = "mux_snapshot_restore_status";
        match mux_snapshots.status(&operation_id).await {
            Some(operation) => Response::with_mux_snapshot_operation(operation),
            None => Response::err(format!("snapshot restore {operation_id:?} was not found")),
        }
    };
    Ok(Flow::Respond(response))
}

// END WS-F: snapshot
async fn work_compose(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let ask = &ctx.ask;
    let kind = &mut turn.kind;
    let RequestBody::WorkCompose {
        description,
        agent,
        current,
        credential,
    } = body
    else {
        unreachable!("dispatch routed another request to work_compose")
    };
    let response = {
        *kind = "work_compose";
        let request = WorkComposeRequest {
            description,
            agent,
            current,
        };
        let installed = work_compose::installed_programs();
        // The drafting turn is read-only whatever `[ask]` says:
        // a model describing a pipeline must not edit files.
        let drafter = Arc::clone(ask);
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
    };
    Ok(Flow::Respond(response))
}

/// Who is declaring a Work's acceptance and checks: the agent whose pane
/// the caller runs in, as tendd observes the caller's process, or else the
/// person.
pub(super) async fn declarer(ctx: &ServerCtx, conn: &mut Conn) -> crate::work_verify::Declarer {
    conn.collaboration_actor.observe_pane(&ctx.backends).await;
    let Some(pane) = conn.collaboration_actor.observed_pane.clone() else {
        return crate::work_verify::Declarer::Operator;
    };
    ctx.store
        .by_pane(&pane)
        .await
        .into_iter()
        .filter(|agent| agent.state != crate::AgentState::Stopped)
        .filter(|agent| {
            !agent
                .session_id
                .starts_with(crate::state::SYNTHETIC_SESSION_PREFIX)
        })
        .max_by_key(|agent| agent.last_activity_at)
        .map_or(crate::work_verify::Declarer::Operator, |agent| {
            crate::work_verify::Declarer::Agent {
                session_id: agent.session_id,
            }
        })
}
