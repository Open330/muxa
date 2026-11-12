use super::{
    collaboration_output, mcp, mcp_output, parse_duration_secs, require_capability, time_range,
    Client, CollaborationOrigin, CollaborationOriginMatch, Context, IdentityCmd, MailboxScope,
    MsgCmd, NewRequest, OffsetDateTime, Participant, Path, RequestKind, RequestMailbox,
    RequestStatus, Result, WorkMode,
};

/// The origin `tend peers`/`msg`/`identity` speak for.
///
/// Shared with the MCP server rather than rebuilt here. Reading `$TMUX_PANE`
/// directly yields a bare `%N`, but hook stamping and the pane scan namespace
/// a pane by its host — `rmux:%N` — and record the endpoint the same way, so a
/// CLI that qualified neither matched no tracked agent at all on any host but
/// tmux. One function means the two can no longer disagree.
pub(super) async fn collaboration_origin(client: &Client) -> Result<CollaborationOrigin> {
    mcp::resolve_collaboration_origin(client)
        .await
        .map_err(anyhow::Error::msg)
}

pub(super) fn collaboration_request_kind(value: &str) -> Result<RequestKind> {
    match value {
        "question" => Ok(RequestKind::Question),
        "review" => Ok(RequestKind::Review),
        "task" => Ok(RequestKind::Task),
        "notice" => Ok(RequestKind::Notice),
        _ => anyhow::bail!("kind must be question, review, task, or notice"),
    }
}

pub(super) fn collaboration_reply_status(value: &str) -> Result<RequestStatus> {
    match value {
        "completed" => Ok(RequestStatus::Completed),
        "blocked" => Ok(RequestStatus::Blocked),
        "declined" => Ok(RequestStatus::Declined),
        "failed" => Ok(RequestStatus::Failed),
        _ => anyhow::bail!("status must be completed, blocked, declined, or failed"),
    }
}

pub(super) fn collaboration_request_status(value: &str) -> Result<RequestStatus> {
    match value {
        "queued" => Ok(RequestStatus::Queued),
        "claimed" => Ok(RequestStatus::Claimed),
        "completed" => Ok(RequestStatus::Completed),
        "blocked" => Ok(RequestStatus::Blocked),
        "declined" => Ok(RequestStatus::Declined),
        "failed" => Ok(RequestStatus::Failed),
        "expired" => Ok(RequestStatus::Expired),
        "cancelled" | "canceled" => Ok(RequestStatus::Cancelled),
        _ => anyhow::bail!(
            "status must be queued, claimed, completed, blocked, declined, failed, expired, or cancelled"
        ),
    }
}

pub(super) fn collaboration_list_scope(value: &str) -> Result<MailboxScope> {
    match value {
        "caller" | "self" | "mine" => Ok(MailboxScope::Caller),
        "room" | "window" => Ok(MailboxScope::Room),
        "all" | "fleet" => Ok(MailboxScope::All),
        _ => anyhow::bail!("scope must be caller, room, or all"),
    }
}

pub(super) fn collaboration_mailbox(value: &str) -> Result<RequestMailbox> {
    match value {
        "incoming" | "inbox" => Ok(RequestMailbox::Incoming),
        "sent" | "outgoing" => Ok(RequestMailbox::Sent),
        "all" => Ok(RequestMailbox::All),
        _ => anyhow::bail!("mailbox must be incoming, sent, or all"),
    }
}

pub(super) async fn cmd_peers(client: &Client, json: bool) -> Result<()> {
    let room = client
        .collaboration_context(&collaboration_origin(client).await?)
        .await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&room)?);
        return Ok(());
    }
    println!(
        "room {} · self {} · {} unread · {} replies",
        room.current.room.window_id,
        room.current.label(),
        room.unread,
        room.unread_replies,
    );
    if room.peers.is_empty() {
        println!("No collaboration peers in this window.");
    } else {
        for peer in room.peers {
            println!(
                "{}  {:<16}  {:<12}  {}",
                peer.pane,
                peer.label(),
                peer.state,
                display_roles(&peer.roles),
            );
        }
    }
    Ok(())
}

