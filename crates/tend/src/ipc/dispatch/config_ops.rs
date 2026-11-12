//! Reading and replacing `config.toml`.

#![allow(clippy::wildcard_imports)] // the arms name the IPC module's items as `dispatch.rs` does

use super::*;

#[allow(clippy::too_many_lines)] // one match arm per request in this family
#[allow(clippy::unused_async)] // same signature as every family; `route` awaits it
pub(super) async fn handle(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let config_path = &ctx.config_path;
    let kind = &mut turn.kind;
    let response = match body {
        RequestBody::ConfigRead {} => {
            *kind = "config_read";
            match config_path.as_deref() {
                Some(path) => match crate::config_file::read(path) {
                    Ok(document) => Response::with_config(document),
                    Err(error) => Response::err(error.to_string()),
                },
                None => Response::err(NO_CONFIG_PATH.to_string()),
            }
        }
        RequestBody::ConfigOrchestrationRead {} => {
            *kind = "config_orchestration_read";
            orchestration_config_response(config_path.as_deref(), None)
        }
        RequestBody::ConfigOrchestrationWrite {
            expected_text,
            settings,
        } => {
            *kind = "config_orchestration_write";
            orchestration_config_response(config_path.as_deref(), Some((&expected_text, &settings)))
        }
        RequestBody::ConfigLaunchRead {} => {
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
        }
        RequestBody::ConfigLaunchWrite {
            expected_text,
            edits,
        } => {
            *kind = "config_launch_write";
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
        }
        other => unreachable!("dispatch routed {other:?} to config_ops"),
    };
    Ok(Flow::Respond(response))
}
