//! The collaboration mailbox: identity, send, inbox, list, reply, wait and
//! the request lifecycle.

#![allow(clippy::wildcard_imports)] // the arms name the IPC module's items as `dispatch.rs` does

use super::*;

pub(super) async fn handle(
    ctx: &ServerCtx,
    conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    match body {
        body @ RequestBody::CollaborationContext { .. } => {
            collaboration_context(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationIssueHandle { .. } => {
            collaboration_issue_handle(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationSetIdentity { .. } => {
            collaboration_set_identity(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationSend { .. } => {
            collaboration_send(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationTurnBoundaryOffer { .. } => {
            collaboration_turn_boundary_offer(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationInbox { .. } => {
            collaboration_inbox(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationPurpose { .. } => {
            collaboration_purpose(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationPromotePurpose { .. } => {
            collaboration_promote_purpose(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationOverview { .. } => {
            collaboration_overview(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationOpenCounts { .. } => {
            collaboration_open_counts(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationMailboxSnapshot { .. } => {
            collaboration_mailbox_snapshot(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationList { .. } => {
            collaboration_list(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationUpdate { .. } => {
            collaboration_update(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationReply { .. } => {
            collaboration_reply(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationGet { .. } => {
            collaboration_get(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationPeek { .. } => {
            collaboration_peek(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationWait { .. } => {
            collaboration_wait(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationHealth => {
            collaboration_health(ctx, conn, turn, body).await
        }
        body @ (RequestBody::CollaborationExpire { .. }
        | RequestBody::CollaborationExpireFiltered { .. }) => {
            collaboration_expire(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationExpireSummary { .. } => {
            collaboration_expire_summary(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationRetire { .. } => {
            collaboration_retire(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationCancel { .. } => {
            collaboration_cancel(ctx, conn, turn, body).await
        }
        body @ RequestBody::CollaborationWithdrawReplyWakes { .. } => {
            collaboration_withdraw_reply_wakes(ctx, conn, turn, body).await
        }
        other => unreachable!("dispatch routed {other:?} to mailbox"),
    }
}

async fn collaboration_context(
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
    let RequestBody::CollaborationContext { origin } = body else {
        unreachable!("dispatch routed another request to collaboration_context")
    };
    let response = {
        *kind = "collaboration_context";
        collaboration_actor.observe_pane(backends).await;
        let audit_context =
            CollaborationAuditContext::new(CollaborationAuditOperation::Context, origin.clone());
        let response = if collaboration.enabled() {
            let topology = collaboration_participants(
                store,
                backends,
                collaboration,
                origin.agent_session_id.as_deref(),
                collaboration_actor.take_request_panes(),
            )
            .await;
            match topology.resolve_origin(&origin) {
                Ok(current) => Response::with_room(
                    collaboration::room_context(
                        collaboration.as_ref(),
                        current,
                        &topology.participants,
                    )
                    .await,
                ),
                Err(error) => Response::err(error.to_string()),
            }
        } else {
            Response::err("agent collaboration is disabled; enable [collaboration].enabled")
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

async fn collaboration_issue_handle(
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
    let RequestBody::CollaborationIssueHandle {
        pane,
        socket,
        request,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_issue_handle")
    };
    let response = {
        *kind = "collaboration_issue_handle";
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            None,
            collaboration_actor.take_request_panes(),
        )
        .await;
        match topology.room_of(&pane, socket.as_deref()) {
            Some(room) => match collaboration
                .issue_handle(&room, &pane, &topology.participants, request)
                .await
            {
                Ok(handle) => Response::with_handle(handle),
                Err(error) => Response::err(error.to_string()),
            },
            // A pane the scan cannot place has no room, so it has
            // no namespace to allocate from. The caller falls back
            // to leaving it unnamed rather than guessing.
            None => Response::with_handle(None),
        }
    };
    Ok(Flow::Respond(response))
}

async fn collaboration_set_identity(
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
    let RequestBody::CollaborationSetIdentity {
        origin,
        alias,
        roles,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_set_identity")
    };
    let response = {
        *kind = "collaboration_set_identity";
        collaboration_actor.observe_pane(backends).await;
        let audit_context = CollaborationAuditContext::new(
            CollaborationAuditOperation::SetIdentity,
            origin.clone(),
        );
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            origin.agent_session_id.as_deref(),
            collaboration_actor.take_request_panes(),
        )
        .await;
        let response = match topology.resolve_origin(&origin) {
            Ok(current) => match collaboration
                .set_identity(&current, &topology.participants, alias, roles)
                .await
            {
                Ok(current) => Response::with_room(
                    collaboration::room_context(
                        collaboration.as_ref(),
                        current,
                        &topology.participants,
                    )
                    .await,
                ),
                Err(error) => Response::err(error.to_string()),
            },
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

async fn collaboration_send(
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
    let response_view = &mut turn.response_view;
    let RequestBody::CollaborationSend {
        origin,
        target,
        mut request,
        view,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_send")
    };
    let response = {
        *response_view = view;
        *kind = "collaboration_send";
        collaboration_actor.observe_pane(backends).await;
        let mut audit_context =
            CollaborationAuditContext::new(CollaborationAuditOperation::Send, origin.clone());
        audit_context.target = Some(target.clone());
        audit_context.message_bytes = Some(request.body.len());
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            origin.agent_session_id.as_deref(),
            collaboration_actor.take_request_panes(),
        )
        .await;
        let result = topology.resolve_origin(&origin).and_then(|sender| {
            topology
                .resolve_target_with_slot(&sender, &target, collaboration.scope())
                .map(|(recipient, slot)| (sender, recipient, slot))
        });
        let response = match result {
            Ok((sender, recipient, slot)) => {
                request.recipient_slot = slot;
                // A console represents the operator, so stamp the
                // pane it dispatches to. Agent-originated sends are
                // stamped from the sending execution surface.
                let anchor = if origin.console { &recipient } else { &sender };
                topology.stamp_request_metadata(anchor, &mut request);
                let provenance = collaboration_actor.provenance(&origin);
                let sender_work = topology.durable_work(&sender);
                match collaboration
                    .create_with_sender_work(
                        sender,
                        recipient,
                        *request,
                        Some(provenance),
                        sender_work,
                    )
                    .await
                {
                    Ok(request) => Response::with_collaboration_request(request),
                    Err(error) => Response::err(error.to_string()),
                }
            }
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

async fn collaboration_turn_boundary_offer(
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
    let RequestBody::CollaborationTurnBoundaryOffer {
        origin,
        event,
        max_requests,
        max_bytes,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_turn_boundary_offer")
    };
    let response = {
        *kind = "collaboration_turn_boundary_offer";
        collaboration_actor.observe_pane(backends).await;
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            origin.agent_session_id.as_deref(),
            collaboration_actor.take_request_panes(),
        )
        .await;
        // Exact-session origins (a paneless Codex app-server hook)
        // resolve only to a live binding; no pane fallback.
        match topology.resolve_origin(&origin) {
            Ok(current) => match collaboration
                .offer_turn_boundary(&current, event, max_requests, max_bytes)
                .await
            {
                Ok(offers) => Response::with_turn_boundary_offers(offers),
                Err(error) => Response::err(error.to_string()),
            },
            Err(error) => Response::err(error.to_string()),
        }
    };
    Ok(Flow::Respond(response))
}

async fn collaboration_inbox(
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
    let RequestBody::CollaborationInbox {
        origin,
        request_id,
        limit,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_inbox")
    };
    let response = {
        *kind = "collaboration_inbox";
        collaboration_actor.observe_pane(backends).await;
        let audit_context =
            CollaborationAuditContext::new(CollaborationAuditOperation::Inbox, origin.clone());
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            origin.agent_session_id.as_deref(),
            collaboration_actor.take_request_panes(),
        )
        .await;
        let response = match topology.resolve_origin(&origin) {
            Ok(current) => match if let Some(id) = request_id.as_deref() {
                collaboration
                    .claim_by_id(&current, id)
                    .await
                    .map(|request| vec![request])
            } else if let Some(limit) = limit {
                collaboration.claim_for_limit(&current, limit).await
            } else {
                collaboration.claim_for(&current).await
            } {
                Ok(requests) => Response::with_collaboration_requests(requests),
                Err(error) => Response::err(error.to_string()),
            },
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

async fn collaboration_purpose(
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
    let RequestBody::CollaborationPurpose { origin, purpose } = body else {
        unreachable!("dispatch routed another request to collaboration_purpose")
    };
    let response = {
        *kind = "collaboration_purpose";
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            origin.agent_session_id.as_deref(),
            collaboration_actor.take_request_panes(),
        )
        .await;
        match topology.resolve_origin(&origin) {
            Ok(current) => match collaboration.set_purpose(&current, purpose).await {
                Ok(()) => Response::ok(),
                Err(error) => Response::err(error.to_string()),
            },
            Err(error) => Response::err(error.to_string()),
        }
    };
    Ok(Flow::Respond(response))
}

async fn collaboration_promote_purpose(
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
    let RequestBody::CollaborationPromotePurpose {
        session_ids,
        purpose,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_promote_purpose")
    };
    let response = {
        *kind = "collaboration_promote_purpose";
        collaboration_actor.observe_pane(backends).await;
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            None,
            collaboration_actor.take_request_panes(),
        )
        .await;
        let origin = CollaborationOrigin {
            exact_session: false,
            pane: collaboration_actor
                .observed_pane
                .clone()
                .unwrap_or_default(),
            socket: None,
            console: true,
            agent_session_id: None,
        };
        let mut audit_context =
            CollaborationAuditContext::new(CollaborationAuditOperation::PromotePurpose, origin);
        audit_context.target_sessions.clone_from(&session_ids);
        audit_context.purpose = Some(purpose);
        let response = match promote_purpose_targets(&topology, &session_ids) {
            Ok(targets) => match collaboration.set_purposes(&targets, purpose).await {
                Ok(()) => Response::ok(),
                Err(error) => Response::err(error.to_string()),
            },
            Err(error) => Response::err(error),
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

async fn collaboration_overview(
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
    let RequestBody::CollaborationOverview { origin } = body else {
        unreachable!("dispatch routed another request to collaboration_overview")
    };
    let response = {
        *kind = "collaboration_overview";
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            None,
            collaboration_actor.take_request_panes(),
        )
        .await;
        match topology.resolve_origin(&origin) {
            Ok(current) => match collaboration
                .overview_for(&current, &topology.participants)
                .await
            {
                Ok(overview) => {
                    let mut response = Response::ok();
                    response.collaboration_overview = Some(overview);
                    response
                }
                Err(error) => Response::err(error.to_string()),
            },
            Err(error) => Response::err(error.to_string()),
        }
    };
    Ok(Flow::Respond(response))
}

async fn collaboration_open_counts(
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
    let RequestBody::CollaborationOpenCounts { origin, scope } = body else {
        unreachable!("dispatch routed another request to collaboration_open_counts")
    };
    let response = {
        *kind = "collaboration_open_counts";
        collaboration_actor.observe_pane(backends).await;
        let mut audit_context =
            CollaborationAuditContext::new(CollaborationAuditOperation::Counts, origin.clone());
        audit_context.scope = Some(scope);
        audit_context.read_view = Some(collaboration::CollaborationReadView::Metadata);
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            origin.agent_session_id.as_deref(),
            collaboration_actor.take_request_panes(),
        )
        .await;
        let response = match topology.resolve_origin(&origin) {
            Ok(current) => match collaboration.open_counts_for(&current, scope).await {
                Ok(counts) => {
                    let mut response = Response::ok();
                    response.collaboration_open_counts = Some(counts);
                    response.collaboration_scope = Some(scope);
                    response
                }
                Err(error) => Response::err(error.to_string()),
            },
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

async fn collaboration_mailbox_snapshot(
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
    let RequestBody::CollaborationMailboxSnapshot {
        origin,
        operator_only,
        if_unchanged,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_mailbox_snapshot")
    };
    let response = {
        *kind = "collaboration_mailbox_snapshot";
        collaboration_actor.observe_pane(backends).await;
        let mut audit_context =
            CollaborationAuditContext::new(CollaborationAuditOperation::List, origin.clone());
        audit_context.mailbox = Some(RequestMailbox::All);
        audit_context.read_view = Some(collaboration::CollaborationReadView::Full);
        audit_context.query_limit = Some(crate::fleet::FLEET_MAILBOX_REQUEST_LIMIT);
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            origin.agent_session_id.as_deref(),
            collaboration_actor.take_request_panes(),
        )
        .await;
        let response = async {
            if !origin.console {
                return Err(collaboration::CollaborationError::ScopeDenied);
            }
            let console = topology.resolve_origin(&origin)?;
            let agent = if operator_only {
                None
            } else {
                Some(topology.resolve_origin(&CollaborationOrigin {
                    console: false,
                    ..origin.clone()
                })?)
            };
            let snapshot = collaboration
                .mailbox_snapshot(&console, agent.as_ref(), if_unchanged.as_deref())
                .await?;
            let mut response = Response::ok();
            response.collaboration_mailbox_snapshot = Some(snapshot);
            Ok::<_, collaboration::CollaborationError>(response)
        }
        .await;
        let response = match response {
            Ok(response) => response,
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

#[allow(clippy::too_many_lines)] // one request, moved whole from the family match
async fn collaboration_list(
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
    let RequestBody::CollaborationList {
        mut origin,
        mailbox,
        mut scope,
        query,
        offset,
        metadata_only,
        view,
        auto_scope,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_list")
    };
    let response = {
        *kind = "collaboration_list";
        collaboration_actor.observe_pane(backends).await;
        let mut audit_context =
            CollaborationAuditContext::new(CollaborationAuditOperation::List, origin.clone());
        audit_context.mailbox = Some(mailbox);
        audit_context.read_view = Some(if query.is_none() {
            collaboration::CollaborationReadView::Full
        } else if metadata_only {
            collaboration::CollaborationReadView::Metadata
        } else {
            view
        });
        audit_context.query_limit = query.as_ref().and_then(|q| q.limit);
        // Only the widened listings are worth a ledger line of
        // their own — a caller-scoped list is the norm and says
        // nothing about reach.
        audit_context.scope = (!matches!(scope, MailboxScope::Caller)).then_some(scope);
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            origin.agent_session_id.as_deref(),
            collaboration_actor.take_request_panes(),
        )
        .await;
        let mut current = topology.resolve_origin(&origin);
        if auto_scope
            && matches!(scope, MailboxScope::Caller)
            && origin.agent_session_id.is_none()
            && matches!(
                current,
                Err(collaboration::CollaborationError::UnknownOrigin(_))
            )
        {
            origin.console = true;
            scope = MailboxScope::Room;
            current = topology.resolve_origin(&origin);
        }
        audit_context.represented_origin = origin.clone();
        audit_context.scope = (!matches!(scope, MailboxScope::Caller)).then_some(scope);
        let response = match current {
            Ok(current) if query.is_some() => {
                match collaboration
                    .query_for_offset(
                        &current,
                        mailbox,
                        scope,
                        query.as_ref().expect("query present"),
                        offset,
                    )
                    .await
                {
                    Ok(mut page) => {
                        let view = if metadata_only {
                            collaboration::CollaborationReadView::Metadata
                        } else {
                            view
                        };
                        for request in &mut page.requests {
                            view.apply(request);
                        }
                        let mut response = Response::ok();
                        response.collaboration_scope = Some(scope);
                        response.collaboration_byte_budget = query
                            .as_ref()
                            .and_then(collaboration::CollaborationQuery::byte_budget);
                        response.collaboration_page = Some(page);
                        response
                    }
                    Err(error) => Response::err(error.to_string()),
                }
            }
            Ok(current) => match collaboration.list_for(&current, mailbox, scope).await {
                Ok(requests) => Response::with_scoped_collaboration_requests(requests, scope),
                Err(error) => Response::err(error.to_string()),
            },
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

async fn collaboration_update(
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
    let response_view = &mut turn.response_view;
    let RequestBody::CollaborationUpdate {
        origin,
        request_id,
        body,
        view,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_update")
    };
    let response = {
        *response_view = view;
        *kind = "collaboration_update";
        collaboration_actor.observe_pane(backends).await;
        let mut audit_context =
            CollaborationAuditContext::new(CollaborationAuditOperation::Update, origin.clone());
        audit_context.request_id = Some(request_id.clone());
        audit_context.message_bytes = Some(body.len());
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            origin.agent_session_id.as_deref(),
            collaboration_actor.take_request_panes(),
        )
        .await;
        let response = match topology.resolve_origin(&origin) {
            Ok(current) => match collaboration
                .update_request(&current, &request_id, body)
                .await
            {
                Ok(request) => Response::with_collaboration_request(request),
                Err(error) => Response::err(error.to_string()),
            },
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

async fn collaboration_reply(
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
    let response_view = &mut turn.response_view;
    let RequestBody::CollaborationReply {
        origin,
        request_id,
        status,
        body,
        artifacts,
        view,
        report,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_reply")
    };
    let response = {
        *response_view = view;
        *kind = "collaboration_reply";
        collaboration_actor.observe_pane(backends).await;
        let mut audit_context =
            CollaborationAuditContext::new(CollaborationAuditOperation::Reply, origin.clone());
        audit_context.request_id = Some(request_id.clone());
        audit_context.status = Some(status);
        audit_context.message_bytes = Some(body.len());
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            origin.agent_session_id.as_deref(),
            collaboration_actor.take_request_panes(),
        )
        .await;
        let response = match topology.resolve_origin(&origin) {
            Ok(current) => match collaboration
                .reply_with_report(&current, &request_id, status, body, artifacts, report)
                .await
            {
                Ok(request) => Response::with_collaboration_request(request),
                Err(error) => Response::err(error.to_string()),
            },
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

async fn collaboration_get(
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
    let response_view = &mut turn.response_view;
    let RequestBody::CollaborationGet {
        origin,
        request_id,
        view,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_get")
    };
    let response = {
        *response_view = view;
        *kind = "collaboration_get";
        collaboration_actor.observe_pane(backends).await;
        let mut audit_context =
            CollaborationAuditContext::new(CollaborationAuditOperation::Get, origin.clone());
        audit_context.request_id = Some(request_id.clone());
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            origin.agent_session_id.as_deref(),
            collaboration_actor.take_request_panes(),
        )
        .await;
        let response = match topology.resolve_origin(&origin) {
            Ok(current) => match collaboration.get_for(&current, &request_id).await {
                Ok(request) => Response::with_collaboration_request(request),
                Err(error) => Response::err(error.to_string()),
            },
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

async fn collaboration_peek(
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
    let response_view = &mut turn.response_view;
    let RequestBody::CollaborationPeek {
        origin,
        request_id,
        view,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_peek")
    };
    let response = {
        *response_view = view;
        *kind = "collaboration_peek";
        collaboration_actor.observe_pane(backends).await;
        let mut audit_context =
            CollaborationAuditContext::new(CollaborationAuditOperation::Peek, origin.clone());
        audit_context.request_id = Some(request_id.clone());
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            origin.agent_session_id.as_deref(),
            collaboration_actor.take_request_panes(),
        )
        .await;
        let response = match topology.resolve_origin(&origin) {
            Ok(current) => match collaboration.peek_for(&current, &request_id).await {
                Ok(request) => Response::with_collaboration_request(request),
                Err(error) => Response::err(error.to_string()),
            },
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

async fn collaboration_wait(
    ctx: &ServerCtx,
    conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let backends = &ctx.backends;
    let collaboration = &ctx.collaboration;
    let stopping = &mut conn.stopping;
    let collaboration_actor = &mut conn.collaboration_actor;
    let kind = &mut turn.kind;
    let pending_audit = &mut turn.pending_audit;
    let response_view = &mut turn.response_view;
    let RequestBody::CollaborationWait {
        origin,
        request_id,
        timeout_secs,
        view,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_wait")
    };
    let response = {
        *response_view = view;
        *kind = "collaboration_wait";
        collaboration_actor.observe_pane(backends).await;
        let mut audit_context =
            CollaborationAuditContext::new(CollaborationAuditOperation::Wait, origin.clone());
        audit_context.request_id = Some(request_id.clone());
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            origin.agent_session_id.as_deref(),
            collaboration_actor.take_request_panes(),
        )
        .await;
        let response = match topology.resolve_origin(&origin) {
            // A read-only long poll may be cut short by shutdown,
            // but it is still answered before the drain deadline:
            // aborting it closed the socket with no reply, which
            // clients read as a broken response.
            Ok(current) => match Box::pin(until_server_shutdown(
                stopping,
                collaboration.wait_for_terminal(
                    &current,
                    &request_id,
                    Duration::from_secs(timeout_secs.clamp(1, MAX_COLLABORATION_WAIT_SECS)),
                ),
            ))
            .await
            {
                Some(Ok(request)) => Response::with_collaboration_request(request),
                Some(Err(error)) => Response::err(error.to_string()),
                None => Response::restarting(),
            },
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

async fn collaboration_health(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let store = &ctx.store;
    let backends = &ctx.backends;
    let collaboration = &ctx.collaboration;
    let agent_directory = &ctx.agent_directory;
    let kind = &mut turn.kind;
    let RequestBody::CollaborationHealth = body else {
        unreachable!("dispatch routed another request to collaboration_health")
    };
    let response = {
        *kind = "collaboration_health";
        match collaboration.health().await {
            Ok(mut health) => {
                let configured = backends.iter().map(|b| b.kind()).collect::<Vec<_>>();
                health.synthetic_agents =
                    store.synthetic_health(agent_directory, &configured).await;
                let mut response = Response::ok();
                response.collaboration_health = Some(health);
                response
            }
            Err(error) => Response::err(error.to_string()),
        }
    };
    Ok(Flow::Respond(response))
}

async fn collaboration_expire(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let collaboration = &ctx.collaboration;
    let kind = &mut turn.kind;
    let (RequestBody::CollaborationExpire {
        older_than_secs,
        dry_run,
        quiet_only,
        request_id,
    }
    | RequestBody::CollaborationExpireFiltered {
        older_than_secs,
        dry_run,
        quiet_only,
        request_id,
    }) = body
    else {
        unreachable!("dispatch routed another request to collaboration_expire")
    };
    let response = {
        *kind = "collaboration_expire";
        let age = time::Duration::seconds(i64::try_from(older_than_secs).unwrap_or(i64::MAX));
        let cutoff = time::OffsetDateTime::now_utc()
            .checked_sub(age)
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
        match collaboration
            .expire_queued_filtered(cutoff, dry_run, quiet_only, request_id.as_deref())
            .await
        {
            Ok(requests) => Response::with_collaboration_requests(requests),
            Err(error) => Response::err(error.to_string()),
        }
    };
    Ok(Flow::Respond(response))
}

async fn collaboration_expire_summary(
    ctx: &ServerCtx,
    _conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let collaboration = &ctx.collaboration;
    let kind = &mut turn.kind;
    let RequestBody::CollaborationExpireSummary {
        older_than_secs,
        dry_run,
        quiet_only,
        request_id,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_expire_summary")
    };
    let response = {
        *kind = "collaboration_expire_summary";
        let age = time::Duration::seconds(i64::try_from(older_than_secs).unwrap_or(i64::MAX));
        let cutoff = time::OffsetDateTime::now_utc()
            .checked_sub(age)
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
        match collaboration
            .expire_queued_filtered(cutoff, dry_run, quiet_only, request_id.as_deref())
            .await
        {
            Ok(requests) => {
                let mut response = Response::ok();
                response.collaboration_expire_summary =
                    Some(collaboration::ExpireSummary::from_requests(&requests));
                response
            }
            Err(error) => Response::err(error.to_string()),
        }
    };
    Ok(Flow::Respond(response))
}

async fn collaboration_retire(
    ctx: &ServerCtx,
    conn: &mut Conn,
    turn: &mut Turn,
    body: RequestBody,
) -> Result<Flow, RuntimeError> {
    let collaboration = &ctx.collaboration;
    let collaboration_actor = &mut conn.collaboration_actor;
    let kind = &mut turn.kind;
    let pending_audit = &mut turn.pending_audit;
    let RequestBody::CollaborationRetire {
        origin,
        request_id,
        reason,
        evidence,
        dry_run,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_retire")
    };
    let response = {
        *kind = "collaboration_retire";
        let mut audit_context =
            CollaborationAuditContext::new(CollaborationAuditOperation::Retire, origin.clone());
        audit_context.request_id = Some(request_id.clone());
        let response =
            if !origin.console || origin.agent_session_id.is_some() || origin.exact_session {
                Response::err("retirement requires an explicit operator console origin")
            } else {
                match collaboration
                    .retire_request(&request_id, &reason, &evidence, dry_run)
                    .await
                {
                    Ok(request) => Response::with_collaboration_request(request),
                    Err(error) => Response::err(error.to_string()),
                }
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

async fn collaboration_cancel(
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
    let response_view = &mut turn.response_view;
    let RequestBody::CollaborationCancel {
        origin,
        request_id,
        view,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_cancel")
    };
    let response = {
        *response_view = view;
        *kind = "collaboration_cancel";
        collaboration_actor.observe_pane(backends).await;
        let mut audit_context =
            CollaborationAuditContext::new(CollaborationAuditOperation::Cancel, origin.clone());
        audit_context.request_id = Some(request_id.clone());
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            origin.agent_session_id.as_deref(),
            collaboration_actor.take_request_panes(),
        )
        .await;
        let response = match topology.resolve_origin(&origin) {
            Ok(current) => match collaboration.cancel_for(&current, &request_id).await {
                Ok(request) => Response::with_collaboration_request(request),
                Err(error) => Response::err(error.to_string()),
            },
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

async fn collaboration_withdraw_reply_wakes(
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
    let RequestBody::CollaborationWithdrawReplyWakes {
        origin,
        request_ids,
    } = body
    else {
        unreachable!("dispatch routed another request to collaboration_withdraw_reply_wakes")
    };
    let response = {
        *kind = "collaboration_withdraw_reply_wakes";
        collaboration_actor.observe_pane(backends).await;
        let mut audit_context = CollaborationAuditContext::new(
            CollaborationAuditOperation::WithdrawReplyWakes,
            origin.clone(),
        );
        audit_context.request_id = request_ids.first().cloned();
        let topology = collaboration_participants(
            store,
            backends,
            collaboration,
            origin.agent_session_id.as_deref(),
            collaboration_actor.take_request_panes(),
        )
        .await;
        let response = match topology.resolve_origin(&origin) {
            Ok(current) => match collaboration
                .withdraw_reply_wakes(&current, &request_ids)
                .await
            {
                Ok(requests) => Response::with_collaboration_requests(requests),
                Err(error) => Response::err(error.to_string()),
            },
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
