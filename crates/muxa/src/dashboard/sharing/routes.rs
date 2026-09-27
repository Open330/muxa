use super::{
    login, normalize_email, Grant, Permission, Session, SharingConfig, Target, MAX_GRANTS,
};
use crate::dashboard::server::AppState;
use crate::tmux::PaneInfo;
use crate::{HostKind, PaneKey, SharedBackend};
use axum::{
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, OwnedSemaphorePermit};

pub(super) type Failure = (StatusCode, &'static str);

pub(in crate::dashboard) fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/api/shares", get(list).post(create))
        .route("/api/shares/status", get(super::admin::status))
        .route("/api/shares/check", post(super::admin::check))
        .route("/api/shares/{id}/revoke", post(revoke))
        .layer(middleware::map_response(
            |mut response: Response| async move {
                response.headers_mut().insert(
                    header::CACHE_CONTROL,
                    axum::http::HeaderValue::from_static("no-store"),
                );
                response
            },
        ))
}

pub(in crate::dashboard) fn recipient_routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/share/auth/login", get(login::begin))
        .route("/share/auth/callback", get(login::callback))
        .route("/share/auth/logout", post(login::logout))
        .route("/share/auth/logout-all", post(login::logout_all))
        .route("/share/{id}", get(page))
        .route("/share/api/{id}", get(view))
        .route("/share/api/{id}/prompt", post(prompt))
        .layer(DefaultBodyLimit::max(32768))
        .layer(middleware::from_fn_with_state(state, boundary))
}

pub(super) fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}

pub(super) fn sharing_config(state: &AppState) -> Result<&SharingConfig, Failure> {
    state
        .sharing
        .config
        .as_ref()
        .ok_or((StatusCode::NOT_FOUND, "pane sharing is not configured"))
}

async fn boundary(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let config = match sharing_config(&state) {
        Ok(config) => config,
        Err((status, message)) => return error(status, message),
    };
    if request.method() != Method::GET
        && (request
            .headers()
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            != Some(config.origin().as_str())
            || request
                .headers()
                .get("x-muxa-share")
                .and_then(|v| v.to_str().ok())
                != Some("1"))
    {
        return error(StatusCode::FORBIDDEN, "same-origin request required");
    }
    let mut response = next.run(request).await;
    for (name, value) in [
        ("cache-control", "no-store"), ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        ("content-security-policy", "default-src 'self'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"),
    ] { response.headers_mut().insert(axum::http::HeaderName::from_static(name), value.parse().expect("static header")); }
    response
}

async fn page() -> Response {
    super::super::assets::serve_asset("share.html")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateShare {
    pane: String,
    socket: Option<String>,
    email: String,
    permission: Permission,
    ttl_seconds: u32,
    #[serde(default)]
    scope: super::targets::Scope,
}

#[derive(Serialize)]
struct ShareInfo {
    id: String,
    pane: String,
    socket: String,
    window: String,
    commands_submitted: usize,
    scope: super::targets::Scope,
    panes: Vec<String>,
    email: String,
    permission: Permission,
    expires_at: i64,
    revoked: bool,
    url: String,
}

fn info(grant: &Grant, config: &SharingConfig) -> ShareInfo {
    ShareInfo {
        id: grant.id.clone(),
        pane: grant.target.key.pane_id.clone(),
        socket: grant.target.socket.clone(),
        window: grant.target.key.window.window_id.clone(),
        scope: if grant.window {
            super::targets::Scope::Window
        } else {
            super::targets::Scope::Pane
        },
        panes: grant
            .targets()
            .map(|target| target.key.pane_id.clone())
            .collect(),
        commands_submitted: grant
            .deliveries
            .values()
            .filter(|delivery| delivery.outcome == "submitted")
            .count(),
        email: grant.email.clone(),
        permission: grant.permission,
        expires_at: grant.expires_at,
        revoked: grant.revoked,
        url: format!("{}/share/{}", config.origin(), grant.id),
    }
}

async fn resolve_target(state: &AppState, input: &CreateShare) -> Result<Target, Failure> {
    let Some(kind @ (HostKind::Tmux | HostKind::Rmux)) =
        crate::backend::pane_id_host_kind(&input.pane)
    else {
        return Err((
            StatusCode::BAD_REQUEST,
            "sharing currently supports local tmux and rmux panes",
        ));
    };
    let Some(backend) = state.backends.iter().find(|b| b.kind() == kind).cloned() else {
        return Err((StatusCode::NOT_FOUND, "pane backend unavailable"));
    };
    if !backend.caps().capture_pane
        || (input.permission == Permission::Prompt && !backend.caps().send_text)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "pane backend does not support the requested access",
        ));
    }
    let Ok(permit) = state.sharing.pane_slots.clone().try_acquire_owned() else {
        return Err((StatusCode::TOO_MANY_REQUESTS, "pane service busy"));
    };
    let pane_id = input.pane.clone();
    let socket = input.socket.clone();
    let observed = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let observation = backend.observe_panes();
        if !observation.is_complete() {
            return Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "pane inventory temporarily unavailable",
            ));
        }
        let panes: Vec<_> = observation
            .panes
            .into_iter()
            .filter(|p| {
                p.pane_id == pane_id
                    && socket.as_ref().is_none_or(|socket| {
                        p.socket.as_ref().is_some_and(|observed| {
                            crate::backend::pane_endpoints_match(Some(&p.pane_id), socket, observed)
                        })
                    })
            })
            .collect();
        match panes.as_slice() {
            [pane] => Ok(pane.clone()),
            [] => Err((StatusCode::NOT_FOUND, "pane not found")),
            _ => Err((StatusCode::CONFLICT, "specify a unique pane socket")),
        }
    })
    .await;
    let pane = match observed {
        Ok(Ok(pane)) => pane,
        Ok(Err((status, message))) => return Err((status, message)),
        Err(_) => return Err((StatusCode::BAD_GATEWAY, "pane lookup failed")),
    };
    super::targets::pin(kind, pane, input.socket.as_deref()).await
}