pub(super) async fn cmd_identity(client: &Client, action: IdentityCmd) -> Result<()> {
    // Operator promotion names its targets; it has no origin of its own.
    if let IdentityCmd::Purpose { purpose, sessions } = &action {
        if !sessions.is_empty() {
            return cmd_promote_purpose(client, sessions, *purpose).await;
        }
    }
    let origin = collaboration_origin(client).await?;
    let (room, json) = match action {
        IdentityCmd::Purpose { purpose, .. } => {
            require_capability(
                client,
                tend::collaboration::HUMAN_PRIORITY_CAPABILITY,
                "identity purpose",
            )
            .await?;
            client.collaboration_purpose(&origin, purpose).await?;
            println!(
                "purpose: {}",
                serde_json::to_value(purpose)?.as_str().unwrap_or("primary")
            );
            return Ok(());
        }
        IdentityCmd::Show { json } => (client.collaboration_context(&origin).await?, json),
        IdentityCmd::Set { alias, roles } => (
            client
                .collaboration_set_identity(&origin, alias.as_deref(), &roles)
                .await?,
            false,
        ),
        IdentityCmd::Clear => (
            client
                .collaboration_set_identity(&origin, None, &[])
                .await?,
            false,
        ),
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&room)?);
    } else {
        println!(
            "self {}  roles: {}  room: {}",
            room.current.label(),
            display_roles(&room.current.roles),
            room.current.room.window_id,
        );
        for peer in room.peers {
            println!(
                "peer {}  roles: {}  state: {}",
                peer.label(),
                display_roles(&peer.roles),
                peer.state,
            );
        }
    }
    Ok(())
}

pub(super) fn display_roles(roles: &[String]) -> String {
    if roles.is_empty() {
        "-".into()
    } else {
        roles.join(",")
    }
}

/// Client-side collaboration history filters.
///
/// Keeping this at the CLI boundary lets a new CLI query an older daemon
/// without changing the wire protocol. The daemon already returns newest
/// first, so filtering retains that order and pagination remains stable for a
/// single snapshot.
#[derive(Debug, Default)]
pub(super) struct CollaborationListFilters {
    pub(super) range: Option<time_range::TimeRange>,
    pub(super) workspace_id: Option<String>,
    pub(super) work_id: Option<String>,
    pub(super) thread_id: Option<String>,
    pub(super) kind: Option<RequestKind>,
    pub(super) status: Option<RequestStatus>,
    pub(super) window: Option<String>,
    pub(super) limit: Option<usize>,
    pub(super) offset: usize,
}

impl CollaborationListFilters {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn parse(
        since: Option<&str>,
        workspace_id: Option<String>,
        work_id: Option<String>,
        thread_id: Option<String>,
        kind: Option<&str>,
        status: Option<&str>,
        window: Option<String>,
        limit: Option<usize>,
        offset: usize,
    ) -> Result<Self> {
        Ok(Self {
            range: since
                .map(|value| {
                    time_range::parse_since(
                        value,
                        OffsetDateTime::now_utc(),
                        "all collaboration history",
                    )
                })
                .transpose()?,
            workspace_id,
            work_id,
            thread_id,
            kind: kind.map(collaboration_request_kind).transpose()?,
            status: status.map(collaboration_request_status).transpose()?,
            window,
            limit,
            offset,
        })
    }

    pub(super) fn apply(
        &self,
        requests: Vec<tend::collaboration::CollaborationRequest>,
    ) -> Vec<tend::collaboration::CollaborationRequest> {
        let matches = requests
            .into_iter()
            .filter(|request| self.matches(request))
            .skip(self.offset);
        match self.limit {
            Some(0) | None => matches.collect(),
            Some(limit) => matches.take(limit).collect(),
        }
    }

    pub(super) fn matches(&self, request: &tend::collaboration::CollaborationRequest) -> bool {
        self.range
            .as_ref()
            .is_none_or(|range| range.includes(request.created_at))
            && self
                .workspace_id
                .as_deref()
                .is_none_or(|wanted| optional_id_matches(request.workspace_id.as_deref(), wanted))
            && self
                .work_id
                .as_deref()
                .is_none_or(|wanted| optional_id_matches(request.work_id.as_deref(), wanted))
            && self
                .thread_id
                .as_deref()
                .is_none_or(|wanted| optional_id_matches(request.thread_id.as_deref(), wanted))
            && self.kind.is_none_or(|wanted| request.kind == wanted)
            && self.status.is_none_or(|wanted| request.status == wanted)
            && self.window.as_deref().is_none_or(|wanted| {
                participant_matches_window(&request.from, wanted)
                    || participant_matches_window(&request.to, wanted)
            })
    }
}

