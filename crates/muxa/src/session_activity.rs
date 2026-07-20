//! Cumulative tmux session foreground-time tracking.
//!
//! The signal this module tracks is intentionally tmux-native:
//! interactive `tmux list-clients` rows grouped by their `client_session`.
//! That maps to "a human has this session in a foreground tmux client",
//! ignores control-mode automation clients, and survives panes/windows
//! coming and going inside the same session.

use crate::activity::{
    ActivityEntry, ActivityLog, HumanInteractionEntry, HumanInteractionInput, HumanInteractionKind,
    SessionForegroundEntry,
};
use crate::backend::herdr;
use crate::tmux::{self, SessionInfo, TmuxError};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use tokio::sync::broadcast;
use tracing::{debug, warn};

#[cfg(unix)]
const SESSION_ACTIVITY_FILE_MODE: u32 = 0o600;

pub const SESSION_ACTIVITY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionActivity {
    pub session_id: String,
    pub name: String,
    pub attached_clients: u32,
    pub total_attached_secs: u64,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub attached_since: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub last_seen_at: OffsetDateTime,
}

impl SessionActivity {
    pub fn is_attached(&self) -> bool {
        self.attached_since.is_some()
    }

    pub fn effective_total_secs(&self, now: OffsetDateTime) -> u64 {
        let active_delta = self.attached_since.map_or(0, |since| {
            u64::try_from((now - since).whole_seconds().max(0)).unwrap_or(u64::MAX)
        });
        self.total_attached_secs.saturating_add(active_delta)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionActivityFile {
    #[serde(default)]
    v: u32,
    #[serde(with = "time::serde::rfc3339")]
    saved_at: OffsetDateTime,
    sessions: Vec<SessionActivity>,
}

pub async fn load(path: &Path) -> Vec<SessionActivity> {
    let bytes = match tokio::fs::read(path).await {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            debug!(
                path = %path.display(),
                "no session activity file on disk; starting empty"
            );
            return Vec::new();
        }
        Err(e) => {
            warn!(error = %e, path = %path.display(), "could not read session activity file");
            return Vec::new();
        }
    };

    let parsed: SessionActivityFile = match serde_json::from_slice(&bytes) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                error = %e,
                path = %path.display(),
                "session activity file is corrupt; starting empty"
            );
            return Vec::new();
        }
    };

    if parsed.v != SESSION_ACTIVITY_SCHEMA_VERSION {
        warn!(
            file_version = parsed.v,
            expected = SESSION_ACTIVITY_SCHEMA_VERSION,
            path = %path.display(),
            "session activity file has unknown schema version; starting empty",
        );
        return Vec::new();
    }

    parsed.sessions
}

async fn save(path: &Path, sessions: &[SessionActivity]) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no parent")
    })?;
    if !parent.as_os_str().is_empty() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let basename = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("session-activity.json");
    let tmp = parent.join(format!(".{}.{}.tmp", basename, std::process::id()));
    let payload = SessionActivityFile {
        v: SESSION_ACTIVITY_SCHEMA_VERSION,
        saved_at: OffsetDateTime::now_utc(),
        sessions: sessions.to_vec(),
    };
    let bytes = serde_json::to_vec(&payload)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    {
        let mut opts = OpenOptions::new();
        opts.create(true).truncate(true).write(true);
        #[cfg(unix)]
        opts.mode(SESSION_ACTIVITY_FILE_MODE);
        let mut f = opts.open(&tmp).await?;
        f.write_all(&bytes).await?;
        f.flush().await?;
        f.sync_all().await?;
    }
    tokio::fs::rename(&tmp, path).await?;
    if !parent.as_os_str().is_empty() {
        if let Ok(dir) = OpenOptions::new().read(true).open(parent).await {
            let _ = dir.sync_all().await;
        }
    }
    Ok(())
}