async fn create(State(state): State<AppState>, Json(input): Json<CreateShare>) -> Response {
    let config = match sharing_config(&state) {
        Ok(config) => config,
        Err((status, message)) => return error(status, message),
    };
    if !(60..=86400).contains(&input.ttl_seconds) {
        return error(
            StatusCode::BAD_REQUEST,
            "expiry must be between 1 minute and 24 hours",
        );
    }
    let email = match normalize_email(&input.email) {
        Ok(email) => email,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let target = match resolve_target(&state, &input).await {
        Ok(target) => target,
        Err((status, message)) => return error(status, message),
    };
    let peers = if input.scope == super::targets::Scope::Window {
        match super::targets::window_members(&state, &target).await {
            Ok(peers) => peers,
            Err((status, message)) => return error(status, message),
        }
    } else {
        Vec::new()
    };
    let id = uuid::Uuid::new_v4().simple().to_string();
    let grant = Grant {
        id: id.clone(),
        target,
        peers,
        window: input.scope == super::targets::Scope::Window,
        email,
        subject: None,
        permission: input.permission,
        expires: Instant::now() + Duration::from_secs(u64::from(input.ttl_seconds)),
        expires_at: time::OffsetDateTime::now_utc().unix_timestamp() + i64::from(input.ttl_seconds),
        revoked: false,
        last_prompt: None,
        capture: None,
        deliveries: std::collections::HashMap::new(),
        dirty: true,
    };
    let response = info(&grant, config);
    let mut registry = state.sharing.registry.lock().await;
    registry.grants.retain(|_, grant| {
        grant
            .try_lock()
            .ok()
            .is_none_or(|g| g.expires > Instant::now())
    });
    if registry.grants.len() >= MAX_GRANTS {
        return error(
            StatusCode::TOO_MANY_REQUESTS,
            "share limit reached; wait for previous invitations to expire",
        );
    }
    let grant = Arc::new(Mutex::new(grant));
    match state
        .sharing
        .persist(grant.clone().lock_owned().await)
        .await
    {
        Ok(guard) => drop(guard),
        Err((status, message)) => return error(status, message),
    }
    registry.grants.insert(id.clone(), grant);
    tracing::info!(share_id = %id, "pane share created");
    (StatusCode::CREATED, Json(response)).into_response()
}

async fn list(State(state): State<AppState>) -> Response {
    let config = match sharing_config(&state) {
        Ok(config) => config,
        Err((status, message)) => return error(status, message),
    };
    let grants: Vec<_> = state
        .sharing
        .registry
        .lock()
        .await
        .grants
        .values()
        .cloned()
        .collect();
    let mut result = Vec::new();
    for grant in grants {
        result.push(info(&*grant.lock().await, config));
    }
    Json(json!({"shares": result})).into_response()
}

async fn revoke(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let grant = state.sharing.registry.lock().await.grants.get(&id).cloned();
    let Some(grant) = grant else {
        return error(StatusCode::NOT_FOUND, "share unavailable");
    };
    let mut grant = grant.lock_owned().await;
    grant.revoked = true;
    grant.dirty = true;
    grant.capture = None;
    if let Err((status, message)) = state.sharing.persist(grant).await {
        return error(status, message);
    }
    state
        .sharing
        .registry
        .lock()
        .await
        .logins
        .retain(|_, flow| flow.share != id);
    tracing::info!(share_id = %id, "pane share revoked");
    StatusCode::NO_CONTENT.into_response()
}

async fn access(
    state: &AppState,
    headers: &HeaderMap,
    id: &str,
) -> Result<(Session, Arc<Mutex<Grant>>), Failure> {
    let config = state
        .sharing
        .config
        .as_ref()
        .ok_or((StatusCode::NOT_FOUND, "sharing unavailable"))?;
    let secret = login::cookie(headers, login::cookie_name(config, false))
        .ok_or((StatusCode::UNAUTHORIZED, "sign in to open this share"))?;
    let registry = state.sharing.registry.lock().await;
    let session = registry
        .sessions
        .get(&secret)
        .filter(|s| s.expires > Instant::now())
        .cloned()
        .ok_or((StatusCode::UNAUTHORIZED, "session expired; sign in again"))?;
    let grant = registry
        .grants
        .get(id)
        .cloned()
        .ok_or((StatusCode::NOT_FOUND, "share unavailable"))?;
    Ok((session, grant))
}

pub(super) fn authorize(grant: &mut Grant, session: &Session, prompt: bool) -> Result<(), Failure> {
    if grant.revoked || grant.expires <= Instant::now() {
        return Err((StatusCode::GONE, "share expired or revoked"));
    }
    if session.expires <= Instant::now() {
        return Err((StatusCode::UNAUTHORIZED, "session expired"));
    }
    if grant.email != session.email
        || grant
            .subject
            .as_ref()
            .is_some_and(|subject| subject != &session.subject)
    {
        return Err((StatusCode::FORBIDDEN, "this account has not been invited"));
    }
    if prompt && grant.permission != Permission::Prompt {
        return Err((StatusCode::FORBIDDEN, "this share is view-only"));
    }
    if grant.subject.is_none() {
        grant.subject = Some(session.subject.clone());
        grant.dirty = true;
    }
    Ok(())
}

#[derive(Default, Deserialize)]
struct ViewQuery {
    pane: Option<String>,
}

async fn view(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Query(query): Query<ViewQuery>,
) -> Response {
    match view_inner(&state, &id, &headers, query.pane.as_deref()).await {
        Ok(value) => Json(value).into_response(),
        Err((status, message)) => error(status, message),
    }
}

async fn view_inner(
    state: &AppState,
    id: &str,
    headers: &HeaderMap,
    pane: Option<&str>,
) -> Result<serde_json::Value, Failure> {
    let (session, grant) = access(state, headers, id).await?;
    let mut grant = grant.lock_owned().await;
    authorize(&mut grant, &session, false)?;
    if grant.dirty {
        grant = state.sharing.persist(grant).await?;
    }
    let target = grant.select(pane)?.clone();
    let mut available = true;
    let mut notice = None;
    let output = if let Some((_, _, output)) = grant.capture.as_ref().filter(|(at, pane, _)| {
        at.elapsed() < Duration::from_secs(1) && pane == &target.key.pane_id
    }) {
        output.clone()
    } else {
        match pane_operation(state, &target, None, None).await {
            Ok(output) => {
                grant.capture = Some((Instant::now(), target.key.pane_id.clone(), output.clone()));
                output
            }
            Err((StatusCode::GONE, message)) if grant.window => {
                available = false;
                notice = Some(message);
                String::new()
            }
            Err(failure) => return Err(failure),
        }
    };
    authorize(&mut grant, &session, false)?;
    let panes: Vec<_> = grant
        .targets()
        .map(|target| target.key.pane_id.clone())
        .collect();
    Ok(
        json!({"pane": target.key.pane_id, "panes":panes, "window":grant.window, "pane_available":available, "notice":notice, "permission": grant.permission, "expires_at": grant.expires_at, "output": output, "email": session.email}),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prompt {
    text: String,
    request_id: Option<String>,
    pane: Option<String>,
}

async fn prompt(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<Prompt>,
) -> Response {
    match prompt_inner(
        &state,
        &id,
        &headers,
        input.text,
        input.request_id,
        input.pane,
    )
    .await
    {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err((status, message)) => error(status, message),
    }
}

async fn prompt_inner(
    state: &AppState,
    id: &str,
    headers: &HeaderMap,
    text: String,
    request_id: Option<String>,
    pane: Option<String>,
) -> Result<(), Failure> {
    if text.trim().is_empty()
        || text.len() > 16384
        || text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "prompt must be 1–16384 bytes without terminal control characters",
        ));
    }
    let request_id = request_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    if request_id.len() > 64
        || request_id.is_empty()
        || !request_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err((StatusCode::BAD_REQUEST, "invalid prompt request_id"));
    }
    let (session, grant) = access(state, headers, id).await?;
    // Serialize acceptance, sending and revocation. A revoke response means
    // every previously admitted send has finished; later sends are refused.
    let mut grant = grant.lock_owned().await;
    authorize(&mut grant, &session, true)?;
    let target = grant.select(pane.as_deref())?.clone();
    let mut hasher = Sha256::new();
    hasher.update(target.key.pane_id.as_bytes());
    hasher.update([0]);
    hasher.update(text.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    if let Some(previous) = grant.deliveries.get(&request_id) {
        return if previous.digest != digest {
            Err((
                StatusCode::CONFLICT,
                "request_id was already used for a different prompt",
            ))
        } else if previous.outcome == "submitted" {
            Ok(())
        } else {
            Err((
                StatusCode::CONFLICT,
                "previous delivery is uncertain; inspect the pane before sending a new command",
            ))
        };
    }
    if grant.deliveries.len() >= 1024 {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "share reached its command limit; request a new invitation",
        ));
    }
    if grant
        .last_prompt
        .is_some_and(|at| at.elapsed() < Duration::from_millis(500))
    {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "wait before sending another prompt",
        ));
    }
    grant.last_prompt = Some(Instant::now());
    grant.capture = None;
    grant.deliveries.insert(
        request_id.clone(),
        super::Delivery {
            digest,
            outcome: "pending".into(),
        },
    );
    grant.dirty = true;
    let grant = state.sharing.persist(grant).await?;
    pane_operation(state, &target, Some((text, request_id)), Some(grant)).await?;
    tracing::info!(share_id = %id, subject = %session.subject, "shared pane prompt sent");
    Ok(())
}

