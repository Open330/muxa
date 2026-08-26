//! axum HTTP server for the dashboard.
//!
//! Read routes can be private, public-read, or fully public depending on
//! [`DashboardAuthMode`](crate::config::DashboardAuthMode). Control routes
//! are separate and always require the configured bearer token; `auth =
//! "none"` disables them rather than making them public. Bearer tokens are
//! sent in `Authorization`, never cookies, so cross-site forms cannot trigger
//! a control action and browser clients can treat the token like a PAT.
//!
//! [`serve`] composes the router with the lifecycle plumbing the daemon
//! needs: TCP bind, graceful shutdown wired to the daemon's existing
//! shutdown channel, structured logging on bind failure.

use axum::{
    body::Body,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse, Json, Response,
    },
    routing::{get, post, put},
    Router,
};
use futures::stream::{self, Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use time::{OffsetDateTime, UtcOffset};
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tower_http::trace::TraceLayer;

use crate::backend::{HostKind, SharedBackend};
use crate::config::{DashboardAuthMode, StatsConfig};
use crate::dashboard::work_store::WorkStore;
use crate::dashboard::{assets, auth, DashboardConfig};
use crate::event::{AgentKind, AgentState, PROTOCOL_VERSION};
use crate::fleet::{FleetOperation, FleetRuntime, LabelSelector};
use crate::metrics::Metrics;
use crate::scope_filter::ScopeExclusions;
use crate::session::{SessionBackend, SharedSessionBackend};
use crate::state::{Agent, SharedStore, Transition};
use crate::timeline::{
    self, TimelineBuildInput, TimelineDocument, TimelineFilters, TimelineLane, TimelineLaneKind,
    TimelineRange, TimelineTotals,
};
use crate::tmux::scanner::{self, MuxaPaneMetadata, PaneCache, PaneSummary, ScanError};
use crate::work::{
    self, ExecutionIdentity, WorkIdentity, WorkMetadataPatch, WorkRecord, WorkSnapshot,
};

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

const TIMELINE_SUMMARY_CACHE_TTL: Duration = Duration::from_secs(15);
const TIMELINE_SUMMARY_CACHE_CAPACITY: usize = 16;
const CONTROL_BODY_LIMIT_BYTES: usize = 64 * 1024;

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

#[derive(Debug, Default)]
struct TimelineSummaryCache {
    values: HashMap<TimelineSummaryCacheKey, (Instant, TimelineSummaryResponse)>,
}

/// Application state shared by every handler. Cheap to clone (all
/// fields are `Arc`-flavoured) so axum's `State` extractor copies
/// freely.
#[derive(Clone)]
pub struct AppState {
    pub store: SharedStore,
    pub config: Arc<DashboardConfig>,
    pub pane_cache: Arc<PaneCache>,
    pub sessions: SharedSessionBackend,
    /// Primary pane backend retained for compatibility with callers that
    /// inspect it directly. Pane scans use `backends` below so every active
    /// host contributes to one dashboard inventory.
    pub backend: Option<SharedBackend>,
    /// Every active backend, used both for pane scans and to route PAT-gated
    /// control actions by pane-id namespace.
    pub backends: Arc<Vec<SharedBackend>>,
    /// Lock-free runtime counters surfaced via `/api/metrics`. Cloned
    /// from the [`Store`](crate::state::Store)'s metrics so SSE
    /// connect/disconnect bumps live alongside event-apply bumps.
    pub metrics: Metrics,
    /// Activity ledger path used by `/api/timeline`. `None` means the
    /// endpoint still returns a well-formed empty timeline with a note.
    pub activity_path: Option<PathBuf>,
    /// Session activity file used only for currently-open tmux foreground
    /// intervals. Closed foreground intervals come from `activity_path`.
    pub session_activity_path: Option<PathBuf>,
    /// ACTIVE estimate tuning shared with `muxa stats`.
    pub stats_config: StatsConfig,
    /// Optional SSH fleet runtime. Fleet reads reuse muxad's per-host cache;
    /// dashboard requests never launch their own SSH processes.
    pub fleet: Option<FleetRuntime>,
    /// Durable operator-authored work metadata. Execution topology remains in
    /// the backend; only workflow annotations are stored here.
    work_store: Arc<tokio::sync::Mutex<WorkStore>>,
    /// 1-second cache for the `agents_by_state` histogram. See the
    /// [`AGENTS_BY_STATE_CACHE_TTL`] doc-comment for the perf rationale
    /// and the eventual plan to replace this with per-state atomics.
    agents_by_state_cache: Arc<tokio::sync::Mutex<AgentsByStateCache>>,
    /// Short-lived parsed ledger cache shared by summary and detail timeline
    /// requests. Entries are immutable behind an `Arc`, so handlers do not
    /// clone the full retained activity set.
    activity_ledger_cache: Arc<tokio::sync::Mutex<crate::activity::ActivityCache>>,
    /// Compact final response cache. This protects the daemon from multiple
    /// dashboard tabs rebuilding the same retained-history projection at once.
    timeline_summary_cache: Arc<tokio::sync::Mutex<TimelineSummaryCache>>,
    /// `[message.skills]`, so a dashboard prompt can expand the same `/name`
    /// templates the CLI and `muxa_call_peer` expand. One composer, three
    /// front doors.
    message_skills: Arc<BTreeMap<String, String>>,
}

#[derive(Clone, Default)]
pub struct DashboardRuntimeConfig {
    pub message_skills: BTreeMap<String, String>,
    pub activity_path: Option<PathBuf>,
    pub session_activity_path: Option<PathBuf>,
    pub work_store_path: Option<PathBuf>,
    pub stats_config: StatsConfig,
    pub fleet: Option<FleetRuntime>,
}

impl AppState {
    #[must_use]
    pub fn new(
        store: SharedStore,
        config: Arc<DashboardConfig>,
        pane_cache: Arc<PaneCache>,
        sessions: SharedSessionBackend,
    ) -> Self {
        let metrics = store.metrics();
        Self {
            store,
            config,
            pane_cache,
            sessions,
            backend: None,
            backends: Arc::new(Vec::new()),
            metrics,
            activity_path: None,
            session_activity_path: None,
            stats_config: StatsConfig::default(),
            fleet: None,
            work_store: Arc::new(tokio::sync::Mutex::new(WorkStore::memory())),
            agents_by_state_cache: Arc::new(tokio::sync::Mutex::new(AgentsByStateCache::default())),
            activity_ledger_cache: Arc::new(tokio::sync::Mutex::new(
                crate::activity::ActivityCache::default(),
            )),
            timeline_summary_cache: Arc::new(tokio::sync::Mutex::new(
                TimelineSummaryCache::default(),
            )),
            message_skills: Arc::new(BTreeMap::new()),
        }
    }

    #[must_use]
    pub fn with_message_skills(mut self, skills: BTreeMap<String, String>) -> Self {
        self.message_skills = Arc::new(skills);
        self
    }

    #[must_use]
    pub fn with_activity_paths(
        mut self,
        activity_path: Option<PathBuf>,
        session_activity_path: Option<PathBuf>,
    ) -> Self {
        self.activity_path = activity_path;
        self.session_activity_path = session_activity_path;
        self
    }

    #[must_use]
    pub fn with_stats_config(mut self, stats_config: StatsConfig) -> Self {
        self.stats_config = stats_config;
        self
    }

    #[must_use]
    pub fn with_fleet(mut self, fleet: Option<FleetRuntime>) -> Self {
        self.fleet = fleet;
        self
    }

    #[must_use]
    pub fn with_work_store_path(mut self, path: Option<PathBuf>) -> Self {
        self.work_store = Arc::new(tokio::sync::Mutex::new(WorkStore::load(path)));
        self
    }

    #[must_use]
    pub fn with_backend(mut self, backend: SharedBackend) -> Self {
        self.backend = Some(backend.clone());
        self.backends = Arc::new(vec![backend]);
        self
    }

    #[must_use]
    pub fn with_backends(mut self, backends: Vec<SharedBackend>) -> Self {
        self.backend = backends.first().cloned();
        self.backends = Arc::new(backends);
        self
    }

    /// Refresh the pane inventory through the pane cache. tmux keeps its
    /// multi-socket scanner; every active non-tmux backend contributes its
    /// own pane inventory. `/api/panes`, control routing, and the timeline's
    /// pane→session map therefore see the same multi-host set.
    async fn refresh_pane_scan(&self) -> scanner::ScanResult {
        let backends = self.backends.clone();
        self.pane_cache
            .get_or_refresh(|| refresh_pane_scan(backends))
            .await
    }
}

/// Pull one multi-host inventory for the pane cache. The tmux scanner and
/// non-tmux backend calls run concurrently; each backend is sync-blocking, so
/// the latter stay on Tokio's blocking pool.
async fn refresh_pane_scan(backends: Arc<Vec<SharedBackend>>) -> scanner::ScanResult {
    let non_tmux = backends
        .iter()
        .filter(|backend| backend.kind() != HostKind::Tmux)
        .cloned()
        .collect::<Vec<_>>();
    let backend_scan = tokio::task::spawn_blocking(move || {
        non_tmux
            .into_iter()
            .map(|backend| (backend.kind(), backend.list_panes()))
            .collect::<Vec<_>>()
    });
    let (mut scan, backend_rows) = tokio::join!(scanner::scan(), backend_scan);
    if let Ok(backend_rows) = backend_rows {
        for (kind, panes) in backend_rows {
            let additional = if kind == HostKind::Herdr {
                scanner::herdr_scan_result(panes)
            } else {
                backend_scan_result(kind, panes)
            };
            scan = merge_pane_scans(scan, additional);
        }
    }
    scan
}

/// Convert a non-tmux backend's common `PaneInfo` rows to the dashboard wire
/// shape. rmux retains its full socket path; hosts without endpoint paths use
/// a stable host label. Bare-terminal attach commands remain tmux-only.
fn backend_scan_result(kind: HostKind, panes: Vec<crate::tmux::PaneInfo>) -> scanner::ScanResult {
    let panes = panes
        .into_iter()
        .map(|pane| {
            let socket = match kind {
                HostKind::Rmux | HostKind::Cmux => pane
                    .socket
                    .as_deref()
                    .map_or_else(|| PathBuf::from(kind.to_string()), PathBuf::from),
                HostKind::Herdr => PathBuf::from("herdr"),
                HostKind::Zellij => PathBuf::from("zellij"),
                HostKind::Tmux => pane
                    .socket
                    .as_deref()
                    .map_or_else(|| PathBuf::from("tmux"), PathBuf::from),
            };
            PaneSummary {
                host: kind,
                pane_id: pane.pane_id,
                session_id: pane.session_id,
                session: pane.session,
                window_id: pane.window_id,
                window_name: pane.window_name,
                window_index: pane.window_index,
                pane_index: pane.pane_index,
                tty: pane.tty,
                current_command: pane.current_command,
                title: pane.title,
                current_path: pane.current_path,
                socket,
                muxa: MuxaPaneMetadata::default(),
                attach_command: String::new(),
            }
        })
        .collect();
    scanner::ScanResult {
        panes,
        errors: Vec::new(),
        fetched_at: OffsetDateTime::now_utc(),
    }
}

/// Append one backend inventory while preserving earlier scan errors and the
/// original fetch timestamp.
fn merge_pane_scans(
    mut base: scanner::ScanResult,
    additional: scanner::ScanResult,
) -> scanner::ScanResult {
    base.panes.extend(additional.panes);
    base.errors.extend(additional.errors);
    base
}

/// Build the dashboard router. Public so `muxad`'s integration tests
/// can mount it on an in-process listener and so that future PRs (e.g.
/// the oh-my-prompt sink) can `.merge()` additional routes without
/// touching this file.
pub fn router(state: AppState) -> Router {
    let host_layer = middleware::from_fn_with_state(state.clone(), host_guard_middleware);
    let read_auth_layer = middleware::from_fn_with_state(state.clone(), read_auth_middleware);
    let write_auth_layer = middleware::from_fn_with_state(state.clone(), write_auth_middleware);
    let read_api = Router::new()
        .route("/api/health", get(health_handler))
        .route("/api/access", get(access_handler))
        .route("/api/agents", get(agents_handler))
        .route("/api/fleet", get(fleet_handler))
        .route("/api/panes", get(panes_handler))
        .route("/api/works", get(works_handler))
        .route("/api/work-metadata", get(work_metadata_handler))
        .route("/api/terminal-sessions", get(terminal_sessions_handler))
        .route(
            "/api/terminal-sessions/{id}/capture",
            get(terminal_capture_handler),
        )
        .route("/api/timeline", get(timeline_handler))
        .route("/api/events", get(events_handler))
        .route("/api/metrics", get(metrics_handler));
    let read_api = if matches!(state.config.auth, DashboardAuthMode::Token) {
        read_api.layer(read_auth_layer)
    } else {
        read_api
    };
    let write_api = Router::new()
        .route("/api/panes/{pane}/prompt", post(pane_prompt_handler))
        .route("/api/fleet/{host}/command", post(fleet_command_handler))
        .route("/api/panes/{pane}/abort", post(pane_abort_handler))
        .route("/api/work-metadata", put(work_metadata_put_handler))
        .route("/api/work-control/prompt", post(work_prompt_handler))
        .route("/api/work-control/up", post(work_up_handler))
        .route("/api/work-control/abort", post(work_abort_handler))
        .route(
            "/api/terminal-sessions/{id}/input",
            post(terminal_input_handler),
        )
        .route(
            "/api/terminal-sessions/{id}/terminate",
            post(terminal_terminate_handler),
        )
        .layer(DefaultBodyLimit::max(CONTROL_BODY_LIMIT_BYTES))
        .layer(write_auth_layer);
    let api = read_api.merge(write_api).with_state(state.clone());
    // Static assets sit OUTSIDE the auth layer — see assets.rs for the
    // rationale (token bootstrap in the browser). The DNS-rebinding host
    // guard, by contrast, wraps *everything* (API + assets). The
    // `TraceLayer` sits outermost so even rejected requests are traced —
    // with a token-scrubbed URI (see `make_request_span`).
    api.merge(assets::router::<()>())
        .layer(host_layer)
        .layer(TraceLayer::new_for_http().make_span_with(make_request_span::<Body>))
}

/// Build the tracing span for one request with the `token` query
/// parameter value redacted. The bootstrap token now travels in the URL
/// *fragment* (never sent to the server), but this is defense-in-depth:
/// a hand-typed or legacy `?token=…` URL must never be recorded verbatim
/// in a span/log where the secret would outlive the client-side scrub.
fn make_request_span<B>(req: &Request<B>) -> tracing::Span {
    tracing::info_span!(
        "http.request",
        method = %req.method(),
        uri = %scrub_token_from_uri(req.uri()),
        version = ?req.version(),
    )
}

/// Render `uri` as `path[?query]` with any `token` query-parameter value
/// replaced by `REDACTED`. Other parameters are preserved verbatim.
fn scrub_token_from_uri(uri: &axum::http::Uri) -> String {
    let path = uri.path();
    match uri.query() {
        None => path.to_string(),
        Some(query) => {
            let scrubbed = query
                .split('&')
                .map(|pair| match pair.split_once('=') {
                    Some((k, _)) if k.eq_ignore_ascii_case("token") => format!("{k}=REDACTED"),
                    _ => pair.to_string(),
                })
                .collect::<Vec<_>>()
                .join("&");
            format!("{path}?{scrubbed}")
        }
    }
}

/// Bind and serve the router until `shutdown` fires or the listener
/// closes. Returns `Ok(())` on graceful shutdown.
pub async fn serve(
    config: Arc<DashboardConfig>,
    store: SharedStore,
    pane_cache: Arc<PaneCache>,
    sessions: SharedSessionBackend,
    backends: Vec<SharedBackend>,
    runtime: DashboardRuntimeConfig,
    mut shutdown: broadcast::Receiver<()>,
) -> std::io::Result<()> {
    let state = AppState::new(store, config.clone(), pane_cache, sessions)
        .with_backends(backends)
        .with_activity_paths(runtime.activity_path, runtime.session_activity_path)
        .with_work_store_path(runtime.work_store_path)
        .with_stats_config(runtime.stats_config)
        .with_message_skills(runtime.message_skills)
        .with_fleet(runtime.fleet);
    let app = router(state);
    let listener = TcpListener::bind(config.bind).await?;
    let local = listener.local_addr()?;
    tracing::info!(addr = %local, "dashboard listening");
    log_access_url(&config, local);
    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            let _ = shutdown.recv().await;
        })
        .await
}