pub fn apply_sample<S: BuildHasher>(
    records: &mut HashMap<String, SessionActivity, S>,
    sessions: &[SessionInfo],
    now: OffsetDateTime,
) -> bool {
    apply_sample_report(records, sessions, now).changed
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplySampleReport {
    pub changed: bool,
    pub intervals: Vec<SessionForegroundEntry>,
}

pub fn apply_sample_report<S: BuildHasher>(
    records: &mut HashMap<String, SessionActivity, S>,
    sessions: &[SessionInfo],
    now: OffsetDateTime,
) -> ApplySampleReport {
    let mut changed = false;
    let mut intervals = Vec::new();
    let live_ids: HashSet<&str> = sessions.iter().map(|s| s.session_id.as_str()).collect();

    for session in sessions {
        let is_attached = session.attached_clients > 0;
        let record = records
            .entry(session.session_id.clone())
            .or_insert_with(|| SessionActivity {
                session_id: session.session_id.clone(),
                name: session.name.clone(),
                attached_clients: 0,
                total_attached_secs: 0,
                attached_since: None,
                last_seen_at: now,
            });

        if record.name != session.name {
            record.name.clone_from(&session.name);
            changed = true;
        }
        if record.attached_clients != session.attached_clients {
            record.attached_clients = session.attached_clients;
            changed = true;
        }
        record.last_seen_at = now;

        match (record.attached_since, is_attached) {
            (None, true) => {
                record.attached_since = Some(now);
                changed = true;
            }
            (Some(since), false) => {
                intervals.push(SessionForegroundEntry::new(
                    record.session_id.clone(),
                    record.name.clone(),
                    since,
                    now,
                ));
                add_elapsed(record, since, now);
                record.attached_since = None;
                changed = true;
            }
            (Some(_), true) | (None, false) => {}
        }
    }

    for record in records.values_mut() {
        if live_ids.contains(record.session_id.as_str()) {
            continue;
        }
        if let Some(since) = record.attached_since {
            intervals.push(SessionForegroundEntry::new(
                record.session_id.clone(),
                record.name.clone(),
                since,
                now,
            ));
            add_elapsed(record, since, now);
            record.attached_since = None;
            record.attached_clients = 0;
            record.last_seen_at = now;
            changed = true;
        }
    }

    ApplySampleReport { changed, intervals }
}

fn add_elapsed(record: &mut SessionActivity, since: OffsetDateTime, now: OffsetDateTime) {
    let secs = u64::try_from((now - since).whole_seconds().max(0)).unwrap_or(u64::MAX);
    record.total_attached_secs = record.total_attached_secs.saturating_add(secs);
}

/// Where a poll samples foreground state from. Only the *sampling* source
/// differs between hosts — everything downstream (`apply_sample_report`,
/// the ledger intervals, `session-activity.json`, input ticks) is shared.
///
/// - `Tmux`: interactive `tmux list-clients` rows grouped by session (also
///   the fallback for zellij, which has no client-attach signal and so
///   degrades to an empty sample, unchanged).
/// - `Herdr`: the focused herdr *workspace* over the herdr socket. herdr
///   has no client-attach or per-client input signal, so it produces the
///   "workspace X is foregrounded" observation only — see
///   [`sample_herdr_activity`] for the accrual limitation this implies.
#[derive(Default)]
pub enum SessionActivitySource {
    #[default]
    Tmux,
    Herdr {
        socket_path: PathBuf,
    },
}

pub struct SessionActivityTracker {
    path: PathBuf,
    interval: Duration,
    activity_log: Option<Arc<ActivityLog>>,
    /// One sampling source per host the daemon observes that *has* a foreground
    /// signal (tmux and/or herdr; zellij has none). A single tracker owns one
    /// `records` map and one `session-activity.json` writer, so polling every
    /// source from this one task is race-free by construction — two independent
    /// trackers writing the same file would clobber each other (each `save()`
    /// rewrites the whole file). Merging is safe because the ledger keys are
    /// disjoint across hosts (tmux `$N` vs herdr workspace ids), so one map
    /// holds both hosts' sessions without collision.
    sources: Vec<SessionActivitySource>,
}

impl SessionActivityTracker {
    pub fn new(path: PathBuf, interval: Duration) -> Self {
        Self {
            path,
            interval,
            activity_log: None,
            sources: vec![SessionActivitySource::default()],
        }
    }

    /// Select a single foreground-sampling source, replacing any already set.
    /// Defaults to [`SessionActivitySource::Tmux`]; the daemon uses
    /// [`Self::with_sources`] to sample several hosts at once.
    #[must_use]
    pub fn with_source(mut self, source: SessionActivitySource) -> Self {
        self.sources = vec![source];
        self
    }

    /// Sample foreground state from several hosts each poll, merged into one
    /// ledger. Used by the multi-host daemon (tmux + herdr during a migration).
    /// An empty slice is treated as "no source" and leaves the default tmux
    /// sampler in place so the tracker never silently stops sampling.
    #[must_use]
    pub fn with_sources(mut self, sources: Vec<SessionActivitySource>) -> Self {
        if !sources.is_empty() {
            self.sources = sources;
        }
        self
    }

    #[must_use]
    pub fn with_activity_log(mut self, activity_log: Option<Arc<ActivityLog>>) -> Self {
        self.activity_log = activity_log;
        self
    }

    /// Take one foreground sample from every configured source, off the async
    /// runtime (both the tmux shell-out and the herdr socket round-trip block),
    /// and merge them into a single [`ActivitySample`]. Sources sample
    /// concurrently. All sources produce the same shape, and their session ids
    /// don't collide across hosts, so the caller's accounting stays
    /// source-agnostic over the merged result.
    ///
    /// If ANY source errors (only the tmux sampler can — the herdr sampler is
    /// infallible, yielding an empty sample when no workspace is focused) the
    /// whole poll is skipped rather than sampling a partial live set: passing a
    /// host's sessions to `apply_sample_report` without the other host's would
    /// wrongly close the missing host's foreground intervals. This matches the
    /// single-host contract, where a failed sample skips the poll entirely.
    async fn sample(&self) -> Result<ActivitySample, String> {
        let handles: Vec<tokio::task::JoinHandle<Result<ActivitySample, String>>> = self
            .sources
            .iter()
            .map(|source| match source {
                SessionActivitySource::Tmux => tokio::task::spawn_blocking(sample_activity),
                SessionActivitySource::Herdr { socket_path } => {
                    let socket_path = socket_path.clone();
                    tokio::task::spawn_blocking(move || Ok(sample_herdr_activity(&socket_path)))
                }
            })
            .collect();

        let mut merged = ActivitySample {
            sessions: Vec::new(),
            client_inputs: Vec::new(),
        };
        for handle in handles {
            let sample = handle.await.map_err(|e| format!("join error: {e}"))??;
            merged.sessions.extend(sample.sessions);
            merged.client_inputs.extend(sample.client_inputs);
        }
        Ok(merged)
    }

    pub async fn run(self, mut shutdown: broadcast::Receiver<()>) {
        let loaded = load(&self.path).await;
        let mut records: HashMap<String, SessionActivity> = loaded
            .into_iter()
            .map(|r| (r.session_id.clone(), r))
            .collect();

        // Per-client (`#{client_name}`) last-seen `(created, activity)`, in
        // memory only. The same attach (matching `created`) whose activity
        // advances is real human input; an unseen name, or the same tty with a
        // new `created` (a reattach), is a fresh attach and only seeds. Cleared
        // on restart (first poll seeds every client, no phantom). Pruned each
        // poll to the live client set.
        let mut seen_clients: HashMap<String, (i64, i64)> = HashMap::new();
        let mut tick = tokio::time::interval(self.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    self.poll_once(&mut records, &mut seen_clients).await;
                }
                _ = shutdown.recv() => {
                    let mut sessions = records.values().cloned().collect::<Vec<_>>();
                    sessions.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.session_id.cmp(&b.session_id)));
                    if let Err(e) = save(&self.path, &sessions).await {
                        warn!(error = %e, path = %self.path.display(), "could not save session activity on shutdown");
                    }
                    debug!("session activity tracker shutting down");
                    break;
                }
            }
        }
    }

    async fn poll_once(
        &self,
        records: &mut HashMap<String, SessionActivity>,
        seen_clients: &mut HashMap<String, (i64, i64)>,
    ) {
        let sample = self.sample().await;
        let sample = match sample {
            Ok(sample) => sample,
            Err(e) => {
                debug!(error = %e, "session activity poll skipped");
                return;
            }
        };

        let now = OffsetDateTime::now_utc();
        let report = apply_sample_report(records, &sample.sessions, now);
        for interval in report.intervals {
            if let Some(activity_log) = &self.activity_log {
                activity_log.append(ActivityEntry::SessionForeground(interval));
            }
        }

        // Reading detection: a TmuxInput tick per session whose *already-seen*
        // client advanced its `client_activity` (a keypress or scroll). A fresh
        // client (reattach / extra client) is unseen and only seeds, so an idle
        // attach never fabricates input.
        for entry in detect_input_ticks(seen_clients, &sample.client_inputs) {
            if let Some(activity_log) = &self.activity_log {
                activity_log.append(ActivityEntry::HumanInteraction(entry));
            }
        }

        // `seen_clients` is in-memory only, so input detection never forces a
        // disk write; persist only when the session foreground state changed.
        if report.changed {
            let mut sessions = records.values().cloned().collect::<Vec<_>>();
            sessions.sort_by(|a, b| {
                a.name
                    .cmp(&b.name)
                    .then_with(|| a.session_id.cmp(&b.session_id))
            });
            if let Err(e) = save(&self.path, &sessions).await {
                warn!(
                    error = %e,
                    path = %self.path.display(),
                    "could not save session activity"
                );
            }
        }
    }
}

