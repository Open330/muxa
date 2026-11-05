use super::{
    board_quick_panes, board_screen, busy_target, collaboration_revision_updates,
    dispatch_quick_action, scan_board, still_sending, ActionOutcome, Agent, App, Client,
    CollaborationOrigin, CollaborationRequest, Duration, HashMap, HintLevel, MailboxScope, PaneKey,
    QuickAction, RealEffects, RequestMailbox, RequestStatus, Result,
};

/// Writes to every agent of a Work take one submit grace per pane; they run
/// in the background so the keyboard stays live meanwhile.
pub(super) fn is_many_pane(action: &QuickAction) -> bool {
    matches!(
        action,
        QuickAction::SendPromptToMany { .. } | QuickAction::InterruptPanes { .. }
    )
}

pub(super) fn start_quick_in_background(app: &mut App, action: QuickAction) {
    start_quick_with(app, action, |action| {
        let mut fx = RealEffects;
        dispatch_quick_action(action, &mut fx)
    });
}

/// Run `action` off the input loop, owning its panes until it finishes.
pub(super) fn start_quick_with(
    app: &mut App,
    action: QuickAction,
    run: impl FnOnce(QuickAction) -> ActionOutcome + Send + 'static,
) {
    let panes: Vec<PaneKey> = board_quick_panes(&action)
        .unwrap_or_default()
        .into_iter()
        .cloned()
        .collect();
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let _ = tx.send(run(action));
    });
    app.pending_outcomes.push((panes, rx));
    app.set_hint("sending to the Work's agents…", HintLevel::Ok);
}

/// Refuse an action that would write to a pane a background action still
/// owns: two writers interleave text and Enter, and an interrupt would race
/// prompts still being typed.
pub(super) fn refuse_busy(app: &mut App, action: &QuickAction) -> bool {
    let panes: Vec<PaneKey> = board_quick_panes(action)
        .unwrap_or_default()
        .into_iter()
        .cloned()
        .collect();
    match busy_target(app, &panes) {
        Some(pane) => {
            app.set_hint(still_sending(&pane), HintLevel::Warn);
            true
        }
        None => false,
    }
}

pub(super) fn take_finished_quick(app: &mut App) -> Vec<ActionOutcome> {
    let mut finished = Vec::new();
    app.pending_outcomes
        .retain_mut(|(_, rx)| match rx.try_recv() {
            Ok(outcome) => {
                finished.push(outcome);
                false
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => true,
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                finished.push(ActionOutcome::Err("✗ action task failed".into()));
                false
            }
        });
    finished
}

/// A value the board fetches in the background: at most one fetch in
/// flight, polled without blocking, repeated no more often than the caller's
/// interval. `None` until the first fetch returns; a failure is kept apart
/// from an empty answer.
#[derive(Debug)]
pub(crate) struct Background<T> {
    pub(super) value: Option<std::result::Result<T, String>>,
    pub(super) in_flight: Option<tokio::sync::oneshot::Receiver<std::result::Result<T, String>>>,
    started: Option<std::time::Instant>,
}

impl<T> Default for Background<T> {
    fn default() -> Self {
        Self {
            value: None,
            in_flight: None,
            started: None,
        }
    }
}

impl<T: Send + 'static> Background<T> {
    pub(super) fn due(&self, interval: Duration) -> bool {
        self.in_flight.is_none()
            && self
                .started
                .is_none_or(|started| started.elapsed() >= interval)
    }

    pub(super) fn start(
        &mut self,
        fetch: impl std::future::Future<Output = std::result::Result<T, String>> + Send + 'static,
    ) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = tx.send(fetch.await);
        });
        self.in_flight = Some(rx);
        self.started = Some(std::time::Instant::now());
    }

    /// Take a finished fetch; returns whether the value changed.
    pub(super) fn poll(&mut self) -> bool {
        let Some(rx) = self.in_flight.as_mut() else {
            return false;
        };
        match rx.try_recv() {
            Ok(value) => {
                self.value = Some(value);
                self.in_flight = None;
                true
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => false,
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                self.value = Some(Err("background task failed".into()));
                self.in_flight = None;
                true
            }
        }
    }
}

