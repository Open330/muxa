//! Connection handshake and daemon lifecycle: `hello`, `health`, `stop`, `restart`.

#![allow(clippy::wildcard_imports)] // the arms name the IPC module's items as `dispatch.rs` does

use super::*;

pub(super) async fn handle(
    ctx: &ServerCtx,
    conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let collaboration = &ctx.collaboration;
    let restart = &ctx.restart;
    let swap = &ctx.swap;
    let negotiated = &mut conn.negotiated;
    let collaboration_actor = &mut conn.collaboration_actor;
    let kind = &mut turn.kind;
    let protocol = turn.protocol;
    let response = match body {
        RequestBody::Hello { client } => {
            *kind = "hello";
            let requested = if protocol == 0 {
                PROTOCOL_VERSION
            } else {
                protocol
            };
            if (MIN_PROTOCOL_VERSION..=PROTOCOL_VERSION).contains(&requested) {
                *negotiated = Some(requested);
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
                r.collaboration_error = collaboration.initialization_error().map(str::to_owned);
                r.protocol = requested;
                // Only a service that would take a job now is
                // advertised; status stays served regardless.
                if let Some(capabilities) = r.capabilities.as_mut() {
                    capabilities.push(SWAP_SERVICE_CAPABILITY);
                    capabilities.extend([SWAP_HEALTH_CAPABILITY, SWAP_HEALTH_MODULE_CAPABILITY]);
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
        RequestBody::Health => {
            *kind = "health";
            let mut response = Response::health();
            response.collaboration_error = collaboration.initialization_error().map(str::to_owned);
            response
        }
        RequestBody::Stop => {
            *kind = "stop";
            match restart {
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
            *kind = "restart";
            match match restart {
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
        other => unreachable!("dispatch routed {other:?} to daemon"),
    };
    Ok(Flow::Respond(response))
}
