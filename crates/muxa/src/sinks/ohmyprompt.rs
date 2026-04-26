//! `oh-my-prompt` HTTP sink.
//!
//! Forwards every `PromptSubmitted` event the store broadcasts to omp's
//! `/api/sync/upload` ingestion endpoint. Batches records up to a
//! configurable size or time threshold, retries 5xx with exponential
//! backoff, honors `Retry-After` on 429, and drops 4xx without retry
//! (the schema is wrong; retrying won't fix it).
//!
//! ## Wire contract
//!
//! - `POST {endpoint}/api/sync/upload`
//! - Header `X-User-Token: <UUID>` (NOT `Authorization`)
//! - Body `{ records: [...], deviceId?: string }`
//!
//! ## Mapping
//!
//! Only `PromptSubmitted` becomes a record. `event_id` is deterministic
//! (`muxa-{session_id}-{at_unix_ms}`) so retries dedup server-side.
//! `AgentKind::Unknown` records are dropped — there is no canonical
//! `source` mapping for them. See the brief in
//! `feature/omp-sink` for the locked field-by-field decisions.
//!
//! ## Failure model
//!
//! A bounded ring buffer (1000 records) drops the oldest records on
//! overflow with a WARN log — the daemon must never block ingest
//! because a remote ingest endpoint is slow. After
//! [`MAX_RETRY_ATTEMPTS`] failed attempts a batch is dropped.

use crate::config::OhMyPromptToml;
use crate::event::AgentKind;
use crate::state::PromptRecord;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::sync::{broadcast, Mutex};

/// Default env var holding the omp `X-User-Token` UUID. Configurable via
/// `[sinks.oh_my_prompt] token_env = "..."`. The token never lives in
/// TOML — only the env-var *name* does.
pub const DEFAULT_TOKEN_ENV: &str = "OMP_SERVER_TOKEN";

/// Default batch size: how many records to accumulate before flushing.
pub const DEFAULT_BATCH_SIZE: usize = 50;

/// Default flush interval. Records are sent on whichever boundary
/// (size or time) hits first.
pub const DEFAULT_FLUSH_INTERVAL: Duration = Duration::from_secs(5);

/// Hard cap on the in-memory ring buffer. Hitting this means upstream
/// is unreachable AND prompts are arriving faster than retries clear
/// the queue — drop oldest with a WARN.
pub const RING_BUFFER_CAPACITY: usize = 1000;

/// Maximum HTTP retry attempts per batch before giving up.
pub const MAX_RETRY_ATTEMPTS: usize = 6;

/// Hard cap on `Retry-After` we honor — past this we fall back to our
/// own backoff curve to avoid a misconfigured server pinning the sink.
const RETRY_AFTER_CAP: Duration = Duration::from_secs(60);

/// HTTP request timeout per attempt. Picked liberal enough for slow
/// home-network round-trips but tight enough that a black-holed peer
/// doesn't stall the whole sink task.
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum OhMyPromptError {
    #[error("oh-my-prompt sink is enabled but [sinks.oh_my_prompt].endpoint is not set")]
    MissingEndpoint,

    #[error(
        "oh-my-prompt sink is enabled but env var {var} is empty/unset \
         (set X-User-Token UUID there, or override via token_env)"
    )]
    MissingToken { var: String },

    #[error("invalid endpoint URL {url:?}: {source}")]
    InvalidEndpoint {
        url: String,
        #[source]
        source: url::ParseError,
    },

    #[error("reqwest client init failed: {0}")]
    ReqwestInit(#[source] reqwest::Error),
}

/// One upload record — mirrors omp's Zod schema. Optional fields are
/// `skip_serializing_if = "Option::is_none"` so missing data is omitted
/// from the JSON instead of being sent as `null` (omp's parser is more
/// forgiving with missing keys than null values).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UploadRecord {
    pub event_id: String,
    pub created_at: String,
    pub prompt_text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_text: Option<String>,
    pub prompt_length: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_length: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cli_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_estimate: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub word_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
}

#[derive(Debug, Serialize)]
struct UploadBody<'a> {
    records: &'a [UploadRecord],
    #[serde(rename = "deviceId", skip_serializing_if = "Option::is_none")]
    device_id: Option<&'a str>,
}

