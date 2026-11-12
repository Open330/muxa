//! Native session backend requests.

#![allow(clippy::wildcard_imports)] // the arms name the IPC module's items as `dispatch.rs` does

use super::*;

#[allow(clippy::too_many_lines)] // one match arm per request in this family
pub(super) async fn handle(
    ctx: &ServerCtx,
    conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let sessions = &ctx.sessions;
    let agent_directory = &ctx.agent_directory;
    let stopping = &mut conn.stopping;
    let kind = &mut turn.kind;
    let response = match body {
        RequestBody::SpawnSession {
            command,
            args,
            env,
            cwd,
            name,
            cols,
            rows,
        } => {
            *kind = "spawn_session";
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
            *kind = "list_sessions";
            match sessions.list_sessions_checked() {
                Ok(list) => Response::with_sessions(list),
                Err(error) => Response::err(error.to_string()),
            }
        }
        RequestBody::CaptureSession { session_id } => {
            *kind = "capture_session";
            match sessions.capture(&session_id) {
                Ok(snapshot) => Response::with_terminal(snapshot),
                Err(e) => Response::err(e.to_string()),
            }
        }
        RequestBody::ReadSession { session_id, offset } => {
            *kind = "read_session";
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
            *kind = "read_session_wait";
            let sessions = Arc::clone(sessions);
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
            let Some(joined) = until_server_shutdown(stopping, wait).await else {
                return Ok(Flow::Close);
            };
            match joined {
                Ok(Ok(output)) => Response::with_output(output),
                Ok(Err(e)) => Response::err(e.to_string()),
                Err(e) => Response::err(format!("session wait task failed: {e}")),
            }
        }
        RequestBody::WriteSession { session_id, data } => {
            *kind = "write_session";
            match sessions.send_input(&session_id, data.as_bytes()) {
                Ok(()) => Response::ok(),
                Err(e) => Response::err(e.to_string()),
            }
        }
        RequestBody::WriteSessionBytes {
            session_id,
            data_base64,
        } => {
            *kind = "write_session_bytes";
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
            *kind = "resize_session";
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
            *kind = "set_session_attached";
            match sessions.set_attached(&session_id, client_id.as_deref(), attached) {
                Ok(()) => Response::ok(),
                Err(e) => Response::err(e.to_string()),
            }
        }
        RequestBody::TerminateSession { session_id } => {
            *kind = "terminate_session";
            match sessions.terminate(&session_id) {
                Ok(()) => {
                    agent_directory.invalidate();
                    Response::ok()
                }
                Err(e) => Response::err(e.to_string()),
            }
        }
        other => unreachable!("dispatch routed {other:?} to sessions"),
    };
    Ok(Flow::Respond(response))
}