/// DNS-rebinding guard. A page on an attacker-controlled domain can,
/// after re-resolving that domain to `127.0.0.1`, issue requests to this
/// loopback server from the victim's browser — but the browser still
/// sends the attacker's domain in the `Host` header. Rejecting any
/// non-loopback `Host` closes that hole while leaving genuine
/// `localhost` / `127.0.0.1` access untouched.
///
/// Only enforced for loopback binds: an operator who deliberately binds
/// a public address (`allow_public`) is reached via a real hostname and
/// rebinding is moot there, so we let those requests through.
///
/// An *absent* `Host` header is allowed: the DNS-rebinding vector is a
/// browser, and browsers always send `Host`. Non-browser local clients
/// (the muxa CLI, tests) that omit it are not the threat this guards.
async fn host_guard_middleware(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    if state.config.bind.ip().is_loopback() {
        let host = req
            .headers()
            .get(header::HOST)
            .and_then(|h| h.to_str().ok());
        if !host_is_loopback(host, state.config.bind.ip()) {
            return Err(StatusCode::FORBIDDEN);
        }
    }
    Ok(next.run(req).await)
}

/// Is `host` (a raw `Host` header value, optionally `host:port` or
/// `[ipv6]:port`) a loopback name/literal, or the configured loopback
/// bind IP? Returns `true` for `None` — see [`host_guard_middleware`] for
/// why an absent header is not an attack vector.
fn host_is_loopback(host: Option<&str>, bind_ip: IpAddr) -> bool {
    let Some(raw) = host else {
        return true;
    };
    let host_only = strip_host_port(raw);
    if host_only.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host_only.parse::<IpAddr>() {
        Ok(ip) => ip.is_loopback() || ip == bind_ip,
        Err(_) => false,
    }
}

/// Split the host component out of a `Host` header value, dropping any
/// `:port` suffix and surrounding IPv6 brackets. `127.0.0.1:7878` →
/// `127.0.0.1`, `[::1]:7878` → `::1`, `localhost` → `localhost`.
fn strip_host_port(raw: &str) -> &str {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix('[') {
        // IPv6 literal: `[::1]` or `[::1]:port`.
        return rest.split(']').next().unwrap_or(rest);
    }
    // IPv4 or hostname: strip a single trailing `:port` if present.
    match raw.rsplit_once(':') {
        Some((h, _port)) => h,
        None => raw,
    }
}

/// Return the non-sensitive URL and whether authentication is enabled for the
/// startup log. The bearer token must never enter tracing fields: launchd
/// redirects info logs to a persistent file, turning a convenience URL into a
/// long-lived credential leak. `muxa init` persists and prints the one-time
/// fragment URL at setup time; daemon logs intentionally show only the base.
fn access_log_details(config: &DashboardConfig, local: SocketAddr) -> (String, bool) {
    let host = match local.ip() {
        IpAddr::V6(ip) => format!("[{ip}]"),
        IpAddr::V4(ip) => ip.to_string(),
    };
    let base = format!("http://{host}:{}/", local.port());
    (base, config.token.is_some())
}

fn log_access_url(config: &DashboardConfig, local: SocketAddr) {
    let (base, auth_enabled) = access_log_details(config, local);
    if auth_enabled {
        tracing::info!(
            url = %base,
            "dashboard ready (authentication enabled; token omitted from logs)"
        );
    } else {
        tracing::info!(url = %base, "dashboard ready (auth disabled via auth = \"none\")");
    }
}

/// Read authentication for `auth = "token"`. Public-read and none modes do
/// not install this layer on their GET/SSE router.
async fn read_auth_middleware(
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

/// Control authentication. This layer always wraps mutation routes. A
/// missing configured token means control is intentionally unavailable
/// (`auth = "none"`), never anonymously writable.
async fn write_auth_middleware(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(expected) = state.config.token.as_deref() else {
        return Err(StatusCode::FORBIDDEN);
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

#[derive(Debug, Serialize)]
struct AccessResponse {
    mode: &'static str,
    read_requires_token: bool,
    write_available: bool,
    write_authorized: bool,
    capabilities: AccessCapabilities,
}

#[derive(Debug, Serialize)]
struct AccessCapabilities {
    work_start: bool,
}

async fn access_handler(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let header_value = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok());
    let write_authorized = state
        .config
        .token
        .as_deref()
        .is_some_and(|expected| auth::check_bearer(header_value, expected));
    let mode = match state.config.auth {
        DashboardAuthMode::Token => "token",
        DashboardAuthMode::PublicRead => "public_read",
        DashboardAuthMode::None => "read_only",
    };
    Json(AccessResponse {
        mode,
        read_requires_token: matches!(state.config.auth, DashboardAuthMode::Token),
        write_available: state.config.token.is_some(),
        write_authorized,
        capabilities: AccessCapabilities {
            work_start: state.config.allow_work_start,
        },
    })
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

#[derive(Debug, Default, Deserialize)]
struct FleetQuery {
    selector: Option<String>,
}

async fn fleet_handler(State(state): State<AppState>, Query(query): Query<FleetQuery>) -> Response {
    let Some(fleet) = &state.fleet else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "fleet manager is not enabled" })),
        )
            .into_response();
    };
    let selector = match query
        .selector
        .as_deref()
        .map(str::parse::<LabelSelector>)
        .transpose()
    {
        Ok(selector) => selector,
        Err(error) => {
            return (StatusCode::BAD_REQUEST, Json(json!({ "error": error }))).into_response();
        }
    };
    Json(fleet.store.snapshot_selected(selector.as_ref()).await).into_response()
}

#[derive(Debug, Deserialize)]
struct FleetCommandRequest {
    operation: FleetOperation,
}

async fn fleet_command_handler(
    State(state): State<AppState>,
    Path(host): Path<String>,
    Json(request): Json<FleetCommandRequest>,
) -> Response {
    let Some(fleet) = &state.fleet else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({ "error": "fleet manager is not enabled" })),
        )
            .into_response();
    };
    match fleet
        .execute(host, request.operation, Duration::from_secs(20))
        .await
    {
        Ok(result) => Json(result).into_response(),
        Err(error) => (StatusCode::CONFLICT, Json(json!({ "error": error }))).into_response(),
    }
}

