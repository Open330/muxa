//! The Ask conversation and its providers, including forwarding shared Ask
//! requests to the configured coordinator.

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
        body @ RequestBody::AskOneShot { .. } => ask_one_shot(ctx, conn, turn, body).await,
        body @ RequestBody::AskSend { .. } => ask_send(ctx, conn, turn, body).await,
        body @ RequestBody::AskSendNew { .. } => ask_send_new(ctx, conn, turn, body).await,
        body @ RequestBody::AskStatus { .. } => ask_status(ctx, conn, turn, body).await,
        body @ RequestBody::AskList { .. } => ask_list(ctx, conn, turn, body).await,
        body @ RequestBody::AskConversationList { .. } => {
            ask_conversation_list(ctx, conn, turn, body).await
        }
        body @ RequestBody::AskConversationSelect { .. } => {
            ask_conversation_select(ctx, conn, turn, body).await
        }
        body @ RequestBody::AskAgent { .. } => ask_agent(ctx, conn, turn, body).await,
        body @ RequestBody::AskReset { .. } => ask_reset(ctx, conn, turn, body).await,
        body @ RequestBody::AskClear { .. } => ask_clear(ctx, conn, turn, body).await,
        body @ RequestBody::AskDelete { .. } => ask_delete(ctx, conn, turn, body).await,
        body @ RequestBody::AskProviders { .. } => ask_providers(ctx, conn, turn, body).await,
        body @ RequestBody::AskProviderConfigure { .. } => {
            ask_provider_configure(ctx, conn, turn, body).await
        }
        body @ RequestBody::AskProviderAdd { .. } => ask_provider_add(ctx, conn, turn, body).await,
        body @ RequestBody::AskProviderDefault { .. } => {
            ask_provider_default(ctx, conn, turn, body).await
        }
        body @ RequestBody::AskProviderRemove { .. } => {
            ask_provider_remove(ctx, conn, turn, body).await
        }
        other => unreachable!("dispatch routed {other:?} to ask_ops"),
    }
}

async fn ask_one_shot(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let ask = &ctx.ask;
    let kind = &mut turn.kind;
    let RequestBody::AskOneShot {
        prompt,
        agent,
        credential,
    } = body
    else {
        unreachable!("dispatch routed another request to ask_one_shot")
    };
    let response = {
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
    };
    Ok(Flow::Respond(response))
}

async fn ask_send(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let ask = &ctx.ask;
    let kind = &mut turn.kind;
    let RequestBody::AskSend { prompt, credential } = body else {
        unreachable!("dispatch routed another request to ask_send")
    };
    let response = {
        *kind = "ask_send";
        match ask.ask_with_credential(&prompt, credential).await {
            Ok(entry) => Response::with_ask_entry(entry),
            Err(error) => Response::err(error.to_string()),
        }
    };
    Ok(Flow::Respond(response))
}

async fn ask_send_new(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let ask = &ctx.ask;
    let kind = &mut turn.kind;
    let RequestBody::AskSendNew { prompt, credential } = body else {
        unreachable!("dispatch routed another request to ask_send_new")
    };
    let response = {
        *kind = "ask_send_new";
        match ask
            .ask_in_new_conversation_with_credential(&prompt, credential)
            .await
        {
            Ok(entry) => Response::with_ask_entry(entry),
            Err(error) => Response::err(error.to_string()),
        }
    };
    Ok(Flow::Respond(response))
}

async fn ask_status(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let ask = &ctx.ask;
    let kind = &mut turn.kind;
    let RequestBody::AskStatus {} = body else {
        unreachable!("dispatch routed another request to ask_status")
    };
    let response = {
        *kind = "ask_status";
        Response::with_ask_status(ask.enabled())
    };
    Ok(Flow::Respond(response))
}

async fn ask_list(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let ask = &ctx.ask;
    let kind = &mut turn.kind;
    let RequestBody::AskList {
        id,
        conversation_id,
    } = body
    else {
        unreachable!("dispatch routed another request to ask_list")
    };
    let response = {
        *kind = "ask_list";
        Response::with_ask_entries(
            ask.list_filtered(id.as_deref(), conversation_id.as_deref())
                .await,
        )
    };
    Ok(Flow::Respond(response))
}

async fn ask_conversation_list(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let ask = &ctx.ask;
    let kind = &mut turn.kind;
    let RequestBody::AskConversationList {} = body else {
        unreachable!("dispatch routed another request to ask_conversation_list")
    };
    let response = {
        *kind = "ask_conversation_list";
        Response::with_ask_conversations(
            ask.list_conversations().await,
            ask.active_conversation().await,
        )
    };
    Ok(Flow::Respond(response))
}