/// ACT/WACT per exact pane over the board's window. Last-touch attribution
/// gives each second to one pane, so these sum to the all-sessions total and
/// to each Work's figure.
pub(crate) type BoardStats =
    Background<std::collections::BTreeMap<String, crate::stats::ActiveDuration>>;
pub(super) const BOARD_STATS_INTERVAL: Duration = Duration::from_secs(30);

impl BoardStats {
    /// `all sessions · today · ACT 3h · WACT 2h`, or why there is none.
    pub(super) fn label(&self, window: &str) -> String {
        format!("all sessions · {window} · {}", self.figures())
    }

    /// Just `ACT 3h · WACT 2h`, for where the full label does not fit.
    pub(super) fn figures(&self) -> String {
        match &self.value {
            None => "ACT …".to_string(),
            Some(Err(_)) => "ACT unavailable".to_string(),
            Some(Ok(by_pane)) => {
                let (active, work) = by_pane.values().fold((0u64, 0u64), |(a, w), row| {
                    (
                        a.saturating_add(row.active_secs),
                        w.saturating_add(row.work_active_secs),
                    )
                });
                format!(
                    "ACT {} · WACT {}",
                    board_screen::active_label(active),
                    board_screen::active_label(work)
                )
            }
        }
    }
}

/// Open-request metadata behind the per-Work summary; never bodies or claims.
pub(crate) type BoardCollab = Background<muxa::collaboration::CollaborationOpenCounts>;
pub(super) const BOARD_COLLAB_INTERVAL: Duration = Duration::from_secs(15);

#[derive(Debug, Default)]
pub(super) struct BoardMailboxCache {
    pub(super) changes: Option<tokio::sync::watch::Receiver<Option<u64>>>,
    revision: Option<u64>,
}

pub(super) fn refresh_board_collaboration(client: &Client, app: &mut App) {
    let Some(origin) = app.collaboration.origin.clone() else {
        return;
    };
    let cache = &mut app.board_collab_cache;
    if cache
        .changes
        .as_ref()
        .is_none_or(|changes| changes.has_changed().is_err())
    {
        cache.changes = Some(collaboration_revision_updates(client.clone()));
        cache.revision = None;
    }
    // Consume before starting the read: a racing change stays pending.
    let revision = cache
        .changes
        .as_mut()
        .and_then(|changes| *changes.borrow_and_update());
    if revision.is_some()
        && revision == cache.revision
        && app.board_collab.value.as_ref().is_some_and(Result::is_ok)
    {
        return;
    }
    cache.revision = revision;
    let client = client.clone();
    let origin = CollaborationOrigin {
        console: true,
        ..origin
    };
    let counts_supported = app.open_counts_supported;
    app.board_collab.start(async move {
        if counts_supported {
            client
                .collaboration_open_counts(&origin, MailboxScope::All)
                .await
                .map_err(|error| error.to_string())
        } else {
            read_board_open_requests(&client, &origin)
                .await
                .map(|rows| muxa::collaboration::CollaborationOpenCounts::from_requests(&rows))
        }
    });
}

pub(super) async fn read_board_open_requests(
    client: &Client,
    origin: &CollaborationOrigin,
) -> Result<Vec<CollaborationRequest>, String> {
    use muxa::collaboration::CollaborationReadView as View;
    let mut requests = HashMap::new();
    for status in [RequestStatus::Queued, RequestStatus::Claimed] {
        let mut query = muxa::collaboration::CollaborationQuery {
            status: Some(status),
            limit: Some(500),
            ..Default::default()
        };
        loop {
            let result = client
                .collaboration_query_result(
                    origin,
                    RequestMailbox::All,
                    MailboxScope::All,
                    &query,
                    0,
                    View::Metadata,
                    false,
                )
                .await
                .map_err(|error| error.to_string())?;
            let legacy = result.legacy_requests.is_some();
            let page = result.page;
            for mut row in result.legacy_requests.unwrap_or(page.requests) {
                if !row.status.is_terminal() {
                    View::Metadata.apply(&mut row);
                    requests.insert(row.id.clone(), row);
                }
            }
            if legacy {
                return Ok(requests.into_values().collect());
            }
            if !page.has_more {
                break;
            }
            query.cursor = Some(
                page.next_cursor
                    .ok_or("open-request page omitted its cursor")?,
            );
        }
    }
    Ok(requests.into_values().collect())
}