#[derive(Debug, Serialize)]
struct PanesResponse {
    panes: Vec<PaneSummary>,
    errors: Vec<ScanError>,
    #[serde(with = "time::serde::rfc3339")]
    fetched_at: OffsetDateTime,
}

async fn panes_handler(State(state): State<AppState>) -> impl IntoResponse {
    let result = state.refresh_pane_scan().await;
    Json(PanesResponse {
        panes: result.panes,
        errors: result.errors,
        fetched_at: result.fetched_at,
    })
}

async fn works_handler(State(state): State<AppState>) -> Json<WorkSnapshot> {
    let scan = state.refresh_pane_scan().await;
    let agents = state.store.snapshot().await;
    let mut work_store = state.work_store.lock().await;
    for pane in &scan.panes {
        if let Some((identity, item)) = work::external_item_for_pane(pane, scan.fetched_at) {
            if let Err(error) = work_store.upsert_external_item(identity, item).await {
                tracing::warn!(%error, "failed to persist discovered external work item");
            }
        }
    }
    let records = work_store.records();
    drop(work_store);
    Json(work::build_snapshot(
        &scan.panes,
        &agents,
        &records,
        scan.fetched_at,
    ))
}

#[derive(Debug, Serialize)]
struct WorkMetadataResponse {
    works: Vec<WorkRecord>,
}

async fn work_metadata_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(WorkMetadataResponse {
        works: state.work_store.lock().await.records(),
    })
}

#[derive(Debug, Deserialize)]
struct WorkMetadataPutRequest {
    #[serde(default)]
    identity: Option<WorkIdentity>,
    #[serde(default)]
    key: Option<ExecutionIdentity>,
    metadata: WorkMetadataPatch,
}

async fn work_metadata_put_handler(
    State(state): State<AppState>,
    Json(input): Json<WorkMetadataPutRequest>,
) -> Response {
    if let Err(error) = validate_work_selector(input.identity.as_ref(), input.key.as_ref()) {
        return control_error(StatusCode::BAD_REQUEST, error);
    }
    let metadata = match input.metadata.validate() {
        Ok(metadata) => metadata,
        Err(error) => return control_error(StatusCode::BAD_REQUEST, error),
    };
    let scan = state.refresh_pane_scan().await;
    let records = state.work_store.lock().await.records();
    let identity_exists = input.identity.as_ref().is_some_and(|identity| {
        records.iter().any(|record| &record.identity == identity)
            || scan
                .panes
                .iter()
                .any(|pane| work::work_identity_for_pane(pane).as_ref() == Some(identity))
    });
    let key_exists = input.key.as_ref().is_some_and(|key| {
        scan.panes
            .iter()
            .any(|pane| pane_matches_execution(pane, key))
    });
    if !identity_exists && !key_exists {
        return control_error(StatusCode::NOT_FOUND, "work item is no longer present");
    }
    let result = if let Some(identity) = input.identity {
        state
            .work_store
            .lock()
            .await
            .upsert_metadata(identity, metadata)
            .await
    } else if let Some(key) = input.key {
        state
            .work_store
            .lock()
            .await
            .upsert_legacy_metadata(key, metadata)
            .await
    } else {
        unreachable!("validated selector")
    };
    match result {
        Ok(record) => Json(json!({ "ok": true, "work": record })).into_response(),
        Err(error) => control_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("persist work metadata: {error}"),
        ),
    }
}

fn pane_socket_identity(pane: &PaneSummary) -> String {
    work::pane_socket_identity(pane)
}

fn pane_matches_execution(pane: &PaneSummary, key: &ExecutionIdentity) -> bool {
    pane.host == key.host
        && pane_socket_identity(pane) == key.socket
        && pane.session_id == key.session_id
        && pane.window_id == key.window_id
}

fn validate_work_selector(
    identity: Option<&WorkIdentity>,
    key: Option<&ExecutionIdentity>,
) -> Result<(), &'static str> {
    match (identity, key) {
        (None, None) => Err("work selector is missing"),
        (Some(_), Some(_)) => Err("provide exactly one work selector: identity or key"),
        _ => Ok(()),
    }
}

#[derive(Debug, Deserialize)]
struct WorkPromptRequest {
    #[serde(default)]
    identity: Option<WorkIdentity>,
    #[serde(default)]
    key: Option<ExecutionIdentity>,
    /// Literal text, kept for callers that compose their own prompt.
    #[serde(default)]
    text: String,
    /// Registered `[message.skills]` name, expanded ahead of `body` exactly
    /// as `muxa_call_peer` and `muxa work up` expand it.
    #[serde(default)]
    skill: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default = "default_true")]
    submit: bool,
}

#[derive(Debug, Deserialize)]
struct WorkAbortRequest {
    #[serde(default)]
    identity: Option<WorkIdentity>,
    #[serde(default)]
    key: Option<ExecutionIdentity>,
}

#[derive(Debug, Serialize)]
struct WorkControlResult {
    pane: String,
    sent: bool,
    submitted: bool,
}

/// Ceiling for one `muxa work up` run. Generous because resolving a ticket
/// spends a headless agent turn, which is minutes rather than milliseconds.
const WORK_UP_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Deserialize)]
struct WorkUpRequest {
    /// Stable Muxa Work id.
    work: String,
    /// Optional external issue key resolved by `muxa work up`.
    #[serde(default)]
    external: Option<String>,
    #[serde(default)]
    pipeline: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    skill: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    no_ticket: bool,
    #[serde(default)]
    dry_run: bool,
}

/// Stand a work item's pipeline up from the dashboard.
///
/// This delegates to the `muxa` binary rather than reimplementing the
/// pipeline here. The launcher and the managed-tmux registry live in the
/// CLI crate, which depends on this one — so calling them in-process would
/// mean either inverting that dependency or keeping a second implementation
/// alive. One implementation behind a subprocess beats two behind none.
///
/// Gated on `[dashboard] allow_work_start` on top of the control token:
/// every other write route steers a process the operator already started,
/// while this one starts new ones with permissions bypassed.
async fn work_up_handler(
    State(state): State<AppState>,
    Json(input): Json<WorkUpRequest>,
) -> Response {
    if !state.config.allow_work_start {
        // Not FORBIDDEN: the browser treats 401/403 as "your token is bad"
        // and drops into read-only. This deployment simply does not offer
        // the capability, which is what 501 says — and what the backend
        // capability checks above already use.
        return control_error(
            StatusCode::NOT_IMPLEMENTED,
            "starting work is disabled; set [dashboard] allow_work_start = true to enable it",
        );
    }
    if input.work.trim().is_empty() {
        return control_error(StatusCode::BAD_REQUEST, "work id is empty");
    }
    if input.no_ticket
        && input
            .external
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
    {
        return control_error(
            StatusCode::BAD_REQUEST,
            "external cannot be combined with no_ticket",
        );
    }

    let mut args: Vec<String> = vec![
        "work".into(),
        "up".into(),
        input.work.trim().into(),
        "--json".into(),
    ];
    let mut push = |flag: &str, value: Option<&String>| {
        if let Some(value) = value.map(|v| v.trim()).filter(|v| !v.is_empty()) {
            args.push(flag.to_string());
            args.push(value.to_string());
        }
    };
    push("--pipeline", input.pipeline.as_ref());
    push("--workspace", input.workspace.as_ref());
    push("--external", input.external.as_ref());
    push("--skill", input.skill.as_ref());
    push("--body", input.body.as_ref());
    push("--context", input.context.as_ref());
    // The dashboard's empty external field means a local Work. The CLI keeps
    // implicit Work-id lookup for compatibility, but the new UI never
    // silently turns a Work id into an external issue key.
    if input.no_ticket
        || input
            .external
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
    {
        args.push("--no-ticket".into());
    }
    if input.dry_run {
        args.push("--dry-run".into());
    }

    let result = match execute_work_up(args).await {
        Ok(result) => result,
        Err((status, error)) => return control_error(status, error),
    };
    let linked_external_item = if input.dry_run {
        false
    } else {
        persist_started_external_item(&state, &result).await
    };
    Json(json!({
        "ok": true,
        // Retained for clients of the first control-plane draft. Provider
        // titles now remain external references instead of becoming local
        // operator-authored Work metadata.
        "seeded_metadata": false,
        "linked_external_item": linked_external_item,
        "result": result
    }))
    .into_response()
}

async fn execute_work_up(args: Vec<String>) -> Result<serde_json::Value, (StatusCode, String)> {
    let mut command = tokio::process::Command::new("muxa");
    command
        .args(&args)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let output = match tokio::time::timeout(WORK_UP_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("spawning muxa: {error}"),
            ));
        }
        Err(_) => {
            return Err((
                StatusCode::GATEWAY_TIMEOUT,
                format!("muxa work up exceeded {}s", WORK_UP_TIMEOUT.as_secs()),
            ));
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim().lines().next_back().unwrap_or("no stderr");
        return Err((
            StatusCode::BAD_REQUEST,
            format!("muxa work up failed: {detail}"),
        ));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(stdout.trim()).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("muxa work up answered with unparseable JSON: {error}"),
        )
    })
}

/// Persist the external reference recorded on the newly created Run without
/// copying its title or provider status into the local Work definition.
async fn persist_started_external_item(state: &AppState, result: &serde_json::Value) -> bool {
    let (Some(workspace_id), Some(work_id)) = (
        result.get("workspace").and_then(serde_json::Value::as_str),
        result.get("work").and_then(serde_json::Value::as_str),
    ) else {
        return false;
    };
    let identity = WorkIdentity::new(workspace_id, work_id);
    let scan = state.refresh_pane_scan().await;
    let item = scan.panes.iter().find_map(|pane| {
        let (candidate, item) = work::external_item_for_pane(pane, scan.fetched_at)?;
        (candidate == identity).then_some(item)
    });
    let Some(item) = item else {
        return false;
    };
    state
        .work_store
        .lock()
        .await
        .upsert_external_item(identity, item)
        .await
        .is_ok()
}

async fn work_prompt_handler(
    State(state): State<AppState>,
    Json(input): Json<WorkPromptRequest>,
) -> Response {
    // `text` stays the escape hatch; skill/body/context go through the one
    // composer so a template registered once works from every front door.
    let composed = match crate::request::compose(
        crate::request::RequestParts {
            skill: input.skill.as_deref(),
            body: input.body.as_deref(),
            context: input.context.as_deref(),
        },
        &state.message_skills,
    ) {
        Ok(composed) => composed,
        Err(error) => return control_error(StatusCode::BAD_REQUEST, error.to_string()),
    };
    let text = match (composed, input.text.trim()) {
        (Some(composed), "") => composed.text,
        (Some(composed), literal) => format!("{}\n\n{literal}", composed.text),
        (None, "") => {
            return control_error(
                StatusCode::BAD_REQUEST,
                "prompt requires text, body, skill, or context",
            )
        }
        (None, literal) => literal.to_string(),
    };
    run_work_control(
        &state,
        input.identity,
        input.key,
        text,
        input.submit,
        "prompt",
    )
    .await
}

async fn work_abort_handler(
    State(state): State<AppState>,
    Json(input): Json<WorkAbortRequest>,
) -> Response {
    run_work_control(
        &state,
        input.identity,
        input.key,
        "\u{3}".to_string(),
        false,
        "abort",
    )
    .await
}

