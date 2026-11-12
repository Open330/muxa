//! The Ask conversation and its providers, including forwarding shared Ask
//! requests to the configured coordinator.

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
    let kind = &mut turn.kind;
    let response = match body {
        RequestBody::AskOneShot {
            prompt,
            agent,
            credential,
        } => {
            *kind = "ask_one_shot";
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
        RequestBody::AskSend { prompt, credential } => {
            *kind = "ask_send";
            match ask.ask_with_credential(&prompt, credential).await {
                Ok(entry) => Response::with_ask_entry(entry),
                Err(error) => Response::err(error.to_string()),
            }
        }
        RequestBody::AskSendNew { prompt, credential } => {
            *kind = "ask_send_new";
            match ask
                .ask_in_new_conversation_with_credential(&prompt, credential)
                .await
            {
                Ok(entry) => Response::with_ask_entry(entry),
                Err(error) => Response::err(error.to_string()),
            }
        }
        RequestBody::AskStatus {} => {
            *kind = "ask_status";
            Response::with_ask_status(ask.enabled())
        }
        RequestBody::AskList {
            id,
            conversation_id,
        } => {
            *kind = "ask_list";
            Response::with_ask_entries(
                ask.list_filtered(id.as_deref(), conversation_id.as_deref())
                    .await,
            )
        }
        RequestBody::AskConversationList {} => {
            *kind = "ask_conversation_list";
            Response::with_ask_conversations(
                ask.list_conversations().await,
                ask.active_conversation().await,
            )
        }
        RequestBody::AskConversationSelect { conversation_id } => {
            *kind = "ask_conversation_select";
            match ask.select_conversation(&conversation_id).await {
                Ok(conversation) => Response::with_ask_conversation(conversation),
                Err(error) => Response::err(error.to_string()),
            }
        }
        RequestBody::AskAgent { agent } => {
            *kind = "ask_agent";
            match agent {
                Some(name) => match ask.set_agent(&name).await {
                    Ok(label) => Response::with_ask_agent(label),
                    Err(error) => Response::err(error.to_string()),
                },
                None => Response::with_ask_agent(ask.agent().await),
            }
        }
        RequestBody::AskReset {} => {
            *kind = "ask_reset";
            Response::with_ask_conversation(ask.reset_thread().await)
        }
        RequestBody::AskClear {} => {
            *kind = "ask_clear";
            Response::with_pruned(ask.clear_history().await)
        }
        RequestBody::AskDelete { id } => {
            *kind = "ask_delete";
            Response::with_pruned(usize::from(ask.delete_history_entry(&id).await))
        }
        RequestBody::AskProviders {} => {
            *kind = "ask_providers";
            Response::with_ask_providers(ask.providers().await)
        }
        RequestBody::AskProviderConfigure {
            provider,
            title,
            model,
            api_key_env,
            executable,
        } => {
            *kind = "ask_provider_configure";
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
            *kind = "ask_provider_add";
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
            *kind = "ask_provider_default";
            match ask.set_default_agent(&provider).await {
                Ok(providers) => Response::with_ask_providers(providers),
                Err(error) => Response::err(error.to_string()),
            }
        }
        RequestBody::AskProviderRemove { id } => {
            *kind = "ask_provider_remove";
            match ask.remove_provider(&id).await {
                Ok(providers) => Response::with_ask_providers(providers),
                Err(error) => Response::err(error.to_string()),
            }
        }
        other => unreachable!("dispatch routed {other:?} to ask_ops"),
    };
    Ok(Flow::Respond(response))
}

/// The largest prompt `ask_one_shot` accepts.
const ASK_ONE_SHOT_MAX_PROMPT_BYTES: usize = 256 * 1024;

/// The wire kind of a request that belongs to the one shared Ask, or `None`.
/// Matches [`crate::orchestration::shared_ask_kind`] plus `ask_subscribe`.
pub(super) fn shared_kind(body: &RequestBody) -> Option<&'static str> {
    Some(match body {
        RequestBody::AskSend { .. } => "ask_send",
        RequestBody::AskSendNew { .. } => "ask_send_new",
        RequestBody::AskStatus { .. } => "ask_status",
        RequestBody::AskList { .. } => "ask_list",
        RequestBody::AskConversationList { .. } => "ask_conversation_list",
        RequestBody::AskConversationSelect { .. } => "ask_conversation_select",
        RequestBody::AskAgent { .. } => "ask_agent",
        RequestBody::AskReset { .. } => "ask_reset",
        RequestBody::AskClear { .. } => "ask_clear",
        RequestBody::AskDelete { .. } => "ask_delete",
        RequestBody::AskProviders { .. } => "ask_providers",
        RequestBody::AskSubscribe => "ask_subscribe",
        _ => return None,
    })
}

/// Forward a shared Ask request to the configured coordinator. Returns the
/// encoded reply line when this host is not the coordinator (or the routing
/// configuration cannot be read), and `None` when the request is served here.
pub(super) async fn forward_shared(
    ctx: &ServerCtx,
    negotiated: Option<u32>,
    ask_kind: &str,
    line: &str,
) -> Result<Option<Vec<u8>>, RuntimeError> {
    let policy = if let Ok(config) = ctx
        .config_path
        .as_deref()
        .map(|p| crate::Config::load_or_default(Some(p)))
        .transpose()
    {
        config.map(|c| c.orchestration)
    } else {
        let response = Response::err("cannot load coordinator routing configuration; refusing to fall back to a local Ask conversation");
        return Ok(Some(encode_line(
            &response,
            negotiated.unwrap_or(PROTOCOL_VERSION),
        )?));
    };
    let Some(host) = policy
        .filter(|p| p.enabled)
        .and_then(|p| p.coordinator)
        .filter(|h| h != "local")
    else {
        return Ok(None);
    };
    let result = if ask_kind == "ask_subscribe" {
        Err("shared Ask streaming requires a direct coordinator connection; use ask_list for snapshots".to_string())
    } else {
        ctx.work_up
            .command(
                Some(host),
                vec!["work".into(), "fleet-ask".into()],
                Some(line.into()),
            )
            .await
            .and_then(|o| {
                if o.exit_code == 0 {
                    serde_json::from_str::<serde_json::Value>(&o.stdout).map_err(|e| e.to_string())
                } else {
                    Err(o.stderr)
                }
            })
    };
    let response = result.unwrap_or_else(
        |error| serde_json::json!({"ok":false,"protocol":PROTOCOL_VERSION,"error":error}),
    );
    Ok(Some(encode_line(
        &response,
        negotiated.unwrap_or(PROTOCOL_VERSION),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_kind_matches_the_wire_kind() {
        for kind in [
            "ask_send",
            "ask_send_new",
            "ask_status",
            "ask_list",
            "ask_conversation_list",
            "ask_conversation_select",
            "ask_agent",
            "ask_reset",
            "ask_clear",
            "ask_delete",
            "ask_providers",
            "ask_subscribe",
        ] {
            let line = serde_json::json!({
                "kind": kind,
                "prompt": "p",
                "conversation_id": "c",
                "id": "i",
            });
            let body: RequestBody = serde_json::from_value(line).unwrap();
            assert_eq!(shared_kind(&body), Some(kind));
            assert!(kind == "ask_subscribe" || crate::orchestration::shared_ask_kind(kind));
        }
        assert_eq!(shared_kind(&RequestBody::Health), None);
    }
}
