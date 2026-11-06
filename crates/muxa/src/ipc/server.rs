use super::*;

/// Daemon-side server. Construct once, call `run` under the tokio runtime.
pub struct Server {
    pub(super) socket_path: PathBuf,
    pub(super) store: SharedStore,
    /// Backend the server forwards `BackendPaneSnapshot` pushes to (the
    /// zellij backend in a multi-host daemon; see `with_backend`).
    pub(super) backend: SharedBackend,
    /// The full set of backends the daemon observes, for namespace-scoped
    /// control routing (`send_prompt` / `capture`). Never empty:
    /// `backends[0]` is the primary/env-preferred host, used as the
    /// fallback when a pane id doesn't classify to a known namespace.
    pub(super) backends: Vec<SharedBackend>,
    pub(super) sessions: SharedSessionBackend,
    pub(super) collaboration: Arc<CollaborationStore>,
    pub(super) collaboration_audit: Arc<CollaborationAuditLog>,
    pub(super) ask: Arc<AskStore>,
    pub(super) agent_directory: Arc<crate::agent_directory::AgentDirectory>,
    pub(super) automation: Arc<AutomationStore>,
    pub(super) restart: Option<Arc<RestartController>>,
    pub(super) fleet: Option<FleetRuntime>,
    pub(super) pipeline_runs: Arc<PipelineRunStore>,
    pub(super) reviews: crate::review::ReviewStore,
    pub(super) work_up: Arc<WorkUpManager>,
    pub(super) mux_snapshots: Arc<crate::mux_snapshot_control::MuxSnapshotControl>, // WS-F: snapshot
    /// The daemon's `config.toml`, for the requests that read and replace it
    /// whole. `None` when muxad was started without one.
    pub(super) config_path: Option<PathBuf>,
    /// Daemon-owned swap jobs; `None` when the daemon could not take its
    /// job store, in which case every swap request is refused.
    pub(super) swap: Option<Arc<dyn crate::swap_service::SwapJobs>>,
    pub(super) handler_limit: usize,
    pub(super) endpoint_check: Duration,
}

impl Server {
    pub fn new(socket_path: PathBuf, store: SharedStore) -> Self {
        let backend = default_backend();
        let work_up = WorkUpManager::new(socket_path.clone());
        let mux_snapshots =
            crate::mux_snapshot_control::MuxSnapshotControl::new(socket_path.clone());
        let ask = AskStore::in_memory(crate::ask::AskOptions::default());
        ask.attach_registry(store.clone());
        Self {
            socket_path,
            store,
            backend: backend.clone(),
            backends: vec![backend],
            sessions: PtySessionBackend::shared(),
            collaboration: CollaborationStore::in_memory(CollaborationOptions::default()),
            collaboration_audit: CollaborationAuditLog::in_memory(),
            ask,
            agent_directory: Arc::default(),
            automation: AutomationStore::in_memory(crate::automation::AutomationConfig::default()),
            restart: None,
            fleet: None,
            pipeline_runs: PipelineRunStore::in_memory(),
            reviews: crate::review::ReviewStore::in_memory(),
            work_up,
            mux_snapshots,
            config_path: None,
            swap: None,
            handler_limit: MAX_INFLIGHT_HANDLERS,
            endpoint_check: ENDPOINT_CHECK_INTERVAL,
        }
    }

    /// Set the pane backend the server forwards `BackendPaneSnapshot`
    /// pushes to. The daemon passes the same `SharedBackend` the
    /// reconciler and discovery hold, so a plugin push updates the
    /// snapshot every consumer reads. Defaults to [`default_backend`]
    /// for callers (mostly tests) that never push.
    #[must_use]
    pub fn with_backend(mut self, backend: SharedBackend) -> Self {
        self.backend = backend;
        self
    }

    /// Thread the daemon's full backend set into the server so control
    /// methods (`send_prompt`, `capture`) can resolve the backend that
    /// governs a given pane id's namespace (`%…` → tmux, `herdr:…` →
    /// herdr, …), falling back to the primary (`backends[0]`) for
    /// unclassifiable ids. An empty set is ignored (keeps the
    /// [`Self::new`] default) so the invariant "`backends` is never
    /// empty" holds for the resolver.
    #[must_use]
    pub fn with_backends(mut self, backends: Vec<SharedBackend>) -> Self {
        if !backends.is_empty() {
            self.backends = backends;
        }
        self
    }