async fn run_work_control(
    state: &AppState,
    identity: Option<WorkIdentity>,
    key: Option<ExecutionIdentity>,
    text: String,
    submit: bool,
    action: &'static str,
) -> Response {
    if let Err(error) = validate_work_selector(identity.as_ref(), key.as_ref()) {
        return control_error(StatusCode::BAD_REQUEST, error);
    }
    let scan = state.refresh_pane_scan().await;
    let agents = state.store.snapshot().await;
    let records = state.work_store.lock().await.records();
    let legacy_binding = identity.as_ref().and_then(|identity| {
        records
            .iter()
            .find(|record| &record.identity == identity)
            .and_then(|record| record.legacy_binding.clone())
    });
    let candidates = scan
        .panes
        .iter()
        .filter(|pane| {
            identity.as_ref().is_some_and(|identity| {
                work::work_identity_for_pane(pane).as_ref() == Some(identity)
                    || legacy_binding
                        .as_ref()
                        .is_some_and(|binding| pane_matches_execution(pane, binding))
            }) || key
                .as_ref()
                .is_some_and(|key| pane_matches_execution(pane, key))
        })
        .filter(|pane| pane.muxa.managed_agent || pane_has_live_agent(pane, &scan.panes, &agents))
        .filter_map(|pane| {
            let backend = state
                .backends
                .iter()
                .find(|backend| backend.kind() == pane.host)
                .filter(|backend| backend.caps().send_text)
                .cloned()?;
            Some((backend, pane_socket_identity(pane), pane.pane_id.clone()))
        })
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return control_error(
            StatusCode::NOT_FOUND,
            "work item has no live or managed agent panes",
        );
    }

    let results = tokio::task::spawn_blocking(move || {
        candidates
            .into_iter()
            .map(|(backend, socket, pane)| {
                let sent = backend.send_text_on(Some(&socket), &pane, &text);
                let submitted = if sent && submit {
                    std::thread::sleep(crate::backend::PROMPT_SUBMIT_GRACE);
                    backend.send_text_on(Some(&socket), &pane, "\r")
                } else {
                    false
                };
                WorkControlResult {
                    pane,
                    sent,
                    submitted,
                }
            })
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let succeeded = results.iter().filter(|result| result.sent).count();
    if succeeded == 0 {
        return control_error(
            StatusCode::BAD_GATEWAY,
            format!("{action} failed for every agent pane"),
        );
    }
    Json(json!({
        "ok": succeeded == results.len(),
        "action": action,
        "attempted": results.len(),
        "succeeded": succeeded,
        "results": results,
    }))
    .into_response()
}

fn pane_has_live_agent(pane: &PaneSummary, panes: &[PaneSummary], agents: &[Agent]) -> bool {
    let duplicates = panes
        .iter()
        .filter(|candidate| candidate.host == pane.host && candidate.pane_id == pane.pane_id)
        .count();
    agents.iter().any(|agent| {
        agent.state != AgentState::Stopped
            && agent.pane.as_deref() == Some(&pane.pane_id)
            && crate::backend::pane_id_host_kind(&pane.pane_id).is_none_or(|host| host == pane.host)
            && match agent.tmux_socket.as_deref() {
                Some(socket) => {
                    crate::backend::pane_endpoint_identity(Some(&pane.pane_id), socket)
                        == pane_socket_identity(pane)
                }
                None => duplicates == 1,
            }
    })
}

#[derive(Debug, Serialize)]
struct TerminalSessionsResponse {
    sessions: Vec<crate::session::SessionRef>,
}

async fn terminal_sessions_handler(State(state): State<AppState>) -> impl IntoResponse {
    Json(TerminalSessionsResponse {
        sessions: state.sessions.list_sessions(),
    })
}

async fn terminal_capture_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    match state.sessions.capture(&id) {
        Ok(snapshot) => Json(snapshot).into_response(),
        Err(e) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct PaneControlTarget {
    /// Optional tmux socket name/path. Required only when the same `%N` pane
    /// id exists on more than one tmux server.
    socket: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PanePromptRequest {
    text: String,
    #[serde(default = "default_true")]
    submit: bool,
    socket: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct TerminalInputRequest {
    data: String,
}

fn control_error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(json!({ "ok": false, "error": message.into() })),
    )
        .into_response()
}

struct ControlFailure {
    status: StatusCode,
    message: String,
}

impl ControlFailure {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn into_response(self) -> Response {
        control_error(self.status, self.message)
    }
}

fn control_backend(state: &AppState, pane: &str) -> Result<SharedBackend, ControlFailure> {
    let Some(primary) = state.backends.first() else {
        return Err(ControlFailure::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "no pane backend is available",
        ));
    };
    match crate::backend::pane_id_host_kind(pane) {
        Some(kind) => state
            .backends
            .iter()
            .find(|backend| backend.kind() == kind)
            .cloned()
            .ok_or_else(|| {
                ControlFailure::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("no active {kind} backend for pane {pane}"),
                )
            }),
        None => Ok(primary.clone()),
    }
}

fn socket_basename(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    std::path::Path::new(trimmed)
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_string)
}

async fn control_socket(
    state: &AppState,
    backend: &SharedBackend,
    pane: &str,
    requested: Option<&str>,
) -> Result<Option<String>, ControlFailure> {
    if backend.kind() == HostKind::Rmux {
        let mut candidates = state
            .store
            .by_pane(pane)
            .await
            .into_iter()
            .filter_map(|agent| agent.tmux_socket)
            .map(|endpoint| crate::backend::pane_endpoint_identity(Some(pane), &endpoint))
            .collect::<Vec<_>>();
        let scan = state.refresh_pane_scan().await;
        candidates.extend(
            scan.panes
                .into_iter()
                .filter(|candidate| candidate.pane_id == pane)
                .filter_map(|candidate| candidate.socket.to_str().map(str::to_string)),
        );
        candidates.sort();
        candidates.dedup();

        if let Some(requested) = requested {
            let requested = crate::backend::pane_endpoint_identity(Some(pane), requested);
            if candidates.iter().any(|candidate| candidate == &requested) {
                return Ok(Some(requested));
            }
            return Err(ControlFailure::new(
                StatusCode::BAD_REQUEST,
                format!("pane {pane} is not present on rmux endpoint {requested}"),
            ));
        }
        return match candidates.as_slice() {
            [] => Ok(None),
            [endpoint] => Ok(Some(endpoint.clone())),
            _ => Err(ControlFailure::new(
                StatusCode::CONFLICT,
                format!("pane {pane} exists on multiple rmux endpoints; specify socket"),
            )),
        };
    }
    if backend.kind() != HostKind::Tmux {
        return Ok(None);
    }

    let mut candidates = state
        .store
        .by_pane(pane)
        .await
        .into_iter()
        .filter_map(|agent| agent.tmux_socket)
        .filter_map(|socket| socket_basename(&socket))
        .collect::<Vec<_>>();
    let scan = state.refresh_pane_scan().await;
    candidates.extend(
        scan.panes
            .into_iter()
            .filter(|candidate| candidate.pane_id == pane)
            .filter_map(|candidate| {
                candidate
                    .socket
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_string)
            }),
    );
    candidates.sort();
    candidates.dedup();

    if let Some(requested) = requested.and_then(socket_basename) {
        if candidates.iter().any(|candidate| candidate == &requested) {
            return Ok(Some(requested));
        }
        return Err(ControlFailure::new(
            StatusCode::BAD_REQUEST,
            format!("pane {pane} is not present on tmux socket {requested}"),
        ));
    }

    match candidates.as_slice() {
        [] => Ok(None),
        [only] => Ok(Some(only.clone())),
        _ => Err(ControlFailure::new(
            StatusCode::CONFLICT,
            format!("pane {pane} exists on multiple tmux sockets; specify socket"),
        )),
    }
}

async fn pane_prompt_handler(
    State(state): State<AppState>,
    Path(pane): Path<String>,
    Json(input): Json<PanePromptRequest>,
) -> Response {
    if input.text.trim().is_empty() {
        return control_error(StatusCode::BAD_REQUEST, "prompt text is empty");
    }
    let backend = match control_backend(&state, &pane) {
        Ok(backend) if backend.caps().send_text => backend,
        Ok(backend) => {
            return control_error(
                StatusCode::NOT_IMPLEMENTED,
                format!("{} backend does not support pane input", backend.kind()),
            );
        }
        Err(failure) => return failure.into_response(),
    };
    let socket = match control_socket(&state, &backend, &pane, input.socket.as_deref()).await {
        Ok(socket) => socket,
        Err(failure) => return failure.into_response(),
    };
    let text = input.text;
    let submit = input.submit;
    let pane_for_send = pane.clone();
    let (sent, submitted) = tokio::task::spawn_blocking(move || {
        let socket = socket.as_deref();
        let sent = backend.send_text_on(socket, &pane_for_send, &text);
        let submitted = if sent && submit {
            std::thread::sleep(crate::backend::PROMPT_SUBMIT_GRACE);
            backend.send_text_on(socket, &pane_for_send, "\r")
        } else {
            false
        };
        (sent, submitted)
    })
    .await
    .unwrap_or((false, false));

    if !sent {
        return control_error(
            StatusCode::BAD_GATEWAY,
            "pane input failed: pane gone or backend unreachable",
        );
    }
    Json(json!({
        "ok": true,
        "pane": pane,
        "sent": sent,
        "submitted": submitted,
    }))
    .into_response()
}

async fn pane_abort_handler(
    State(state): State<AppState>,
    Path(pane): Path<String>,
    Json(input): Json<PaneControlTarget>,
) -> Response {
    let backend = match control_backend(&state, &pane) {
        Ok(backend) if backend.caps().send_text => backend,
        Ok(backend) => {
            return control_error(
                StatusCode::NOT_IMPLEMENTED,
                format!("{} backend does not support pane input", backend.kind()),
            );
        }
        Err(failure) => return failure.into_response(),
    };
    let socket = match control_socket(&state, &backend, &pane, input.socket.as_deref()).await {
        Ok(socket) => socket,
        Err(failure) => return failure.into_response(),
    };
    let pane_for_send = pane.clone();
    let sent = tokio::task::spawn_blocking(move || {
        backend.send_text_on(socket.as_deref(), &pane_for_send, "\u{3}")
    })
    .await
    .unwrap_or(false);
    if !sent {
        return control_error(
            StatusCode::BAD_GATEWAY,
            "abort failed: pane gone or backend unreachable",
        );
    }
    Json(json!({ "ok": true, "pane": pane, "aborted": true })).into_response()
}

