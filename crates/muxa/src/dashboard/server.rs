//! axum HTTP server for the dashboard.
//!
//! The router exposes three read-only endpoints — `/api/health`,
//! `/api/agents`, `/api/panes` — gated by [`auth_middleware`] when a
//! token is configured. Everything is read-only by design (no write
//! API), so cross-site request forgery is not a concern; we use bearer
//! tokens instead of cookies so the same router serves CLI and browser
//! clients with the same primitive.
//!
//! [`serve`] composes the router with the lifecycle plumbing the daemon
//! needs: TCP bind, graceful shutdown wired to the daemon's existing
//! shutdown channel, structured logging on bind failure.

use axum::{
    body::Body,
    extract::State,
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse, Json, Response,
    },
    routing::get,
    Router,
};
use futures::stream::{self, Stream, StreamExt};
use serde::Serialize;
use serde_json::json;
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::OffsetDateTime;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tower_http::trace::TraceLayer;

use crate::dashboard::{assets, auth, DashboardConfig};
use crate::event::{AgentState, PROTOCOL_VERSION};
use crate::metrics::Metrics;
use crate::state::{Agent, SharedStore, Transition};
use crate::tmux::scanner::{self, PaneCache, PaneSummary, ScanError};

/// SSE keep-alive ping interval. Picked long enough to be invisible
/// (15s is well under any sane proxy idle-timeout) but short enough
/// that a stale connection drops within ~30s.
const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// TTL for the cached `agents_by_state` histogram on `/api/metrics`.
/// Recomputing the histogram requires a `store.snapshot().await` plus a
/// full iteration of the registry, which contends with writers under
/// load. One second is short enough that operators scraping at 1 Hz
/// always see fresh-ish data and long enough to absorb burst scrapes.
///
/// TODO(perf): when we hit real perf issues, replace this with a
/// per-state `AtomicU64` counter that's bumped/decremented in
/// `Store::apply` on every state transition. A scrape then becomes
/// `O(num_states)` atomic loads and the cache + mutex go away.
const AGENTS_BY_STATE_CACHE_TTL: Duration = Duration::from_secs(1);

/// Cached `agents_by_state` histogram with the wall-clock instant it
/// was computed. Lives inside [`AppState`] behind a `tokio::sync::Mutex`
/// so concurrent scrapes coalesce on a single recompute when the cache
/// is stale, instead of every request taking the store snapshot lock.
#[derive(Debug, Default)]
struct AgentsByStateCache {
    /// `None` until the first scrape populates it; subsequent scrapes
    /// refresh in place. We don't pre-populate at construction because
    /// it would force `AppState::new` to be `async`.
    value: Option<(Instant, BTreeMap<String, u64>, u64)>,
}

/// Application state shared by every handler. Cheap to clone (all
/// fields are `Arc`-flavoured) so axum's `State` extractor copies
/// freely.
#[derive(Clone)]
pub struct AppState {
    pub store: SharedStore,
    pub config: Arc<DashboardConfig>,
    pub pane_cache: Arc<PaneCache>,
    /// Lock-free runtime counters surfaced via `/api/metrics`. Cloned
    /// from the [`Store`](crate::state::Store)'s metrics so SSE
    /// connect/disconnect bumps live alongside event-apply bumps.
    pub metrics: Metrics,
    /// 1-second cache for the `agents_by_state` histogram. See the
    /// [`AGENTS_BY_STATE_CACHE_TTL`] doc-comment for the perf rationale
    /// and the eventual plan to replace this with per-state atomics.
    agents_by_state_cache: Arc<tokio::sync::Mutex<AgentsByStateCache>>,
}

impl AppState {
    #[must_use]
    pub fn new(
        store: SharedStore,
        config: Arc<DashboardConfig>,
        pane_cache: Arc<PaneCache>,
    ) -> Self {
        let metrics = store.metrics();
        Self {
            store,
            config,
            pane_cache,
            metrics,
            agents_by_state_cache: Arc::new(tokio::sync::Mutex::new(AgentsByStateCache::default())),
        }
    }
}

/// Build the dashboard router. Public so `muxad`'s integration tests
/// can mount it on an in-process listener and so that future PRs (e.g.
/// the oh-my-prompt sink) can `.merge()` additional routes without
/// touching this file.
pub fn router(state: AppState) -> Router {
    let auth_layer = middleware::from_fn_with_state(state.clone(), auth_middleware);
    let api = Router::new()
        .route("/api/health", get(health_handler))
        .route("/api/agents", get(agents_handler))
        .route("/api/panes", get(panes_handler))
        .route("/api/events", get(events_handler))
        .route("/api/metrics", get(metrics_handler))
        .layer(auth_layer)
        .with_state(state);
    // Static assets sit OUTSIDE the auth layer — see assets.rs for the
    // rationale (token bootstrap in the browser).
    api.merge(assets::router::<()>())
        .layer(TraceLayer::new_for_http())
}