pub(super) fn optional_id_matches(actual: Option<&str>, wanted: &str) -> bool {
    actual == Some(wanted)
}

pub(super) fn participant_matches_window(participant: &Participant, wanted: &str) -> bool {
    participant
        .window_name
        .as_deref()
        .is_some_and(|name| name.eq_ignore_ascii_case(wanted))
        || participant.room.window_id.eq_ignore_ascii_case(wanted)
        || participant_location(participant).eq_ignore_ascii_case(wanted)
}

pub(super) async fn cmd_promote_purpose(
    client: &Client,
    sessions: &[String],
    purpose: tend::collaboration::LaunchPurpose,
) -> Result<()> {
    require_capability(
        client,
        tend::collaboration::PURPOSE_PROMOTE_CAPABILITY,
        "identity purpose --session",
    )
    .await?;
    client
        .collaboration_promote_purpose(sessions, purpose)
        .await?;
    let label = serde_json::to_value(purpose)?;
    for session in sessions {
        println!(
            "purpose: {}  {session}",
            label.as_str().unwrap_or("primary")
        );
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // explicit message subcommand dispatch keeps output compatibility visible
pub(super) async fn cmd_msg(client: &Client, action: MsgCmd) -> Result<()> {
    // Housekeeping acts on the whole store, not as a participant, so it must
    // not require the caller to be a tracked agent.
    if let MsgCmd::Expire {
        older_than,
        dry_run,
        json,
        quiet_only,
        request_id,
    } = action
    {
        return cmd_msg_expire(
            client,
            &older_than,
            dry_run,
            json,
            quiet_only,
            request_id.as_deref(),
        )
        .await;
    }
    if let MsgCmd::Retire {
        request_id,
        reason,
        evidence,
        execute,
        json,
    } = action
    {
        let operator = CollaborationOrigin {
            pane: String::new(),
            socket: None,
            console: true,
            agent_session_id: None,
            exact_session: false,
        };
        let request = client
            .collaboration_retire(&operator, &request_id, &reason, &evidence, !execute)
            .await?;
        let mut output = collaboration_output::receipt(&request);
        output["dry_run"] = serde_json::json!(!execute);
        if json {
            println!("{}", serde_json::to_string(&output)?);
        } else {
            println!(
                "{} {}: {} (evidence: {})",
                if execute { "retired" } else { "would retire" },
                request.id,
                reason,
                evidence
            );
        }
        return Ok(());
    }
    let origin = collaboration_origin(client).await?;
    match action {
        MsgCmd::Accept { request_id, json } => {
            require_capability(
                client,
                tend::collaboration::HUMAN_PRIORITY_CAPABILITY,
                "msg accept",
            )
            .await?;
            let requests = client.collaboration_claim(&origin, &request_id).await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&collaboration_output::inbox(&requests, false))?
                );
            } else {
                for request in requests {
                    println!("{}", render_inbox_request(&request));
                }
            }
        }

        MsgCmd::Attention {
            body,
            action,
            json,
            full,
        } => {
            mcp_output::require_human_actions(client)
                .await
                .map_err(anyhow::Error::msg)?;
            let request = client
                .collaboration_send_view(
                    &origin,
                    "human",
                    &NewRequest {
                        body,
                        human_action: Some(serde_json::from_value(serde_json::Value::String(
                            action,
                        ))?),
                        notify: Some(true),
                        ..NewRequest::default()
                    },
                    if full {
                        tend::collaboration::CollaborationReadView::Full
                    } else {
                        tend::collaboration::CollaborationReadView::Metadata
                    },
                )
                .await?;
            print_collaboration_receipt(&request, json, full);
        }
        MsgCmd::Send {
            target,
            body,
            task_contract,
            supersedes,
            human_action,
            kind,
            thread,
            parent,
            work,
            workspace,
            run,
            execute,
            paths,
            artifacts,
            links,
            no_reply,
            notify,
            withdraw,
            user_authorized,
            json,
            full,
        } => {
            if user_authorized {
                require_capability(
                    client,
                    tend::collaboration::USER_AUTHORIZED_DELIVERY_CAPABILITY,
                    "msg send --user-authorized",
                )
                .await?;
            }
            if !withdraw.is_empty() {
                require_capability(
                    client,
                    tend::collaboration::REPLY_WAKE_WITHDRAW_CAPABILITY,
                    "msg send --withdraw",
                )
                .await?;
            }
            if target.eq_ignore_ascii_case("human") {
                mcp_output::require_human_actions(client)
                    .await
                    .map_err(anyhow::Error::msg)?;
            }
            let kind = collaboration_request_kind(&kind)?;
            let request = client
                .collaboration_send_view(
                    &origin,
                    &target,
                    &NewRequest {
                        task_contract: task_contract
                            .as_deref()
                            .map(|p| {
                                let value = read_collaboration_json(p)?;
                                tend::collaboration::TaskContract::from_input(value)
                                    .map_err(anyhow::Error::msg)
                            })
                            .transpose()?,
                        supersedes_request_id: supersedes,
                        dispatch_id: None,
                        source_node_id: None,
                        delegation_parent: None,
                        human_action: human_action
                            .map(|value| serde_json::from_value(serde_json::Value::String(value)))
                            .transpose()?,
                        initiator: None,
                        kind,
                        body,
                        expects_reply: !no_reply && kind != RequestKind::Notice,
                        notify,
                        work_mode: if execute {
                            WorkMode::Execute
                        } else {
                            WorkMode::ReadOnly
                        },
                        paths,
                        thread_id: thread,
                        parent_request_id: parent,
                        workspace_id: workspace,
                        work_id: work,
                        run_id: run,
                        artifacts,
                        links,
                        withdraw_reply_wakes: withdraw,
                        recipient_slot: None,
                        user_authorized,
                    },
                    if full {
                        tend::collaboration::CollaborationReadView::Full
                    } else {
                        tend::collaboration::CollaborationReadView::Metadata
                    },
                )
                .await?;
            print_collaboration_receipt(&request, json, full);
        }
        MsgCmd::Update {
            request_id,
            body,
            json,
        } => {
            require_capability(client, "collaboration_update", "msg update").await?;
            let request = client
                .collaboration_update_view(
                    &origin,
                    &request_id,
                    &body,
                    tend::collaboration::CollaborationReadView::Metadata,
                )
                .await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(&collaboration_output::mutation_receipt(&request))?
                );
            } else {
                println!(
                    "updated  {}  sequence {}",
                    request.id,
                    request.updates.last().map_or(0, |update| update.sequence)
                );
            }
        }
        MsgCmd::Inbox { json, full } => {
            let requests = client.collaboration_inbox(&origin).await?;
            if json {
                println!("{}", collaboration_output::inbox(&requests, full));
            } else if requests.is_empty() {
                println!("Inbox is empty.");
            } else {
                for request in requests {
                    println!("{}", render_inbox_request(&request));
                }
            }
        }
        MsgCmd::List {
            mailbox,
            scope,
            since,
            work,
            workspace,
            thread,
            kind,
            status,
            window,
            limit,
            request_id,
            after_update,
            full,
            offset,
            json,
        } => {
            let filters = CollaborationListFilters::parse(
                since.as_deref(),
                workspace,
                work,
                thread,
                kind.as_deref(),
                status.as_deref(),
                window,
                limit,
                offset,
            )?;
            cmd_msg_list(
                client,
                &origin,
                &mailbox,
                &scope,
                &filters,
                request_id.as_deref(),
                after_update,
                full,
                json,
            )
            .await?;
        }
        MsgCmd::Reply {
            request_id,
            report,
            body,
            status,
            artifacts,
            json,
            full,
        } => {
            let report: Option<tend::collaboration::TaskReport> = report
                .as_deref()
                .map(|p| {
                    let value = read_collaboration_json(p)?;
                    tend::collaboration::TaskReport::from_input(value).map_err(anyhow::Error::msg)
                })
                .transpose()?;
            let request = client
                .collaboration_reply_report_view(
                    &origin,
                    &request_id,
                    collaboration_reply_status(&status)?,
                    &body,
                    &artifacts,
                    report.as_ref(),
                    if full {
                        tend::collaboration::CollaborationReadView::Full
                    } else {
                        tend::collaboration::CollaborationReadView::Metadata
                    },
                )
                .await?;
            print_collaboration_receipt(&request, json, full);
        }
        MsgCmd::Wait {
            request_id,
            timeout_secs,
            body,
            full,
            output,
        } => {
            cmd_msg_wait(
                client,
                &origin,
                &request_id,
                timeout_secs,
                body,
                full,
                output.as_deref(),
            )
            .await?;
        }
        MsgCmd::Cancel {
            request_id,
            json,
            full,
        } => {
            let request = client
                .collaboration_cancel_view(
                    &origin,
                    &request_id,
                    if full {
                        tend::collaboration::CollaborationReadView::Full
                    } else {
                        tend::collaboration::CollaborationReadView::Metadata
                    },
                )
                .await?;
            print_collaboration_receipt(&request, json, full);
        }
        MsgCmd::Withdraw { request_ids, json } => {
            require_capability(
                client,
                tend::collaboration::REPLY_WAKE_WITHDRAW_CAPABILITY,
                "msg withdraw",
            )
            .await?;
            let requests = client
                .collaboration_withdraw_reply_wakes(&origin, &request_ids)
                .await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string(
                        &requests
                            .iter()
                            .map(collaboration_output::mutation_receipt)
                            .collect::<Vec<_>>()
                    )?
                );
            } else {
                for request in requests {
                    println!(
                        "withdrawn  {}  {}  (reply wake off; work and reply unchanged)",
                        request.id,
                        format!("{:?}", request.status).to_lowercase()
                    );
                }
            }
        }
        MsgCmd::Expire { .. } | MsgCmd::Retire { .. } => {
            unreachable!("handled before resolving an origin")
        }
    }
    Ok(())
}