/// A single interactive tmux client's reading for this poll.
struct ClientInput {
    session_id: String,
    session_name: String,
    /// `#{client_name}` (tty). Paired with `created` to identify one attach.
    name: String,
    /// `#{client_activity}` unix epoch (seconds) — last keypress/scroll.
    epoch: i64,
    /// `#{client_created}` unix epoch (seconds) — when this client attached.
    created: i64,
    /// `#{pane_in_mode}` at poll time: the client's active pane is in copy/view
    /// mode, so an activity advance is scrollback navigation (reading), not
    /// typing. Tags the emitted tick as scroll vs. keypress.
    in_copy_mode: bool,
}

/// One tmux poll: live sessions (with attached-client counts) plus the per-client
/// last-activity readings used for reading detection.
struct ActivitySample {
    sessions: Vec<SessionInfo>,
    client_inputs: Vec<ClientInput>,
}

/// Width of the synthetic interval emitted for a detected input tick. The tick
/// itself is a point in time; `active` pads it with its own window, so a hair of
/// width here just keeps the interval non-empty.
const INPUT_TICK_SECS: i64 = 1;

fn sample_activity() -> Result<ActivitySample, String> {
    let empty = || ActivitySample {
        sessions: Vec::new(),
        client_inputs: Vec::new(),
    };
    let mut sessions = match tmux::list_sessions() {
        Ok(sessions) => sessions,
        Err(TmuxError::NonZero(msg)) if msg.trim_start().starts_with("no server running") => {
            return Ok(empty());
        }
        Err(e) => return Err(e.to_string()),
    };
    let clients = match tmux::list_clients() {
        Ok(clients) => clients,
        Err(TmuxError::NonZero(msg)) if msg.trim_start().starts_with("no server running") => {
            return Ok(empty());
        }
        Err(e) => return Err(e.to_string()),
    };
    apply_client_counts(&mut sessions, &clients);
    let client_inputs = client_inputs(&sessions, &clients);
    Ok(ActivitySample {
        sessions,
        client_inputs,
    })
}

