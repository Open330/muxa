//! Shared, event-driven Fleet reply waits. No long wait occupies a host command slot.
use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use tokio::sync::{broadcast, watch, Mutex};
use tokio::time::Instant;

use crate::collaboration::CollaborationRequest;
use crate::fleet::{FleetHostState, FleetOperation, FleetRuntime, HostAccessMode};
use crate::topology::PaneKey;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
type WaitValue = Option<Result<CollaborationRequest, String>>;
type WaitKey = (String, PaneKey, String);

#[derive(Clone, Copy)]
pub(crate) struct ReplyHost {
    pub local: bool,
    pub mode: HostAccessMode,
    pub state: FleetHostState,
    pub generation: Option<u64>,
    pub event_driven: bool,
}

#[derive(Default)]
pub(crate) struct ReplyWaits(Mutex<HashMap<WaitKey, Weak<watch::Sender<WaitValue>>>>);

impl FleetRuntime {
    /// Share reads across IPC clients while retaining each caller's own deadline.
    pub async fn wait_for_reply(
        &self,
        host: String,
        pane: PaneKey,
        request_id: String,
        timeout: Duration,
        after_update: Option<u64>,
    ) -> Result<Option<CollaborationRequest>, String> {
        let deadline = Instant::now() + timeout;
        let metadata = self
            .store
            .reply_wait_host(&host)
            .await
            .ok_or_else(|| format!("Fleet host '{host}' is not known"))?;
        if !metadata.local && metadata.mode != HostAccessMode::Control {
            return Err(format!("Fleet host '{host}' is observe-only"));
        }
        let key = (host, pane, request_id);
        let mut rx = {
            let mut waits = self.reply_waits.0.lock().await;
            waits.retain(|_, sender| sender.strong_count() > 0);
            if let Some(sender) = waits.get(&key).and_then(Weak::upgrade) {
                sender.subscribe()
            } else {
                let (sender, receiver) = watch::channel(None);
                let sender = Arc::new(sender);
                waits.insert(key.clone(), Arc::downgrade(&sender));
                let runtime = self.clone();
                tokio::spawn(async move {
                    // Dropping the last caller cancels both pending IO and the subscription.
                    let work = runtime.drive_reply_wait(&key, &sender);
                    tokio::pin!(work);
                    loop {
                        tokio::select! {
                            () = sender.closed() => {
                                let mut waits = runtime.reply_waits.0.lock().await;
                                if sender.receiver_count() == 0 {
                                    waits.remove(&key);
                                    break;
                                }
                            },
                            () = &mut work => break,
                        }
                    }
                });
                receiver
            }
        };
        loop {
            let current = rx.borrow_and_update().clone();
            match current {
                Some(Err(error)) => return Err(error),
                Some(Ok(request)) if request.status.is_terminal() => return Ok(Some(request)),
                Some(Ok(request))
                    if after_update.is_some_and(|seen| {
                        request.updates.last().is_some_and(|u| u.sequence > seen)
                    }) =>
                {
                    return Ok(Some(request))
                }
                _ => {}
            }
            if tokio::time::timeout_at(deadline, rx.changed())
                .await
                .is_err()
            {
                return rx.borrow().clone().transpose();
            }
            if rx.has_changed().is_err() {
                return rx.borrow().clone().transpose();
            }
        }
    }