async fn ask_conversation_select(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let ask = &ctx.ask;
    let kind = &mut turn.kind;
    let RequestBody::AskConversationSelect { conversation_id } = body else {
        unreachable!("dispatch routed another request to ask_conversation_select")
    };
    let response = {
        *kind = "ask_conversation_select";
        match ask.select_conversation(&conversation_id).await {
            Ok(conversation) => Response::with_ask_conversation(conversation),
            Err(error) => Response::err(error.to_string()),
        }
    };
    Ok(Flow::Respond(response))
}

async fn ask_agent(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let ask = &ctx.ask;
    let kind = &mut turn.kind;
    let RequestBody::AskAgent { agent } = body else {
        unreachable!("dispatch routed another request to ask_agent")
    };
    let response = {
        *kind = "ask_agent";
        match agent {
            Some(name) => match ask.set_agent(&name).await {
                Ok(label) => Response::with_ask_agent(label),
                Err(error) => Response::err(error.to_string()),
            },
            None => Response::with_ask_agent(ask.agent().await),
        }
    };
    Ok(Flow::Respond(response))
}

async fn ask_reset(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let ask = &ctx.ask;
    let kind = &mut turn.kind;
    let RequestBody::AskReset {} = body else {
        unreachable!("dispatch routed another request to ask_reset")
    };
    let response = {
        *kind = "ask_reset";
        Response::with_ask_conversation(ask.reset_thread().await)
    };
    Ok(Flow::Respond(response))
}

async fn ask_clear(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let ask = &ctx.ask;
    let kind = &mut turn.kind;
    let RequestBody::AskClear {} = body else {
        unreachable!("dispatch routed another request to ask_clear")
    };
    let response = {
        *kind = "ask_clear";
        Response::with_pruned(ask.clear_history().await)
    };
    Ok(Flow::Respond(response))
}

async fn ask_delete(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let ask = &ctx.ask;
    let kind = &mut turn.kind;
    let RequestBody::AskDelete { id } = body else {
        unreachable!("dispatch routed another request to ask_delete")
    };
    let response = {
        *kind = "ask_delete";
        Response::with_pruned(usize::from(ask.delete_history_entry(&id).await))
    };
    Ok(Flow::Respond(response))
}

async fn ask_providers(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let ask = &ctx.ask;
    let kind = &mut turn.kind;
    let RequestBody::AskProviders {} = body else {
        unreachable!("dispatch routed another request to ask_providers")
    };
    let response = {
        *kind = "ask_providers";
        Response::with_ask_providers(ask.providers().await)
    };
    Ok(Flow::Respond(response))
}

async fn ask_provider_configure(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let ask = &ctx.ask;
    let kind = &mut turn.kind;
    let RequestBody::AskProviderConfigure {
        provider,
        title,
        model,
        api_key_env,
        executable,
    } = body
    else {
        unreachable!("dispatch routed another request to ask_provider_configure")
    };
    let response = {
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
    };
    Ok(Flow::Respond(response))
}

async fn ask_provider_add(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let ask = &ctx.ask;
    let kind = &mut turn.kind;
    let RequestBody::AskProviderAdd {
        id,
        engine,
        title,
        model,
        api_key_env,
        executable,
    } = body
    else {
        unreachable!("dispatch routed another request to ask_provider_add")
    };
    let response = {
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
    };
    Ok(Flow::Respond(response))
}

async fn ask_provider_default(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let ask = &ctx.ask;
    let kind = &mut turn.kind;
    let RequestBody::AskProviderDefault { provider } = body else {
        unreachable!("dispatch routed another request to ask_provider_default")
    };
    let response = {
        *kind = "ask_provider_default";
        match ask.set_default_agent(&provider).await {
            Ok(providers) => Response::with_ask_providers(providers),
            Err(error) => Response::err(error.to_string()),
        }
    };
    Ok(Flow::Respond(response))
}

async fn ask_provider_remove(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let ask = &ctx.ask;
    let kind = &mut turn.kind;
    let RequestBody::AskProviderRemove { id } = body else {
        unreachable!("dispatch routed another request to ask_provider_remove")
    };
    let response = {
        *kind = "ask_provider_remove";
        match ask.remove_provider(&id).await {
            Ok(providers) => Response::with_ask_providers(providers),
            Err(error) => Response::err(error.to_string()),
        }
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