/// Resolved oh-my-prompt sink configuration + the long-lived
/// [`reqwest::Client`].
#[derive(Debug)]
pub struct OhMyPromptSink {
    upload_url: String,
    token: String,
    device_id: Option<String>,
    batch_size: usize,
    flush_interval: Duration,
    client: reqwest::Client,
}

impl OhMyPromptSink {
    /// Resolve the TOML sub-table into a runtime sink, or `Ok(None)` if
    /// the sink is disabled. Errors when `enabled = true` but required
    /// fields (`endpoint`, token env var) are missing or empty.
    pub fn resolve(toml: &OhMyPromptToml) -> Result<Option<Self>, OhMyPromptError> {
        if !toml.enabled.unwrap_or(false) {
            return Ok(None);
        }

        let endpoint = toml
            .endpoint
            .as_deref()
            .filter(|s| !s.is_empty())
            .ok_or(OhMyPromptError::MissingEndpoint)?;

        // Validate the endpoint up front so a typo surfaces at startup
        // rather than on the first prompt.
        let parsed =
            url::Url::parse(endpoint).map_err(|source| OhMyPromptError::InvalidEndpoint {
                url: endpoint.to_string(),
                source,
            })?;
        let upload_url = build_upload_url(&parsed);

        let token_var = toml
            .token_env
            .as_deref()
            .unwrap_or(DEFAULT_TOKEN_ENV)
            .to_string();
        let token = std::env::var(&token_var)
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or(OhMyPromptError::MissingToken { var: token_var })?;

        let client = reqwest::Client::builder()
            .timeout(HTTP_REQUEST_TIMEOUT)
            .build()
            .map_err(OhMyPromptError::ReqwestInit)?;

        Ok(Some(Self {
            upload_url,
            token,
            device_id: toml.device_id.clone(),
            batch_size: toml.batch_size.unwrap_or(DEFAULT_BATCH_SIZE).max(1),
            flush_interval: toml
                .flush_interval_ms
                .map_or(DEFAULT_FLUSH_INTERVAL, Duration::from_millis),
            client,
        }))
    }

    /// Run the sink loop until the shutdown channel fires or the prompt
    /// broadcast is closed.
    ///
    /// Architecturally this spawns *two* tasks behind the scenes — an
    /// ingest loop that pulls from the broadcast and pushes into a
    /// bounded ring buffer (drop-oldest on overflow) and a flush loop
    /// that drains the ring on size/time triggers and POSTs. Splitting
    /// the two means a slow remote HTTP endpoint never blocks ingest
    /// — the ring buffer is the single backpressure surface.
    pub async fn run(
        self,
        prompt_rx: broadcast::Receiver<PromptRecord>,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) -> Result<(), OhMyPromptError> {
        let buffer: Arc<Mutex<VecDeque<UploadRecord>>> =
            Arc::new(Mutex::new(VecDeque::with_capacity(self.batch_size)));

        // Notify the flusher that the buffer crossed the size
        // threshold (or that shutdown is in progress and any leftover
        // records should be drained promptly).
        let (kick_tx, kick_rx) = tokio::sync::mpsc::channel::<KickReason>(8);

        let ingest = tokio::spawn(ingest_loop(
            prompt_rx,
            buffer.clone(),
            kick_tx.clone(),
            self.batch_size,
        ));

        // Borrow `self` into the flusher via Arc so the ingest task can
        // run independently. The flusher handles its own shutdown via
        // the shared shutdown broadcast.
        let this = Arc::new(self);
        let flusher_shutdown = shutdown_rx.resubscribe();
        let flusher = tokio::spawn(flush_loop(
            this.clone(),
            buffer.clone(),
            kick_rx,
            flusher_shutdown,
        ));

        // Wait for shutdown, then stop both subtasks. Aborting is
        // safe here — the broadcast channel is the source of truth
        // and any in-flight HTTP request loses an at-most-best-effort
        // batch. Matches the notifier task shape: shutdown is fast,
        // not graceful.
        let _ = shutdown_rx.recv().await;
        drop(kick_tx);
        ingest.abort();
        flusher.abort();
        let _ = ingest.await;
        let _ = flusher.await;
        Ok(())
    }

    /// Pop up to `batch_size` records and POST them. Errors are
    /// swallowed (logged) — the run loop never exits because of a bad
    /// HTTP response.
    async fn flush_some(&self, buffer: &Mutex<VecDeque<UploadRecord>>) {
        let batch = take_up_to(buffer, self.batch_size).await;
        if batch.is_empty() {
            return;
        }
        let count = batch.len();
        if let Err(e) = self.post_batch_with_retry(&batch).await {
            tracing::warn!(
                error = %e,
                count,
                "oh-my-prompt: dropped batch after retries exhausted"
            );
        }
    }