async fn terminal_input_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<TerminalInputRequest>,
) -> Response {
    match state.sessions.send_input(&id, input.data.as_bytes()) {
        Ok(()) => Json(json!({ "ok": true, "session_id": id })).into_response(),
        Err(crate::session::SessionError::NotFound(_)) => {
            control_error(StatusCode::NOT_FOUND, format!("session not found: {id}"))
        }
        Err(crate::session::SessionError::Exited(_)) => control_error(
            StatusCode::CONFLICT,
            format!("session already exited: {id}"),
        ),
        Err(error) => control_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

async fn terminal_terminate_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    match state.sessions.terminate(&id) {
        Ok(()) => Json(json!({ "ok": true, "session_id": id })).into_response(),
        Err(crate::session::SessionError::NotFound(_)) => {
            control_error(StatusCode::NOT_FOUND, format!("session not found: {id}"))
        }
        Err(crate::session::SessionError::Exited(_)) => control_error(
            StatusCode::CONFLICT,
            format!("session already exited: {id}"),
        ),
        Err(error) => control_error(StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct TimelineQuery {
    /// Same grammar as the CLI: today, yesterday, week, month, last-week, last-month, 24h, 7d, RFC3339, all.
    since: Option<String>,
    /// tmux session name, tmux session id, or pane id.
    session: Option<String>,
    /// Comma-separated pane id glob exclusions. `exclude-pane` is accepted too.
    #[serde(alias = "exclude-pane")]
    exclude_pane: Option<String>,
    /// Comma-separated tmux session name/id glob exclusions. `exclude-session` is accepted too.
    #[serde(alias = "exclude-session")]
    exclude_session: Option<String>,
    /// Agent kind in `snake_case`.
    agent: Option<String>,
    /// `detail` preserves the original response. `summary` omits raw intervals
    /// and returns compact all-session aggregates for the dashboard.
    view: Option<TimelineView>,
    /// Browser-local UTC offset used to group summary days. For example, Seoul
    /// sends `540` and New York standard time sends `-300`.
    timezone_offset_minutes: Option<i32>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum TimelineView {
    Detail,
    Summary,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct TimelineSummaryCacheKey {
    since: String,
    session: Option<String>,
    exclude_pane: Option<String>,
    exclude_session: Option<String>,
    agent: Option<String>,
    timezone_offset_minutes: i32,
}

impl TimelineSummaryCacheKey {
    fn from_query(query: &TimelineQuery) -> Self {
        Self {
            since: query.since.clone().unwrap_or_else(|| "24h".to_string()),
            session: query.session.clone(),
            exclude_pane: query.exclude_pane.clone(),
            exclude_session: query.exclude_session.clone(),
            agent: query.agent.clone(),
            timezone_offset_minutes: query.timezone_offset_minutes.unwrap_or(0),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
struct TimelineSummaryResponse {
    #[serde(with = "time::serde::rfc3339")]
    generated_at: OffsetDateTime,
    range: TimelineRange,
    #[serde(with = "time::serde::rfc3339")]
    window_started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    window_ended_at: OffsetDateTime,
    lanes: Vec<TimelineLane>,
    totals: TimelineTotals,
    active_sessions: Vec<timeline::TimelineActiveSession>,
    notes: Vec<String>,
    summary: TimelineSummary,
}

#[derive(Debug, Clone, Serialize)]
struct TimelineSummary {
    version: u8,
    sessions: Vec<TimelineSessionSummary>,
    days: Vec<TimelineDaySummary>,
    sources: Vec<TimelineSourceSummary>,
    human_presence_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
struct TimelineSessionSummary {
    label: String,
    lanes: usize,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    latest_at: Option<OffsetDateTime>,
    totals: TimelineTotals,
    human_presence_secs: u64,
}

#[derive(Debug, Clone, Serialize)]
struct TimelineDaySummary {
    date: String,
    totals: TimelineTotals,
    top_sessions: Vec<timeline::TimelineActiveSession>,
}

#[derive(Debug, Clone, Serialize)]
struct TimelineSourceSummary {
    kind: TimelineLaneKind,
    lanes: usize,
    sessions: usize,
    totals: TimelineTotals,
}

enum BuiltTimeline {
    Detail(TimelineDocument),
    Summary(TimelineSummaryResponse),
}

struct TimelineProjectionInput {
    now: OffsetDateTime,
    range: TimelineRange,
    prompt_entries: Vec<crate::history::HistoryEntry>,
    activity_entries: Arc<Vec<crate::activity::ActivityEntry>>,
    agents: Vec<Agent>,
    session_activities: Vec<crate::session_activity::SessionActivity>,
    pane_sessions: HashMap<String, String>,
    stats_config: StatsConfig,
    filters: TimelineFilters,
    notes: Vec<String>,
    wants_summary: bool,
    timezone_offset: UtcOffset,
}

async fn timeline_handler(
    State(state): State<AppState>,
    Query(query): Query<TimelineQuery>,
) -> Response {
    let summary_cache_key = (query.view == Some(TimelineView::Summary))
        .then(|| TimelineSummaryCacheKey::from_query(&query));
    if let Some(cache_key) = summary_cache_key.as_ref() {
        if let Some(response) = cached_timeline_summary(&state, cache_key).await {
            return Json(response).into_response();
        }
    }

    let now = OffsetDateTime::now_utc();
    let since = query.since.as_deref().unwrap_or("24h");
    let range = match timeline::parse_since(since, now, "all retained activity") {
        Ok(range) => range,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "ok": false, "error": error })),
            )
                .into_response();
        }
    };
    let agent_kind = match query.agent.as_deref().map(parse_agent_kind).transpose() {
        Ok(kind) => kind,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "ok": false, "error": error })),
            )
                .into_response();
        }
    };

    let timezone_offset = match timeline_timezone_offset(query.timezone_offset_minutes) {
        Ok(offset) => offset,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "ok": false, "error": error })),
            )
                .into_response();
        }
    };

    let mut notes = Vec::new();
    let activity_entries = load_cached_activity_entries(&state, &mut notes).await;
    let session_activities = match state.session_activity_path.as_ref() {
        Some(path) => crate::session_activity::load(path).await,
        None => Vec::new(),
    };
    let agents = state.store.snapshot().await;
    let prompt_entries = state.store.recent_prompts(None, 0).await;
    let pane_scan = state.refresh_pane_scan().await;
    let pane_sessions = pane_scan
        .panes
        .iter()
        .map(|pane| (pane.pane_id.clone(), pane.session.clone()))
        .collect::<HashMap<_, _>>();

    let built = match build_timeline_projection(TimelineProjectionInput {
        now,
        range,
        prompt_entries,
        activity_entries,
        agents,
        session_activities,
        pane_sessions,
        stats_config: state.stats_config.clone(),
        filters: TimelineFilters {
            session: query.session,
            agent_kind,
            exclusions: ScopeExclusions::new(
                split_query_patterns(query.exclude_pane),
                split_query_patterns(query.exclude_session),
            ),
        },
        notes,
        wants_summary: summary_cache_key.is_some(),
        timezone_offset,
    })
    .await
    {
        Ok(built) => built,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    json!({ "ok": false, "error": format!("timeline projection failed: {error}") }),
                ),
            )
                .into_response();
        }
    };

    match built {
        BuiltTimeline::Summary(response) => {
            if let Some(cache_key) = summary_cache_key {
                store_timeline_summary(&state, cache_key, response.clone()).await;
            }
            Json(response).into_response()
        }
        BuiltTimeline::Detail(document) => Json(document).into_response(),
    }
}

async fn build_timeline_projection(
    projection: TimelineProjectionInput,
) -> Result<BuiltTimeline, tokio::task::JoinError> {
    tokio::task::spawn_blocking(move || {
        let input = TimelineBuildInput {
            now: projection.now,
            range: projection.range,
            prompt_entries: &projection.prompt_entries,
            activity_entries: projection.activity_entries.as_slice(),
            agents: &projection.agents,
            session_activities: &projection.session_activities,
            pane_sessions: &projection.pane_sessions,
            active_lookback_secs: projection.stats_config.active_lookback_secs,
            active_timeout_secs: projection.stats_config.active_timeout_secs,
            active_tick_timeout_secs: projection.stats_config.active_tick_timeout_secs,
            count_tmux_input: projection.stats_config.count_tmux_input,
            filters: projection.filters,
            notes: projection.notes,
        };
        if projection.wants_summary {
            BuiltTimeline::Summary(build_timeline_summary_response(timeline::build_summary(
                input,
                projection.timezone_offset,
            )))
        } else {
            BuiltTimeline::Detail(timeline::build_document(input))
        }
    })
    .await
}

async fn cached_timeline_summary(
    state: &AppState,
    cache_key: &TimelineSummaryCacheKey,
) -> Option<TimelineSummaryResponse> {
    let mut cache = state.timeline_summary_cache.lock().await;
    cache
        .values
        .retain(|_, (cached_at, _)| cached_at.elapsed() < TIMELINE_SUMMARY_CACHE_TTL);
    cache
        .values
        .get(cache_key)
        .map(|(_, response)| response.clone())
}

async fn store_timeline_summary(
    state: &AppState,
    cache_key: TimelineSummaryCacheKey,
    response: TimelineSummaryResponse,
) {
    let mut cache = state.timeline_summary_cache.lock().await;
    cache
        .values
        .retain(|_, (cached_at, _)| cached_at.elapsed() < TIMELINE_SUMMARY_CACHE_TTL);
    if cache.values.len() >= TIMELINE_SUMMARY_CACHE_CAPACITY
        && !cache.values.contains_key(&cache_key)
    {
        let oldest = cache
            .values
            .iter()
            .min_by_key(|(_, (cached_at, _))| *cached_at)
            .map(|(key, _)| key.clone());
        if let Some(oldest) = oldest {
            cache.values.remove(&oldest);
        }
    }
    cache.values.insert(cache_key, (Instant::now(), response));
}

async fn load_cached_activity_entries(
    state: &AppState,
    notes: &mut Vec<String>,
) -> Arc<Vec<crate::activity::ActivityEntry>> {
    let Some(path) = state.activity_path.as_ref() else {
        notes.push("activity ledger is not available to the dashboard".to_string());
        return Arc::new(Vec::new());
    };

    let mut cache = state.activity_ledger_cache.lock().await;
    match cache.refresh(path).await {
        Ok(entries) => entries,
        Err(error) => {
            notes.push(format!(
                "could not refresh activity ledger; using cached data: {error}"
            ));
            cache.snapshot()
        }
    }
}

fn timeline_timezone_offset(minutes: Option<i32>) -> Result<UtcOffset, String> {
    let minutes = minutes.unwrap_or(0);
    let seconds = minutes
        .checked_mul(60)
        .ok_or_else(|| format!("invalid timezone offset {minutes}"))?;
    UtcOffset::from_whole_seconds(seconds)
        .map_err(|_| format!("invalid timezone offset {minutes}; expected minutes from UTC"))
}

fn build_timeline_summary_response(
    projection: timeline::TimelineSummaryProjection,
) -> TimelineSummaryResponse {
    let summary = TimelineSummary {
        version: 1,
        sessions: projection
            .sessions
            .into_iter()
            .map(|session| TimelineSessionSummary {
                label: session.label,
                lanes: session.lanes,
                latest_at: session.latest_at,
                totals: session.totals,
                human_presence_secs: session.human_presence_secs,
            })
            .collect(),
        days: projection
            .days
            .into_iter()
            .map(|day| TimelineDaySummary {
                date: day.date,
                totals: day.totals,
                top_sessions: day.top_sessions,
            })
            .collect(),
        sources: projection
            .sources
            .into_iter()
            .map(|source| TimelineSourceSummary {
                kind: source.kind,
                lanes: source.lanes,
                sessions: source.sessions,
                totals: source.totals,
            })
            .collect(),
        human_presence_secs: projection.human_presence_secs,
    };
    TimelineSummaryResponse {
        generated_at: projection.generated_at,
        range: projection.range,
        window_started_at: projection.window_started_at,
        window_ended_at: projection.window_ended_at,
        lanes: Vec::new(),
        totals: projection.totals,
        active_sessions: projection.active_sessions,
        notes: projection.notes,
        summary,
    }
}