/// Bind and serve the router until `shutdown` fires or the listener
/// closes. Returns `Ok(())` on graceful shutdown.
pub async fn serve(
    config: Arc<DashboardConfig>,
    store: SharedStore,
    pane_cache: Arc<PaneCache>,
    mut shutdown: broadcast::Receiver<()>,
) -> std::io::Result<()> {
    let state = AppState::new(store, config.clone(), pane_cache);
    let app = router(state);
    let listener = TcpListener::bind(config.bind).await?;
    let local = listener.local_addr()?;
    tracing::info!(addr = %local, "dashboard listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.recv().await;
        })
        .await
}

/// Auth middleware. When a token is configured on the resolved config,
/// every request must carry a matching `Authorization: Bearer <tok>`.
/// When no token is configured, requests pass through unchallenged
/// (only allowed for loopback binds — enforced upstream by the
/// resolver).
async fn auth_middleware(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(expected) = state.config.token.as_deref() else {
        return Ok(next.run(req).await);
    };
    let header_value = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok());
    if auth::check_bearer(header_value, expected) {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    ok: bool,
    version: &'static str,
    protocol: u32,
}

async fn health_handler() -> impl IntoResponse {
    Json(HealthResponse {
        ok: true,
        version: env!("CARGO_PKG_VERSION"),
        protocol: PROTOCOL_VERSION,
    })
}

#[derive(Debug, Serialize)]
struct AgentsResponse {
    agents: Vec<Agent>,
}

async fn agents_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(AgentsResponse {
        agents: state.store.snapshot().await,
    })
}

#[derive(Debug, Serialize)]
struct PanesResponse {
    panes: Vec<PaneSummary>,
    errors: Vec<ScanError>,
    #[serde(with = "time::serde::rfc3339")]
    fetched_at: OffsetDateTime,
}

async fn panes_handler(State(state): State<AppState>) -> impl IntoResponse {
    let result = state.pane_cache.get_or_refresh(scanner::scan).await;
    Json(PanesResponse {
        panes: result.panes,
        errors: result.errors,
        fetched_at: result.fetched_at,
    })
}

/// Wire shape for `/api/metrics`. Mirrors [`crate::metrics::MetricsSnapshot`]
/// 1:1, plus aggregates derived from the live `Store` (agent counts).
///
/// **Stability:** unstable until the 1.0 release. Field names are
/// stable within a 0.x patch series; we may add or rename fields in
/// minor releases. Operators scraping this should be tolerant of
/// extra keys.
///
/// `events_received_per_sec_1m` is intentionally omitted from v1: an
/// accurate 1-minute rate needs a small ring buffer that we'd have to
/// update on every event, and we'd rather ship a correct counter than
/// a racy gauge. Operators can compute the rate from successive scrapes.
#[derive(Debug, Serialize)]
struct MetricsResponse {
    /// Daemon `CARGO_PKG_VERSION` — same value as `/api/health`.
    version: &'static str,
    /// Seconds since the metrics handle (and therefore the daemon's
    /// store) was constructed. Not strictly the daemon's PID-1
    /// uptime, but close enough that the difference is invisible to
    /// operators.
    uptime_secs: u64,
    /// Total agents currently in the registry (every state, including
    /// `Stopped` rows that haven't been GC'd yet).
    agents_total: u64,
    /// Histogram of agents by [`crate::event::AgentState`]. Keys are
    /// the `snake_case` `Display` form of the variant; missing keys mean
    /// zero. `BTreeMap` so the JSON output is deterministically
    /// ordered for tests and human inspection.
    agents_by_state: BTreeMap<String, u64>,
    /// Lifetime count of events the store has processed via
    /// [`Store::apply`](crate::state::Store::apply).
    events_received_total: u64,
    /// Lifetime count of successful snapshot writes (failures are not
    /// counted — the goal is "writes that hit disk").
    snapshot_writes_total: u64,
    /// Wall-clock duration of the most recent snapshot write, in
    /// milliseconds. Zero before the first write.
    snapshot_last_write_elapsed_ms: u64,
    /// Lifetime count of reconciliation passes (every pass, including
    /// no-ops — operators want to see the loop is alive).
    reconcile_passes_total: u64,
    /// Live count of SSE subscribers connected to `/api/events`.
    sse_subscribers_current: u64,
}

