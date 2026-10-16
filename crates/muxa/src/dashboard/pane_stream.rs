//! Live pane output for `GET /api/panes/{pane}/output/stream`.
//!
//! Every subscriber to the same `(socket, pane)` shares one capture loop.
//! The loop exists only while at least one subscriber is connected: it
//! captures every [`PaneStreamLimits::interval`], sanitizes the text and
//! publishes it on a `watch` channel only when it changed, so an idle pane
//! costs one tmux call per tick and no network traffic. When the last
//! subscriber goes away the loop notices on its next tick and ends.
//!
//! Captures never pile up: the loop awaits each capture before the next
//! tick, and the interval skips ticks that elapsed meanwhile, so at most one
//! capture per pane is ever in flight. The number of loops and the number of
//! subscribers per pane are bounded ([`SubscribeError`] maps to 429).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use time::OffsetDateTime;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

use crate::backend::SharedBackend;

/// Tunables for live pane output. The defaults are what the daemon uses;
/// tests shorten the interval.
#[derive(Debug, Clone, Copy)]
pub struct PaneStreamLimits {
    /// How often a pane with subscribers is captured.
    pub interval: Duration,
    /// Capture loops (distinct panes) running at once, across all clients.
    pub max_tasks: usize,
    /// Concurrent subscribers to one pane.
    pub max_subscribers_per_pane: usize,
    /// A stream closes after this long; the client reconnects. Bounds how
    /// long a stale credential keeps a stream open.
    pub lifetime: Duration,
    /// Consecutive failed captures before the pane is reported gone. One
    /// failure can be a busy tmux server; two in a row is a closed pane.
    pub gone_after_failures: u32,
}

impl Default for PaneStreamLimits {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(300),
            max_tasks: 32,
            max_subscribers_per_pane: 8,
            lifetime: Duration::from_secs(30 * 60),
            gone_after_failures: 2,
        }
    }
}

/// What a pane's channel currently holds.
#[derive(Debug, Clone)]
pub(crate) enum PaneFrame {
    /// No capture has finished yet.
    Pending,
    /// The latest sanitized capture (trailing blank rows removed, not yet
    /// cut to any subscriber's line count).
    Output {
        text: Arc<str>,
        captured_at: Arc<str>,
    },
    /// The pane no longer exists (or its backend stopped answering).
    Gone,
}

/// Why a subscription was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubscribeError {
    /// [`PaneStreamLimits::max_tasks`] panes are already streaming.
    TooManyPanes,
    /// [`PaneStreamLimits::max_subscribers_per_pane`] reached for this pane.
    TooManySubscribers,
}

impl SubscribeError {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::TooManyPanes => "too many panes are streaming output; try again later",
            Self::TooManySubscribers => "too many live viewers for this pane; try again later",
        }
    }
}

type PaneKey = (Option<String>, String);

struct Channel {
    tx: watch::Sender<PaneFrame>,
    /// Deepest history any subscriber asked for; the loop captures this
    /// many lines and each subscriber cuts its own tail.
    lines: AtomicUsize,
}

/// Registry of running capture loops, shared through `AppState`.
pub struct PaneStreamHub {
    limits: PaneStreamLimits,
    channels: Mutex<HashMap<PaneKey, Arc<Channel>>>,
}

impl PaneStreamHub {
    #[must_use]
    pub fn new(limits: PaneStreamLimits) -> Self {
        Self {
            limits,
            channels: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn limits(&self) -> PaneStreamLimits {
        self.limits
    }

    /// Capture loops currently running.
    #[cfg(test)]
    #[must_use]
    pub fn active_tasks(&self) -> usize {
        self.lock().len()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PaneKey, Arc<Channel>>> {
        // A panic while holding this lock leaves only a map of channels
        // behind, which stays consistent; keep serving.
        self.channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Join the pane's capture loop, starting it if this is the first
    /// subscriber. Must be called inside a Tokio runtime.
    pub(crate) fn subscribe(
        self: &Arc<Self>,
        backend: SharedBackend,
        socket: Option<String>,
        pane: String,
        lines: usize,
    ) -> Result<watch::Receiver<PaneFrame>, SubscribeError> {
        let key = (socket, pane);
        let mut channels = self.lock();
        if let Some(channel) = channels.get(&key) {
            if channel.tx.receiver_count() >= self.limits.max_subscribers_per_pane {
                return Err(SubscribeError::TooManySubscribers);
            }
            channel.lines.fetch_max(lines, Ordering::Relaxed);
            return Ok(channel.tx.subscribe());
        }
        if channels.len() >= self.limits.max_tasks {
            return Err(SubscribeError::TooManyPanes);
        }
        let (tx, rx) = watch::channel(PaneFrame::Pending);
        let channel = Arc::new(Channel {
            tx,
            lines: AtomicUsize::new(lines),
        });
        channels.insert(key.clone(), channel.clone());
        drop(channels);
        tokio::spawn(capture_loop(self.clone(), key, channel, backend));
        Ok(rx)
    }

    /// Remove `channel` if nobody listens any more. Checked under the map
    /// lock so a subscriber arriving at the same moment either joins before
    /// the check (and keeps the loop alive) or starts a fresh loop after.
    fn release_if_idle(&self, key: &PaneKey, channel: &Arc<Channel>) -> bool {
        let mut channels = self.lock();
        if channel.tx.receiver_count() > 0 {
            return false;
        }
        Self::remove_locked(&mut channels, key, channel);
        true
    }

    fn remove(&self, key: &PaneKey, channel: &Arc<Channel>) {
        Self::remove_locked(&mut self.lock(), key, channel);
    }

    fn remove_locked(
        channels: &mut HashMap<PaneKey, Arc<Channel>>,
        key: &PaneKey,
        channel: &Arc<Channel>,
    ) {
        if channels
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, channel))
        {
            channels.remove(key);
        }
    }
}

async fn capture_loop(
    hub: Arc<PaneStreamHub>,
    key: PaneKey,
    channel: Arc<Channel>,
    backend: SharedBackend,
) {
    let mut ticker = tokio::time::interval(hub.limits.interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last: Option<Arc<str>> = None;
    let mut failures = 0u32;
    loop {
        ticker.tick().await;
        if hub.release_if_idle(&key, &channel) {
            return;
        }
        let lines = channel.lines.load(Ordering::Relaxed);
        let (socket, pane) = key.clone();
        let backend = backend.clone();
        let captured = tokio::task::spawn_blocking(move || {
            backend.capture_pane_history_on(socket.as_deref(), &pane, lines)
        })
        .await
        .ok()
        .flatten();
        if let Some(raw) = captured {
            failures = 0;
            let clean = clean_capture(&raw);
            if last.as_deref() != Some(clean.as_str()) {
                let text: Arc<str> = clean.into();
                last = Some(text.clone());
                channel.tx.send_replace(PaneFrame::Output {
                    text,
                    captured_at: now_rfc3339().into(),
                });
            }
        } else {
            failures += 1;
            if failures >= hub.limits.gone_after_failures {
                channel.tx.send_replace(PaneFrame::Gone);
                hub.remove(&key, &channel);
                return;
            }
        }
    }
}

/// Escapes and control bytes stripped (the sanitizer every pane-output
/// surface uses) and the screen's trailing blank rows dropped.
pub(crate) fn clean_capture(raw: &str) -> String {
    let mut clean = crate::fleet::sanitize_terminal_text(raw);
    clean.truncate(clean.trim_end().len());
    clean
}

pub(crate) fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