    /// HTTP send with retry/backoff. See the module docs for the
    /// poison/retry matrix.
    async fn post_batch_with_retry(&self, batch: &[UploadRecord]) -> Result<(), reqwest::Error> {
        let body = UploadBody {
            records: batch,
            device_id: self.device_id.as_deref(),
        };

        for attempt in 0..MAX_RETRY_ATTEMPTS {
            let response = self
                .client
                .post(&self.upload_url)
                .header("X-User-Token", &self.token)
                .json(&body)
                .send()
                .await;

            match response {
                Ok(resp) => {
                    let status = resp.status();
                    if status.is_success() {
                        if status.as_u16() == 207 {
                            // 207 partial — log the rejected entries
                            // for visibility and treat the batch as
                            // accepted (do NOT retry).
                            let detail = resp.text().await.unwrap_or_default();
                            tracing::warn!(detail = %detail, "oh-my-prompt: partial 207 response");
                        }
                        return Ok(());
                    }

                    if status.as_u16() == 429 {
                        let wait = retry_after(&resp).unwrap_or_else(|| backoff(attempt));
                        let body_text = resp.text().await.unwrap_or_default();
                        tracing::warn!(
                            wait_ms = u64::try_from(wait.as_millis()).unwrap_or(u64::MAX),
                            detail = %body_text,
                            "oh-my-prompt: 429 rate-limited; honoring Retry-After"
                        );
                        tokio::time::sleep(wait).await;
                        continue;
                    }

                    if status.is_client_error() {
                        // 4xx other than 429 = poison. Don't retry.
                        let body_text = resp.text().await.unwrap_or_default();
                        tracing::warn!(
                            status = status.as_u16(),
                            detail = %body_text,
                            "oh-my-prompt: 4xx client error; dropping batch"
                        );
                        return Ok(());
                    }

                    // 5xx — retryable.
                    let body_text = resp.text().await.unwrap_or_default();
                    tracing::warn!(
                        attempt = attempt + 1,
                        status = status.as_u16(),
                        detail = %body_text,
                        "oh-my-prompt: 5xx; retrying"
                    );
                    tokio::time::sleep(backoff(attempt)).await;
                }
                Err(e) => {
                    // Network / timeout. Retry.
                    tracing::warn!(
                        attempt = attempt + 1,
                        error = %e,
                        "oh-my-prompt: transport error; retrying"
                    );
                    tokio::time::sleep(backoff(attempt)).await;
                }
            }
        }

        // Manufacture an error with no underlying transport detail —
        // we've already logged each attempt.
        tracing::error!(
            attempts = MAX_RETRY_ATTEMPTS,
            "oh-my-prompt: exhausted retries; batch dropped"
        );
        Ok(())
    }
}

/// Reason the flusher woke up. Today the only meaningful distinction
/// is "buffer crossed the batch threshold"; we keep this as an enum so
/// future kicks (e.g. shutdown-drain hint) plug in without a wire-format
/// rewrite.
#[derive(Debug, Clone, Copy)]
enum KickReason {
    BatchReady,
}

/// Pull `PromptRecord`s off the broadcast forever, convert them into
/// `UploadRecord`s, and push them into the bounded ring. Sends a
/// `BatchReady` kick whenever the buffer crosses `batch_size` so the
/// flusher wakes up promptly. Exits when the broadcast closes.
async fn ingest_loop(
    mut prompt_rx: broadcast::Receiver<PromptRecord>,
    buffer: Arc<Mutex<VecDeque<UploadRecord>>>,
    kick_tx: tokio::sync::mpsc::Sender<KickReason>,
    batch_size: usize,
) {
    loop {
        match prompt_rx.recv().await {
            Ok(record) => {
                let Some(upload) = prompt_to_record(&record) else {
                    continue;
                };
                let len = push_with_overflow(&buffer, upload).await;
                if len >= batch_size {
                    // Channel full = a kick is already pending; it's
                    // fine to skip rather than block.
                    let _ = kick_tx.try_send(KickReason::BatchReady);
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    missed = n,
                    "oh-my-prompt sink lagged behind prompt stream; continuing"
                );
            }
            Err(broadcast::error::RecvError::Closed) => {
                tracing::debug!("prompt broadcast closed; ingest loop exiting");
                return;
            }
        }
    }
}