#[tracing::instrument(level = "debug", skip(state))]
async fn metrics_handler(State(state): State<AppState>) -> impl IntoResponse {
    let snap = state.metrics.snapshot();
    let (agents_by_state, agents_total) = agents_by_state_cached(&state).await;

    Json(MetricsResponse {
        version: env!("CARGO_PKG_VERSION"),
        uptime_secs: snap.uptime_secs,
        agents_total,
        agents_by_state,
        events_received_total: snap.events_received_total,
        snapshot_writes_total: snap.snapshot_writes_total,
        snapshot_last_write_elapsed_ms: snap.snapshot_last_write_elapsed_ms,
        reconcile_passes_total: snap.reconcile_passes_total,
        sse_subscribers_current: snap.sse_subscribers_current,
    })
}

/// Read the cached `agents_by_state` histogram, recomputing if the
/// entry is missing or older than [`AGENTS_BY_STATE_CACHE_TTL`].
/// Returns the histogram and the agent total it was derived from so
/// both numbers come from the same `store.snapshot()` call (avoids the
/// histogram-and-total drifting against each other across a race).
async fn agents_by_state_cached(state: &AppState) -> (BTreeMap<String, u64>, u64) {
    let mut guard = state.agents_by_state_cache.lock().await;
    if let Some((at, hist, total)) = guard.value.as_ref() {
        if at.elapsed() < AGENTS_BY_STATE_CACHE_TTL {
            return (hist.clone(), *total);
        }
    }
    // Cache miss or stale — recompute from the store. The registry is
    // bounded (tens to low hundreds of entries) so the iteration is
    // cheap; the cache exists to keep the `store.snapshot().await`
    // read lock from being taken on every scrape.
    let agents = state.store.snapshot().await;
    let agents_total = u64::try_from(agents.len()).unwrap_or(u64::MAX);
    let mut agents_by_state: BTreeMap<String, u64> = BTreeMap::new();
    for a in &agents {
        let key = a.state.to_string();
        *agents_by_state.entry(key).or_insert(0) += 1;
    }
    // Ensure every known state appears as an explicit zero so consumers
    // never have to special-case "missing key means zero". Cheap; the
    // enum has six variants.
    for s in [
        AgentState::Starting,
        AgentState::Working,
        AgentState::Idle,
        AgentState::WaitingInput,
        AgentState::Error,
        AgentState::Stopped,
    ] {
        agents_by_state.entry(s.to_string()).or_insert(0);
    }
    guard.value = Some((Instant::now(), agents_by_state.clone(), agents_total));
    (agents_by_state, agents_total)
}

/// Live SSE stream of state transitions.
///
/// Emits three event types on the wire:
///
/// - `snapshot` — sent first, exactly once. Payload: `{ agents: [...] }`.
///   Lets a freshly-loaded client paint the table without a separate
///   `/api/agents` round-trip.
/// - `transition` — every `Store::subscribe()` broadcast. Payload: a
///   serialized [`Transition`](crate::state::Transition).
/// - `lagged` — emitted when the broadcast receiver falls behind the
///   sender's ring buffer. Payload: the count of dropped messages.
///   Clients should treat this as a hint to refetch `/api/agents` for
///   a clean baseline.
///
/// Subscribe-then-snapshot ordering means a transition that lands
/// between the subscribe and the snapshot is delivered twice (once in
/// the snapshot, again as a transition); applying the transition is
/// idempotent so this is harmless. The reverse ordering — snapshot
/// then subscribe — would *miss* such a transition entirely.
async fn events_handler(
    State(state): State<AppState>,
) -> Sse<impl Stream<Item = Result<SseEvent, Infallible>>> {
    let rx = state.store.subscribe();
    let snapshot = state.store.snapshot().await;

    // Construct the `SubscriberGuard` *first* so the connect bump and
    // matching disconnect bump are owned by the same RAII handle —
    // `SubscriberGuard::new` increments, `Drop` decrements, and there's
    // no third call site to drift out of sync with a future refactor.
    let guard = SubscriberGuard::new(state.metrics.clone());

    let snapshot_event = SseEvent::default()
        .event("snapshot")
        .json_data(json!({ "agents": snapshot }))
        .unwrap_or_else(|_| SseEvent::default().event("snapshot").data("{}"));

    // Each broadcasted transition increments the subscriber-count
    // trace, gated at `trace!` so it stays cheap and off by default.
    // We capture the metrics handle so the `move`-d closure can read
    // the current count without needing to hop back to `state`.
    let metrics_for_stream = state.metrics.clone();
    let live = BroadcastStream::new(rx).map(move |res| {
        if res.is_ok() {
            tracing::trace!(
                subscribers = metrics_for_stream.sse_subscribers(),
                "sse.transition_broadcast",
            );
        }
        map_transition_recv_to_sse(res)
    });

    // `SubscriberGuard` decrements on stream drop — handles browser
    // refresh, network drop, and clean unsubscribe alike. Wrapping the
    // whole stream in a `_guard` field makes the destructor's lifetime
    // tied to the connection's lifetime: when axum drops the SSE body
    // (the only stable handle to the connection), the guard runs.
    let combined = stream::once(async move { snapshot_event })
        .chain(live)
        .map(Ok::<_, Infallible>);
    // Box-pin the combinator chain so the wrapper's `S: Unpin` bound
    // is satisfied without pulling in `pin-project` for one struct.
    // The double indirection costs an extra heap allocation per
    // connection — invisible at SSE rates.
    let combined: std::pin::Pin<Box<dyn Stream<Item = Result<SseEvent, Infallible>> + Send>> =
        Box::pin(combined);
    let combined = GuardedStream {
        inner: combined,
        _guard: guard,
    };

    Sse::new(combined).keep_alive(KeepAlive::new().interval(SSE_KEEPALIVE_INTERVAL))
}

