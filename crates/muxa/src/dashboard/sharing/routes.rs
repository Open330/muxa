use super::{
    login, normalize_email, Grant, Permission, Session, SharingConfig, Target, MAX_GRANTS,
};
use crate::dashboard::server::AppState;
use crate::tmux::PaneInfo;
use crate::{HostKind, PaneKey, SharedBackend};
use axum::{
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{header, HeaderMap, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, OwnedSemaphorePermit};

type Failure = (StatusCode, &'static str);

pub(in crate::dashboard) fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/api/shares", get(list).post(create))
        .route("/api/shares/{id}/revoke", post(revoke))
}

pub(in crate::dashboard) fn recipient_routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/share/auth/login", get(login::begin))
        .route("/share/auth/callback", get(login::callback))
        .route("/share/auth/logout", post(login::logout))
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
}

#[derive(Serialize)]
struct ShareInfo {
    id: String,
    pane: String,
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
        let panes: Vec<_> = backend
            .list_panes()
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
    if pane.socket.as_ref().is_none_or(String::is_empty)
        || pane.session_id.is_empty()
        || pane.window_id.is_empty()
    {
        return Err((
            StatusCode::CONFLICT,
            "pane must expose an exact socket, session and window identity",
        ));
    }
    // Dashboard tmux rows carry full paths; backend rows use short names.
    // Resolve once and retain the absolute path for every later operation.
    let observed_socket = pane.socket.as_deref().expect("checked socket");
    let socket = if kind == HostKind::Tmux {
        crate::tmux::socket_path_or_default(observed_socket)
            .to_string_lossy()
            .into_owned()
    } else {
        observed_socket.to_owned()
    };
    if input.socket.as_ref().is_some_and(|requested| {
        requested.contains('/')
            && std::fs::canonicalize(requested).unwrap_or_else(|_| requested.into())
                != std::fs::canonicalize(&socket).unwrap_or_else(|_| (&socket).into())
    }) {
        return Err((
            StatusCode::CONFLICT,
            "pane socket does not match the selected server",
        ));
    }
    let Ok(stamp) = process_stamp(pane.pane_pid).await else {
        return Err((
            StatusCode::CONFLICT,
            "cannot verify this pane's process identity",
        ));
    };
    Ok(Target {
        socket,
        key: PaneKey::from_pane(kind, &pane),
        pid: pane.pane_pid,
        tty: pane.tty,
        process_stamp: stamp,
    })
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
    let id = uuid::Uuid::new_v4().simple().to_string();
    let grant = Grant {
        id: id.clone(),
        target,
        email,
        subject: None,
        permission: input.permission,
        expires: Instant::now() + Duration::from_secs(u64::from(input.ttl_seconds)),
        expires_at: time::OffsetDateTime::now_utc().unix_timestamp() + i64::from(input.ttl_seconds),
        revoked: false,
        last_prompt: None,
        capture: None,
    };
    let response = info(&grant, config);
    let mut registry = state.sharing.registry.lock().await;
    registry.grants.retain(|_, grant| {
        grant
            .try_lock()
            .ok()
            .is_none_or(|g| !g.revoked && g.expires > Instant::now())
    });
    if registry.grants.len() >= MAX_GRANTS {
        return error(StatusCode::TOO_MANY_REQUESTS, "too many active shares");
    }
    registry
        .grants
        .insert(id.clone(), Arc::new(Mutex::new(grant)));
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
    let mut grant = grant.lock().await;
    grant.revoked = true;
    grant.capture = None;
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
    grant.subject.get_or_insert_with(|| session.subject.clone());
    Ok(())
}

async fn view(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    match view_inner(&state, &id, &headers).await {
        Ok(value) => Json(value).into_response(),
        Err((status, message)) => error(status, message),
    }
}