    /// Serve swap jobs from `swap`.
    #[must_use]
    pub fn with_swap(mut self, swap: Option<Arc<dyn crate::swap_service::SwapJobs>>) -> Self {
        self.swap = swap;
        self
    }

    /// Thread the daemon's config file path in so `config_read` /
    /// `config_write` can serve it. Without one, both refuse.
    #[must_use]
    pub fn with_config_path(mut self, path: Option<PathBuf>) -> Self {
        if let Some(manager) = Arc::get_mut(&mut self.work_up) {
            manager.config_path.clone_from(&path);
        }
        self.config_path = path;
        self
    }

    #[must_use]
    pub fn with_sessions(mut self, sessions: SharedSessionBackend) -> Self {
        self.sessions = sessions;
        self
    }

    #[must_use]
    pub fn with_ask(mut self, ask: Arc<AskStore>) -> Self {
        ask.attach_registry(self.store.clone());
        self.ask = ask;
        self
    }

    #[must_use]
    pub fn with_agent_directory(
        mut self,
        directory: Arc<crate::agent_directory::AgentDirectory>,
    ) -> Self {
        self.agent_directory = directory;
        self
    }

    /// Install the live automation engine state. Optional so embedders and
    /// tests keep an in-memory store with no rules — which does nothing.
    #[must_use]
    pub fn with_automation(mut self, automation: Arc<AutomationStore>) -> Self {
        self.automation = automation;
        self
    }

    #[must_use]
    pub fn with_collaboration(mut self, collaboration: Arc<CollaborationStore>) -> Self {
        self.collaboration = collaboration;
        self
    }

    #[must_use]
    pub fn with_collaboration_audit(mut self, audit: Arc<CollaborationAuditLog>) -> Self {
        self.collaboration_audit = audit;
        self
    }

    /// Allow this server to accept the restart control method. Kept opt-in so
    /// embedded servers and tests never drain unless they can re-exec.
    #[must_use]
    pub fn with_restart_controller(mut self, restart: Arc<RestartController>) -> Self {
        self.restart = Some(restart);
        self
    }

    /// Install the physical-host fleet cache and command router. Keeping this
    /// optional preserves embedders/tests and makes a disabled fleet consume
    /// no SSH processes or background resources.
    #[must_use]
    pub fn with_fleet(mut self, fleet: FleetRuntime) -> Self {
        self.work_up = WorkUpManager::with_remote(
            self.socket_path.clone(),
            Some(Arc::new(FleetWorkRunner {
                fleet: fleet.clone(),
            })),
        );
        if let Some(manager) = Arc::get_mut(&mut self.work_up) {
            manager.config_path.clone_from(&self.config_path);
        }
        self.fleet = Some(fleet);
        self
    }

    // WS-F: snapshot
    /// Share muxad's snapshot control with its automatic-snapshot task, so a
    /// restore started from the app holds that task off.
    #[must_use]
    pub fn with_mux_snapshots(
        mut self,
        mux_snapshots: Arc<crate::mux_snapshot_control::MuxSnapshotControl>,
    ) -> Self {
        self.mux_snapshots = mux_snapshots;
        self
    }

    #[must_use]
    pub fn with_pipeline_runs(mut self, pipeline_runs: Arc<PipelineRunStore>) -> Self {
        self.pipeline_runs = pipeline_runs;
        self
    }

    /// Serve review items from `reviews` (see `crate::review`).
    #[must_use]
    pub fn with_reviews(mut self, reviews: crate::review::ReviewStore) -> Self {
        self.reviews = reviews;
        self
    }

    #[cfg(test)]
    #[must_use]
    pub(super) fn with_handler_limit(mut self, handler_limit: usize) -> Self {
        self.handler_limit = handler_limit;
        self
    }

    #[cfg(test)]
    #[must_use]
    pub(super) fn with_endpoint_check(mut self, interval: Duration) -> Self {
        self.endpoint_check = interval;
        self
    }