impl BoardCollab {
    /// The detail line for one Work: counts, still loading, or why not.
    pub(super) fn line(&self, work: &muxa::work::WorkSnapshotItem) -> String {
        match &self.value {
            None => "collab  …".into(),
            Some(Err(_)) => "collab  unavailable".into(),
            Some(Ok(requests)) => {
                let summary = board_screen::collab_summary_counts(work, requests);
                format!(
                    "collab  {} open to its agents · {} open from them",
                    summary.open_to, summary.open_from
                )
            }
        }
    }
}

/// The daemon's worker inventory, for the per-Work worker line. Read-only:
/// retiring workers stays with `muxa workers prune`, which dry-runs first.
pub(crate) type BoardWorkers = Background<muxa::collaboration::WorkerInventory>;
pub(super) const BOARD_WORKERS_INTERVAL: Duration = Duration::from_secs(30);
/// Idle time past which the board flags a worker, as `muxa workers` does.
pub(super) const BOARD_WORKER_IDLE_SECS: u64 = 24 * 3600;

impl BoardWorkers {
    /// The detail line for one Work: its workers and what is wrong with
    /// them, by exact agent session.
    pub(super) fn line(&self, work: &muxa::work::WorkSnapshotItem) -> String {
        let inventory = match &self.value {
            None => return "workers  …".into(),
            Some(Err(_)) => return "workers  unavailable".into(),
            Some(Ok(inventory)) => inventory,
        };
        let summary = board_screen::worker_summary(work, &inventory.workers);
        if summary.workers == 0 {
            return "workers  none".into();
        }
        let mut line = format!("workers  {}", summary.workers);
        let flags: Vec<String> = [
            (summary.long_idle, "long idle"),
            (summary.error, "in error"),
            (summary.overlap, "possibly overlapping"),
        ]
        .into_iter()
        .filter(|(count, _)| *count > 0)
        .map(|(count, label)| format!("{count} {label}"))
        .collect();
        if !flags.is_empty() {
            line.push_str(" · ");
            line.push_str(&flags.join(" · "));
        }
        if summary.long_idle > 0 || summary.error > 0 {
            // A pointer, not a promise: prune plans host-wide, dry-runs first,
            // skips workers with open work, and takes errored ones only with
            // --error.
            line.push_str(" — review candidates with `muxa workers prune` (--error for errored)");
        }
        line
    }
}

/// The ACT/WACT window the board summarises.
pub(super) fn board_window(app: &App) -> String {
    app.watch_cfg
        .board_since
        .clone()
        .unwrap_or_else(|| "today".into())
}

/// One Work-board scan as it arrives: the projection and any scan problems.
pub(super) type BoardScan = (muxa::work::WorkSnapshot, Vec<String>);

/// At most one Work-board scan in flight, polled without blocking.
#[derive(Debug, Default)]
pub(crate) struct BoardRefresher {
    in_flight: Option<tokio::sync::oneshot::Receiver<BoardScan>>,
}

impl BoardRefresher {
    /// Start a scan with the agents watch already holds, unless one is
    /// running. Returns whether a scan was started.
    pub(super) fn start_scan(&mut self, agents: Vec<Agent>) -> bool {
        self.start(scan_board(agents))
    }

    pub(super) fn busy(&self) -> bool {
        self.in_flight.is_some()
    }

    pub(super) fn start(
        &mut self,
        scan: impl std::future::Future<Output = BoardScan> + Send + 'static,
    ) -> bool {
        if self.in_flight.is_some() {
            return false;
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _ = tx.send(scan.await);
        });
        self.in_flight = Some(rx);
        true
    }

    /// The finished scan, if there is one; never waits.
    pub(super) fn take_finished(&mut self) -> Option<BoardScan> {
        let rx = self.in_flight.as_mut()?;
        match rx.try_recv() {
            Ok(scan) => {
                self.in_flight = None;
                Some(scan)
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => None,
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                // The task died; let the next tick start another.
                self.in_flight = None;
                None
            }
        }
    }
}