pub(super) async fn cmd_msg_expire(
    client: &Client,
    older_than: &str,
    dry_run: bool,
    json: bool,
    quiet_only: bool,
    request_id: Option<&str>,
) -> Result<()> {
    let secs = parse_duration_secs(older_than)?;
    anyhow::ensure!(secs > 0, "--older-than must be greater than zero");
    anyhow::ensure!(
        request_id.is_none_or(|id| !id.trim().is_empty()),
        "--request-id must not be blank"
    );
    let summary = client
        .collaboration_expire_summary(secs, dry_run, quiet_only, request_id)
        .await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }
    let verb = if dry_run { "would expire" } else { "expired" };
    if summary.total == 0 {
        println!("no eligible unclaimed requests match the age and cleanup filters ({older_than})");
        return Ok(());
    }
    for request in &summary.requests {
        println!(
            "{verb}  {}  {:?}  {} -> {}  (queued since {})",
            request.id, request.kind, request.from, request.to, request.created_at
        );
    }
    if summary.truncated {
        println!("(showing a bounded sample of requests)");
    }
    println!("{} request(s) {verb}", summary.total);
    Ok(())
}

// Only an unpinned, untracked pane can be an ordinary operator terminal.
// A missing pinned agent or a transport/permission failure must stay an error.
pub(super) fn is_operator_list_origin(
    origin: &CollaborationOrigin,
    error: &tend::ipc::RuntimeError,
) -> bool {
    origin.agent_session_id.is_none()
        && matches!(error, tend::ipc::RuntimeError::Daemon(message)
            if message.starts_with("collaboration origin is not a hook-correlated tracked pane agent:"))
}