async fn pane_operation(
    state: &AppState,
    target: &Target,
    text: Option<(String, String)>,
    guard: Option<tokio::sync::OwnedMutexGuard<Grant>>,
) -> Result<String, Failure> {
    let execution_lock = {
        let mut locks = state
            .sharing
            .execution_locks
            .lock()
            .map_err(|_| (StatusCode::SERVICE_UNAVAILABLE, "pane service unavailable"))?;
        locks.retain(|_, lock| lock.strong_count() > 0);
        let key = (target.socket.clone(), target.key.pane_id.clone());
        if let Some(lock) = locks.get(&key).and_then(std::sync::Weak::upgrade) {
            lock
        } else {
            let lock = Arc::new(Mutex::new(()));
            locks.insert(key, Arc::downgrade(&lock));
            lock
        }
    };
    let execution_guard = tokio::time::timeout(Duration::from_secs(2), execution_lock.lock_owned())
        .await
        .map_err(|_| {
            (
                StatusCode::TOO_MANY_REQUESTS,
                "pane is busy; inspect output before retrying",
            )
        })?;
    if guard
        .as_ref()
        .is_some_and(|grant| grant.revoked || grant.expires <= Instant::now())
    {
        return Err((StatusCode::GONE, "share expired or revoked"));
    }
    let backend = state
        .backends
        .iter()
        .find(|b| b.kind() == target.key.window.session.endpoint.host)
        .cloned()
        .ok_or((StatusCode::GONE, "shared pane backend unavailable"))?;
    let permit = state
        .sharing
        .pane_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| (StatusCode::TOO_MANY_REQUESTS, "pane service busy"))?;
    if process_stamp(target.pid).await.ok().as_ref() != Some(&target.process_stamp) {
        return Err((StatusCode::GONE, "shared pane process has ended"));
    }
    let target = target.clone();
    let storage = guard.as_ref().and_then(|_| state.sharing.storage.clone());
    tokio::task::spawn_blocking(move || {
        let _execution_guard = execution_guard;
        let mut guard = guard;
        let (text, request_id) = text.map_or((None, None), |(text, id)| (Some(text), Some(id)));
        let result = perform(backend, &target, text, permit);
        if let (Some(grant), Some(id)) = (&mut guard, request_id) {
            if let Some(delivery) = grant.deliveries.get_mut(&id) {
                delivery.outcome = if result.is_ok() {
                    "submitted"
                } else {
                    "uncertain"
                }
                .into();
            }
            grant.dirty = true;
            if let Some(storage) = storage {
                storage.save(grant).map_err(|error| {
                    tracing::error!(%error, "prompt receipt could not be saved");
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        "delivery is uncertain; inspect the pane before retrying",
                    )
                })?;
            }
            grant.dirty = false;
        }
        result
    })
    .await
    .map_err(|_| (StatusCode::BAD_GATEWAY, "pane operation failed"))?
}

