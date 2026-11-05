use super::{
    broadcast, AtomicOrdering, AtomicU8, Duration, RESTART_REQUESTED, RESTART_RUNNING,
    RESTART_STOPPING,
};

/// Coordinates daemon shutdown and self-restart without allowing an already
/// open IPC handler to undo an operator's later SIGTERM/SIGINT.
///
/// The state transition is monotonic: `running -> restart_requested ->
/// stopping`, while a signal may move `running -> stopping` directly. Once
/// stopping, a restart request is refused permanently. This closes the race
/// in which a signal cleared a boolean and a draining handler set it again.
#[derive(Debug)]
pub struct RestartController {
    pub(super) generation: u64,
    pub(super) state: AtomicU8,
    pub(super) trigger: broadcast::Sender<()>,
    pub(super) upgrade_check: Option<crate::binary_upgrade::UpgradeCheck>,
    pub(super) approved: tokio::sync::Mutex<Option<crate::binary_upgrade::ValidatedLaunch>>,
}

impl RestartController {
    #[must_use]
    pub fn new(generation: u64, trigger: broadcast::Sender<()>) -> Self {
        Self {
            generation,
            state: AtomicU8::new(RESTART_RUNNING),
            trigger,
            upgrade_check: None,
            approved: tokio::sync::Mutex::new(None),
        }
    }

    #[must_use]
    pub fn with_upgrade_check(
        mut self,
        check: Option<crate::binary_upgrade::UpgradeCheck>,
    ) -> Self {
        self.upgrade_check = check;
        self
    }
    pub fn upgrade_stamp(&self) -> Result<Option<crate::binary_upgrade::LaunchStamp>, String> {
        self.upgrade_check
            .as_ref()
            .map(crate::binary_upgrade::UpgradeCheck::stamp)
            .transpose()
    }
    pub async fn validated_launch(&self) -> Option<crate::binary_upgrade::ValidatedLaunch> {
        self.approved.lock().await.clone()
    }
    /// Probe before broadcasting shutdown; refusal keeps IPC and writers alive.
    pub async fn request_checked_restart(&self) -> Result<bool, String> {
        let mut approved = self.approved.lock().await;
        if self.restart_requested() {
            return Ok(true);
        }
        if self.state.load(AtomicOrdering::SeqCst) == RESTART_STOPPING {
            return Ok(false);
        }
        if let Some(check) = &self.upgrade_check {
            *approved = Some(check.validate().await?);
        }
        Ok(self.request_restart())
    }
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Commit to a normal stop and wake every shutdown subscriber. A later
    /// IPC request cannot move the state back to restart-requested.
    pub fn stop(&self) {
        self.state.store(RESTART_STOPPING, AtomicOrdering::SeqCst);
        let _ = self.trigger.send(());
    }

    #[must_use]
    pub fn restart_requested(&self) -> bool {
        self.state.load(AtomicOrdering::SeqCst) == RESTART_REQUESTED
    }

    /// Returns false only after an explicit stop has won. Repeated restart
    /// requests are idempotently accepted while the first request drains.
    pub(super) fn request_restart(&self) -> bool {
        match self.state.compare_exchange(
            RESTART_RUNNING,
            RESTART_REQUESTED,
            AtomicOrdering::SeqCst,
            AtomicOrdering::SeqCst,
        ) {
            Ok(_) => {
                let _ = self.trigger.send(());
                true
            }
            Err(RESTART_REQUESTED) => true,
            Err(_) => false,
        }
    }
}

pub(super) const MAX_RETAINED_WORK_OPERATIONS: usize = 32;
pub(super) const MAX_CONCURRENT_WORK_OPERATIONS: usize = 4;
pub(super) const MAX_CONCURRENT_WORK_COMMANDS: usize = 4;
/// Transport slack added to a child's own budget when a `work` argv travels
/// through the Fleet manager (relay round trip or OpenSSH session setup).
pub(super) const FLEET_WORK_COMMAND_SLACK: Duration = Duration::from_secs(15);
