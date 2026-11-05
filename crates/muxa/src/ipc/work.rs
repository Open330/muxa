use super::{
    work_control, Arc, AtomicOrdering, AtomicU64, BTreeMap, FleetOperation, FleetRuntime, PathBuf,
    RemoteWorkRunner, Serialize, VecDeque, WorkCommandLimits, WorkCommandOutput,
    WorkCommandSurface, WorkUpRequest, FLEET_WORK_COMMAND_SLACK, MAX_CONCURRENT_WORK_COMMANDS,
    MAX_CONCURRENT_WORK_OPERATIONS, MAX_RETAINED_WORK_OPERATIONS,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkUpOperationState {
    Running,
    Succeeded,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct WorkUpOperation {
    pub operation_id: String,
    pub state: WorkUpOperationState,
    pub work: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
}

#[derive(Debug)]
pub(super) struct TrackedWorkUp {
    pub(super) request: WorkUpRequest,
    pub(super) operation: WorkUpOperation,
}

#[derive(Debug, Default)]
pub(super) struct WorkUpOperations {
    pub(super) values: BTreeMap<String, TrackedWorkUp>,
    pub(super) order: VecDeque<String>,
}

pub(super) struct WorkUpManager {
    pub(super) socket_path: PathBuf,
    pub(super) config_path: Option<PathBuf>,
    /// Runs `work` argv on remote Fleet hosts. `None` while Fleet is not
    /// installed, in which case every non-local `host` is refused.
    pub(super) remote: Option<Arc<dyn RemoteWorkRunner>>,
    pub(super) next_id: AtomicU64,
    pub(super) operations: tokio::sync::Mutex<WorkUpOperations>,
    pub(super) commands: tokio::sync::Semaphore,
}

impl std::fmt::Debug for WorkUpManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WorkUpManager")
            .field("socket_path", &self.socket_path)
            .field("remote", &self.remote.is_some())
            .finish_non_exhaustive()
    }
}

/// Routes remote `work` argv through the Fleet manager, which owns per-host
/// authorization, the SSH relay, and the OpenSSH fallback for older relays.
pub(super) struct FleetWorkRunner {
    pub(super) fleet: FleetRuntime,
}

impl RemoteWorkRunner for FleetWorkRunner {
    fn host_mode<'a>(&'a self, host: &'a str) -> work_control::HostModeFuture<'a> {
        Box::pin(async move {
            self.fleet
                .store
                .snapshot()
                .await
                .hosts
                .into_iter()
                .find(|candidate| candidate.alias == host)
                .map(|candidate| candidate.mode)
                .ok_or_else(|| {
                    work_control::WorkCommandError::Invalid(format!(
                        "fleet host '{host}' is not configured"
                    ))
                })
        })
    }

    fn run<'a>(
        &'a self,
        host: &'a str,
        args: Vec<String>,
        stdin: Option<String>,
        limits: WorkCommandLimits,
    ) -> work_control::RemoteWorkFuture<'a> {
        Box::pin(async move {
            let result = self
                .fleet
                .execute(
                    host,
                    FleetOperation::WorkCommand { args, stdin },
                    limits.timeout + FLEET_WORK_COMMAND_SLACK,
                )
                .await
                .map_err(work_control::WorkCommandError::Failed)?;
            result.command_output().ok_or_else(|| {
                work_control::WorkCommandError::Failed(format!(
                    "fleet host '{host}' answered without a command result"
                ))
            })
        })
    }
}

impl WorkUpManager {
    pub(super) fn new(socket_path: PathBuf) -> Arc<Self> {
        Self::with_remote(socket_path, None)
    }