fn matches_target(pane: &PaneInfo, target: &Target, kind: HostKind) -> bool {
    PaneKey::from_pane(kind, pane) == target.key
        && pane.pane_pid == target.pid
        && pane.tty == target.tty
}

fn observed_target(backend: &SharedBackend, target: &Target) -> Result<PaneInfo, Failure> {
    let observation = backend.observe_panes();
    if let Some(pane) = observation
        .panes
        .iter()
        .find(|pane| matches_target(pane, target, backend.kind()))
    {
        return Ok(pane.clone());
    }
    if observation.is_complete() {
        Err((
            StatusCode::GONE,
            "shared pane has ended or moved; request a new invitation",
        ))
    } else {
        Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "pane inventory temporarily unavailable",
        ))
    }
}

fn perform(
    backend: SharedBackend,
    target: &Target,
    text: Option<String>,
    _permit: OwnedSemaphorePermit,
) -> Result<String, Failure> {
    let pane = observed_target(&backend, target)?;
    let socket = Some(target.socket.as_str());
    if let Some(text) = text {
        if !backend.send_text_on(socket, &pane.pane_id, &text) {
            return Err((
                StatusCode::BAD_GATEWAY,
                "prompt delivery failed; inspect output before retrying",
            ));
        }
        std::thread::sleep(crate::backend::PROMPT_SUBMIT_GRACE);
        if observed_target(&backend, target).is_err()
            || !backend.send_text_on(socket, &pane.pane_id, "\r")
        {
            return Err((
                StatusCode::BAD_GATEWAY,
                "text may be present but was not submitted; inspect the pane",
            ));
        }
        Ok(String::new())
    } else {
        let output = backend
            .capture_pane_on(socket, &pane.pane_id)
            .ok_or((StatusCode::BAD_GATEWAY, "pane output unavailable"))?;
        observed_target(&backend, target)?;
        Ok(crate::fleet::sanitize_capture_text(output)
            .chars()
            .take(32768)
            .collect())
    }
}

