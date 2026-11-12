use super::*;
impl Client {
    /// Subscribe to content-free durable collaboration invalidations. This
    /// does not grant mailbox read access and is primarily used by Fleet
    /// relays to wake native operator inboxes.
    pub async fn collaboration_subscribe(&self) -> Result<CollaborationUpdateStream, RuntimeError> {
        tokio::time::timeout(CLIENT_CALL_TIMEOUT, self.collaboration_subscribe_inner())
            .await
            .map_err(|_| RuntimeError::Timeout(CLIENT_CALL_TIMEOUT))?
    }

    /// Invalidate request-history caches without reacting to worker lifecycle metadata.
    /// Older daemons ignore the filter and retain safe, broader invalidations.
    pub async fn collaboration_requests_subscribe(
        &self,
    ) -> Result<CollaborationUpdateStream, RuntimeError> {
        tokio::time::timeout(
            CLIENT_CALL_TIMEOUT,
            self.revision_subscribe_inner(&serde_json::json!({"protocol":PROTOCOL_VERSION,
                "kind":"collaboration_subscribe","requests_only":true})),
        )
        .await
        .map_err(|_| RuntimeError::Timeout(CLIENT_CALL_TIMEOUT))?
    }

    pub(super) async fn collaboration_subscribe_inner(
        &self,
    ) -> Result<CollaborationUpdateStream, RuntimeError> {
        self.revision_subscribe_inner(
            &serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"collaboration_subscribe"}),
        )
        .await
    }

    /// Execute an exact operation on one configured host. Mutations are
    /// authorized again by the manager's per-host access mode.
    pub async fn collaboration_update(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
        body: &str,
    ) -> Result<CollaborationRequest, RuntimeError> {
        self.collaboration_update_view(
            origin,
            request_id,
            body,
            collaboration::CollaborationReadView::Full,
        )
        .await
    }

    pub async fn collaboration_update_view(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
        body: &str,
        view: collaboration::CollaborationReadView,
    ) -> Result<CollaborationRequest, RuntimeError> {
        let response = self
            .call_checked(&serde_json::json!({
                "protocol": PROTOCOL_VERSION, "view": view, "kind": "collaboration_update",
                "origin": origin, "request_id": request_id, "body": body,
            }))
            .await?;
        serde_json::from_value(response["collaboration_request"].clone())
            .map_err(RuntimeError::Json)
    }

    pub async fn worker_drain_plan(
        &self,
        origin: &CollaborationOrigin,
        policy: &collaboration::WorkerDrainPolicy,
    ) -> Result<collaboration::WorkerDrainPlan, RuntimeError> {
        let response=self.call_with_timeout(&serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"worker_drain_plan","origin":origin,"policy":policy}),std::time::Duration::from_secs(25)).await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            return Err(response_error(&response));
        }
        serde_json::from_value(response["worker_drain_plan"].clone()).map_err(RuntimeError::Json)
    }
    pub async fn worker_drain_acquire(
        &self,
        origin: &CollaborationOrigin,
        target: &collaboration::WorkerDrainTarget,
        policy: &collaboration::WorkerDrainPolicy,
    ) -> Result<collaboration::WorkerDrainAcquire, RuntimeError> {
        let response=self.call_with_timeout(&serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"worker_drain_acquire","origin":origin,"target":target,"policy":policy}),std::time::Duration::from_secs(25)).await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            return Err(response_error(&response));
        }
        serde_json::from_value(response["worker_drain_acquire"].clone()).map_err(RuntimeError::Json)
    }
    pub async fn worker_drain_renew(
        &self,
        origin: &CollaborationOrigin,
        token: &collaboration::WorkerDrainToken,
    ) -> Result<collaboration::WorkerDrainToken, RuntimeError> {
        let response=self.call_with_timeout(&serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"worker_drain_renew","origin":origin,"token":token}),std::time::Duration::from_secs(25)).await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            return Err(response_error(&response));
        }
        serde_json::from_value(response["worker_drain_token"].clone()).map_err(RuntimeError::Json)
    }
    pub async fn worker_drain_stop(
        &self,
        origin: &CollaborationOrigin,
        token: &collaboration::WorkerDrainToken,
        operation_id: &str,
    ) -> Result<collaboration::WorkerDrainStop, RuntimeError> {
        let response=self.call_with_timeout(&serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"worker_drain_stop","origin":origin,"token":token,"operation_id":operation_id}),std::time::Duration::from_secs(25)).await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            return Err(response_error(&response));
        }
        serde_json::from_value(response["worker_drain_stop"].clone()).map_err(RuntimeError::Json)
    }
    pub async fn worker_drain_finish(
        &self,
        origin: &CollaborationOrigin,
        token: &collaboration::WorkerDrainToken,
        outcome: collaboration::WorkerDrainFinish,
        detail: &str,
    ) -> Result<collaboration::WorkerDrainView, RuntimeError> {
        let response=self.call_with_timeout(&serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"worker_drain_finish","origin":origin,"token":token,"outcome":outcome,"detail":detail}),std::time::Duration::from_secs(25)).await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            return Err(response_error(&response));
        }
        serde_json::from_value(response["worker_drain"].clone()).map_err(RuntimeError::Json)
    }
    pub async fn worker_drains(
        &self,
        origin: &CollaborationOrigin,
    ) -> Result<Vec<collaboration::WorkerDrainView>, RuntimeError> {
        let response=self.call_with_timeout(&serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"worker_drains","origin":origin}),std::time::Duration::from_secs(25)).await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            return Err(response_error(&response));
        }
        serde_json::from_value(response["worker_drains"].clone()).map_err(RuntimeError::Json)
    }
    pub async fn collaboration_worker_spawns(
        &self,
    ) -> Result<Vec<collaboration::WorkerSpawnView>, RuntimeError> {
        let response = self.call_checked(&serde_json::json!({"protocol":PROTOCOL_VERSION, "kind":"collaboration_worker_spawns"})).await?;
        serde_json::from_value(response["worker_spawns"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn collaboration_worker_spawn_release(
        &self,
        origin: &CollaborationOrigin,
        launch_id: &str,
        reason: &str,
    ) -> Result<(), RuntimeError> {
        // Release probes the launch's process fence across every pane, which
        // outlasts the default budget on a busy host; a timeout would hide
        // the daemon's actual answer (released, or why it refused).
        let response = self.call_with_timeout(&serde_json::json!({"protocol":PROTOCOL_VERSION, "kind":"collaboration_worker_spawn_release", "origin":origin, "launch_id":launch_id, "reason":reason}), std::time::Duration::from_secs(25)).await?;
        if !response["ok"].as_bool().unwrap_or(false) {
            return Err(response_error(&response));
        }
        Ok(())
    }

    pub async fn collaboration_worker_acquire(
        &self,
        origin: &CollaborationOrigin,
        spec: &collaboration::WorkerSpawnSpec,
    ) -> Result<collaboration::WorkerSpawnAcquisition, RuntimeError> {
        let response = self.call_checked(&serde_json::json!({
            "protocol":PROTOCOL_VERSION, "kind":"collaboration_worker_acquire", "origin":origin, "spec":spec,
        })).await?;
        serde_json::from_value(response["worker_spawn"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn collaboration_worker_spawn(
        &self,
        ticket: &collaboration::WorkerSpawnTicket,
        action: &collaboration::WorkerSpawnAction,
    ) -> Result<(), RuntimeError> {
        self.call_checked(&serde_json::json!({
            "protocol":PROTOCOL_VERSION, "kind":"collaboration_worker_spawn", "ticket":ticket, "action":action,
        })).await?;
        Ok(())
    }

    pub async fn collaboration_context(
        &self,
        origin: &CollaborationOrigin,
    ) -> Result<RoomContext, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "collaboration_context",
            "origin": origin,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["room"].clone()).map_err(RuntimeError::Json)
    }

    /// Ask the room's arbiter for a handle for `pane`.
    ///
    /// `Ok(None)` covers every "carry on without a name" case — no free name,
    /// a pane the daemon cannot place, or a daemon too old to know the
    /// request — so callers do not have to tell them apart. `Err` is reserved
    /// for a `Reserve` the room refused, which the caller does need to see.
    pub async fn collaboration_issue_handle(
        &self,
        pane: &str,
        socket: Option<&str>,
        request: &crate::collaboration::HandleRequest,
        deadline: Duration,
    ) -> Result<Option<String>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "collaboration_issue_handle",
            "pane": pane,
            "socket": socket,
            "request": request,
        });
        // A daemon that predates the arbiter cannot referee the namespace, and
        // naming a pane without one is what this change exists to stop.
        // Leaving it unnamed costs a `%1242`.
        let Ok(resp) = self.call_with_timeout(&req, deadline).await else {
            return Ok(None);
        };
        if resp["ok"].as_bool() != Some(true) {
            let message = resp["error"].as_str().unwrap_or("issue handle failed");
            return Err(RuntimeError::Daemon(message.to_string()));
        }
        Ok(resp["handle"].as_str().map(ToString::to_string))
    }

    pub async fn collaboration_set_identity(
        &self,
        origin: &CollaborationOrigin,
        alias: Option<&str>,
        roles: &[String],
    ) -> Result<RoomContext, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "collaboration_set_identity",
            "origin": origin,
            "alias": alias,
            "roles": roles,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["room"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn collaboration_send(
        &self,
        origin: &CollaborationOrigin,
        target: &str,
        request: &NewRequest,
    ) -> Result<CollaborationRequest, RuntimeError> {
        self.collaboration_send_view(
            origin,
            target,
            request,
            collaboration::CollaborationReadView::Full,
        )
        .await
    }

    pub async fn collaboration_send_view(
        &self,
        origin: &CollaborationOrigin,
        target: &str,
        request: &NewRequest,
        view: collaboration::CollaborationReadView,
    ) -> Result<CollaborationRequest, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "view": view,
            "kind": if request.task_contract.is_some() || request.supersedes_request_id.is_some() { "collaboration_send_contract" } else { "collaboration_send" },
            "origin": origin,
            "target": target,
            "request": request,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["collaboration_request"].clone()).map_err(RuntimeError::Json)
    }

    /// Ask for turn-boundary offers of user-authorized work for `origin`.
    /// Nothing is claimed; the recipient model claims each offer by id.
    pub async fn collaboration_turn_boundary_offer(
        &self,
        origin: &CollaborationOrigin,
        event: collaboration::turn_offer::TurnBoundaryEvent,
    ) -> Result<Vec<serde_json::Value>, RuntimeError> {
        let resp = self
            .call_checked(&serde_json::json!({"protocol": PROTOCOL_VERSION,
            "kind":"collaboration_turn_boundary_offer", "origin":origin, "event":event}))
            .await?;
        serde_json::from_value(resp["turn_boundary_offers"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn collaboration_claim(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
    ) -> Result<Vec<CollaborationRequest>, RuntimeError> {
        let resp = self
            .call_checked(&serde_json::json!({"protocol": PROTOCOL_VERSION,
            "kind":"collaboration_accept", "origin":origin, "request_id":request_id}))
            .await?;
        serde_json::from_value(resp["collaboration_requests"].clone()).map_err(RuntimeError::Json)
    }

    /// Operator: set the purpose of exact live agent sessions.
    pub async fn collaboration_promote_purpose(
        &self,
        session_ids: &[String],
        purpose: collaboration::LaunchPurpose,
    ) -> Result<(), RuntimeError> {
        self.call_checked(&serde_json::json!({"protocol":PROTOCOL_VERSION,
            "kind":"collaboration_promote_purpose", "session_ids":session_ids, "purpose":purpose}))
            .await?;
        Ok(())
    }

    pub async fn collaboration_purpose(
        &self,
        origin: &CollaborationOrigin,
        purpose: collaboration::LaunchPurpose,
    ) -> Result<(), RuntimeError> {
        self.call_checked(&serde_json::json!({"protocol":PROTOCOL_VERSION,
            "kind":"collaboration_purpose", "origin":origin, "purpose":purpose}))
            .await?;
        Ok(())
    }

    pub async fn collaboration_inbox(
        &self,
        origin: &CollaborationOrigin,
    ) -> Result<Vec<CollaborationRequest>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "collaboration_inbox",
            "origin": origin,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["collaboration_requests"].clone()).map_err(RuntimeError::Json)
    }

    /// Claim the batch returned to a Fleet mailbox surface.
    pub async fn collaboration_claim_recent_mailbox(
        &self,
        origin: &CollaborationOrigin,
    ) -> Result<Vec<CollaborationRequest>, RuntimeError> {
        let hello = self.hello(CLIENT_CALL_TIMEOUT).await?;
        if !hello
            .capabilities
            .iter()
            .any(|cap| cap == collaboration::INBOX_LIMIT_CAPABILITY)
        {
            return Err(RuntimeError::Daemon("bounded Fleet inbox claims require a newer tendd; refusing an unbounded claim that could hide requests".into()));
        }
        let response = self
            .call_checked(&serde_json::json!({
                "protocol": PROTOCOL_VERSION, "kind":"collaboration_inbox", "origin":origin,
                "limit":crate::fleet::FLEET_MAILBOX_REQUEST_LIMIT
            }))
            .await?;
        serde_json::from_value(response["collaboration_requests"].clone())
            .map_err(RuntimeError::Json)
    }

    /// Body-free room relationship data for an operator surface.
    /// Every live worker tend launched, host-wide. Read-only.
    pub async fn collaboration_workers(
        &self,
        idle_threshold_secs: u64,
    ) -> Result<collaboration::WorkerInventory, RuntimeError> {
        let response = self
            .call_checked(&serde_json::json!({
                "protocol": PROTOCOL_VERSION,
                "kind": "collaboration_workers",
                "idle_threshold_secs": idle_threshold_secs,
            }))
            .await?;
        serde_json::from_value(response["worker_inventory"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn collaboration_overview(
        &self,
        origin: &CollaborationOrigin,
    ) -> Result<collaboration::CollaborationOverview, RuntimeError> {
        let response = self.call_checked(&serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"collaboration_overview","origin":origin})).await?;
        serde_json::from_value(response["collaboration_overview"].clone())
            .map_err(RuntimeError::Json)
    }

    /// Small read model for dashboards; never execution instructions or claims.
    pub async fn collaboration_open_counts(
        &self,
        origin: &CollaborationOrigin,
        scope: MailboxScope,
    ) -> Result<collaboration::CollaborationOpenCounts, RuntimeError> {
        let mut response = self
            .call_checked(&serde_json::json!({
                "protocol": PROTOCOL_VERSION, "kind": "collaboration_open_counts",
                "origin": origin, "scope": scope,
            }))
            .await?;
        let value = response
            .get_mut("collaboration_open_counts")
            .map_or(serde_json::Value::Null, serde_json::Value::take);
        serde_json::from_value(value).map_err(RuntimeError::Json)
    }

    /// Atomically read a Fleet display snapshot, or validate its prior token.
    /// Legacy callers may opt into the bounded relay compatibility cache;
    /// every reuse still requires a fresh authorized response from the daemon.
    pub async fn collaboration_mailbox_snapshot(
        &self,
        origin: &CollaborationOrigin,
        operator_only: bool,
        if_unchanged: Option<&str>,
    ) -> Result<collaboration::MailboxSnapshot, RuntimeError> {
        let key = serde_json::to_string(&(origin, operator_only)).map_err(RuntimeError::Json)?;
        let cached = if if_unchanged.is_none() {
            if let Some(cache) = &self.mailbox_cache {
                cache.lock().await.get(&key)
            } else {
                None
            }
        } else {
            None
        };
        let token = if_unchanged.or_else(|| cached.as_ref().and_then(|s| s.token.as_deref()));
        let result = self.call_checked(&serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"collaboration_mailbox_snapshot","origin":origin,"operator_only":operator_only,"if_unchanged":token})).await;
        match result {
            Ok(mut response) => {
                let value = response
                    .get_mut("collaboration_mailbox_snapshot")
                    .map_or(serde_json::Value::Null, serde_json::Value::take);
                let snapshot: collaboration::MailboxSnapshot = match serde_json::from_value(value) {
                    Ok(snapshot) => snapshot,
                    Err(error) => {
                        self.forget_mailbox_cache(&key).await;
                        return Err(RuntimeError::Json(error));
                    }
                };
                if snapshot.unchanged {
                    if token.is_none()
                        || snapshot.token.as_deref() != token
                        || !snapshot.incoming.is_empty()
                        || !snapshot.sent.is_empty()
                    {
                        self.forget_mailbox_cache(&key).await;
                        return Err(RuntimeError::Daemon(
                            "invalid conditional mailbox response".into(),
                        ));
                    }
                    if if_unchanged.is_none() {
                        return cached.ok_or_else(|| {
                            RuntimeError::Daemon(
                                "conditional mailbox cache was not available".into(),
                            )
                        });
                    }
                    return Ok(snapshot);
                }
                if let Some(cache) = &self.mailbox_cache {
                    cache.lock().await.put(key, &snapshot);
                }
                Ok(snapshot)
            }
            Err(RuntimeError::Daemon(error))
                if error.contains("unknown variant")
                    && error.contains("collaboration_mailbox_snapshot") =>
            {
                self.forget_mailbox_cache(&key).await;
                // Unsupported reads may fall back; origin/data failures never do.
                let agent = CollaborationOrigin {
                    console: false,
                    ..origin.clone()
                };
                let (mut incoming, sent, human) = tokio::try_join!(
                    async {
                        if operator_only {
                            Ok(Vec::new())
                        } else {
                            self.collaboration_recent_mailbox(&agent, RequestMailbox::Incoming)
                                .await
                        }
                    },
                    self.collaboration_recent_mailbox(origin, RequestMailbox::Sent),
                    self.collaboration_recent_mailbox(origin, RequestMailbox::Incoming)
                )?;
                incoming.extend(human);
                Ok(collaboration::MailboxSnapshot {
                    token: None,
                    unchanged: false,
                    incoming,
                    sent,
                })
            }
            Err(error) => {
                self.forget_mailbox_cache(&key).await;
                Err(error)
            }
        }
    }

    /// One participant's mailbox — the caller's own.
    /// Fleet mailbox tabs already expose at most 32 recent full records. Apply
    /// that bound at the daemon query, before transferring and decoding history.
    /// The existing query fallback keeps older daemons compatible.
    pub async fn collaboration_recent_mailbox(
        &self,
        origin: &CollaborationOrigin,
        mailbox: RequestMailbox,
    ) -> Result<Vec<collaboration::CollaborationRequest>, RuntimeError> {
        self.collaboration_query_view(
            origin,
            mailbox,
            MailboxScope::Caller,
            &collaboration::CollaborationQuery {
                limit: Some(crate::fleet::FLEET_MAILBOX_REQUEST_LIMIT),
                ..Default::default()
            },
            0,
            collaboration::CollaborationReadView::Full,
        )
        .await
        .map(|page| page.requests)
    }

    pub async fn collaboration_list(
        &self,
        origin: &CollaborationOrigin,
        mailbox: RequestMailbox,
    ) -> Result<Vec<CollaborationRequest>, RuntimeError> {
        self.collaboration_list_scoped(origin, mailbox, MailboxScope::Caller)
            .await
    }

    /// A mailbox listing that may reach past the caller's own endpoint.
    ///
    /// Anything wider than [`MailboxScope::Caller`] needs a console origin;
    /// the daemon rejects the rest.
    pub async fn collaboration_list_scoped(
        &self,
        origin: &CollaborationOrigin,
        mailbox: RequestMailbox,
        scope: MailboxScope,
    ) -> Result<Vec<CollaborationRequest>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "collaboration_list",
            "origin": origin,
            "mailbox": mailbox,
            "scope": scope,
        });
        let resp = self.call_checked(&req).await?;
        // An older daemon ignores the field and answers the caller-scoped
        // listing it has always answered. Reporting that as the fleet would be
        // worse than failing: the operator would read "no cross-session
        // traffic" off a mailbox that was never asked about it.
        if !matches!(scope, MailboxScope::Caller) && resp["collaboration_scope"].is_null() {
            return Err(RuntimeError::Daemon(
                "this tendd predates scoped collaboration listing; restart the daemon from the same tree as the CLI (`tend daemon restart`)"
                    .into(),
            ));
        }
        serde_json::from_value(resp["collaboration_requests"].clone()).map_err(RuntimeError::Json)
    }

    /// Read a bounded display page; `metadata_only` removes instruction/reply bodies.
    /// Older daemons fall back to the same local filtering and pagination.
    pub async fn collaboration_query(
        &self,
        origin: &CollaborationOrigin,
        mailbox: RequestMailbox,
        scope: MailboxScope,
        query: &collaboration::CollaborationQuery,
        offset: usize,
        metadata_only: bool,
    ) -> Result<collaboration::CollaborationPage, RuntimeError> {
        self.collaboration_query_view(
            origin,
            mailbox,
            scope,
            query,
            offset,
            if metadata_only {
                collaboration::CollaborationReadView::Metadata
            } else {
                collaboration::CollaborationReadView::Full
            },
        )
        .await
    }

    pub async fn collaboration_query_view(
        &self,
        origin: &CollaborationOrigin,
        mailbox: RequestMailbox,
        scope: MailboxScope,
        query: &collaboration::CollaborationQuery,
        offset: usize,
        view: collaboration::CollaborationReadView,
    ) -> Result<collaboration::CollaborationPage, RuntimeError> {
        self.collaboration_query_result(origin, mailbox, scope, query, offset, view, false)
            .await
            .map(|result| result.page)
    }

    pub async fn collaboration_query_auto(
        &self,
        origin: &CollaborationOrigin,
        mailbox: RequestMailbox,
        query: &collaboration::CollaborationQuery,
        offset: usize,
        view: collaboration::CollaborationReadView,
    ) -> Result<(collaboration::CollaborationPage, MailboxScope), RuntimeError> {
        self.collaboration_query_result(
            origin,
            mailbox,
            MailboxScope::Caller,
            query,
            offset,
            view,
            true,
        )
        .await
        .map(|result| (result.page, result.scope))
    }

    #[allow(clippy::too_many_arguments)] // Existing list controls plus opt-in projection and scope resolution.
    pub async fn collaboration_query_result(
        &self,
        origin: &CollaborationOrigin,
        mailbox: RequestMailbox,
        scope: MailboxScope,
        query: &collaboration::CollaborationQuery,
        offset: usize,
        view: collaboration::CollaborationReadView,
        auto_scope: bool,
    ) -> Result<CollaborationQueryResult, RuntimeError> {
        let make_request = |origin: &CollaborationOrigin, scope, auto_scope| {
            serde_json::json!({"protocol":PROTOCOL_VERSION,
            "kind":"collaboration_list","origin":origin,"mailbox":mailbox,"scope":scope,"query":query,"offset":offset,
            "metadata_only":view == collaboration::CollaborationReadView::Metadata, "view":view,"auto_scope":auto_scope})
        };
        let (response, requested_scope) = match self
            .call_checked(&make_request(origin, scope, auto_scope))
            .await
        {
            Ok(response) => (response, scope),
            Err(RuntimeError::Daemon(error))
                if auto_scope
                    && origin.agent_session_id.is_none()
                    && error.starts_with(
                        "collaboration origin is not a hook-correlated tracked pane agent:",
                    ) =>
            {
                let console = CollaborationOrigin {
                    console: true,
                    ..origin.clone()
                };
                (
                    self.call_checked(&make_request(&console, MailboxScope::Room, false))
                        .await?,
                    MailboxScope::Room,
                )
            }
            Err(error) => return Err(error),
        };
        if !matches!(requested_scope, MailboxScope::Caller)
            && response["collaboration_scope"].is_null()
        {
            return Err(RuntimeError::Daemon("this tendd predates scoped collaboration listing; restart the daemon (`tend daemon restart`)".into()));
        }
        let actual_scope = response
            .get("collaboration_scope")
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(RuntimeError::Json)?
            .unwrap_or(requested_scope);
        let legacy_requests: Option<Vec<CollaborationRequest>> =
            if response["collaboration_page"].is_null() {
                Some(
                    serde_json::from_value(response["collaboration_requests"].clone())
                        .map_err(RuntimeError::Json)?,
                )
            } else {
                None
            };
        let mut page = if let Some(requests) = &legacy_requests {
            query.page(requests, offset)
        } else {
            serde_json::from_value(response["collaboration_page"].clone())
                .map_err(RuntimeError::Json)?
        };
        for request in &mut page.requests {
            view.apply(request);
        }
        Ok(CollaborationQueryResult {
            byte_budget_supported: response["collaboration_byte_budget"].as_u64().is_some(),
            page,
            scope: actual_scope,
            legacy_requests,
        })
    }

    pub async fn collaboration_reply(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
        status: RequestStatus,
        body: &str,
        artifacts: &[String],
    ) -> Result<CollaborationRequest, RuntimeError> {
        self.collaboration_reply_view(
            origin,
            request_id,
            status,
            body,
            artifacts,
            collaboration::CollaborationReadView::Full,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)] // Existing reply contract plus opt-in response view.
    pub async fn collaboration_reply_view(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
        status: RequestStatus,
        body: &str,
        artifacts: &[String],
        view: collaboration::CollaborationReadView,
    ) -> Result<CollaborationRequest, RuntimeError> {
        self.collaboration_reply_report_view(
            origin, request_id, status, body, artifacts, None, view,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn collaboration_reply_report_view(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
        status: RequestStatus,
        body: &str,
        artifacts: &[String],
        report: Option<&collaboration::TaskReport>,
        view: collaboration::CollaborationReadView,
    ) -> Result<CollaborationRequest, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION, "view": view,
            "kind": if report.is_some() {"collaboration_reply_contract"} else {"collaboration_reply"},
            "report": report,
            "origin": origin,
            "request_id": request_id,
            "status": status,
            "body": body,
            "artifacts": artifacts,
        });
        let resp = self.call_checked(&req).await?;
        let mut request = serde_json::from_value(resp["collaboration_request"].clone())
            .map_err(RuntimeError::Json)?;
        view.apply(&mut request);
        Ok(request)
    }

    pub async fn collaboration_get(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
    ) -> Result<CollaborationRequest, RuntimeError> {
        self.collaboration_get_view(
            origin,
            request_id,
            collaboration::CollaborationReadView::Full,
        )
        .await
    }

    pub async fn collaboration_get_view(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
        view: collaboration::CollaborationReadView,
    ) -> Result<CollaborationRequest, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION, "view": view,
            "kind": "collaboration_get",
            "origin": origin,
            "request_id": request_id,
        });
        let resp = self.call_checked(&req).await?;
        let mut request = serde_json::from_value(resp["collaboration_request"].clone())
            .map_err(RuntimeError::Json)?;
        view.apply(&mut request);
        Ok(request)
    }

    /// Read without marking the reply read; older daemons reject this operation.
    pub async fn collaboration_peek(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
    ) -> Result<CollaborationRequest, RuntimeError> {
        self.collaboration_peek_view(
            origin,
            request_id,
            collaboration::CollaborationReadView::Full,
        )
        .await
    }

    pub async fn collaboration_peek_view(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
        view: collaboration::CollaborationReadView,
    ) -> Result<CollaborationRequest, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION, "view": view,
            "kind": "collaboration_peek",
            "origin": origin,
            "request_id": request_id,
        });
        let resp = self.call_checked(&req).await?;
        let mut request = serde_json::from_value(resp["collaboration_request"].clone())
            .map_err(RuntimeError::Json)?;
        view.apply(&mut request);
        Ok(request)
    }

    /// Wait for a request to become terminal through tendd's collaboration
    /// revision signal. The daemon returns the latest request on timeout, so
    /// callers distinguish completion from timeout via `status.is_terminal()`.
    pub async fn collaboration_wait(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
        timeout_secs: u64,
    ) -> Result<CollaborationRequest, RuntimeError> {
        self.collaboration_wait_view(
            origin,
            request_id,
            timeout_secs,
            collaboration::CollaborationReadView::Full,
        )
        .await
    }

    pub async fn collaboration_wait_view(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
        timeout_secs: u64,
        view: collaboration::CollaborationReadView,
    ) -> Result<CollaborationRequest, RuntimeError> {
        let timeout_secs = timeout_secs.clamp(1, MAX_COLLABORATION_WAIT_SECS);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        let mut backoff = WAIT_RETRY_INITIAL;
        let mut reached = false;
        loop {
            // Waiting is read-only, so after a restart it is asked again with
            // the same request id, and only for what is left of the caller's
            // original deadline.
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            let secs = (remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0)).max(1);
            let req = serde_json::json!({
                "protocol": PROTOCOL_VERSION,
                "kind": "collaboration_wait",
                "view": view,
                "origin": origin,
                "request_id": request_id,
                "timeout_secs": secs,
            });
            // Allow the normal connect/hello/serialization budget beyond the
            // daemon-side wait deadline. This remains bounded even if tendd wedges.
            let budget = Duration::from_secs(secs).saturating_add(CLIENT_CALL_TIMEOUT);
            let retry = match self.call_with_timeout(&req, budget).await {
                Ok(resp) if resp["ok"].as_bool().unwrap_or(false) => {
                    let mut request: CollaborationRequest =
                        serde_json::from_value(resp["collaboration_request"].clone())
                            .map_err(RuntimeError::Json)?;
                    view.apply(&mut request);
                    return Ok(request);
                }
                Ok(resp) if resp["restarting"].as_bool() == Some(true) => {
                    reached = true;
                    RuntimeError::Daemon(
                        resp["error"]
                            .as_str()
                            .unwrap_or("tendd is restarting")
                            .to_owned(),
                    )
                }
                Ok(resp) => {
                    let error = resp["error"]
                        .as_str()
                        .unwrap_or("collaboration wait failed");
                    if collaboration_wait_is_unsupported(error) {
                        tracing::debug!(
                            request_id,
                            "daemon lacks collaboration_wait; using bounded client-side compatibility wait"
                        );
                        let mut request = self
                            .collaboration_wait_legacy(origin, request_id, deadline)
                            .await?;
                        view.apply(&mut request);
                        return Ok(request);
                    }
                    return Err(RuntimeError::Json(serde::de::Error::custom(
                        error.to_string(),
                    )));
                }
                // The connection dropped mid-wait: an older daemon restarting
                // without answering, or one that died.
                Err(RuntimeError::Disconnected) => {
                    reached = true;
                    RuntimeError::Disconnected
                }
                // A connection the daemon closed while stopping can also
                // fail on the write, before any read sees the end.
                Err(error) if error.is_client_disconnect() => {
                    reached = true;
                    error
                }
                // While a restart is under way the socket can briefly be
                // absent; a daemon that was never reached is simply down.
                Err(error @ RuntimeError::NotConnected(_)) if reached => error,
                Err(error) => return Err(error),
            };
            if tokio::time::Instant::now() + backoff >= deadline {
                return Err(retry);
            }
            tracing::debug!(request_id, %retry, "collaboration wait interrupted; retrying");
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(WAIT_RETRY_MAX);
        }
    }

    pub(super) async fn collaboration_wait_legacy(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
        deadline: tokio::time::Instant,
    ) -> Result<CollaborationRequest, RuntimeError> {
        loop {
            let request = self.collaboration_get(origin, request_id).await?;
            if request.status.is_terminal() || tokio::time::Instant::now() >= deadline {
                return Ok(request);
            }
            tokio::time::sleep_until(
                deadline.min(tokio::time::Instant::now() + LEGACY_COLLABORATION_WAIT_POLL_INTERVAL),
            )
            .await;
        }
    }

    pub async fn collaboration_health(
        &self,
    ) -> Result<collaboration::CollaborationHealth, RuntimeError> {
        let response = self
            .call_checked(
                &serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"collaboration_health"}),
            )
            .await?;
        serde_json::from_value(response["collaboration_health"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn collaboration_retire(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
        reason: &str,
        evidence: &str,
        dry_run: bool,
    ) -> Result<CollaborationRequest, RuntimeError> {
        let hello = self.hello(CLIENT_CALL_TIMEOUT).await?;
        if !hello
            .capabilities
            .iter()
            .any(|c| c == collaboration::RETIRE_CAPABILITY)
        {
            return Err(RuntimeError::Daemon(
                "daemon lacks collaboration_retire_v1; upgrade tendd before retirement".into(),
            ));
        }
        let response = self.call_checked(&serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"collaboration_retire", "origin":origin, "request_id":request_id,"reason":reason,"evidence":evidence,"dry_run":dry_run})).await?;
        serde_json::from_value(response["collaboration_request"].clone())
            .map_err(RuntimeError::Json)
    }

    /// Filtered expiry without sending request bodies over IPC.
    pub async fn collaboration_expire_summary(
        &self,
        older_than_secs: u64,
        dry_run: bool,
        quiet_only: bool,
        request_id: Option<&str>,
    ) -> Result<collaboration::ExpireSummary, RuntimeError> {
        let hello = self.hello(CLIENT_CALL_TIMEOUT).await?;
        if !hello
            .capabilities
            .iter()
            .any(|c| c == collaboration::EXPIRE_SUMMARY_CAPABILITY)
        {
            // Brew can upgrade the CLI before the running daemon. Preserve
            // its filtered safety contract, and surface any legacy size error.
            let requests = self
                .collaboration_expire_filtered(older_than_secs, dry_run, quiet_only, request_id)
                .await?;
            return Ok(collaboration::ExpireSummary::from_requests(&requests));
        }
        let response = self.call_checked(&serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"collaboration_expire_summary",
            "older_than_secs":older_than_secs,"dry_run":dry_run,"quiet_only":quiet_only,"request_id":request_id})).await?;
        serde_json::from_value(response["collaboration_expire_summary"].clone())
            .map_err(RuntimeError::Json)
    }

    pub async fn collaboration_expire_filtered(
        &self,
        older_than_secs: u64,
        dry_run: bool,
        quiet_only: bool,
        request_id: Option<&str>,
    ) -> Result<Vec<CollaborationRequest>, RuntimeError> {
        {
            let hello = self.hello(CLIENT_CALL_TIMEOUT).await?;
            if !hello
                .capabilities
                .iter()
                .any(|c| c == collaboration::FILTERED_EXPIRE_CAPABILITY)
            {
                return Err(RuntimeError::Daemon(
                    "daemon lacks filtered expiry v2; upgrade tendd before cleanup".into(),
                ));
            }
        }
        let response = self.call_checked(&serde_json::json!({"protocol":PROTOCOL_VERSION,"kind":"collaboration_expire_filtered",
            "older_than_secs":older_than_secs,"dry_run":dry_run,"quiet_only":quiet_only,"request_id":request_id})).await?;
        serde_json::from_value(response["collaboration_requests"].clone())
            .map_err(RuntimeError::Json)
    }

    /// Expire never-claimed requests older than `older_than_secs`, or list
    /// them with `dry_run`. Requires the `collaboration_expire_v1` capability.
    pub async fn collaboration_expire(
        &self,
        older_than_secs: u64,
        dry_run: bool,
    ) -> Result<Vec<CollaborationRequest>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "collaboration_expire",
            "older_than_secs": older_than_secs,
            "dry_run": dry_run,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["collaboration_requests"].clone()).map_err(RuntimeError::Json)
    }

    pub async fn collaboration_cancel(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
    ) -> Result<CollaborationRequest, RuntimeError> {
        self.collaboration_cancel_view(
            origin,
            request_id,
            collaboration::CollaborationReadView::Full,
        )
        .await
    }

    pub async fn collaboration_cancel_view(
        &self,
        origin: &CollaborationOrigin,
        request_id: &str,
        view: collaboration::CollaborationReadView,
    ) -> Result<CollaborationRequest, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "view": view,
            "kind": "collaboration_cancel",
            "origin": origin,
            "request_id": request_id,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["collaboration_request"].clone()).map_err(RuntimeError::Json)
    }

    /// Withdraw the caller's interest in replies to its own requests. The
    /// caller must first confirm the daemon advertises
    /// [`crate::collaboration::REPLY_WAKE_WITHDRAW_CAPABILITY`].
    pub async fn collaboration_withdraw_reply_wakes(
        &self,
        origin: &CollaborationOrigin,
        request_ids: &[String],
    ) -> Result<Vec<CollaborationRequest>, RuntimeError> {
        let req = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "kind": "collaboration_withdraw_reply_wakes",
            "origin": origin,
            "request_ids": request_ids,
        });
        let resp = self.call_checked(&req).await?;
        serde_json::from_value(resp["collaboration_requests"].clone()).map_err(RuntimeError::Json)
    }
}
