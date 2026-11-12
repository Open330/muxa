//! Work launch, work commands, agent start, mux snapshots and work compose.

#![allow(clippy::wildcard_imports)] // the arms name the IPC module's items as `dispatch.rs` does

use super::*;

#[allow(clippy::too_many_lines)] // one match arm per request in this family
pub(super) async fn handle(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let ask = &ctx.ask;
    let work_up = &ctx.work_up;
    let mux_snapshots = &ctx.mux_snapshots;
    let kind = &mut turn.kind;
    let response = match body {
        RequestBody::WorkUp { request } => {
            *kind = "work_up";
            match work_up.start(request).await {
                Ok(operation) => Response::with_work_operation(operation),
                Err(error) => Response::err(error),
            }
        }
        RequestBody::WorkUpStatus { operation_id } => {
            *kind = "work_up_status";
            match work_up.status(&operation_id).await {
                Some(operation) => Response::with_work_operation(operation),
                None => Response::err(format!("Work operation {operation_id:?} was not found")),
            }
        }
        RequestBody::WorkCommand { host, args, stdin } => {
            *kind = "work_command";
            match work_up.command(host, args, stdin).await {
                Ok(output) => Response::with_work_command(output),
                Err(error) => Response::err(error),
            }
        }
        // WS-B: new agent
        RequestBody::AgentStart { request } => {
            *kind = "agent_start";
            match work_up.agent_start(request).await {
                Ok(result) => Response::with_agent_start(result),
                Err(error) => Response::err(error),
            }
        }
        // WS-F: snapshot
        RequestBody::MuxSnapshotList => {
            *kind = "mux_snapshot_list";
            mux_snapshot_response(mux_snapshots.list().await)
        }
        RequestBody::MuxSnapshotSave => {
            *kind = "mux_snapshot_save";
            mux_snapshot_response(mux_snapshots.save().await)
        }
        RequestBody::MuxSnapshotPlan { id, only_missing } => {
            *kind = "mux_snapshot_plan";
            mux_snapshot_response(mux_snapshots.plan(&id, only_missing).await)
        }
        RequestBody::MuxSnapshotDelete { id } => {
            *kind = "mux_snapshot_delete";
            mux_snapshot_response(mux_snapshots.delete(&id).await)
        }
        RequestBody::MuxSnapshotRestore {
            id,
            layout_only,
            only_missing,
            continue_text,
        } => {
            *kind = "mux_snapshot_restore";
            match mux_snapshots
                .restore(&id, layout_only, only_missing, continue_text.as_deref())
                .await
            {
                Ok(operation) => Response::with_mux_snapshot_operation(operation),
                Err(error) => Response::err(error.to_string()),
            }
        }
        RequestBody::MuxSnapshotRestoreStatus { operation_id } => {
            *kind = "mux_snapshot_restore_status";
            match mux_snapshots.status(&operation_id).await {
                Some(operation) => Response::with_mux_snapshot_operation(operation),
                None => Response::err(format!("snapshot restore {operation_id:?} was not found")),
            }
        }
        // END WS-F: snapshot
        RequestBody::WorkCompose {
            description,
            agent,
            current,
            credential,
        } => {
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
        }
        other => unreachable!("dispatch routed {other:?} to work_ops"),
    };
    Ok(Flow::Respond(response))
}