    /// Run until `shutdown` fires or an I/O error occurs.
    ///
    /// In-flight connection handlers are tracked on a `JoinSet` so a
    /// clean shutdown can drain them before returning. Without that
    /// drain, an ingest landing during shutdown could call
    /// `Store::apply` *after* the snapshotter task has already done its
    /// final flush, losing that event on the next restart. Drained with
    /// a bounded timeout so a hung handler can't block daemon exit.
    #[allow(clippy::too_many_lines)] // accept loop plus bounded drain and socket cleanup
    pub async fn run(self, mut shutdown: broadcast::Receiver<()>) -> Result<(), RuntimeError> {
        // Held until `run` returns, so a second daemon can never clear this
        // one's socket.
        let _claim = self.claim_endpoint().await?;
        self.bind_with_perms()?;
        let mut listener = Listener::bind(&self.socket_path)?;
        let mut bound = transport::endpoint_identity(&self.socket_path);
        tracing::info!(socket = %self.socket_path.display(), "listening");
        let mut endpoint_check = tokio::time::interval(self.endpoint_check);
        endpoint_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        let mut handlers: JoinSet<()> = JoinSet::new();
        // Sticky signal: a handler entering push mode after shutdown still
        // observes it. Request mutations themselves are never cancelled by it.
        let (stopping_tx, stopping_rx) = watch::channel(false);
        // Fixed budget of concurrent handlers. A permit is held for the
        // lifetime of each handler and released when it ends, so live fds
        // from handlers can never exceed `MAX_INFLIGHT_HANDLERS` — keeping
        // the process comfortably below its fd limit no matter how many
        // clients (or hung hooks) pile up.
        let handler_limit = self.handler_limit;
        let permits = std::sync::Arc::new(tokio::sync::Semaphore::new(handler_limit));

        loop {
            let permit = tokio::select! {
                _ = shutdown.recv() => {
                    tracing::info!("shutdown signal received; closing listener");
                    break;
                }
                _ = endpoint_check.tick() => {
                    self.restore_endpoint(&mut listener, &mut bound);
                    continue;
                }
                joined = handlers.join_next(), if !handlers.is_empty() => {
                    if let Some(Err(e)) = joined {
                        tracing::warn!(error = %e, "connection handler task failed");
                    }
                    continue;
                }
                permit = permits.clone().acquire_owned() => {
                    match permit {
                        Ok(permit) => permit,
                        Err(_) => break,
                    }
                }
            };

            tokio::select! {
                _ = shutdown.recv() => {
                    drop(permit);
                    tracing::info!("shutdown signal received; closing listener");
                    break;
                }
                _ = endpoint_check.tick() => {
                    drop(permit);
                    self.restore_endpoint(&mut listener, &mut bound);
                }
                accept = listener.accept() => {
                    let stream = match accept {
                        Ok(pair) => pair,
                        Err(e) if is_fd_exhaustion(&e) => {
                            // Out of file descriptors. Do NOT propagate: a
                            // returned error kills the accept loop and wedges
                            // the daemon into refusing every connection
                            // forever (the failure mode this whole change
                            // exists to prevent). Back off briefly so we
                            // neither spin at 100% CPU nor starve in-flight
                            // handlers of the CPU they need to free fds.
                            tracing::error!(
                                error = %e,
                                inflight = handler_limit - permits.available_permits(),
                                "accept hit fd exhaustion; backing off",
                            );
                            drop(permit);
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            continue;
                        }
                        Err(e) => {
                            // Per-connection errors (client aborted mid-accept,
                            // etc.) are transient; log and keep serving.
                            tracing::warn!(error = %e, "accept error; continuing");
                            drop(permit);
                            continue;
                        }
                    };
                    let store = self.store.clone();
                    let backend = self.backend.clone();
                    let backends = self.backends.clone();
                    let sessions = self.sessions.clone();
                    let collaboration = self.collaboration.clone();
                    let collaboration_audit = self.collaboration_audit.clone();
                    let ask = self.ask.clone();
                    let agent_directory = self.agent_directory.clone();
                    let automation = self.automation.clone();
                    let restart = self.restart.clone();
                    let fleet = self.fleet.clone();
                    let pipeline_runs = self.pipeline_runs.clone();
                    let reviews = self.reviews.clone();
                    let work_up = self.work_up.clone();
                    let mux_snapshots = self.mux_snapshots.clone();
                    let config_path = self.config_path.clone();
                    let swap = self.swap.clone();
                    let stopping = stopping_rx.clone();
                    handlers.spawn(async move {
                        // Held for the handler's lifetime; released here on exit.
                        let _permit = permit;
                        if let Err(e) =
                            Box::pin(handle(
                                stream,
                                store,
                                backend,
                                backends,
                                sessions,
                                collaboration,
                                collaboration_audit,
                                ask,
                                agent_directory,
                                automation,
                                restart,
                                fleet,
                                pipeline_runs,
                                reviews,
                                work_up,
                                mux_snapshots,
                                config_path,
                                swap,
                                stopping,
                            ))
                            .await
                        {
                            if e.is_client_disconnect() {
                                tracing::debug!(error = %e, "client disconnected");
                                return;
                            }
                            tracing::warn!(error = %e, "connection handler failed");
                        }
                    });
                    // Reap finished handlers opportunistically so the JoinSet
                    // doesn't grow unboundedly under steady traffic.
                    while handlers.try_join_next().is_some() {}
                }
            }
        }

        stopping_tx.send_replace(true);
        let drain_started = Instant::now();

        // Drain in-flight handlers with a bounded timeout. Closes the
        // lost-update window where a handler could call `Store::apply`
        // after the daemon's snapshotter has already exited.
        let drain = async { while handlers.join_next().await.is_some() {} };
        if tokio::time::timeout(HANDLER_DRAIN_TIMEOUT, drain)
            .await
            .is_err()
        {
            tracing::warn!(
                timeout_secs = HANDLER_DRAIN_TIMEOUT.as_secs(),
                remaining = handlers.len(),
                "ipc handlers did not drain within timeout; aborting",
            );
            handlers.abort_all();
            // Best-effort: let the abort propagate.
            while handlers.join_next().await.is_some() {}
        } else {
            tracing::debug!(
                elapsed_ms = u64::try_from(drain_started.elapsed().as_millis()).unwrap_or(u64::MAX),
                "ipc handlers drained cleanly"
            );
        }

        // Remove our own socket file so next startup is clean — but only
        // ours: a file someone else put there since is not ours to delete.
        if transport::endpoint_identity(&self.socket_path) == bound {
            transport::remove_endpoint(&self.socket_path);
        }
        Ok(())
    }