fn split_query_patterns(raw: Option<String>) -> Vec<String> {
    raw.into_iter()
        .flat_map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|pattern| !pattern.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .collect()
}

fn parse_agent_kind(raw: &str) -> Result<AgentKind, String> {
    match raw {
        "claude_code" => Ok(AgentKind::ClaudeCode),
        "codex" => Ok(AgentKind::Codex),
        "gemini_cli" => Ok(AgentKind::GeminiCli),
        "antigravity" => Ok(AgentKind::Antigravity),
        "opencode" => Ok(AgentKind::Opencode),
        "unknown" => Ok(AgentKind::Unknown),
        _ => Err(format!("unknown agent kind {raw:?}")),
    }
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
    // enum has seven variants.
    for s in [
        AgentState::Starting,
        AgentState::Working,
        AgentState::Idle,
        AgentState::WaitingInput,
        AgentState::WaitingChoice,
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
    use crate::backend::{BackendCaps, PaneBackend};
    use crate::event::{AgentEvent, AgentId, AgentKind};
    use crate::state::Store;
    use crate::tmux::PaneInfo;
    use axum::body::to_bytes;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use serde_json::Value;
    use std::collections::{BTreeMap, HashMap};
    use std::sync::Mutex;
    use tower::ServiceExt;

    struct RecordingControlBackend {
        sent: Arc<Mutex<Vec<(String, String)>>>,
        panes: Vec<PaneInfo>,
    }

    struct InventoryBackend {
        kind: HostKind,
        panes: Vec<PaneInfo>,
    }

    impl PaneBackend for InventoryBackend {
        fn kind(&self) -> HostKind {
            self.kind
        }

        fn list_panes(&self) -> Vec<PaneInfo> {
            self.panes.clone()
        }

        fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo> {
            self.panes
                .iter()
                .find(|pane| pane.pane_id == pane_id)
                .cloned()
        }

        fn capture_pane(&self, _pane_id: &str) -> Option<String> {
            None
        }

        fn pane_pid_map(&self) -> HashMap<u32, String> {
            HashMap::new()
        }

        fn current_pane(&self) -> Option<String> {
            None
        }

        fn focus_pane(&self, _pane_id: &str) -> bool {
            false
        }

        fn send_text(&self, _pane_id: &str, _text: &str) -> bool {
            false
        }
    }

    impl PaneBackend for RecordingControlBackend {
        fn kind(&self) -> HostKind {
            HostKind::Herdr
        }

        fn list_panes(&self) -> Vec<PaneInfo> {
            self.panes.clone()
        }

        fn resolve_pane(&self, pane_id: &str) -> Option<PaneInfo> {
            self.panes
                .iter()
                .find(|pane| pane.pane_id == pane_id)
                .cloned()
        }

        fn capture_pane(&self, _pane_id: &str) -> Option<String> {
            None
        }

        fn pane_pid_map(&self) -> HashMap<u32, String> {
            HashMap::new()
        }

        fn current_pane(&self) -> Option<String> {
            None
        }

        fn focus_pane(&self, _pane_id: &str) -> bool {
            false
        }

        fn send_text(&self, pane_id: &str, text: &str) -> bool {
            self.sent
                .lock()
                .unwrap()
                .push((pane_id.to_string(), text.to_string()));
            true
        }

        fn caps(&self) -> BackendCaps {
            BackendCaps::default()
        }
    }

    fn fresh_state() -> AppState {
        AppState::new(
            Store::shared(),
            Arc::new(DashboardConfig::loopback_default()),
            Arc::new(PaneCache::new(Duration::from_secs(60))),
            crate::session::PtySessionBackend::shared(),
        )
    }

    fn state_with_token(token: &str) -> AppState {
        let mut cfg = DashboardConfig::loopback_default();
        cfg.token = Some(token.to_string());
        AppState::new(
            Store::shared(),
            Arc::new(cfg),
            Arc::new(PaneCache::new(Duration::from_secs(60))),
            crate::session::PtySessionBackend::shared(),
        )
    }

    fn public_read_state(token: &str) -> AppState {
        let mut cfg = DashboardConfig::loopback_default();
        cfg.auth = DashboardAuthMode::PublicRead;
        cfg.token = Some(token.to_string());
        state_from(cfg)
    }

    fn test_pane(
        pane_id: &str,
        session_id: &str,
        session: &str,
        window_id: &str,
        window_name: &str,
    ) -> PaneInfo {
        PaneInfo {
            agent_role: None,
            agent_alias: None,
            socket: None,
            pane_id: pane_id.into(),
            session_id: session_id.into(),
            session: session.into(),
            window_id: window_id.into(),
            window_name: window_name.into(),
            window_index: "0".into(),
            pane_index: "0".into(),
            tty: String::new(),
            current_command: "codex".into(),
            title: String::new(),
            pane_pid: 0,
            current_path: "/work/muxa".into(),
        }
    }

    async fn seed_agent(state: &AppState, session_id: &str, pane: &str) {
        state
            .store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    tmux_socket: None,
                    kind: AgentKind::Codex,
                    session_id: session_id.into(),
                    surface: None,
                    pane: Some(pane.into()),
                    cwd: None,
                },
                at: OffsetDateTime::now_utc(),
            })
            .await;
    }

    fn state_from(cfg: DashboardConfig) -> AppState {
        AppState::new(
            Store::shared(),
            Arc::new(cfg),
            Arc::new(PaneCache::new(Duration::from_secs(60))),
            crate::session::PtySessionBackend::shared(),
        )
    }

    /// Resolve a config the way the daemon does and return the `(state,
    /// token)` pair so tests exercise the real config path.
    fn resolved_state(toml: crate::config::DashboardTomlConfig) -> (AppState, Option<String>) {
        let cfg = DashboardConfig::resolve(&toml, &crate::dashboard::DashboardOverrides::default())
            .unwrap();
        let token = cfg.token.clone();
        (state_from(cfg), token)
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

    fn fleet_host(alias: &str, environment: &str) -> crate::fleet::FleetHostSnapshot {
        crate::fleet::FleetHostSnapshot {
            alias: alias.into(),
            local: false,
            ssh_target: alias.into(),
            labels: BTreeMap::from([("environment".into(), environment.into())]),
            annotations: BTreeMap::new(),
            mode: crate::fleet::HostAccessMode::Observe,
            state: crate::fleet::FleetHostState::Online,
            node_id: None,
            hostname: None,
            os: None,
            arch: None,
            muxa_version: None,
            protocol: None,
            capabilities: Vec::new(),
            daemon_generation: None,
            boot_id: None,
            latency_ms: None,
            last_seen_at: None,
            received_at: None,
            error: None,
            remote: None,
        }
    }

    #[tokio::test]
    async fn fleet_endpoint_reads_cache_and_applies_label_selector() {
        let store = Arc::new(crate::fleet::FleetStore::new());
        store.upsert_host(fleet_host("dev", "development")).await;
        store.upsert_host(fleet_host("prod", "production")).await;
        let (runtime, _commands) = FleetRuntime::new(store);
        let app = router(fresh_state().with_fleet(Some(runtime)));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/fleet?selector=environment%3Dproduction")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["hosts"].as_array().unwrap().len(), 1);
        assert_eq!(body["hosts"][0]["alias"], "prod");
    }

    #[tokio::test]
    async fn fleet_command_endpoint_is_pat_gated_and_dispatches() {
        let (runtime, mut commands) = FleetRuntime::new(Arc::new(crate::fleet::FleetStore::new()));
        let app = router(state_with_token("secret").with_fleet(Some(runtime)));
        let unauthorized = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/fleet/dev/command")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"operation":{"kind":"refresh"}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

        let responder = tokio::spawn(async move {
            let command = commands.recv().await.unwrap();
            assert_eq!(command.host, "dev");
            assert!(matches!(command.operation, FleetOperation::Refresh));
            command
                .reply
                .send(Ok(crate::fleet::FleetCommandResult::accepted("refreshed")))
                .unwrap();
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/fleet/dev/command")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, "Bearer secret")
                    .body(Body::from(r#"{"operation":{"kind":"refresh"}}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await["message"], "refreshed");
        responder.await.unwrap();
    }

    #[test]
    fn dashboard_access_log_never_contains_bearer_token() {
        let secret = "do-not-log-this-token";
        let mut config = DashboardConfig::loopback_default();
        config.token = Some(secret.into());
        let local: SocketAddr = "127.0.0.1:7878".parse().unwrap();

        let (url, auth_enabled) = access_log_details(&config, local);

        assert!(auth_enabled);
        assert_eq!(url, "http://127.0.0.1:7878/");
        assert!(!url.contains(secret));
        assert!(!url.contains("#token="));
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
                        tmux_socket: None,
                        kind,
                        session_id: sid.into(),
                        surface: None,
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

    /// On a herdr host `/api/panes` must source rows from the herdr backend,
    /// not the (empty-on-herdr) tmux scanner. We stand up a minimal herdr
    /// mock socket, point a real `HerdrBackend` at it, and assert the pane
    /// surfaces with the herdr synthetic socket identity.
    #[tokio::test]
    async fn panes_endpoint_sources_from_herdr_backend_when_host_is_herdr() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("herdr.sock");
        let listener = UnixListener::bind(&socket_path).unwrap();
        // Answer pane.list (+ per-pane pane.process_info) on a detached
        // accept loop; the same newline-JSON shape herdr.rs tests use.
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() || line.is_empty() {
                    continue;
                }
                let req: Value = serde_json::from_str(&line).unwrap();
                let id = req.get("id").and_then(Value::as_str).unwrap_or("");
                let method = req.get("method").and_then(Value::as_str).unwrap_or("");
                let result = match method {
                    "pane.list" => json!({
                        "type": "pane_list",
                        "panes": [{
                            "pane_id": "p1", "terminal_id": "t1",
                            "workspace_id": "ws1", "tab_id": "tab1",
                            "focused": true, "agent_status": "idle", "revision": 1,
                            "cwd": "/home/u/proj", "title": "editor",
                            "terminal_title": "raw",
                        }],
                    }),
                    "pane.process_info" => json!({
                        "type": "pane_process_info",
                        "process_info": {
                            "pane_id": "p1", "shell_pid": 4242, "tty": "/dev/pts/3",
                            "foreground_processes": [
                                { "pid": 4242, "name": "zsh" },
                                { "pid": 5001, "name": "vim" },
                            ],
                        },
                    }),
                    _ => json!({ "type": "ok" }),
                };
                let mut out =
                    serde_json::to_string(&json!({ "id": id, "result": result })).unwrap();
                out.push('\n');
                let _ = stream.write_all(out.as_bytes());
            }
        });

        let backend: SharedBackend = Arc::new(
            crate::backend::herdr::HerdrBackend::with_socket_path(socket_path),
        );
        let state = fresh_state().with_backend(backend);
        let app = router(state);
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
        let panes = v["panes"].as_array().expect("panes array");
        // The herdr branch now merges the tmux scan too, so any stray tmux
        // server on the test host may add panes — find the herdr pane rather
        // than asserting an exact count.
        let herdr = panes
            .iter()
            .find(|p| p["pane_id"] == "herdr:p1")
            .unwrap_or_else(|| panic!("herdr pane must surface: {v:?}"));
        assert_eq!(herdr["session"], "ws1");
        assert_eq!(herdr["window_index"], "tab1");
        assert_eq!(herdr["socket"], "herdr");
        assert_eq!(herdr["current_command"], "vim");
    }

    /// The herdr pane-scan path merges rather than replaces: a herdr backend's
    /// panes are appended onto whatever the tmux scan returned, and the tmux
    /// side's per-socket errors survive. Exercised on the pure merge helper so
    /// the tmux side can be fabricated (a real tmux server isn't needed).
    #[test]
    fn merge_pane_scans_appends_herdr_onto_tmux_and_keeps_errors() {
        use crate::tmux::scanner::{PaneSummary, ScanError, ScanResult};
        use std::path::PathBuf;

        let tmux_pane = PaneSummary {
            host: HostKind::Tmux,
            pane_id: "%1".into(),
            session_id: "$1".into(),
            session: "main".into(),
            window_id: "@1".into(),
            window_name: "main".into(),
            window_index: "0".into(),
            pane_index: "0".into(),
            tty: String::new(),
            current_command: "zsh".into(),
            title: String::new(),
            current_path: "/tmp/project".into(),
            socket: PathBuf::from("default"),
            muxa: MuxaPaneMetadata::default(),
            attach_command: "tmux attach".into(),
        };
        let tmux = ScanResult {
            panes: vec![tmux_pane],
            errors: vec![ScanError {
                socket: PathBuf::from("wedged"),
                message: "timed out".into(),
            }],
            fetched_at: OffsetDateTime::now_utc(),
        };
        let herdr = scanner::herdr_scan_result(vec![crate::tmux::PaneInfo {
            agent_role: None,
            agent_alias: None,
            socket: None,
            pane_id: "herdr:p1".into(),
            session_id: "ws1".into(),
            session: "ws1".into(),
            window_id: "tab1".into(),
            window_name: "tab1".into(),
            window_index: "tab1".into(),
            pane_index: "p1".into(),
            tty: String::new(),
            current_command: "vim".into(),
            title: String::new(),
            pane_pid: 0,
            current_path: String::new(),
        }]);

        let merged = merge_pane_scans(tmux, herdr);
        let ids: Vec<&str> = merged.panes.iter().map(|p| p.pane_id.as_str()).collect();
        assert_eq!(ids, ["%1", "herdr:p1"], "tmux first, herdr appended");
        assert_eq!(merged.errors.len(), 1, "tmux scan errors preserved");
        assert_eq!(merged.errors[0].socket, PathBuf::from("wedged"));
    }

    #[test]
    fn rmux_dashboard_rows_preserve_full_endpoint() {
        let scan = backend_scan_result(
            HostKind::Rmux,
            vec![crate::tmux::PaneInfo {
                agent_role: None,
                agent_alias: None,
                socket: Some("/tmp/rmux-user/default".into()),
                pane_id: "rmux:%4".into(),
                session_id: "$1".into(),
                session: "work".into(),
                window_id: "@2".into(),
                window_name: "editor".into(),
                window_index: "0".into(),
                pane_index: "0".into(),
                tty: "/dev/pts/4".into(),
                current_command: "zsh".into(),
                title: "work".into(),
                pane_pid: 42,
                current_path: "/tmp/project".into(),
            }],
        );
        assert_eq!(scan.panes.len(), 1);
        assert_eq!(scan.panes[0].pane_id, "rmux:%4");
        assert_eq!(
            scan.panes[0].socket,
            PathBuf::from("/tmp/rmux-user/default")
        );
        assert!(scan.panes[0].attach_command.is_empty());
    }

    #[tokio::test]
    async fn dashboard_scan_includes_non_primary_rmux_backend() {
        let rmux: SharedBackend = Arc::new(InventoryBackend {
            kind: HostKind::Rmux,
            panes: vec![crate::tmux::PaneInfo {
                agent_role: None,
                agent_alias: None,
                socket: Some("/tmp/rmux-secondary/default".into()),
                pane_id: "rmux:%8".into(),
                session_id: "$2".into(),
                session: "secondary".into(),
                window_id: "@3".into(),
                window_name: "shell".into(),
                window_index: "0".into(),
                pane_index: "0".into(),
                tty: "/dev/pts/8".into(),
                current_command: "zsh".into(),
                title: "secondary".into(),
                pane_pid: 88,
                current_path: "/tmp/project".into(),
            }],
        });

        let scan = refresh_pane_scan(Arc::new(vec![rmux.clone()])).await;
        let pane = scan
            .panes
            .iter()
            .find(|pane| pane.pane_id == "rmux:%8")
            .expect("secondary rmux backend should contribute to dashboard scan");
        assert_eq!(pane.socket, PathBuf::from("/tmp/rmux-secondary/default"));

        let state = fresh_state().with_backend(rmux.clone());
        let endpoint = match control_socket(&state, &rmux, "rmux:%8", None).await {
            Ok(endpoint) => endpoint,
            Err(failure) => panic!(
                "untracked pane should resolve from the dashboard scan: {}",
                failure.message
            ),
        };
        assert_eq!(endpoint.as_deref(), Some("/tmp/rmux-secondary/default"));
    }

    #[tokio::test]
    async fn timeline_endpoint_returns_well_formed_json_shape() {
        let app = router(fresh_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/timeline?since=24h")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert!(v["generated_at"].is_string());
        assert!(v["window_started_at"].is_string());
        assert!(v["window_ended_at"].is_string());
        assert!(v["lanes"].is_array());
        assert!(v["active_sessions"].is_array());
        assert!(v["totals"]["active_secs"].is_number());
        assert!(v["notes"].is_array());
    }

    #[tokio::test]
    async fn timeline_summary_endpoint_omits_raw_lanes_and_returns_aggregates() {
        let app = router(fresh_state());
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/timeline?since=24h&view=summary&timezone_offset_minutes=540")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["lanes"].as_array().map(Vec::len), Some(0));
        assert_eq!(v["summary"]["version"], 1);
        assert!(v["summary"]["sessions"].is_array());
        assert!(v["summary"]["days"].is_array());
        assert_eq!(v["summary"]["sources"].as_array().map(Vec::len), Some(3));
        assert!(v["summary"]["human_presence_secs"].is_number());

        let other_range = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/timeline?since=7d&view=summary&timezone_offset_minutes=540")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(other_range.status(), StatusCode::OK);

        let cached = app
            .oneshot(
                Request::builder()
                    .uri("/api/timeline?since=24h&view=summary&timezone_offset_minutes=540")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(cached.status(), StatusCode::OK);
        let cached = body_json(cached).await;
        assert_eq!(cached["generated_at"], v["generated_at"]);
    }

    #[tokio::test]
    async fn timeline_summary_rejects_invalid_timezone_offset() {
        let app = router(fresh_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/timeline?view=summary&timezone_offset_minutes=99999")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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
        // Only auth = "none" leaves the token unset; such a state passes.
        let mut cfg = DashboardConfig::loopback_default();
        cfg.auth = crate::config::DashboardAuthMode::None;
        cfg.token = None;
        let app = router(state_from(cfg));
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

    #[tokio::test]
    async fn public_read_allows_anonymous_reads_and_reports_locked_control() {
        let app = router(public_read_state("edit-pat"));
        let agents = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/agents")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(agents.status(), StatusCode::OK);

        let access = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/access")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(access.status(), StatusCode::OK);
        let access = body_json(access).await;
        assert_eq!(access["mode"], "public_read");
        assert_eq!(access["read_requires_token"], false);
        assert_eq!(access["write_available"], true);
        assert_eq!(access["write_authorized"], false);

        let unlocked = app
            .oneshot(
                Request::builder()
                    .uri("/api/access")
                    .header(header::AUTHORIZATION, "Bearer edit-pat")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let unlocked = body_json(unlocked).await;
        assert_eq!(unlocked["write_authorized"], true);
    }

    #[tokio::test]
    async fn public_read_mutations_require_the_control_pat() {
        let app = router(public_read_state("edit-pat"));
        let locked = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/terminal-sessions/missing/terminate")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(locked.status(), StatusCode::UNAUTHORIZED);

        let unlocked = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/terminal-sessions/missing/terminate")
                    .header(header::AUTHORIZATION, "Bearer edit-pat")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unlocked.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn public_read_pat_can_send_and_submit_a_pane_prompt() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let backend: SharedBackend = Arc::new(RecordingControlBackend {
            sent: sent.clone(),
            panes: Vec::new(),
        });
        let app = router(public_read_state("edit-pat").with_backend(backend));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/panes/herdr%3Ap1/prompt")
                    .header(header::AUTHORIZATION, "Bearer edit-pat")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"text":"review this","submit":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            *sent.lock().unwrap(),
            vec![
                ("herdr:p1".to_string(), "review this".to_string()),
                ("herdr:p1".to_string(), "\r".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn work_metadata_requires_pat_and_persists_for_the_exact_work() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("dashboard-work.json");
        let backend: SharedBackend = Arc::new(InventoryBackend {
            kind: HostKind::Herdr,
            panes: vec![test_pane(
                "herdr:p1",
                "session-1",
                "muxa",
                "window-1",
                "CAL-101",
            )],
        });
        let state = public_read_state("edit-pat")
            .with_backend(backend)
            .with_work_store_path(Some(path.clone()));
        let app = router(state);
        let body = r#"{
            "key":{"host":"herdr","socket":"herdr","session_id":"session-1","window_id":"window-1"},
            "metadata":{"title":"Repair auth","goal":"No stale tokens","next_action":"Run tests","stage":"review"}
        }"#;

        let locked = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/work-metadata")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(locked.status(), StatusCode::UNAUTHORIZED);

        let saved = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/work-metadata")
                    .header(header::AUTHORIZATION, "Bearer edit-pat")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(saved.status(), StatusCode::OK);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/work-metadata")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let response = body_json(response).await;
        assert_eq!(response["works"][0]["metadata"]["title"], "Repair auth");
        assert_eq!(response["works"][0]["metadata"]["stage"], "review");

        let snapshot = app
            .oneshot(
                Request::builder()
                    .uri("/api/works")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let snapshot = body_json(snapshot).await;
        assert_eq!(snapshot["schema_version"], work::WORK_SCHEMA_VERSION);
        // Select the work this test created rather than asserting a global
        // count. The pane scan always includes the host's own tmux server,
        // so a machine with any muxa-managed work of its own would fail an
        // assertion about how many works exist — a real failure this test
        // hit once the developer's box had one.
        let mine = snapshot["works"]
            .as_array()
            .expect("works array")
            .iter()
            .find(|work| work["title"] == "Repair auth")
            .expect("the work this test annotated is present");
        assert_eq!(mine["stage"], "review");
        assert_eq!(mine["runs"].as_array().unwrap().len(), 1);

        // Reload from disk and look for this test's own record. Counting is
        // not safe here: `GET /api/works` upserts an external item for every
        // pane it discovers, and the scan includes the host's real tmux — so
        // the file legitimately holds a row per work running on the machine.
        let reloaded = WorkStore::load(Some(path));
        assert!(
            reloaded
                .records()
                .iter()
                .any(|record| record.metadata.title.as_deref() == Some("Repair auth")),
            "the annotated work survived the round trip"
        );
    }

    #[tokio::test]
    async fn work_mutations_refuse_ambiguous_logical_and_execution_selectors() {
        let app = router(public_read_state("edit-pat"));
        let metadata = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/work-metadata")
                    .header(header::AUTHORIZATION, "Bearer edit-pat")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{
                            "identity":{"workspace_id":"muxa","work_id":"CAL-101"},
                            "key":{"host":"herdr","socket":"herdr","session_id":"session-1","window_id":"window-1"},
                            "metadata":{"stage":"review"}
                        }"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(metadata.status(), StatusCode::BAD_REQUEST);
        assert!(body_json(metadata).await["error"]
            .as_str()
            .unwrap_or_default()
            .contains("exactly one"));

        let prompt = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/work-control/prompt")
                    .header(header::AUTHORIZATION, "Bearer edit-pat")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{
                            "identity":{"workspace_id":"muxa","work_id":"CAL-101"},
                            "key":{"host":"herdr","socket":"herdr","session_id":"session-1","window_id":"window-1"},
                            "text":"report status"
                        }"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(prompt.status(), StatusCode::BAD_REQUEST);
        assert!(body_json(prompt).await["error"]
            .as_str()
            .unwrap_or_default()
            .contains("exactly one"));
    }

    #[tokio::test]
    async fn starting_work_needs_its_own_grant_beyond_the_control_token() {
        // A valid control token is not enough: every other write route steers
        // a process the operator already started, while this one starts new
        // ones with permissions bypassed.
        let state = public_read_state("edit-pat");
        assert!(!state.config.allow_work_start, "default must be off");
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/work-control/up")
                    .header(header::AUTHORIZATION, "Bearer edit-pat")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"work":"cal-1234"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        // Not 403: the browser reads 401/403 as a bad token and flips the
        // whole dashboard to read-only, which would be a lie here.
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = body_json(response).await;
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("allow_work_start"),
            "the refusal should name the switch that enables it: {body}"
        );
    }

    #[tokio::test]
    async fn starting_work_still_refuses_without_the_control_token() {
        let mut cfg = DashboardConfig::loopback_default();
        cfg.auth = DashboardAuthMode::PublicRead;
        cfg.token = Some("edit-pat".into());
        cfg.allow_work_start = true;
        let response = router(state_from(cfg))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/work-control/up")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"work":"cal-1234"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_dashboard_prompt_expands_the_same_registered_skill_the_cli_does() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let panes = vec![test_pane(
            "herdr:p1",
            "session-1",
            "muxa",
            "window-1",
            "CAL-101",
        )];
        let backend: SharedBackend = Arc::new(RecordingControlBackend {
            sent: sent.clone(),
            panes,
        });
        let state = public_read_state("edit-pat")
            .with_backend(backend)
            .with_message_skills(BTreeMap::from([(
                "review-plan".to_string(),
                "Run the peer review workflow.".to_string(),
            )]));
        seed_agent(&state, "agent-1", "herdr:p1").await;

        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/work-control/prompt")
                    .header(header::AUTHORIZATION, "Bearer edit-pat")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"key":{"host":"herdr","socket":"herdr","session_id":"session-1","window_id":"window-1"},"skill":"/review-plan","body":"check CAL-101","context":"tests pass","submit":false}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            sent.lock().unwrap()[0].1,
            "Run the peer review workflow.\n\ncheck CAL-101\n\nInvocation context:\ntests pass"
        );
    }

    #[tokio::test]
    async fn a_dashboard_prompt_with_nothing_to_say_is_refused() {
        let response = router(public_read_state("edit-pat"))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/work-control/prompt")
                    .header(header::AUTHORIZATION, "Bearer edit-pat")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"key":{"host":"herdr","socket":"herdr","session_id":"s","window_id":"w"}}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn work_prompt_targets_only_live_agents_in_the_selected_work() {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let panes = vec![
            test_pane("herdr:p1", "session-1", "muxa", "window-1", "CAL-101"),
            test_pane("herdr:p2", "session-1", "muxa", "window-1", "CAL-101"),
            test_pane("herdr:p3", "session-1", "muxa", "window-2", "CAL-102"),
            test_pane("herdr:shell", "session-1", "muxa", "window-1", "CAL-101"),
        ];
        let backend: SharedBackend = Arc::new(RecordingControlBackend {
            sent: sent.clone(),
            panes,
        });
        let state = public_read_state("edit-pat").with_backend(backend);
        seed_agent(&state, "agent-1", "herdr:p1").await;
        seed_agent(&state, "agent-2", "herdr:p2").await;
        seed_agent(&state, "agent-3", "herdr:p3").await;

        let response = router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/work-control/prompt")
                    .header(header::AUTHORIZATION, "Bearer edit-pat")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        r#"{"key":{"host":"herdr","socket":"herdr","session_id":"session-1","window_id":"window-1"},"text":"report status","submit":false}"#,
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response = body_json(response).await;
        assert_eq!(response["attempted"], 2);
        assert_eq!(response["succeeded"], 2);
        assert_eq!(
            *sent.lock().unwrap(),
            vec![
                ("herdr:p1".to_string(), "report status".to_string()),
                ("herdr:p2".to_string(), "report status".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn auth_none_is_read_only_not_anonymously_writable() {
        let mut cfg = DashboardConfig::loopback_default();
        cfg.auth = DashboardAuthMode::None;
        cfg.token = None;
        let app = router(state_from(cfg));
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/terminal-sessions/missing/terminate")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// A persisted explicit token (the path used by `muxa init`) gates the API.
    #[tokio::test]
    async fn enabled_dashboard_with_explicit_token_requires_it() {
        let (state, token) = resolved_state(crate::config::DashboardTomlConfig {
            enabled: Some(true),
            token: Some("persisted-token".into()),
            ..Default::default()
        });
        let token = token.expect("explicit token must be preserved");
        let app = router(state);

        // No credentials → rejected.
        let unauthed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/agents")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unauthed.status(), StatusCode::UNAUTHORIZED);

        // Correct configured token → allowed.
        let authed = app
            .oneshot(
                Request::builder()
                    .uri("/api/agents")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(authed.status(), StatusCode::OK);
    }

    /// Explicit opt-out: `auth = "none"` runs the API unauthenticated
    /// even when enabled.
    #[tokio::test]
    async fn enabled_dashboard_auth_none_opts_out() {
        let (state, token) = resolved_state(crate::config::DashboardTomlConfig {
            enabled: Some(true),
            auth: Some(crate::config::DashboardAuthMode::None),
            ..Default::default()
        });
        assert!(token.is_none(), "auth = none must leave the token unset");
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
    }

    /// DNS-rebinding guard: a non-loopback `Host` header is rejected with
    /// 403 on a loopback bind, before any handler runs.
    #[tokio::test]
    async fn host_guard_rejects_non_loopback_host() {
        let app = router(fresh_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header(header::HOST, "attacker.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    /// Loopback `Host` values (with and without port, IPv4 and IPv6) are
    /// allowed through the guard.
    #[tokio::test]
    async fn host_guard_allows_loopback_hosts() {
        for host in ["127.0.0.1:7878", "localhost", "[::1]:7878", "127.0.0.1"] {
            let app = router(fresh_state());
            let resp = app
                .oneshot(
                    Request::builder()
                        .uri("/api/health")
                        .header(header::HOST, host)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "host {host:?} must pass");
        }
    }

    /// The guard is a no-op for a deliberately public bind — an operator
    /// who set `allow_public` is reached via a real hostname.
    #[tokio::test]
    async fn host_guard_skipped_for_public_bind() {
        let mut cfg = DashboardConfig::loopback_default();
        cfg.bind = "0.0.0.0:7878".parse().unwrap();
        cfg.auth = crate::config::DashboardAuthMode::None;
        cfg.token = None;
        let app = router(state_from(cfg));
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header(header::HOST, "dash.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn host_is_loopback_classifies_correctly() {
        let v4 = "127.0.0.1".parse().unwrap();
        // Absent header is allowed (non-browser clients).
        assert!(host_is_loopback(None, v4));
        assert!(host_is_loopback(Some("localhost"), v4));
        assert!(host_is_loopback(Some("LocalHost:7878"), v4));
        assert!(host_is_loopback(Some("127.0.0.1:7878"), v4));
        assert!(host_is_loopback(Some("127.0.0.5"), v4));
        assert!(host_is_loopback(Some("[::1]:7878"), v4));
        assert!(!host_is_loopback(Some("attacker.example.com"), v4));
        assert!(!host_is_loopback(Some("10.0.0.1:7878"), v4));
        // A non-loopback bind IP is honoured as an allowed Host literal.
        let public: IpAddr = "203.0.113.7".parse().unwrap();
        assert!(host_is_loopback(Some("203.0.113.7:7878"), public));
    }

    #[test]
    fn strip_host_port_variants() {
        assert_eq!(strip_host_port("127.0.0.1:7878"), "127.0.0.1");
        assert_eq!(strip_host_port("localhost"), "localhost");
        assert_eq!(strip_host_port("[::1]:7878"), "::1");
        assert_eq!(strip_host_port("[::1]"), "::1");
    }

    /// Defense-in-depth: the traced request URI must never carry the
    /// token, even if a legacy `?token=` slips through.
    #[test]
    fn scrub_token_from_uri_redacts_token_param() {
        let uri: axum::http::Uri = "/api/agents?token=s3cret&since=24h".parse().unwrap();
        let scrubbed = scrub_token_from_uri(&uri);
        assert!(!scrubbed.contains("s3cret"), "scrubbed: {scrubbed}");
        assert!(scrubbed.contains("token=REDACTED"), "scrubbed: {scrubbed}");
        assert!(scrubbed.contains("since=24h"), "scrubbed: {scrubbed}");

        // No query → untouched path.
        let plain: axum::http::Uri = "/api/health".parse().unwrap();
        assert_eq!(scrub_token_from_uri(&plain), "/api/health");
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
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "s1".into(),
                    surface: None,
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
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "snap-1".into(),
                    surface: None,
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
                    tmux_socket: None,
                    kind: AgentKind::ClaudeCode,
                    session_id: "s1".into(),
                    surface: None,
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
                        tmux_socket: None,
                        kind: AgentKind::ClaudeCode,
                        session_id: "s1".into(),
                        surface: None,
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
            tmux_socket: None,
            tmux_session: None,
            kind: AgentKind::ClaudeCode,
            session_id: "s1".into(),
            surface: None,
            pane: Some("%1".into()),
            cwd: None,
            pid: None,
            workload: crate::WorkloadSummary::default(),
            subagents: Vec::new(),
            state: AgentState::Idle,
            last_prompt: None,
            last_response: None,
            recap: None,
            ai_title: None,
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
            state_entered_at: OffsetDateTime::now_utc(),
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
        // `agent.agent_session_id`). `Transition` itself isn't `Deserialize` (it's
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
        assert_eq!(v["agent"]["agent_session_id"], "s1");
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
            tmux_socket: None,
            tmux_session: None,
            kind: AgentKind::ClaudeCode,
            session_id: "lag".into(),
            surface: None,
            pane: Some("%1".into()),
            cwd: None,
            pid: None,
            workload: crate::WorkloadSummary::default(),
            subagents: Vec::new(),
            state: AgentState::Idle,
            last_prompt: None,
            last_response: None,
            recap: None,
            ai_title: None,
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
            state_entered_at: OffsetDateTime::now_utc(),
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
                        tmux_socket: None,
                        kind,
                        session_id: sid.into(),
                        surface: None,
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