async fn view_inner(
    state: &AppState,
    id: &str,
    headers: &HeaderMap,
) -> Result<serde_json::Value, Failure> {
    let (session, grant) = access(state, headers, id).await?;
    let mut grant = grant.lock().await;
    authorize(&mut grant, &session, false)?;
    let output = if let Some((_, output)) = grant
        .capture
        .as_ref()
        .filter(|(at, _)| at.elapsed() < Duration::from_secs(1))
    {
        output.clone()
    } else {
        let output = pane_operation(state, &grant.target, None, None).await?;
        grant.capture = Some((Instant::now(), output.clone()));
        output
    };
    authorize(&mut grant, &session, false)?;
    Ok(
        json!({"pane": grant.target.key.pane_id, "permission": grant.permission, "expires_at": grant.expires_at, "output": output, "email": session.email}),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Prompt {
    text: String,
}

async fn prompt(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(input): Json<Prompt>,
) -> Response {
    match prompt_inner(&state, &id, &headers, input.text).await {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err((status, message)) => error(status, message),
    }
}

async fn prompt_inner(
    state: &AppState,
    id: &str,
    headers: &HeaderMap,
    text: String,
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
    let (session, grant) = access(state, headers, id).await?;
    // Serialize acceptance, sending and revocation. A revoke response means
    // every previously admitted send has finished; later sends are refused.
    let mut grant = grant.lock_owned().await;
    authorize(&mut grant, &session, true)?;
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
    let target = grant.target.clone();
    pane_operation(state, &target, Some(text), Some(grant)).await?;
    tracing::info!(share_id = %id, subject = %session.subject, "shared pane prompt sent");
    Ok(())
}

async fn pane_operation(
    state: &AppState,
    target: &Target,
    text: Option<String>,
    guard: Option<tokio::sync::OwnedMutexGuard<Grant>>,
) -> Result<String, Failure> {
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
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        perform(backend, &target, text, permit)
    })
    .await
    .map_err(|_| (StatusCode::BAD_GATEWAY, "pane operation failed"))?
}

fn matches_target(pane: &PaneInfo, target: &Target, kind: HostKind) -> bool {
    PaneKey::from_pane(kind, pane) == target.key
        && pane.pane_pid == target.pid
        && pane.tty == target.tty
}

fn perform(
    backend: SharedBackend,
    target: &Target,
    text: Option<String>,
    _permit: OwnedSemaphorePermit,
) -> Result<String, Failure> {
    let panes = backend.list_panes();
    let Some(pane) = panes
        .iter()
        .find(|pane| matches_target(pane, target, backend.kind()))
    else {
        return Err((
            StatusCode::GONE,
            "shared pane has ended or moved; create a new share",
        ));
    };
    let socket = Some(target.socket.as_str());
    if let Some(text) = text {
        if !backend.send_text_on(socket, &pane.pane_id, &text) {
            return Err((
                StatusCode::BAD_GATEWAY,
                "prompt delivery failed; inspect output before retrying",
            ));
        }
        std::thread::sleep(crate::backend::PROMPT_SUBMIT_GRACE);
        if !backend
            .list_panes()
            .iter()
            .any(|pane| matches_target(pane, target, backend.kind()))
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
        if !backend
            .list_panes()
            .iter()
            .any(|pane| matches_target(pane, target, backend.kind()))
        {
            return Err((StatusCode::GONE, "shared pane changed during capture"));
        }
        Ok(crate::fleet::sanitize_capture_text(output)
            .chars()
            .take(32768)
            .collect())
    }
}

async fn process_stamp(pid: u32) -> Result<String, ()> {
    if pid == 0 {
        return Err(());
    }
    #[cfg(target_os = "linux")]
    {
        let stat = tokio::fs::read_to_string(format!("/proc/{pid}/stat"))
            .await
            .map_err(|_| ())?;
        // After comm's closing parenthesis, field 3 is first; starttime is 22.
        let (_, fields) = stat.rsplit_once(") ").ok_or(())?;
        fields
            .split_whitespace()
            .nth(19)
            .map(str::to_owned)
            .ok_or(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let output = crate::work_control::execute_work_command(
            std::path::Path::new("/bin/ps"),
            &["-p".into(), pid.to_string(), "-o".into(), "lstart=".into()],
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