    /// Take the endpoint claim, waiting out a brief hold. A child forked
    /// a moment ago keeps a copy of the lock until it execs, so the daemon
    /// that just released the claim — or this same daemon, re-executed —
    /// can find it held for a few milliseconds by no daemon at all.
    pub(super) async fn claim_endpoint(&self) -> Result<transport::EndpointClaim, RuntimeError> {
        let deadline = Instant::now() + CLAIM_WAIT;
        loop {
            if let Some(claim) = transport::claim_endpoint(&self.socket_path)? {
                return Ok(claim);
            }
            if Instant::now() >= deadline {
                return Err(RuntimeError::SocketInUse(self.socket_path.clone()));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Bind the socket path again if it no longer holds the file this server
    /// bound. The claim proves no other daemon owns the path, so whatever is
    /// there now (nothing, or a stray file) is safe to replace. Connections
    /// already accepted are unaffected.
    pub(super) fn restore_endpoint(
        &self,
        listener: &mut Listener,
        bound: &mut Option<transport::EndpointIdentity>,
    ) {
        if bound.is_none() || transport::endpoint_identity(&self.socket_path) == *bound {
            return;
        }
        tracing::warn!(
            socket = %self.socket_path.display(),
            "IPC socket file was removed or replaced; binding it again",
        );
        let rebound = transport::clear_endpoint(&self.socket_path)
            .and_then(|()| Listener::bind(&self.socket_path));
        match rebound {
            Ok(fresh) => {
                *listener = fresh;
                *bound = transport::endpoint_identity(&self.socket_path);
            }
            Err(error) => tracing::error!(
                socket = %self.socket_path.display(),
                %error,
                "could not bind the IPC socket again; retrying",
            ),
        }
    }

    /// Pre-bind sequence: if a stale socket exists, remove it; then the
    /// caller binds and immediately chmods 0600 in `run`.
    pub(super) fn bind_with_perms(&self) -> Result<(), RuntimeError> {
        // A live endpoint means a daemon is already serving this path. Anything
        // else that occupies it — an orphaned socket file from an abnormal exit —
        // is ours to clear, and clearing a path that was never there succeeds.
        if transport::endpoint_is_live(&self.socket_path) {
            return Err(RuntimeError::SocketInUse(self.socket_path.clone()));
        }
        transport::clear_endpoint(&self.socket_path)?;
        if let Some(parent) = self.socket_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(())
    }
}