    async fn drive_reply_wait(&self, key: &WaitKey, sender: &watch::Sender<WaitValue>) {
        // Subscribe before the initial read: completion racing that read stays queued.
        let mut updates = self.store.subscribe();
        let mut backoff = Duration::from_secs(1);
        let mut reads = 0_u64;
        loop {
            // Coalesce pending invalidations before the authoritative read.
            while matches!(
                updates.try_recv(),
                Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_))
            ) {}
            let Some(host) = self.store.reply_wait_host(&key.0).await else {
                sender.send_replace(Some(Err(format!("Fleet host '{}' is not known", key.0))));
                return;
            };
            if !host.local && host.mode != HostAccessMode::Control {
                sender.send_replace(Some(Err(format!("Fleet host '{}' is observe-only", key.0))));
                return;
            }
            let event_driven = host.event_driven;
            let generation = host.generation;
            let state = host.state;
            let result = self
                .execute(
                    &key.0,
                    FleetOperation::CollaborationGet {
                        pane: key.1.clone(),
                        request_id: key.2.clone(),
                    },
                    COMMAND_TIMEOUT,
                )
                .await
                .and_then(|result| {
                    result
                        .collaboration_request
                        .map(|r| *r)
                        .ok_or_else(|| "Fleet get returned no collaboration request".to_string())
                });
            reads += 1;
            let mut disconnected = false;
            match result {
                Ok(request) => {
                    let terminal = request.status.is_terminal();
                    sender.send_replace(Some(Ok(request)));
                    if terminal {
                        tracing::debug!(request_id = %key.2, reads, "fleet reply wait completed");
                        return;
                    }
                }
                Err(error) => {
                    disconnected = self.store.reply_wait_host(&key.0).await.is_some_and(|h| {
                        matches!(
                            h.state,
                            FleetHostState::Connecting
                                | FleetHostState::Offline
                                | FleetHostState::Degraded
                        )
                    });
                    if !disconnected {
                        sender.send_replace(Some(Err(error)));
                        return;
                    }
                }
            }
            let retry_at = Instant::now() + backoff;
            // A healthy current host is silent until a mailbox invalidation; legacy or
            // disconnected hosts reconcile with bounded exponential backoff.
            let retry = disconnected || !event_driven || !matches!(state, FleetHostState::Online);
            loop {
                tokio::select! {
                    () = tokio::time::sleep_until(retry_at), if retry => {
                        backoff = (backoff * 2).min(MAX_BACKOFF);
                        break;
                    }
                    update = updates.recv() => {
                        match update {
                            Ok(update) if update.resync => break,
                            Ok(update) if update.host == key.0 => {
                                let changed = self.store.reply_wait_host(&key.0).await
                                    .is_none_or(|h| h.state != state || h.generation != generation || (!h.local && h.mode != HostAccessMode::Control));
                                if update.mailbox_revision.is_some() || changed {
                                    break;
                                }
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => break,
                            Err(broadcast::error::RecvError::Closed) => return,
                            _ => {},
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collaboration::{
        CollaborationOptions, CollaborationStore, NewRequest, Participant, RequestStatus,
    };
    use crate::fleet::{
        FleetCommandReceiver, FleetCommandResult, FleetHostSnapshot, FleetStore,
        FLEET_MAILBOX_WATCH_CAPABILITY,
    };
    use serde_json::json;

    async fn fixture(
        watch: bool,
    ) -> (
        FleetRuntime,
        FleetCommandReceiver,
        PaneKey,
        CollaborationRequest,
    ) {
        let store = Arc::new(FleetStore::new());
        let host: FleetHostSnapshot = serde_json::from_value(json!({
            "alias":"local", "local":true, "ssh_target":"", "labels":{}, "annotations":{},
            "mode":"control", "state":"online", "capabilities": if watch { vec![FLEET_MAILBOX_WATCH_CAPABILITY] } else { vec![] },
        })).unwrap();
        store.upsert_host(host).await;
        let (runtime, receiver) = FleetRuntime::new(store);
        let pane = serde_json::from_value(json!({"window":{"session":{"host":"tmux","socket":"default","session_id":"$1"},"window_id":"@1"},"pane_id":"%2"})).unwrap();
        let participant: Participant = serde_json::from_value(json!({
            "agent_kind":"codex","agent_session_id":"agent", "pane":"%2", "socket":"default",
            "room":{"host":"tmux","socket":"default","window_id":"@1"},"state":"idle","roles":[]
        }))
        .unwrap();
        let mailbox = CollaborationStore::in_memory(CollaborationOptions::default());
        let request = mailbox
            .create(
                participant.clone(),
                participant,
                NewRequest {
                    body: "work".into(),
                    ..NewRequest::default()
                },
            )
            .await
            .unwrap();
        (runtime, receiver, pane, request)
    }

    fn wait(
        runtime: FleetRuntime,
        pane: PaneKey,
        request: &CollaborationRequest,
        seconds: u64,
    ) -> tokio::task::JoinHandle<Result<Option<CollaborationRequest>, String>> {
        let id = request.id.clone();
        tokio::spawn(async move {
            runtime
                .wait_for_reply("local".into(), pane, id, Duration::from_secs(seconds), None)
                .await
        })
    }

    #[tokio::test(start_paused = true)]
    async fn quiet_five_minutes_performs_one_read_and_shares_waiters() {
        let (runtime, mut commands, pane, request) = fixture(true).await;
        let first = wait(runtime.clone(), pane.clone(), &request, 300);
        let second = wait(runtime.clone(), pane, &request, 600);
        let command = commands.recv().await.unwrap();
        command
            .reply
            .send(Ok(FleetCommandResult::collaboration_request(
                request.clone(),
            )))
            .unwrap();
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(300)).await;
        assert_eq!(
            first.await.unwrap().unwrap().unwrap().status,
            request.status
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(1), commands.recv())
                .await
                .is_err()
        );
        assert!(!second.is_finished());
        runtime.store.notify_mailbox("local", 1).await;
        let command = commands.recv().await.unwrap();
        let mut completed = request;
        completed.status = RequestStatus::Completed;
        command
            .reply
            .send(Ok(FleetCommandResult::collaboration_request(completed)))
            .unwrap();
        assert_eq!(
            second.await.unwrap().unwrap().unwrap().status,
            RequestStatus::Completed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn completion_during_initial_read_is_not_lost_and_other_commands_run() {
        let (runtime, mut commands, pane, request) = fixture(true).await;
        let waiting = wait(runtime.clone(), pane.clone(), &request, 300);
        let initial = commands.recv().await.unwrap();
        runtime.store.notify_mailbox("local", 1).await;
        initial
            .reply
            .send(Ok(FleetCommandResult::collaboration_request(
                request.clone(),
            )))
            .unwrap();
        let refresh = commands.recv().await.unwrap();
        let operation = tokio::spawn(async move {
            runtime
                .execute(
                    "local",
                    FleetOperation::Capture { pane },
                    Duration::from_secs(10),
                )
                .await
        });
        let other = commands.recv().await.unwrap();
        assert!(matches!(other.operation, FleetOperation::Capture { .. }));
        other.reply.send(Err("test capture".into())).unwrap();
        assert!(operation.await.unwrap().is_err());
        let mut completed = request;
        completed.status = RequestStatus::Completed;
        refresh
            .reply
            .send(Ok(FleetCommandResult::collaboration_request(completed)))
            .unwrap();
        assert!(waiting
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .status
            .is_terminal());
    }

    #[tokio::test(start_paused = true)]
    async fn unrelated_topology_is_ignored_but_reconnect_reconciles() {
        let (runtime, mut commands, pane, request) = fixture(true).await;
        let waiting = wait(runtime.clone(), pane, &request, 300);
        commands
            .recv()
            .await
            .unwrap()
            .reply
            .send(Ok(FleetCommandResult::collaboration_request(
                request.clone(),
            )))
            .unwrap();
        tokio::task::yield_now().await;
        runtime.store.mutate_host("local", |_| {}).await;
        runtime.store.notify_mailbox("unknown", 1).await;
        assert!(
            tokio::time::timeout(Duration::from_secs(2), commands.recv())
                .await
                .is_err()
        );
        runtime
            .store
            .mutate_host("local", |h| h.daemon_generation = Some(2))
            .await;
        let refresh = commands.recv().await.unwrap();
        let mut completed = request;
        completed.status = RequestStatus::Cancelled;
        refresh
            .reply
            .send(Ok(FleetCommandResult::collaboration_request(completed)))
            .unwrap();
        assert_eq!(
            waiting.await.unwrap().unwrap().unwrap().status,
            RequestStatus::Cancelled
        );
    }

    #[tokio::test(start_paused = true)]
    async fn legacy_backoff_and_last_waiter_cancellation_are_bounded() {
        let (runtime, mut commands, pane, request) = fixture(false).await;
        let waiting = wait(runtime.clone(), pane, &request, 300);
        let started = Instant::now();
        for seconds in [0, 1, 3, 7, 15, 31, 61] {
            let command = commands.recv().await.unwrap();
            assert_eq!(
                Instant::now().duration_since(started),
                Duration::from_secs(seconds)
            );
            command
                .reply
                .send(Ok(FleetCommandResult::collaboration_request(
                    request.clone(),
                )))
                .unwrap();
        }
        waiting.abort();
        let _ = waiting.await;
        tokio::task::yield_now().await;
        assert!(runtime.reply_waits.0.lock().await.is_empty());
        assert!(
            tokio::time::timeout(Duration::from_secs(60), commands.recv())
                .await
                .is_err()
        );
    }
    #[tokio::test(start_paused = true)]
    async fn progress_wait_returns_without_completing_or_duplicating_terminal_wait() {
        let (runtime, mut commands, pane, request) = fixture(true).await;
        let terminal = wait(runtime.clone(), pane.clone(), &request, 300);
        let progress_runtime = runtime.clone();
        let id = request.id.clone();
        let progress = tokio::spawn(async move {
            progress_runtime
                .wait_for_reply("local".into(), pane, id, Duration::from_secs(300), Some(0))
                .await
        });
        commands
            .recv()
            .await
            .unwrap()
            .reply
            .send(Ok(FleetCommandResult::collaboration_request(
                request.clone(),
            )))
            .unwrap();
        tokio::task::yield_now().await;
        runtime.store.notify_mailbox("local", 1).await;
        let update = commands.recv().await.unwrap();
        let mut changed = request;
        changed
            .updates
            .push(crate::collaboration::CollaborationUpdate {
                sequence: 1,
                author: changed.to.clone(),
                body: "tests passed".into(),
                at: time::OffsetDateTime::now_utc(),
            });
        update
            .reply
            .send(Ok(FleetCommandResult::collaboration_request(changed)))
            .unwrap();
        assert_eq!(
            progress.await.unwrap().unwrap().unwrap().updates[0].sequence,
            1
        );
        assert!(!terminal.is_finished());
        terminal.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn upstream_resubscription_and_broadcast_lag_reconcile() {
        let (runtime, mut commands, pane, request) = fixture(true).await;
        let waiting = wait(runtime.clone(), pane, &request, 300);
        let initial = commands.recv().await.unwrap();
        // Overrun the subscribed receiver while its first read is in flight.
        for _ in 0..600 {
            runtime.store.mutate_host("local", |_| {}).await;
        }
        initial
            .reply
            .send(Ok(FleetCommandResult::collaboration_request(
                request.clone(),
            )))
            .unwrap();
        let lagged = commands.recv().await.unwrap();
        lagged
            .reply
            .send(Ok(FleetCommandResult::collaboration_request(
                request.clone(),
            )))
            .unwrap();
        tokio::task::yield_now().await;
        // A reconnect emits a zero revision even if no further writes occur.
        runtime.store.notify_mailbox("local", 0).await;
        let resync = commands.recv().await.unwrap();
        let mut completed = request;
        completed.status = RequestStatus::Completed;
        resync
            .reply
            .send(Ok(FleetCommandResult::collaboration_request(completed)))
            .unwrap();
        assert!(waiting
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .status
            .is_terminal());
    }

    #[tokio::test(start_paused = true)]
    async fn transient_disconnect_retries_and_observe_mode_revocation_stops_reads() {
        let (runtime, mut commands, pane, request) = fixture(true).await;
        runtime
            .store
            .mutate_host("local", |h| h.local = false)
            .await;
        let waiting = wait(runtime.clone(), pane, &request, 300);
        let initial = commands.recv().await.unwrap();
        runtime
            .store
            .mutate_host("local", |h| h.state = FleetHostState::Offline)
            .await;
        initial
            .reply
            .send(Err("relay disconnected".into()))
            .unwrap();
        let retry = commands.recv().await.unwrap();
        runtime
            .store
            .mutate_host("local", |h| h.state = FleetHostState::Online)
            .await;
        retry
            .reply
            .send(Ok(FleetCommandResult::collaboration_request(
                request.clone(),
            )))
            .unwrap();
        let restored = commands.recv().await.unwrap();
        restored
            .reply
            .send(Ok(FleetCommandResult::collaboration_request(request)))
            .unwrap();
        tokio::task::yield_now().await;
        runtime
            .store
            .mutate_host("local", |h| h.mode = HostAccessMode::Observe)
            .await;
        assert!(waiting.await.unwrap().unwrap_err().contains("observe-only"));
        assert!(
            tokio::time::timeout(Duration::from_secs(1), commands.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn ipc_wait_disconnect_releases_worker_and_keeps_other_connections_responsive() {
        use crate::ipc::{Client, Server};
        let (runtime, mut commands, pane, request) = fixture(true).await;
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("wait.sock");
        let (shutdown, receiver) = broadcast::channel(1);
        let server =
            Server::new(socket.clone(), crate::state::Store::shared()).with_fleet(runtime.clone());
        let serving = tokio::spawn(async move { server.run(receiver).await.unwrap() });
        let client = Client::new(socket.clone());
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let other = client.clone();
        let id = request.id.clone();
        let waiting = tokio::spawn(async move {
            client
                .fleet_wait_reply("local", &pane, &id, 300, None)
                .await
        });
        let command = tokio::time::timeout(Duration::from_secs(2), commands.recv())
            .await
            .unwrap()
            .unwrap();
        command
            .reply
            .send(Ok(FleetCommandResult::collaboration_request(request)))
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), other.fleet_snapshot(None))
                .await
                .unwrap()
                .is_ok()
        );
        waiting.abort();
        let _ = waiting.await;
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if runtime.reply_waits.0.lock().await.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        shutdown.send(()).unwrap();
        serving.await.unwrap();
    }
}