/// herdr foreground sample: the currently focused herdr *workspace*, mapped
/// to a single [`SessionInfo`] with one "attached client" so
/// [`apply_sample_report`] credits foreground time to it exactly as it would
/// an attached tmux session. The `session_id` is the raw `workspace_id`
/// (`w1`), matching `HerdrBackend::list_panes`'s `PaneInfo.session` so the
/// ledger keys line up with the pane rows. No focused workspace (or an
/// unreachable/absent server) yields an empty sample — the tmux "no server
/// running" analog, which closes any open foreground interval.
///
/// LIMITATION (documented, mitigation out of scope): herdr's socket API
/// exposes no client-attach state — there is no `client.list` analog. So,
/// unlike the tmux path (which credits time only while an interactive client
/// is attached), herdr focus time accrues even when the server sits detached
/// with no client attached. This inflates ACT for always-on detached herdr
/// servers. herdr also has no per-client input/scroll signal, so
/// `client_inputs` is always empty and no `HumanInteraction` (`TmuxInput` /
/// `TmuxScroll`) ticks are ever emitted on herdr hosts.
fn sample_herdr_activity(socket_path: &Path) -> ActivitySample {
    let sessions = match herdr::herdr_focused_workspace(socket_path) {
        Some(ws) => vec![SessionInfo {
            session_id: ws.id,
            name: ws.label,
            attached_clients: 1,
        }],
        None => Vec::new(),
    };
    ActivitySample {
        sessions,
        client_inputs: Vec::new(),
    }
}

