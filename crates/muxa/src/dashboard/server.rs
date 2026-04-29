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
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use time::OffsetDateTime;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tower_http::trace::TraceLayer;

use crate::dashboard::{assets, auth, DashboardConfig};
use crate::event::PROTOCOL_VERSION;
use crate::state::{Agent, SharedStore, Transition};
use crate::tmux::scanner::{self, PaneCache, PaneSummary, ScanError};

/// SSE keep-alive ping interval. Picked long enough to be invisible
/// (15s is well under any sane proxy idle-timeout) but short enough
/// that a stale connection drops within ~30s.
const SSE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Application state shared by every handler. Cheap to clone (all
/// fields are `Arc`-flavoured) so axum's `State` extractor copies
/// freely.
#[derive(Clone)]
pub struct AppState {
    pub store: SharedStore,
    pub config: Arc<DashboardConfig>,
    pub pane_cache: Arc<PaneCache>,
}

impl AppState {
    #[must_use]
    pub fn new(
        store: SharedStore,
        config: Arc<DashboardConfig>,
        pane_cache: Arc<PaneCache>,
    ) -> Self {
        Self {
            store,
            config,
            pane_cache,
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

    let snapshot_event = SseEvent::default()
        .event("snapshot")
        .json_data(json!({ "agents": snapshot }))
        .unwrap_or_else(|_| SseEvent::default().event("snapshot").data("{}"));

    let live = BroadcastStream::new(rx).map(map_transition_recv_to_sse);

    let combined = stream::once(async move { snapshot_event })
        .chain(live)
        .map(Ok::<_, Infallible>);

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

    /// `Store::apply` should fan out a `Transition` on the broadcast that
    /// the dashboard SSE handler subscribes to. We exercise the exact
    /// channel the handler uses (via `Store::subscribe`) to confirm the
    /// delivery contract end-to-end without the HTTP plumbing.
    #[tokio::test]
    async fn store_subscribe_delivers_transition_for_sse_handler() {
        let store = Store::shared();
        // Subscribe BEFORE applying — same ordering the SSE handler uses
        // (subscribe-then-snapshot) so a transition that lands between
        // the two is never missed.
        let mut rx = store.subscribe();

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

        let t = tokio::time::timeout(Duration::from_millis(200), rx.recv())
            .await
            .expect("transition should be broadcast within the timeout")
            .expect("broadcast channel should not error");
        assert_eq!(t.agent.session_id, "s1");
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
            started_at: OffsetDateTime::now_utc(),
            last_activity_at: OffsetDateTime::now_utc(),
        };
        for _ in 0..5 {
            tx.send(Transition {
                from: AgentState::Starting,
                to: AgentState::Idle,
                agent: agent.clone(),
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
}