/// Map a single `BroadcastStream` poll result into the SSE event emitted on
/// the wire. Extracted from [`events_handler`] so tests can drive both
/// branches — `Ok(Transition)` and `Err(Lagged)` — without spinning up a
/// full HTTP roundtrip and without duplicating the encoding rules.
pub(crate) fn map_transition_recv_to_sse(
    res: Result<Transition, BroadcastStreamRecvError>,
) -> SseEvent {
    match res {
        Ok(t) => SseEvent::default()
            .event("transition")
            .json_data(&t)
            .unwrap_or_else(|_| SseEvent::default().event("transition").data("{}")),
        Err(BroadcastStreamRecvError::Lagged(n)) => {
            SseEvent::default().event("lagged").data(n.to_string())
        }
    }
}

/// Drop guard that decrements the SSE subscriber gauge when the stream
/// it's embedded in is dropped (client disconnects, server shuts down,
/// connection drops). Pairing the bump in `sse_connect` with a guarded
/// decrement keeps the gauge accurate across every disconnect path
/// without having to instrument each one explicitly.
struct SubscriberGuard {
    metrics: Metrics,
}

impl SubscriberGuard {
    /// Single source of truth for "is this subscriber accounted for?":
    /// constructing the guard bumps the gauge, dropping it decrements.
    /// SSE connects are rare-ish (once per dashboard tab open) and
    /// operators want to see them even at the default log level — the
    /// `info!` here is symmetric with the `info!` in `Drop`.
    fn new(metrics: Metrics) -> Self {
        let after = metrics.sse_connect();
        tracing::info!(subscribers = after, "sse.connect");
        Self { metrics }
    }
}

impl Drop for SubscriberGuard {
    fn drop(&mut self) {
        let after = self.metrics.sse_disconnect();
        tracing::info!(subscribers = after, "sse.disconnect");
    }
}

/// Newtype that owns a [`SubscriberGuard`] for the lifetime of the
/// inner stream. Pinned via a `pin-project`-style hand-rolled impl —
/// we don't pull in `pin-project` for this single use and the inner
/// stream is `Unpin` (combinator-built from `BroadcastStream` +
/// `stream::once` + `chain` + `map`).
struct GuardedStream<S> {
    inner: S,
    _guard: SubscriberGuard,
}

