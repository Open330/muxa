//! Fleet snapshots, commands and remote reply waits.

#![allow(clippy::wildcard_imports)] // the arms name the IPC module's items as `dispatch.rs` does

use super::*;

pub(super) async fn handle(
    ctx: &ServerCtx,
    conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let fleet = &ctx.fleet;
    let reader = &mut conn.reader;
    let stopping = &mut conn.stopping;
    let kind = &mut turn.kind;
    let response = match body {
        RequestBody::FleetSnapshot { selector } => {
            *kind = "fleet_snapshot";
            match fleet {
                Some(fleet) => match selector
                    .as_deref()
                    .map(str::parse::<LabelSelector>)
                    .transpose()
                {
                    Ok(selector) => {
                        Response::with_fleet(fleet.store.snapshot_selected(selector.as_ref()).await)
                    }
                    Err(error) => Response::err(format!("invalid label selector: {error}")),
                },
                None => Response::err("fleet is not enabled in tendd"),
            }
        }
        RequestBody::FleetCommand { host, operation } => {
            *kind = "fleet_command";
            let invalid = match &operation {
                FleetOperation::CollaborationSend { request, .. } => {
                    request.validate_fleet_contract().err()
                }
                // A person's decision on a result, taken only through the
                // dashboard's operator-authenticated, audited path: over
                // this socket an agent could accept its own work elsewhere.
                FleetOperation::ReviewDispose { .. } => Some(
                    "review decisions over Fleet are taken from the dashboard's Needs you page"
                        .into(),
                ),
                _ => None,
            };
            if let Some(error) = invalid {
                Response::err(error)
            } else {
                match fleet {
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
            *kind = "fleet_wait_reply";
            let response = async {
                match fleet {
                    Some(fleet) => match fleet
                        .wait_for_reply(
                            host,
                            pane,
                            request_id,
                            Duration::from_secs(timeout_secs.clamp(1, MAX_COLLABORATION_WAIT_SECS)),
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
                _ = reader.fill_buf() => return Ok(Flow::Close),
                _ = stopping.wait_for(|value| *value) => return Ok(Flow::Close),
            }
        }
        other => unreachable!("dispatch routed {other:?} to fleet_ops"),
    };
    Ok(Flow::Respond(response))
}