pub(super) async fn process_stamp(pid: u32) -> Result<String, ()> {
    if pid == 0 {
        return Err(());
    }
    #[cfg(target_os = "linux")]
    {
        static BOOT: tokio::sync::OnceCell<Result<String, ()>> = tokio::sync::OnceCell::const_new();
        let boot = BOOT
            .get_or_init(|| async {
                tokio::fs::read_to_string("/proc/sys/kernel/random/boot_id")
                    .await
                    .map(|value| value.trim().to_owned())
                    .map_err(|_| ())
            })
            .await;
        let Ok(boot) = boot else {
            return Err(());
        };
        let stat = tokio::fs::read_to_string(format!("/proc/{pid}/stat"))
            .await
            .map_err(|_| ())?;
        // After comm's closing parenthesis, field 3 is first; starttime is 22.
        let (_, fields) = stat.rsplit_once(") ").ok_or(())?;
        fields
            .split_whitespace()
            .nth(19)
            .map(|start| format!("{boot}:{start}"))
            .ok_or(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = crate::work_control::execute_work_command(
            std::path::Path::new("/usr/bin/env"),
            &[
                "LC_ALL=C".into(),
                "TZ=UTC".into(),
                "/bin/ps".into(),
                "-p".into(),
                pid.to_string(),
                "-o".into(),
                "lstart=".into(),
            ],
            None,
            None,
            crate::work_control::WorkCommandLimits {
                timeout: Duration::from_secs(2),
                max_output_bytes: 1024,
            },
        )
        .await
        .map_err(|_| ())?;
        if output.exit_code != 0 || output.stdout.trim().is_empty() {
            return Err(());
        }
        Ok(output.stdout.trim().to_owned())
    }
}