#[allow(clippy::too_many_arguments)] // CLI scope, filters and output flags
pub(super) async fn cmd_msg_list(
    client: &Client,
    origin: &CollaborationOrigin,
    mailbox: &str,
    scope: &str,
    filters: &CollaborationListFilters,
    request_id: Option<&str>,
    after_update: Option<u64>,
    full: bool,
    json: bool,
) -> Result<()> {
    if request_id.is_none() && scope == "auto" {
        return cmd_msg_list_page(
            client,
            origin,
            collaboration_mailbox(mailbox)?,
            MailboxScope::Caller,
            filters,
            full,
            json,
            true,
        )
        .await;
    }
    let scope = if scope == "auto" {
        match client.collaboration_context(origin).await {
            Ok(_) => MailboxScope::Caller,
            Err(error) if is_operator_list_origin(origin, &error) => MailboxScope::Room,
            Err(error) => return Err(error.into()),
        }
    } else {
        collaboration_list_scope(scope)?
    };
    // A widened listing is the operator asking what the fleet is saying, not
    // the pane agent asking after its own inbox — so it goes out under the
    // console identity the daemon requires for it.
    let listing_origin = CollaborationOrigin {
        console: !matches!(scope, MailboxScope::Caller),
        ..origin.clone()
    };
    let mailbox = collaboration_mailbox(mailbox)?;
    let requests = if let Some(id) = request_id {
        let request = if after_update.is_some() {
            client
                .collaboration_peek_view(
                    &listing_origin,
                    id,
                    tend::collaboration::CollaborationReadView::Progress {
                        after_update: after_update.unwrap_or(0),
                    },
                )
                .await?
        } else {
            client
                .collaboration_get_view(
                    &listing_origin,
                    id,
                    if full {
                        tend::collaboration::CollaborationReadView::Full
                    } else {
                        tend::collaboration::CollaborationReadView::Metadata
                    },
                )
                .await?
        };
        if matches!(scope, MailboxScope::Room) {
            let room = client
                .collaboration_context(&listing_origin)
                .await?
                .current
                .room;
            if request.from.room != room && request.to.room != room {
                anyhow::bail!("request {id} is outside the selected room");
            }
        }
        if let Some(after) = after_update {
            if json {
                let value = collaboration_output::progress(&request, after);
                println!("{}", serde_json::to_string(&value)?);
            } else {
                if !request.updates.iter().any(|u| u.sequence > after) {
                    println!("No new updates.");
                }
                for update in request.updates.iter().filter(|u| u.sequence > after) {
                    println!("{}  {}", update.sequence, update.body);
                }
            }
            return Ok(());
        }
        vec![request]
    } else {
        return cmd_msg_list_page(
            client,
            &listing_origin,
            mailbox,
            scope,
            filters,
            full,
            json,
            false,
        )
        .await;
    };
    let total = requests
        .iter()
        .filter(|request| filters.matches(request))
        .count();
    let requests = filters.apply(requests);
    if requests.len() < total.saturating_sub(filters.offset) {
        eprintln!("Showing {} of {total} matching messages; next page: --offset {} (or --limit 0 for all).", requests.len(), filters.offset.saturating_add(requests.len()));
    }
    print_collaboration_messages(&requests, json, full, origin, scope)
}