/// Drain the buffer on size kicks, time ticks, or shutdown.
async fn flush_loop(
    sink: Arc<OhMyPromptSink>,
    buffer: Arc<Mutex<VecDeque<UploadRecord>>>,
    mut kick_rx: tokio::sync::mpsc::Receiver<KickReason>,
    mut shutdown_rx: broadcast::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(sink.flush_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Skip the immediate first tick so we don't flush an empty buffer
    // before any prompts arrive.
    ticker.tick().await;

    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.recv() => {
                // Match the notifier task's shutdown shape: just exit.
                // We deliberately do *not* drain-with-retries here —
                // doing so on an unreachable endpoint can stall daemon
                // shutdown for minutes. Buffered records that haven't
                // yet flushed are lost; the sink is best-effort.
                tracing::debug!("oh-my-prompt sink received shutdown");
                return;
            }
            maybe = kick_rx.recv() => {
                match maybe {
                    Some(KickReason::BatchReady) => {
                        sink.flush_some(&buffer).await;
                    }
                    None => {
                        // Ingest task gone — flush remaining and exit.
                        sink.flush_some(&buffer).await;
                        return;
                    }
                }
            }
            _ = ticker.tick() => {
                sink.flush_some(&buffer).await;
            }
        }
    }
}

/// Combine the user-supplied base URL with the omp upload path. We
/// preserve any path prefix the user set (so a reverse-proxied
/// `https://example.com/omp/` works).
fn build_upload_url(parsed: &url::Url) -> String {
    let trimmed = parsed.as_str().trim_end_matches('/');
    format!("{trimmed}/api/sync/upload")
}

/// Push a record into the ring buffer. If the buffer is at capacity
/// the oldest record is dropped with a WARN log. Returns the resulting
/// buffer length so the caller can decide whether to flush.
async fn push_with_overflow(buffer: &Mutex<VecDeque<UploadRecord>>, record: UploadRecord) -> usize {
    let mut guard = buffer.lock().await;
    if guard.len() >= RING_BUFFER_CAPACITY {
        let dropped = guard.pop_front();
        if let Some(d) = dropped {
            tracing::warn!(
                event_id = %d.event_id,
                "oh-my-prompt ring buffer full; dropped oldest record"
            );
        }
    }
    guard.push_back(record);
    guard.len()
}

async fn take_up_to(buffer: &Mutex<VecDeque<UploadRecord>>, max: usize) -> Vec<UploadRecord> {
    let mut guard = buffer.lock().await;
    let take = guard.len().min(max);
    guard.drain(..take).collect()
}

/// Exponential backoff: 500ms, 1s, 2s, 4s, 8s, 16s.
fn backoff(attempt: usize) -> Duration {
    let base = Duration::from_millis(500);
    // 2^attempt is fine for attempts in 0..6 (max 32). Use checked
    // shifts to keep clippy quiet for higher hypothetical values.
    let mult = 1u32 << u32::try_from(attempt).unwrap_or(0).min(6);
    base.saturating_mul(mult)
}

/// Parse `Retry-After` (seconds form) and clamp at [`RETRY_AFTER_CAP`].
fn retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let header = resp.headers().get(reqwest::header::RETRY_AFTER)?;
    let s = header.to_str().ok()?;
    let secs: u64 = s.trim().parse().ok()?;
    Some(Duration::from_secs(secs).min(RETRY_AFTER_CAP))
}

/// Map an `AgentKind` to the omp `source` string. `Unknown` returns
/// `None` so the caller drops the record (no canonical mapping).
pub fn agent_kind_to_source(kind: AgentKind) -> Option<&'static str> {
    match kind {
        AgentKind::ClaudeCode => Some("claude-code"),
        AgentKind::Codex => Some("codex"),
        AgentKind::GeminiCli => Some("gemini"),
        AgentKind::Opencode => Some("opencode"),
        AgentKind::Unknown => None,
    }
}

/// Map an `AgentKind` to the omp `cli_name` string.
pub fn agent_kind_to_cli_name(kind: AgentKind) -> Option<&'static str> {
    match kind {
        AgentKind::ClaudeCode => Some("claude"),
        AgentKind::Codex => Some("codex"),
        AgentKind::GeminiCli => Some("gemini"),
        AgentKind::Opencode => Some("opencode"),
        AgentKind::Unknown => None,
    }
}