/// Build the per-client input readings for this poll: one entry per interactive
/// client whose session is live, carrying the client name + attach time, its
/// session id/name, and the `client_activity` epoch. Control-mode (automation)
/// clients and unreported activity (`<= 0`) are dropped. Pure (no tmux) so it is
/// tested directly.
fn client_inputs(sessions: &[SessionInfo], clients: &[tmux::ClientInfo]) -> Vec<ClientInput> {
    let id_by_name: HashMap<&str, &str> = sessions
        .iter()
        .map(|s| (s.name.as_str(), s.session_id.as_str()))
        .collect();
    let mut out = Vec::new();
    for client in clients {
        // Need a usable attach identity (name + created) to tell a keypress from
        // a reattach; without it (control-mode, or tmux didn't report the
        // fields) skip input detection rather than risk a false tick. Reading
        // detection simply goes dark on such clients; `active` still has prompts
        // and thinking.
        if client.control_mode
            || client.last_activity <= 0
            || client.created <= 0
            || client.name.is_empty()
        {
            continue;
        }
        if let Some(&session_id) = id_by_name.get(client.session.as_str()) {
            out.push(ClientInput {
                session_id: session_id.to_string(),
                session_name: client.session.clone(),
                name: client.name.clone(),
                epoch: client.last_activity,
                created: client.created,
                in_copy_mode: client.in_copy_mode,
            });
        }
    }
    out
}

/// Detect real human input by tracking each tmux client across polls, keyed by
/// `#{client_name}` (the tty) and identified by `#{client_created}` (its attach
/// time). Input is `client_activity` past a *baseline*: for a client/attach we
/// already saw (same `created`), the baseline is its last activity; for a first
/// sight or a new attach (unseen name, or same tty with a newer `created`), the
/// baseline is `created` itself. So `activity > created` on first sight counts
/// real post-attach reading (attach-and-read before the first poll), while a pure
/// attach (`activity == created`) only seeds — no reattach or extra idle client
/// can fabricate input. Emits at most one input tick per session (at the latest
/// advancing client's epoch), tagged `TmuxScroll` when that client's active pane
/// was in copy mode (scrollback reading) or `TmuxInput` otherwise (keypress).
/// `seen` is updated to the current `(created, activity)` and pruned to the live
/// client set.
fn detect_input_ticks(
    seen: &mut HashMap<String, (i64, i64)>,
    inputs: &[ClientInput],
) -> Vec<HumanInteractionEntry> {
    // Per session id: (session name, latest epoch, that client's copy-mode flag)
    // among clients that advanced.
    let mut advanced: HashMap<&str, (&str, i64, bool)> = HashMap::new();
    for input in inputs {
        // Baseline = the last activity of this exact attach if we've seen it,
        // else the attach time. Activity strictly past the baseline is real input.
        let baseline = match seen.get(&input.name) {
            Some(&(prev_created, prev_epoch)) if prev_created == input.created => prev_epoch,
            _ => input.created,
        };
        let is_real = input.epoch > baseline;
        seen.insert(input.name.clone(), (input.created, input.epoch));
        if is_real {
            let slot = advanced.entry(&input.session_id).or_insert((
                &input.session_name,
                input.epoch,
                input.in_copy_mode,
            ));
            if input.epoch > slot.1 {
                *slot = (&input.session_name, input.epoch, input.in_copy_mode);
            }
        }
    }

    let mut entries = Vec::new();
    for (session_id, (session_name, epoch, in_copy_mode)) in advanced {
        let Ok(at) = OffsetDateTime::from_unix_timestamp(epoch) else {
            continue;
        };
        let kind = if in_copy_mode {
            HumanInteractionKind::TmuxScroll
        } else {
            HumanInteractionKind::TmuxInput
        };
        entries.push(HumanInteractionEntry::new(HumanInteractionInput {
            kind,
            pane: None,
            session_id: Some(session_id.to_string()),
            session_name: Some(session_name.to_string()),
            started_at: at - time::Duration::seconds(INPUT_TICK_SECS),
            ended_at: at,
        }));
    }

    // Drop clients that are no longer present so `seen` can't grow unbounded.
    let live: HashSet<&str> = inputs.iter().map(|i| i.name.as_str()).collect();
    seen.retain(|name, _| live.contains(name.as_str()));

    entries
}