#[allow(clippy::too_many_arguments)] // Existing display flags plus server-side auto scope.
pub(super) async fn cmd_msg_list_page(
    client: &Client,
    origin: &CollaborationOrigin,
    mailbox: RequestMailbox,
    mut scope: MailboxScope,
    filters: &CollaborationListFilters,
    full: bool,
    json: bool,
    mut auto_scope: bool,
) -> Result<()> {
    let mut query = tend::collaboration::CollaborationQuery {
        since: filters.range.as_ref().and_then(|range| range.since_at),
        until: filters.range.as_ref().and_then(|range| range.until_at),
        until_exclusive: true,
        workspace_id: filters.workspace_id.clone(),
        work_id: filters.work_id.clone(),
        thread_id: filters.thread_id.clone(),
        kind: filters.kind,
        status: filters.status,
        window: filters.window.clone(),
        ..Default::default()
    };
    let wanted = filters
        .limit
        .filter(|limit| *limit != 0)
        .unwrap_or(usize::MAX);
    let mut requests = Vec::new();
    let mut offset = filters.offset;
    let mut total = None;
    loop {
        query.limit = Some(wanted.saturating_sub(requests.len()).min(500));
        let view = if full {
            tend::collaboration::CollaborationReadView::Full
        } else {
            tend::collaboration::CollaborationReadView::Metadata
        };
        let listing_origin = if auto_scope {
            origin.clone()
        } else {
            CollaborationOrigin {
                console: !matches!(scope, MailboxScope::Caller),
                ..origin.clone()
            }
        };
        let result = client
            .collaboration_query_result(
                &listing_origin,
                mailbox,
                scope,
                &query,
                offset,
                view,
                auto_scope,
            )
            .await?;
        scope = result.scope;
        auto_scope = false;
        if let Some(legacy) = result.legacy_requests {
            total = Some(result.page.total);
            requests = filters.apply(legacy);
            break;
        }
        let page = result.page;
        total.get_or_insert(page.total);
        let done = !page.has_more || page.requests.is_empty();
        requests.extend(page.requests);
        if done || requests.len() >= wanted {
            break;
        }
        // A newly arrived message must not shift later pages and duplicate rows.
        query.cursor = Some(page.next_cursor.ok_or_else(|| {
            anyhow::anyhow!("daemon returned more history without a continuation cursor")
        })?);
        offset = 0;
    }
    let total = total.unwrap_or(0);
    if requests.len() < total.saturating_sub(filters.offset) {
        eprintln!("Showing {} of {total} matching messages; next page: --offset {} (or --limit 0 for all).", requests.len(), filters.offset.saturating_add(requests.len()));
    }
    print_collaboration_messages(&requests, json, full, origin, scope)
}