    pub(super) fn with_remote(
        socket_path: PathBuf,
        remote: Option<Arc<dyn RemoteWorkRunner>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            socket_path,
            config_path: None,
            remote,
            next_id: AtomicU64::new(1),
            operations: tokio::sync::Mutex::new(WorkUpOperations::default()),
            commands: tokio::sync::Semaphore::new(MAX_CONCURRENT_WORK_COMMANDS),
        })
    }

    /// Resolve and authorize the runner for `host`; `None` means the daemon's
    /// own host. Observe-only hosts are refused here, before any operation is
    /// recorded, so the caller gets a synchronous error rather than a failed
    /// operation.
    pub(super) async fn remote_for(
        &self,
        host: Option<&str>,
        args: &[String],
    ) -> Result<Option<(String, Arc<dyn RemoteWorkRunner>)>, String> {
        let Some(host) = work_control::remote_host_alias(host) else {
            return Ok(None);
        };
        let runner = self
            .remote
            .as_ref()
            .ok_or_else(|| format!("fleet is not enabled in muxad; cannot reach host '{host}'"))?;
        let mode = runner
            .host_mode(host)
            .await
            .map_err(|error| error.to_string())?;
        work_control::authorize_work_command(host, mode, args)
            .map_err(|error| error.to_string())?;
        Ok(Some((host.to_string(), Arc::clone(runner))))
    }

    /// Run one allowlisted `work` subcommand to completion.
    pub(super) async fn command(
        &self,
        host: Option<String>,
        args: Vec<String>,
        stdin: Option<String>,
    ) -> Result<WorkCommandOutput, String> {
        work_control::validate_work_command(&args, stdin.as_deref(), WorkCommandSurface::Ipc)
            .map_err(|error| error.to_string())?;
        if host.as_deref().is_some_and(|host| host.trim().is_empty()) {
            return Err("host alias is empty".into());
        }
        let remote = self.remote_for(host.as_deref(), &args).await?;
        let _permit = self.commands.try_acquire().map_err(|_| {
            format!("at most {MAX_CONCURRENT_WORK_COMMANDS} work commands may run at once")
        })?;
        let limits = WorkCommandLimits::for_args(&args);
        let mut local_args = args.clone();
        if let Some(path) = &self.config_path {
            local_args.splice(0..0, ["--config".into(), path.display().to_string()]);
        }
        match remote {
            Some((host, runner)) => runner
                .run(&host, args, stdin, limits)
                .await
                .map_err(|error| error.to_string()),
            None => work_control::execute_work_command(
                &work_control::resolve_muxa_binary(),
                &local_args,
                stdin.as_deref(),
                Some(&self.socket_path),
                limits,
            )
            .await
            .map_err(|error| error.to_string()),
        }
    }

    // WS-B: new agent
    /// Start one agent with the canonical `muxa agent start`, here or on a
    /// control-mode Fleet host, sharing the `work_command` concurrency cap.
    pub(super) async fn agent_start(
        &self,
        request: crate::agent_control::AgentStartRequest,
    ) -> Result<crate::agent_control::AgentStartResult, String> {
        request.validate().map_err(|error| error.to_string())?;
        let args = request.arguments();
        let remote = self.remote_for(request.host.as_deref(), &args).await?;
        let _permit = self.commands.try_acquire().map_err(|_| {
            format!("at most {MAX_CONCURRENT_WORK_COMMANDS} work commands may run at once")
        })?;
        match remote {
            Some((host, runner)) => {
                let output = runner
                    .run(&host, args, None, WorkCommandLimits::COMMAND)
                    .await
                    .map_err(|error| error.to_string())?;
                crate::agent_control::parse_output(&output, Some(&host))
                    .map_err(|error| error.to_string())
            }
            None => crate::agent_control::execute_local(
                &work_control::resolve_muxa_binary(),
                &request,
                Some(&self.socket_path),
            )
            .await
            .map_err(|error| error.to_string()),
        }
    }

    pub(super) async fn start(
        self: &Arc<Self>,
        request: WorkUpRequest,
    ) -> Result<WorkUpOperation, String> {
        request.validate().map_err(|error| error.to_string())?;
        let arguments = request.arguments();
        let remote = self.remote_for(request.host.as_deref(), &arguments).await?;
        let mut operations = self.operations.lock().await;
        if let Some(existing) = operations.values.values().find(|tracked| {
            tracked.operation.state == WorkUpOperationState::Running && tracked.request == request
        }) {
            return Ok(existing.operation.clone());
        }
        let running = operations
            .values
            .values()
            .filter(|tracked| tracked.operation.state == WorkUpOperationState::Running)
            .count();
        if running >= MAX_CONCURRENT_WORK_OPERATIONS {
            return Err(format!(
                "at most {MAX_CONCURRENT_WORK_OPERATIONS} Work operations may run at once"
            ));
        }
        while operations.values.len() >= MAX_RETAINED_WORK_OPERATIONS {
            let removable = operations.order.iter().position(|id| {
                operations
                    .values
                    .get(id)
                    .is_some_and(|tracked| tracked.operation.state != WorkUpOperationState::Running)
            });
            let Some(index) = removable else {
                return Err("Work operation history is full of running operations".into());
            };
            if let Some(id) = operations.order.remove(index) {
                operations.values.remove(&id);
            }
        }

        let operation_id = format!(
            "native-work-{}",
            self.next_id.fetch_add(1, AtomicOrdering::Relaxed)
        );
        let operation = WorkUpOperation {
            operation_id: operation_id.clone(),
            state: WorkUpOperationState::Running,
            work: request.work.trim().to_string(),
            workspace: request
                .workspace
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string),
            message: "Starting configured Work pipeline…".into(),
            result: None,
        };
        operations.order.push_back(operation_id.clone());
        operations.values.insert(
            operation_id.clone(),
            TrackedWorkUp {
                request: request.clone(),
                operation: operation.clone(),
            },
        );
        drop(operations);

        let manager = Arc::clone(self);
        tokio::spawn(async move {
            let outcome = match remote {
                Some((host, runner)) => runner
                    .run(&host, arguments, None, WorkCommandLimits::WORK_UP)
                    .await
                    .map_err(work_control::WorkUpError::from)
                    .and_then(|output| work_control::work_up_result(&output)),
                None => work_control::execute_work_up(&request, Some(&manager.socket_path)).await,
            };
            let mut operations = manager.operations.lock().await;
            let Some(tracked) = operations.values.get_mut(&operation_id) else {
                return;
            };
            match outcome {
                Ok(result) => {
                    tracked.operation.state = WorkUpOperationState::Succeeded;
                    tracked.operation.message = if request.dry_run {
                        "Work plan is ready".into()
                    } else {
                        "Work pipeline started".into()
                    };
                    tracked.operation.result = Some(result);
                }
                Err(error) => {
                    tracked.operation.state = WorkUpOperationState::Failed;
                    tracked.operation.message = error.to_string();
                }
            }
        });
        Ok(operation)
    }

    pub(super) async fn status(&self, operation_id: &str) -> Option<WorkUpOperation> {
        self.operations
            .lock()
            .await
            .values
            .get(operation_id)
            .map(|tracked| tracked.operation.clone())
    }
}