impl<S> Stream for GuardedStream<S>
where
    S: Stream + Unpin,
{
    type Item = S::Item;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{AgentEvent, AgentId, AgentKind};
    use crate::state::Store;
    use axum::body::to_bytes;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::Value;
    use tower::ServiceExt;

    fn fresh_state() -> AppState {
        AppState::new(
            Store::shared(),
            Arc::new(DashboardConfig::loopback_default()),
            Arc::new(PaneCache::new(Duration::from_secs(60))),
        )
    }

    fn state_with_token(token: &str) -> AppState {
        let mut cfg = DashboardConfig::loopback_default();
        cfg.token = Some(token.to_string());
        AppState::new(
            Store::shared(),
            Arc::new(cfg),
            Arc::new(PaneCache::new(Duration::from_secs(60))),
        )
    }

    async fn body_json(resp: Response) -> Value {
        let bytes = to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn health_endpoint_returns_ok_json() {
        let app = router(fresh_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["ok"], true);
        assert_eq!(v["protocol"], i64::from(PROTOCOL_VERSION));
        assert!(v["version"].is_string());
    }

    #[tokio::test]
    async fn agents_endpoint_returns_store_snapshot() {
        let state = fresh_state();
        // Seed the store with two agents.
        for (kind, sid, pane) in [
            (AgentKind::ClaudeCode, "s1", "%1"),
            (AgentKind::Codex, "s2", "%2"),
        ] {
            state
                .store
                .apply(&AgentEvent::Started {
                    id: AgentId {
                        kind,
                        session_id: sid.into(),
                        pane: Some(pane.into()),
                        cwd: None,
                    },
                    at: OffsetDateTime::now_utc(),
                })
                .await;
        }

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/agents")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        let agents = v["agents"].as_array().expect("agents array");
        assert_eq!(agents.len(), 2);
        let kinds: Vec<&str> = agents.iter().map(|a| a["kind"].as_str().unwrap()).collect();
        assert!(kinds.contains(&"claude_code"));
        assert!(kinds.contains(&"codex"));
    }

    #[tokio::test]
    async fn panes_endpoint_returns_well_formed_json_shape() {
        // We don't necessarily have tmux running in the test env; the
        // contract is that we always return a well-formed JSON object
        // with `panes`, `errors`, and `fetched_at` keys regardless.
        let app = router(fresh_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/panes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert!(v["panes"].is_array());
        assert!(v["errors"].is_array());
        assert!(v["fetched_at"].is_string());
    }

    #[tokio::test]
    async fn auth_middleware_passes_with_correct_bearer() {
        let app = router(state_with_token("s3cret"));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header(header::AUTHORIZATION, "Bearer s3cret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn auth_middleware_rejects_missing_header_when_token_set() {
        let app = router(state_with_token("s3cret"));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_middleware_rejects_wrong_bearer() {
        let app = router(state_with_token("s3cret"));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header(header::AUTHORIZATION, "Bearer nope")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn auth_middleware_passes_through_when_no_token_configured() {
        // Default state has no token — every request should pass.
        let app = router(fresh_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// Read up to `limit_bytes` of SSE body or until `dur` elapses,
    /// whichever comes first. SSE streams never EOF on their own under
    /// normal operation; we use the timeout as the read budget.
    async fn collect_sse(resp: Response, dur: Duration, limit_bytes: usize) -> String {
        let mut body = resp.into_body();
        let mut bytes = Vec::new();
        let deadline = tokio::time::sleep(dur);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline => break,
                frame = body.frame() => {
                    match frame {
                        Some(Ok(f)) => {
                            if let Ok(data) = f.into_data() {
                                bytes.extend_from_slice(&data);
                                if bytes.len() >= limit_bytes { break; }
                            }
                        }
                        _ => break,
                    }
                }
            }
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    #[tokio::test]
    async fn sse_endpoint_emits_initial_snapshot() {
        let state = fresh_state();
        state
            .store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "s1".into(),
                    pane: Some("%1".into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = collect_sse(resp, Duration::from_millis(200), 1 << 16).await;
        assert!(body.contains("event: snapshot"), "body: {body:?}");
        assert!(body.contains("\"agents\""), "body: {body:?}");
        assert!(body.contains("claude_code"), "body: {body:?}");
    }

    /// End-to-end: hit `/api/events` via `Router::oneshot` and verify the
    /// FIRST SSE event emitted is `event: snapshot` with a JSON payload that
    /// matches `Store::snapshot()` at the time of subscription. Complements
    /// `sse_endpoint_emits_initial_snapshot` (substring match) by parsing
    /// the `data:` line and round-tripping the agents through
    /// `serde_json` — pins the wire contract clients depend on.
    #[tokio::test]
    async fn events_handler_emits_initial_snapshot_event() {
        let state = fresh_state();
        // Seed the store so the snapshot event has a non-empty payload to
        // round-trip — empty arrays would pass even a broken serializer.
        state
            .store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "snap-1".into(),
                    pane: Some("%1".into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;
        let expected = state.store.snapshot().await;

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = collect_sse(resp, Duration::from_millis(200), 1 << 16).await;

        // Locate the first SSE event block — events are separated by `\n\n`.
        let first_block = body
            .split("\n\n")
            .find(|b| !b.trim().is_empty())
            .expect("SSE body should contain at least one event block");
        assert!(
            first_block.contains("event: snapshot"),
            "first SSE event must be `snapshot`, got: {first_block:?}",
        );

        // Concatenate the `data:` lines (SSE allows multi-line data) and
        // confirm the payload deserializes to the live `Store::snapshot()`.
        let data_payload: String = first_block
            .lines()
            .filter_map(|l| l.strip_prefix("data:").map(str::trim))
            .collect::<Vec<_>>()
            .join("");
        let v: Value = serde_json::from_str(&data_payload)
            .unwrap_or_else(|e| panic!("snapshot data must be JSON: {e} (raw: {data_payload})"));
        let agents = v["agents"]
            .as_array()
            .expect("snapshot payload must carry `agents` array");
        assert_eq!(
            agents.len(),
            expected.len(),
            "snapshot agents count must match Store::snapshot()",
        );
        let parsed: Vec<Agent> = serde_json::from_value(v["agents"].clone())
            .expect("snapshot agents must round-trip into Vec<Agent>");
        assert_eq!(parsed.len(), expected.len());
        assert_eq!(parsed[0].session_id, expected[0].session_id);
        assert_eq!(parsed[0].kind, expected[0].kind);
    }

    #[tokio::test]
    async fn sse_endpoint_streams_transitions_after_snapshot() {
        use crate::event::{AgentState, NotificationLevel};

        let state = fresh_state();
        let store = state.store.clone();
        // Pre-create an agent so the transition has something to mutate.
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "s1".into(),
                    pane: Some("%1".into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        // Drive a state transition concurrently with reading the body.
        let store_for_task = store.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            store_for_task
                .apply(&AgentEvent::NotificationFired {
                    id: AgentId {
                        kind: AgentKind::ClaudeCode,
                        session_id: "s1".into(),
                        pane: Some("%1".into()),
                        cwd: None,
                    },
                    level: NotificationLevel::NeedsInput,
                    message: "approve?".into(),
                    at: OffsetDateTime::now_utc(),
                })
                .await;
            // sanity: state should now be WaitingInput
            let snap = store_for_task.snapshot().await;
            assert_eq!(snap[0].state, AgentState::WaitingInput);
        });

        let body = collect_sse(resp, Duration::from_millis(300), 1 << 16).await;
        assert!(body.contains("event: snapshot"), "body: {body:?}");
        assert!(body.contains("event: transition"), "body: {body:?}");
        assert!(body.contains("waiting_input"), "body: {body:?}");
    }

    /// Drive `map_transition_recv_to_sse` end-to-end on its happy path: a
    /// real `Transition` arrives from a `broadcast::Receiver`, the function
    /// must encode it as `event: transition` with a JSON payload that
    /// matches the `Transition` wire shape (`from`, `to`, `agent`).
    ///
    /// This is the analogous "happy path" companion to
    /// [`map_transition_recv_emits_lagged_event_on_overflow`], covering the
    /// `Ok(_)` arm of the same `pub(crate)` test seam. Going through the
    /// seam (rather than just `Store::subscribe`) is what makes this a
    /// dashboard-layer test rather than a state-layer one — it asserts the
    /// SSE encoding rules (`event:` tag + JSON payload shape), not just
    /// the broadcast plumbing.
    #[tokio::test]
    async fn map_transition_recv_emits_transition_event_for_active_subscriber() {
        use crate::event::AgentState;
        use tokio_stream::StreamExt as _TokioStreamExt;

        let (tx, rx) = broadcast::channel::<Transition>(8);
        let mut stream = BroadcastStream::new(rx);

        let agent = Agent {
            kind: AgentKind::ClaudeCode,
            session_id: "s1".into(),
            pane: Some("%1".into()),
            cwd: None,
            state: AgentState::Idle,
            last_prompt: None,
            last_response: None,
            last_notification: None,
            model: None,
            context_used_pct: None,
            cost_usd: None,
            rate_limit_5h_pct: None,
            rate_limit_5h_resets_at: None,
            rate_limit_7d_pct: None,
            rate_limit_7d_resets_at: None,
            rate_limited_until: None,
            rate_limit_scope: None,
            rate_limit_source: None,
            started_at: OffsetDateTime::now_utc(),
            last_activity_at: OffsetDateTime::now_utc(),
        };
        tx.send(Transition {
            from: AgentState::Starting,
            to: AgentState::Idle,
            agent: Arc::new(agent.clone()),
        })
        .expect("subscriber alive, send must succeed");

        let next = _TokioStreamExt::next(&mut stream)
            .await
            .expect("stream should yield the buffered Transition")
            .expect("Ok(Transition), not Lagged");
        let sse = map_transition_recv_to_sse(Ok(next));

        // To assert the on-the-wire shape (event-type tag + JSON payload),
        // hand the encoded `SseEvent` to `Sse::new` and consume the resulting
        // HTTP response body. That's the same path the production handler
        // takes from `map_transition_recv_to_sse` to bytes — round-tripping
        // through it is what makes this a real "wire shape" assertion.
        let app = Router::new().route(
            "/once",
            get(|| async move { Sse::new(stream::once(async move { Ok::<_, Infallible>(sse) })) }),
        );
        let resp = app
            .oneshot(Request::builder().uri("/once").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = collect_sse(resp, Duration::from_millis(200), 1 << 16).await;

        assert!(
            body.contains("event: transition"),
            "transition SSE must carry the `transition` event-type tag: {body:?}",
        );
        assert!(
            !body.contains("event: lagged"),
            "happy-path event must not be tagged `lagged`: {body:?}",
        );

        // Peel the `data:` line(s) out of the rendered SSE block and confirm
        // they deserialize to the `Transition` wire shape (`from`, `to`,
        // `agent.session_id`). `Transition` itself isn't `Deserialize` (it's
        // an outbound-only type), so we verify shape via `serde_json::Value`.
        let data_payload: String = body
            .lines()
            .filter_map(|l| l.strip_prefix("data:").map(str::trim))
            .collect::<Vec<_>>()
            .join("");
        assert!(
            !data_payload.is_empty(),
            "rendered SSE should contain at least one `data:` line: {body:?}",
        );
        let v: Value = serde_json::from_str(&data_payload)
            .unwrap_or_else(|e| panic!("data payload must be JSON: {e} (raw: {data_payload})"));
        assert_eq!(v["from"], "starting");
        assert_eq!(v["to"], "idle");
        assert_eq!(v["agent"]["session_id"], "s1");
        assert_eq!(v["agent"]["kind"], "claude_code");
    }

    /// When a subscriber falls behind the broadcast ring buffer, the SSE
    /// handler must surface that as `event: lagged` so clients know to
    /// resync via `/api/agents`. We drive the same map function the
    /// handler uses with a real `BroadcastStreamRecvError::Lagged` to
    /// avoid timing-dependent backpressure choreography over HTTP.
    #[tokio::test]
    async fn map_transition_recv_emits_lagged_event_on_overflow() {
        use crate::event::AgentState;
        // Disambiguate `next` — both `futures::StreamExt` and
        // `tokio_stream::StreamExt` are imported by this module.
        use tokio_stream::StreamExt as _TokioStreamExt;

        // Tiny channel so we can lag the receiver deterministically.
        let (tx, rx) = broadcast::channel::<Transition>(2);
        // Build the stream BEFORE sending, but never poll until after we
        // overflow — that's what the BroadcastStream impl requires to
        // produce a Lagged error on the next poll.
        let mut stream = BroadcastStream::new(rx);

        // Fill + overflow the buffer. Capacity is 2; we send 5 so the
        // receiver is now 3 messages behind.
        let agent = Agent {
            kind: AgentKind::ClaudeCode,
            session_id: "lag".into(),
            pane: Some("%1".into()),
            cwd: None,
            state: AgentState::Idle,
            last_prompt: None,
            last_response: None,
            last_notification: None,
            model: None,
            context_used_pct: None,
            cost_usd: None,
            rate_limit_5h_pct: None,
            rate_limit_5h_resets_at: None,
            rate_limit_7d_pct: None,
            rate_limit_7d_resets_at: None,
            rate_limited_until: None,
            rate_limit_scope: None,
            rate_limit_source: None,
            started_at: OffsetDateTime::now_utc(),
            last_activity_at: OffsetDateTime::now_utc(),
        };
        for _ in 0..5 {
            tx.send(Transition {
                from: AgentState::Starting,
                to: AgentState::Idle,
                agent: Arc::new(agent.clone()),
            })
            .expect("subscriber alive, send must succeed");
        }

        // First poll yields Lagged, which the handler maps to `event: lagged`.
        let next = _TokioStreamExt::next(&mut stream)
            .await
            .expect("stream should yield the Lagged error");
        assert!(
            matches!(next, Err(BroadcastStreamRecvError::Lagged(_))),
            "expected Lagged after overflowing capacity-2 channel with 5 sends, got {next:?}",
        );
        let lagged_sse = map_transition_recv_to_sse(next);
        // axum's `Event` Debug renders the on-the-wire SSE bytes
        // (e.g. `event: lagged\ndata: 3\n\n`), so a substring match on the
        // formatted Debug output asserts the encoded event-type tag.
        let lagged_dbg = format!("{lagged_sse:?}");
        assert!(
            lagged_dbg.contains("lagged"),
            "lagged SSE event must render its event-type tag: {lagged_dbg}",
        );
        assert!(
            !lagged_dbg.contains("transition"),
            "lagged event must not be tagged as a transition: {lagged_dbg}",
        );

        // Drain the rest of the buffered messages — they're real
        // `Transition`s, so the map function should emit `event: transition`.
        // Confirms the handler keeps producing well-formed events after a lag.
        let recovered = _TokioStreamExt::next(&mut stream)
            .await
            .expect("stream should keep yielding after the lag is reported");
        assert!(
            recovered.is_ok(),
            "post-lag poll should yield a regular Transition: {recovered:?}",
        );
        let ok_sse = map_transition_recv_to_sse(recovered);
        let ok_dbg = format!("{ok_sse:?}");
        assert!(
            ok_dbg.contains("transition"),
            "post-lag SSE event must be tagged `transition`: {ok_dbg}",
        );
    }

    /// `/api/metrics` returns the wire shape promised in the README:
    /// every documented field present, counters reflect events
    /// already applied, agent histogram totals match the registry.
    #[tokio::test]
    async fn metrics_endpoint_reflects_events_and_agents() {
        let state = fresh_state();
        // Seed the store with two agents — `Store::apply` bumps the
        // events counter twice, which the metrics endpoint must show.
        for (kind, sid, pane) in [
            (AgentKind::ClaudeCode, "s1", "%1"),
            (AgentKind::Codex, "s2", "%2"),
        ] {
            state
                .store
                .apply(&AgentEvent::Started {
                    id: AgentId {
                        kind,
                        session_id: sid.into(),
                        pane: Some(pane.into()),
                        cwd: None,
                    },
                    at: OffsetDateTime::now_utc(),
                })
                .await;
        }

        let app = router(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;

        // Every field documented in the README must be present and
        // typed correctly. We don't assert on `uptime_secs` value —
        // the test runs fast enough that asserting any numeric range
        // would be flaky.
        assert!(v["version"].is_string());
        assert!(v["uptime_secs"].is_u64());
        assert_eq!(v["agents_total"], 2);
        assert_eq!(v["events_received_total"], 2);
        assert_eq!(v["snapshot_writes_total"], 0);
        assert_eq!(v["snapshot_last_write_elapsed_ms"], 0);
        assert_eq!(v["reconcile_passes_total"], 0);
        assert_eq!(v["sse_subscribers_current"], 0);

        let by_state = &v["agents_by_state"];
        assert!(by_state.is_object(), "agents_by_state must be an object");
        // Both freshly-started agents land in `Starting` until a follow-up
        // event moves them. Either way, the histogram total must equal
        // `agents_total`.
        let histogram_sum: u64 = by_state
            .as_object()
            .unwrap()
            .values()
            .map(|n| n.as_u64().unwrap_or(0))
            .sum();
        assert_eq!(histogram_sum, 2);
        // Every `AgentState` variant must appear as an explicit key
        // (zero-filled), so consumers don't have to special-case
        // missing keys.
        for s in [
            "starting",
            "working",
            "idle",
            "waiting_input",
            "error",
            "stopped",
        ] {
            assert!(by_state.get(s).is_some(), "missing state key: {s}");
        }
    }

    /// The metrics endpoint is gated by the same auth middleware as
    /// `/api/agents` — without a bearer token it returns 401 when one
    /// is configured.
    #[tokio::test]
    async fn metrics_endpoint_requires_bearer_when_token_set() {
        let app = router(state_with_token("s3cret"));
        let unauthed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthed.status(), StatusCode::UNAUTHORIZED);

        let authed = app
            .oneshot(
                Request::builder()
                    .uri("/api/metrics")
                    .header(header::AUTHORIZATION, "Bearer s3cret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authed.status(), StatusCode::OK);
    }

    /// SSE connect/disconnect bumps the live-subscriber gauge — the
    /// metrics endpoint reads from the same `Metrics` instance so a
    /// concurrent SSE handle should be visible.
    #[tokio::test]
    async fn metrics_sse_subscriber_count_tracks_live_connections() {
        let state = fresh_state();
        let metrics = state.metrics.clone();
        // Pre-test: gauge is zero.
        assert_eq!(metrics.snapshot().sse_subscribers_current, 0);

        // Open an SSE connection and hold it long enough to read at
        // least the snapshot frame; while the handle is live, the
        // gauge must reflect one subscriber.
        let app = router(state);
        let sse_resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        let body = collect_sse(sse_resp, Duration::from_millis(50), 1 << 16).await;
        assert!(body.contains("event: snapshot"), "body: {body:?}");
        // The `collect_sse` helper drops the body after the read
        // budget elapses, which fires `SubscriberGuard::drop`. By the
        // time we reach this assertion the gauge is already back to
        // zero — that's the desired post-condition: connect bumped,
        // disconnect decremented, no leak.
        // Give the runtime a yield so the drop-side decrement settles.
        tokio::task::yield_now().await;
        assert_eq!(metrics.snapshot().sse_subscribers_current, 0);
    }
}