/// Deterministic event id. omp's server sanitizes non-`[A-Za-z0-9_-]`
/// — we use `-` separators so a session id like `abc-123` round-trips
/// cleanly. Same `(session_id, at)` always yields the same id, so
/// retries dedup server-side.
pub fn event_id(session_id: &str, at: OffsetDateTime) -> String {
    let unix_ms = at.unix_timestamp_nanos() / 1_000_000;
    format!("muxa-{session_id}-{unix_ms}")
}

/// Convert one `PromptRecord` into the omp wire shape, or `None` if
/// the agent kind has no mapping (currently only `Unknown`).
pub fn prompt_to_record(rec: &PromptRecord) -> Option<UploadRecord> {
    let source = agent_kind_to_source(rec.id.kind)?;
    let cli_name = agent_kind_to_cli_name(rec.id.kind);

    let prompt_length = rec.prompt.chars().count();
    let word_count = rec.prompt.split_whitespace().count();
    let token_estimate = prompt_length / 4;

    let cwd = rec.id.cwd.clone();
    let project = cwd.as_deref().and_then(|p| {
        std::path::Path::new(p)
            .file_name()
            .and_then(|s| s.to_str())
            .map(str::to_string)
    });

    let created_at = rec
        .at
        .format(&time::format_description::well_known::Rfc3339)
        .ok()?;

    Some(UploadRecord {
        event_id: event_id(&rec.id.session_id, rec.at),
        created_at,
        prompt_text: rec.prompt.clone(),
        response_text: None,
        prompt_length,
        response_length: None,
        project,
        cwd,
        source: Some(source.to_string()),
        session_id: Some(rec.id.session_id.clone()),
        role: Some("user".to_string()),
        model: rec.model.clone(),
        cli_name: cli_name.map(str::to_string),
        cli_version: Some(env!("CARGO_PKG_VERSION").to_string()),
        token_estimate: Some(token_estimate),
        word_count: Some(word_count),
        content_hash: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentId, AgentKind};
    use time::macros::datetime;

    fn rec(kind: AgentKind, prompt: &str, cwd: Option<&str>, model: Option<&str>) -> PromptRecord {
        PromptRecord {
            id: AgentId {
                kind,
                session_id: "sess-1".into(),
                pane: Some("%1".into()),
                cwd: cwd.map(str::to_string),
            },
            prompt: prompt.into(),
            at: datetime!(2026-04-26 12:00:00 UTC),
            model: model.map(str::to_string),
        }
    }

    #[test]
    fn event_id_is_deterministic() {
        let at = datetime!(2026-04-26 12:00:00 UTC);
        let a = event_id("sess-1", at);
        let b = event_id("sess-1", at);
        assert_eq!(a, b);

        let c = event_id("sess-2", at);
        assert_ne!(a, c, "different session must yield a different id");

        let d = event_id("sess-1", at + time::Duration::milliseconds(1));
        assert_ne!(a, d, "different timestamp must yield a different id");
    }

    #[test]
    fn event_id_separator_avoids_colons() {
        let at = datetime!(2026-04-26 12:00:00 UTC);
        let id = event_id("sess-1", at);
        assert!(!id.contains(':'), "event_id {id:?} contains a colon");
        assert!(id.starts_with("muxa-"));
    }

    #[test]
    fn agent_kind_maps_to_source_string() {
        assert_eq!(
            agent_kind_to_source(AgentKind::ClaudeCode),
            Some("claude-code")
        );
        assert_eq!(agent_kind_to_source(AgentKind::Codex), Some("codex"));
        assert_eq!(agent_kind_to_source(AgentKind::GeminiCli), Some("gemini"));
        assert_eq!(agent_kind_to_source(AgentKind::Opencode), Some("opencode"));
        assert_eq!(agent_kind_to_source(AgentKind::Unknown), None);
    }

    #[test]
    fn unknown_agent_kind_skips_record() {
        let r = rec(AgentKind::Unknown, "hi", None, None);
        assert!(prompt_to_record(&r).is_none());
    }

    #[test]
    fn prompt_to_record_fills_required_fields() {
        let r = rec(AgentKind::ClaudeCode, "hello world", None, None);
        let u = prompt_to_record(&r).expect("record produced");
        assert!(!u.event_id.is_empty());
        assert!(!u.created_at.is_empty());
        assert_eq!(u.prompt_text, "hello world");
        assert_eq!(u.prompt_length, 11);
        assert_eq!(u.source.as_deref(), Some("claude-code"));
        assert_eq!(u.cli_name.as_deref(), Some("claude"));
        assert_eq!(u.cli_version.as_deref(), Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(u.role.as_deref(), Some("user"));
        assert_eq!(u.session_id.as_deref(), Some("sess-1"));
        assert_eq!(u.word_count, Some(2));
        assert_eq!(u.token_estimate, Some(11 / 4));
    }

    #[test]
    fn prompt_to_record_drops_optional_fields_when_unknown() {
        let r = rec(AgentKind::ClaudeCode, "hi", None, None);
        let u = prompt_to_record(&r).unwrap();
        assert!(u.cwd.is_none());
        assert!(u.project.is_none());
        assert!(u.model.is_none());
        // Round-trip JSON to confirm the keys are absent (not null).
        let v = serde_json::to_value(&u).unwrap();
        let obj = v.as_object().unwrap();
        assert!(!obj.contains_key("cwd"));
        assert!(!obj.contains_key("project"));
        assert!(!obj.contains_key("model"));
        assert!(!obj.contains_key("response_text"));
        assert!(!obj.contains_key("content_hash"));
    }

    #[test]
    fn prompt_to_record_uses_cwd_basename_for_project() {
        let r = rec(AgentKind::ClaudeCode, "hi", Some("/home/u/work/muxa"), None);
        let u = prompt_to_record(&r).unwrap();
        assert_eq!(u.cwd.as_deref(), Some("/home/u/work/muxa"));
        assert_eq!(u.project.as_deref(), Some("muxa"));
    }

    #[test]
    fn prompt_length_is_char_count_not_bytes() {
        let r = rec(AgentKind::ClaudeCode, "héllo", None, None);
        let u = prompt_to_record(&r).unwrap();
        assert_eq!(u.prompt_length, 5);
    }

    #[test]
    fn backoff_grows_exponentially_then_caps() {
        assert_eq!(backoff(0), Duration::from_millis(500));
        assert_eq!(backoff(1), Duration::from_secs(1));
        assert_eq!(backoff(2), Duration::from_secs(2));
        assert_eq!(backoff(3), Duration::from_secs(4));
        assert_eq!(backoff(4), Duration::from_secs(8));
        assert_eq!(backoff(5), Duration::from_secs(16));
    }

    #[test]
    fn build_upload_url_appends_path_to_base() {
        let parsed = url::Url::parse("https://prompt.example.dev/").unwrap();
        assert_eq!(
            build_upload_url(&parsed),
            "https://prompt.example.dev/api/sync/upload"
        );
        let parsed = url::Url::parse("https://prompt.example.dev").unwrap();
        assert_eq!(
            build_upload_url(&parsed),
            "https://prompt.example.dev/api/sync/upload"
        );
        let parsed = url::Url::parse("https://example.com/omp").unwrap();
        assert_eq!(
            build_upload_url(&parsed),
            "https://example.com/omp/api/sync/upload"
        );
    }

    #[test]
    fn resolve_returns_none_when_disabled() {
        let toml = OhMyPromptToml::default();
        let out = OhMyPromptSink::resolve(&toml).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn resolve_errors_when_enabled_without_endpoint() {
        let toml = OhMyPromptToml {
            enabled: Some(true),
            ..OhMyPromptToml::default()
        };
        let err = OhMyPromptSink::resolve(&toml).unwrap_err();
        assert!(matches!(err, OhMyPromptError::MissingEndpoint));
    }

    #[test]
    fn resolve_errors_when_enabled_without_token() {
        // Use a unique env var name so the test is hermetic.
        let var = "OMP_TEST_TOKEN_FOR_MISSING_TOKEN_TEST";
        // Make sure it's unset.
        // SAFETY: tests are single-threaded by default for this var.
        std::env::remove_var(var);
        let toml = OhMyPromptToml {
            enabled: Some(true),
            endpoint: Some("https://example.dev".into()),
            token_env: Some(var.into()),
            ..OhMyPromptToml::default()
        };
        let err = OhMyPromptSink::resolve(&toml).unwrap_err();
        assert!(matches!(err, OhMyPromptError::MissingToken { .. }));
    }
}