pub(super) fn read_collaboration_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    use std::io::Read as _;
    let mut data = Vec::new();
    std::fs::File::open(path)?
        .take(65537)
        .read_to_end(&mut data)?;
    if data.len() > 65536 {
        anyhow::bail!("collaboration JSON file exceeds 64 KiB");
    }
    serde_json::from_slice(&data).map_err(Into::into)
}

pub(super) fn render_inbox_request(request: &tend::collaboration::CollaborationRequest) -> String {
    let mut text = format!(
        "{}  {:?} from {}  {:?}  expects_reply={}\n  {}",
        request.id,
        request.kind,
        request.from.label(),
        request.work_mode,
        request.expects_reply,
        request.body
    );
    // Preserve the complete execution contract without requiring another full read.
    let contract = collaboration_output::message(request, true);
    for key in [
        "task_contract",
        "attempt",
        "supersedes_request_id",
        "superseded_by",
        "paths",
        "artifacts",
        "links",
        "updates",
        "human_action",
        "interruption",
    ] {
        if let Some(value) = contract
            .get(key)
            .filter(|v| !v.is_null() && !v.as_array().is_some_and(Vec::is_empty))
        {
            use std::fmt::Write as _;
            let _ = write!(text, "\n  {key}: {value}");
        }
    }
    text
}

pub(super) async fn cmd_msg_wait(
    client: &Client,
    origin: &CollaborationOrigin,
    request_id: &str,
    timeout_secs: u64,
    body: bool,
    full: bool,
    output: Option<&Path>,
) -> Result<()> {
    let request = client
        .collaboration_wait_view(
            origin,
            request_id,
            timeout_secs,
            if full {
                tend::collaboration::CollaborationReadView::Full
            } else {
                tend::collaboration::CollaborationReadView::Outcome
            },
        )
        .await?;
    if !request.status.is_terminal() && body {
        eprintln!("wait deadline reached for {request_id}; no final reply received");
        std::process::exit(2);
    }
    let text = render_reply_output(&request, body, full)?;
    if let Some(path) = output {
        std::fs::write(path, text).with_context(|| format!("write reply to {}", path.display()))?;
    } else {
        println!("{text}");
    }
    if !request.status.is_terminal() {
        // No work judgement: this invocation reached its wait deadline.
        std::process::exit(2);
    }
    Ok(())
}

pub(super) fn render_reply_output(
    request: &tend::collaboration::CollaborationRequest,
    body: bool,
    full: bool,
) -> Result<String> {
    if body {
        return request
            .reply
            .as_ref()
            .map(|reply| reply.body.clone())
            .ok_or_else(|| anyhow::anyhow!("no final reply received for {}", request.id));
    }
    if full {
        return Ok(serde_json::to_string(request)?);
    }
    let mut value = collaboration_output::outcome(request);
    if !request.status.is_terminal() {
        value["reason"] = serde_json::json!("timeout");
    }
    Ok(serde_json::to_string(&value)?)
}

/// What `send`, `reply`, and `cancel` say when they worked.
///
/// They used to print the whole stored request — every timestamp, every
/// `null` — which buries the two facts the caller wants (it went through, and
/// whether an answer is coming). `--json` acknowledges without body echoes;
/// `--json --full` opts into the legacy record.
pub(super) fn print_collaboration_receipt(
    request: &tend::collaboration::CollaborationRequest,
    json: bool,
    full: bool,
) {
    if json {
        println!("{}", collaboration_output::acknowledgement(request, full));
        return;
    }
    match request.status {
        RequestStatus::Cancelled => println!("cancelled  {}", request.id),
        RequestStatus::Queued
            if !request.bypasses_primary_protection() && !request.to.accepts_peer_work() =>
        {
            println!(
                "queued  {}  to {}  (protected primary; explicit acceptance required)",
                request.id,
                request.to.label()
            );
            println!("For a generic peer request, cancel this queued request first (`tend msg cancel {}`), then use `tend peer {} BODY` to reuse or create a worker.", request.id, request.to.pane);
        }
        // A reply on the request means this was `msg reply`, not `msg send`.
        _ if request.reply.is_some() => {
            println!("replied  {}  to {}", request.id, request.from.label());
        }
        _ => {
            let awaiting = if request.expects_reply {
                "awaiting reply"
            } else {
                "no reply expected"
            };
            let withdrew = if request.withdrew_reply_wakes.is_empty() {
                String::new()
            } else {
                format!(
                    "; reply wake withdrawn for {}",
                    request.withdrew_reply_wakes.join(", ")
                )
            };
            println!(
                "sent  {}  to {}  ({awaiting}{withdrew})",
                request.id,
                request.to.label()
            );
        }
    }
}

