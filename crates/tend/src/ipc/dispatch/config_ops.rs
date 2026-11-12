//! Reading and replacing `config.toml`.

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
        body @ RequestBody::ConfigRead { .. } => config_read(ctx, conn, turn, body).await,
        body @ RequestBody::ConfigOrchestrationRead { .. } => {
            config_orchestration_read(ctx, conn, turn, body).await
        }
        body @ RequestBody::ConfigOrchestrationWrite { .. } => {
            config_orchestration_write(ctx, conn, turn, body).await
        }
        body @ RequestBody::ConfigLaunchRead { .. } => {
            config_launch_read(ctx, conn, turn, body).await
        }
        body @ RequestBody::ConfigLaunchWrite { .. } => {
            config_launch_write(ctx, conn, turn, body).await
        }
        body @ RequestBody::ConfigWrite { .. } => config_write(ctx, conn, turn, body).await,
        other => unreachable!("dispatch routed {other:?} to config_ops"),
    }
}

async fn config_read(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let config_path = &ctx.config_path;
    let kind = &mut turn.kind;
    let RequestBody::ConfigRead {} = body else {
        unreachable!("dispatch routed another request to config_read")
    };
    let response = {
        *kind = "config_read";
        match config_path.as_deref() {
            Some(path) => match crate::config_file::read(path) {
                Ok(document) => Response::with_config(document),
                Err(error) => Response::err(error.to_string()),
            },
            None => Response::err(NO_CONFIG_PATH.to_string()),
        }
    };
    Ok(Flow::Respond(response))
}

async fn config_orchestration_read(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let config_path = &ctx.config_path;
    let kind = &mut turn.kind;
    let RequestBody::ConfigOrchestrationRead {} = body else {
        unreachable!("dispatch routed another request to config_orchestration_read")
    };
    let response = {
        *kind = "config_orchestration_read";
        orchestration_config_response(config_path.as_deref(), None)
    };
    Ok(Flow::Respond(response))
}

async fn config_orchestration_write(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let config_path = &ctx.config_path;
    let kind = &mut turn.kind;
    let RequestBody::ConfigOrchestrationWrite {
        expected_text,
        settings,
    } = body
    else {
        unreachable!("dispatch routed another request to config_orchestration_write")
    };
    let response = {
        *kind = "config_orchestration_write";
        orchestration_config_response(config_path.as_deref(), Some((&expected_text, &settings)))
    };
    Ok(Flow::Respond(response))
}

async fn config_launch_read(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let config_path = &ctx.config_path;
    let kind = &mut turn.kind;
    let RequestBody::ConfigLaunchRead {} = body else {
        unreachable!("dispatch routed another request to config_launch_read")
    };
    let response = {
        *kind = "config_launch_read";
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
    };
    Ok(Flow::Respond(response))
}

async fn config_launch_write(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let config_path = &ctx.config_path;
    let kind = &mut turn.kind;
    let RequestBody::ConfigLaunchWrite {
        expected_text,
        edits,
    } = body
    else {
        unreachable!("dispatch routed another request to config_launch_write")
    };
    let response = {
        *kind = "config_launch_write";
        match config_path.as_deref() {
            Some(path) => match crate::config_file::write_launch(path, &expected_text, &edits) {
                Ok(document) => {
                    let mut response = Response::with_config(document.config);
                    response.launch = Some(document.launch);
                    response
                }
                Err(crate::config_file::ConfigFileError::Conflict { current }) => {
                    let mut response =
                        Response::err("config.toml changed; reload before saving launch options");
                    response.config = Some(crate::config_file::ConfigDocument {
                        path: path.to_path_buf(),
                        exists: true,
                        text: current,
                    });
                    response
                }
                Err(error) => Response::err(error.to_string()),
            },
            None => Response::err(NO_CONFIG_PATH.to_string()),
        }
    };
    Ok(Flow::Respond(response))
}

async fn config_write(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let config_path = &ctx.config_path;
    let kind = &mut turn.kind;
    let RequestBody::ConfigWrite {
        text,
        expected_text,
    } = body
    else {
        unreachable!("dispatch routed another request to config_write")
    };
    let response = {
        *kind = "config_write";
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
    };
    Ok(Flow::Respond(response))
}