fn apply_client_counts(sessions: &mut [SessionInfo], clients: &[tmux::ClientInfo]) {
    let mut counts: HashMap<&str, u32> = HashMap::new();
    for client in clients {
        if client.control_mode {
            continue;
        }
        *counts.entry(client.session.as_str()).or_default() += 1;
    }
    for session in sessions {
        session.attached_clients = counts.get(session.name.as_str()).copied().unwrap_or(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn session(id: &str, name: &str, attached_clients: u32) -> SessionInfo {
        SessionInfo {
            session_id: id.into(),
            name: name.into(),
            attached_clients,
        }
    }

    #[test]
    fn apply_sample_accumulates_attached_edges() {
        let mut records = HashMap::new();
        let t0 = datetime!(2026-05-29 00:00:00 UTC);
        let t1 = datetime!(2026-05-29 00:00:10 UTC);
        let t2 = datetime!(2026-05-29 00:00:15 UTC);

        assert!(apply_sample(&mut records, &[session("$1", "main", 1)], t0));
        let record = records.get("$1").unwrap();
        assert_eq!(record.attached_since, Some(t0));
        assert_eq!(record.effective_total_secs(t1), 10);

        assert!(apply_sample(&mut records, &[session("$1", "main", 0)], t2));
        let record = records.get("$1").unwrap();
        assert_eq!(record.attached_since, None);
        assert_eq!(record.total_attached_secs, 15);
    }

    /// A single tracker samples several hosts into one records map. The merged
    /// live set carries both a tmux `$N` and a herdr workspace id at once;
    /// because the keyspaces are disjoint, both accrue foreground time and
    /// neither host's presence closes the other's interval. This is the
    /// invariant that lets one tracker (one writer) poll both sources safely.
    #[test]
    fn merged_multi_host_sample_credits_both_keyspaces() {
        let mut records = HashMap::new();
        let t0 = datetime!(2026-05-29 00:00:00 UTC);
        let t1 = datetime!(2026-05-29 00:00:10 UTC);

        // Poll 1: a merged sample from tmux (`$1`) + herdr (`w1`).
        let merged = [session("$1", "main", 1), session("w1", "work", 1)];
        assert!(apply_sample(&mut records, &merged, t0));
        assert!(records.get("$1").unwrap().is_attached());
        assert!(records.get("w1").unwrap().is_attached());

        // Poll 2: same merged set — both keep accruing, neither is closed by the
        // other host being present in the same sample.
        assert!(!apply_sample(&mut records, &merged, t1));
        assert_eq!(records.get("$1").unwrap().effective_total_secs(t1), 10);
        assert_eq!(records.get("w1").unwrap().effective_total_secs(t1), 10);
    }

    #[test]
    fn apply_sample_report_emits_detach_interval() {
        let mut records = HashMap::new();
        let t0 = datetime!(2026-05-29 00:00:00 UTC);
        let t1 = datetime!(2026-05-29 00:00:15 UTC);

        apply_sample_report(&mut records, &[session("$1", "main", 1)], t0);
        let report = apply_sample_report(&mut records, &[session("$1", "main", 0)], t1);

        assert_eq!(report.intervals.len(), 1);
        assert_eq!(report.intervals[0].session_id, "$1");
        assert_eq!(report.intervals[0].session_name, "main");
        assert_eq!(report.intervals[0].duration_secs, 15);
    }

    #[test]
    fn disappearing_attached_session_closes_interval() {
        let mut records = HashMap::new();
        let t0 = datetime!(2026-05-29 00:00:00 UTC);
        let t1 = datetime!(2026-05-29 00:00:20 UTC);

        apply_sample_report(&mut records, &[session("$1", "main", 1)], t0);
        let report = apply_sample_report(&mut records, &[], t1);

        assert_eq!(report.intervals.len(), 1);
        assert_eq!(report.intervals[0].session_id, "$1");
        assert_eq!(report.intervals[0].duration_secs, 20);
        let record = records.get("$1").unwrap();
        assert_eq!(record.attached_since, None);
        assert_eq!(record.total_attached_secs, 20);
        assert_eq!(record.attached_clients, 0);
    }

    // Fixed attach time for helpers whose tests don't vary it (same attach).
    const TEST_CREATED: i64 = 1_000;

    fn client(
        name: &str,
        session: &str,
        control_mode: bool,
        last_activity: i64,
    ) -> tmux::ClientInfo {
        tmux::ClientInfo {
            name: name.into(),
            session: session.into(),
            control_mode,
            last_activity,
            created: TEST_CREATED,
            in_copy_mode: false,
        }
    }

    fn cinput(session_id: &str, session_name: &str, name: &str, epoch: i64) -> ClientInput {
        cinput_mode(session_id, session_name, name, epoch, false)
    }

    fn cinput_mode(
        session_id: &str,
        session_name: &str,
        name: &str,
        epoch: i64,
        in_copy_mode: bool,
    ) -> ClientInput {
        ClientInput {
            session_id: session_id.into(),
            session_name: session_name.into(),
            name: name.into(),
            epoch,
            created: TEST_CREATED,
            in_copy_mode,
        }
    }

    #[test]
    fn client_counts_drive_user_attached_sessions() {
        let mut sessions = vec![session("$1", "main", 99), session("$2", "work", 99)];
        let clients = vec![
            client("/dev/pts/0", "main", false, 0),
            client("/dev/pts/1", "main", true, 0),
        ];

        apply_client_counts(&mut sessions, &clients);

        assert_eq!(sessions[0].attached_clients, 1);
        assert_eq!(sessions[1].attached_clients, 0);
    }

    #[test]
    fn detect_input_emits_when_a_seen_client_advances() {
        let mut seen = HashMap::new();

        // First sight of the client only seeds the baseline.
        let e = detect_input_ticks(&mut seen, &[cinput("$1", "main", "/dev/pts/3", 100)]);
        assert!(e.is_empty());

        // Same client, higher activity = a real keypress/scroll → one tick.
        let e = detect_input_ticks(&mut seen, &[cinput("$1", "main", "/dev/pts/3", 130)]);
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].kind, HumanInteractionKind::TmuxInput);
        assert_eq!(e[0].session_id.as_deref(), Some("$1"));

        // No further advance → nothing.
        let e = detect_input_ticks(&mut seen, &[cinput("$1", "main", "/dev/pts/3", 130)]);
        assert!(e.is_empty());
    }

    #[test]
    fn detect_input_tags_scroll_when_client_in_copy_mode() {
        let mut seen = HashMap::new();
        // Seed.
        detect_input_ticks(
            &mut seen,
            &[cinput_mode("$1", "main", "/dev/pts/3", 100, true)],
        );

        // Advance while the active pane is in copy mode → a scroll tick.
        let e = detect_input_ticks(
            &mut seen,
            &[cinput_mode("$1", "main", "/dev/pts/3", 130, true)],
        );
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].kind, HumanInteractionKind::TmuxScroll);

        // A later advance with the pane no longer in copy mode → a keypress tick.
        let e = detect_input_ticks(
            &mut seen,
            &[cinput_mode("$1", "main", "/dev/pts/3", 160, false)],
        );
        assert_eq!(e.len(), 1);
        assert_eq!(e[0].kind, HumanInteractionKind::TmuxInput);
    }

    #[test]
    fn detect_input_emits_on_first_sight_with_post_attach_activity() {
        // Attach-and-read before the first poll samples the client: its
        // client_activity is already later than client_created (the attach time),
        // so the very first sight is genuine input, not just an attach. `cinput`
        // uses created = 1000, so epoch 1100 sits past the attach baseline.
        let mut seen = HashMap::new();
        let e = detect_input_ticks(&mut seen, &[cinput("$1", "main", "/dev/pts/3", 1100)]);
        assert_eq!(
            e.len(),
            1,
            "post-attach activity on first sight is real input"
        );
    }

    #[test]
    fn detect_input_seeds_new_clients_without_emitting() {
        let mut seen = HashMap::new();
        // Establish a baseline for an existing client.
        detect_input_ticks(&mut seen, &[cinput("$1", "main", "/dev/pts/3", 100)]);

        // A brand-new client appears (a reattach with a new tty, or an extra
        // client) at a higher epoch — its `client_activity` is just the attach
        // time, so it must seed silently, not emit.
        let e = detect_input_ticks(
            &mut seen,
            &[
                cinput("$1", "main", "/dev/pts/3", 100),
                cinput("$1", "main", "/dev/pts/9", 200),
            ],
        );
        assert!(e.is_empty(), "a fresh client must not fabricate input");

        // The original client genuinely advancing still emits.
        let e = detect_input_ticks(
            &mut seen,
            &[
                cinput("$1", "main", "/dev/pts/3", 140),
                cinput("$1", "main", "/dev/pts/9", 200),
            ],
        );
        assert_eq!(e.len(), 1);
    }

    #[test]
    fn detect_input_prunes_gone_clients_so_they_reseed() {
        let mut seen = HashMap::new();
        detect_input_ticks(&mut seen, &[cinput("$1", "main", "/dev/pts/3", 100)]);
        // pts/3 goes away this poll → pruned from `seen`.
        detect_input_ticks(&mut seen, &[cinput("$1", "main", "/dev/pts/9", 50)]);
        assert!(!seen.contains_key("/dev/pts/3"));
        // pts/3 returns at a much higher epoch: treated as new → seed, no emit.
        let e = detect_input_ticks(&mut seen, &[cinput("$1", "main", "/dev/pts/3", 999)]);
        assert!(e.is_empty(), "a returning (pruned) client reseeds");
    }

    #[test]
    fn same_tty_reattach_does_not_emit() {
        // A detach + reattach reusing the same tty between polls keeps the
        // client_name but bumps client_created; the new client_activity is just
        // the attach time. Keying on (name, created) treats it as a fresh attach
        // → seed only, no phantom tick. A real keypress in the new attach emits.
        let mut seen = HashMap::new();
        let mk = |epoch: i64, created: i64| ClientInput {
            session_id: "$1".into(),
            session_name: "main".into(),
            name: "/dev/pts/3".into(),
            epoch,
            created,
            in_copy_mode: false,
        };

        // Baseline: attach #1 (created 1000), last activity 1000.
        detect_input_ticks(&mut seen, &[mk(1000, 1000)]);

        // Reattach (created 1500), activity = attach time 1500, same tty.
        let e = detect_input_ticks(&mut seen, &[mk(1500, 1500)]);
        assert!(
            e.is_empty(),
            "an idle reattach reusing the tty must not emit"
        );

        // A genuine keypress within the new attach (same created 1500) emits.
        let e = detect_input_ticks(&mut seen, &[mk(1600, 1500)]);
        assert_eq!(e.len(), 1);
    }

    #[test]
    fn client_inputs_filters_control_zero_and_unknown_sessions() {
        let sessions = vec![session("$1", "main", 9), session("$2", "work", 9)];
        let clients = vec![
            client("/dev/pts/1", "main", false, 100),
            client("/dev/pts/2", "main", true, 999), // control → excluded
            client("/dev/pts/3", "work", false, 0),  // unreported activity → excluded
            client("/dev/pts/4", "ghost", false, 50), // no live session → excluded
        ];

        let inputs = client_inputs(&sessions, &clients);

        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].session_id, "$1");
        assert_eq!(inputs[0].name, "/dev/pts/1");
        assert_eq!(inputs[0].epoch, 100);
    }

    #[test]
    fn client_inputs_skips_clients_without_attach_identity() {
        // tmux that doesn't report client_name/client_created (parsed as
        // empty/0) leaves no way to tell a keypress from a reattach, so such
        // clients must be excluded from input detection entirely.
        let sessions = vec![session("$1", "main", 9)];
        let no_created = tmux::ClientInfo {
            name: "/dev/pts/1".into(),
            session: "main".into(),
            control_mode: false,
            last_activity: 100,
            created: 0,
            in_copy_mode: false,
        };
        let no_name = tmux::ClientInfo {
            name: String::new(),
            session: "main".into(),
            control_mode: false,
            last_activity: 100,
            created: 1000,
            in_copy_mode: false,
        };

        let inputs = client_inputs(&sessions, &[no_created, no_name]);

        assert!(inputs.is_empty());
    }
}