/// Where a participant sits, for listings that span more than one room.
///
/// A room id alone (`@7` on some socket) tells an operator nothing; the tmux
/// names they navigate by do. Fall back to the ids only when the scan that
/// would have supplied the names has not run yet.
pub(super) fn participant_location(participant: &Participant) -> String {
    if participant.console {
        return "console".to_string();
    }
    let session = participant
        .tmux_session_name
        .as_deref()
        .or(participant.tmux_session_id.as_deref())
        .unwrap_or("?");
    let window = participant
        .window_name
        .as_deref()
        .unwrap_or(&participant.room.window_id);
    format!("{session}:{window}")
}

pub(super) fn render_message_list(
    requests: &[tend::collaboration::CollaborationRequest],
    full: bool,
) -> Result<String> {
    if full {
        return Ok(serde_json::to_string_pretty(requests)?);
    }
    let values = requests
        .iter()
        .map(|request| -> Result<serde_json::Value> {
            let mut value = collaboration_output::message(request, false);
            let object = value.as_object_mut().expect("message is an object");
            let id = object.remove("request_id").expect("message has an id");
            object.insert("id".into(), id);
            // Preserve CLI filter consumers; this is the stored declaration, not a task judgement.
            object.insert("status".into(), serde_json::json!(request.status));
            object.insert(
                "created_at".into(),
                serde_json::json!(request
                    .created_at
                    .format(&time::format_description::well_known::Rfc3339)?),
            );
            Ok(value)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(serde_json::to_string(&values)?)
}

pub(super) fn print_collaboration_messages(
    requests: &[tend::collaboration::CollaborationRequest],
    json: bool,
    full: bool,
    origin: &CollaborationOrigin,
    scope: MailboxScope,
) -> Result<()> {
    if json {
        println!("{}", render_message_list(requests, full)?);
    } else if requests.is_empty() {
        println!("No collaboration messages.");
    } else {
        for request in requests {
            // Past the caller's own mailbox there is no "the other end" to
            // name: the operator is reading other agents' traffic, so both
            // ends and the window each sits in have to be on the line.
            let direction = if matches!(scope, MailboxScope::Caller) {
                if request.from.pane == origin.pane
                    && origin
                        .socket
                        .as_deref()
                        .is_none_or(|socket| request.from.socket.as_deref() == Some(socket))
                {
                    format!("to {}", request.to.label())
                } else {
                    format!("from {}", request.from.label())
                }
            } else {
                format!(
                    "{} [{}] -> {} [{}]",
                    request.from.label(),
                    participant_location(&request.from),
                    request.to.label(),
                    participant_location(&request.to),
                )
            };
            // Only when it is worth knowing. `matched` is every ordinary
            // request, and printing the caller's pid and pane on each of them
            // turns a mailbox into a debug log — which is what a first look at
            // `tend msg list` used to be.
            let provenance = request
                .provenance
                .as_ref()
                .filter(|p| p.origin_match != CollaborationOriginMatch::Matched)
                .map_or_else(String::new, |p| {
                    let caller = p
                        .observed_pane
                        .as_deref()
                        .map_or_else(|| "pane=?".into(), |pane| format!("pane={pane}"));
                    let pid = p
                        .caller_pid
                        .map_or_else(String::new, |pid| format!(" pid={pid}"));
                    format!("  [via {} {caller}{pid} {}]", p.client_kind, p.origin_match)
                });
            println!(
                "{}  {:?}  {:?}  {}{}",
                request.id, request.status, request.kind, direction, provenance
            );
            if !full {
                continue;
            }
            println!("  {}", request.body);
            // Without this the sender sees their own question and a status, and
            // the answer they were told to come here for is nowhere on screen.
            if let Some(reply) = &request.reply {
                println!("  <- {:?}: {}", reply.status, reply.body);
                for artifact in &reply.artifacts {
                    println!("     {artifact}");
                }
            }
        }
    }
    Ok(())
}
