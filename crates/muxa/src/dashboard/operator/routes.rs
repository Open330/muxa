use super::{
    claim_groups, Enrolled, ExtraClaims, LoginConfig, OperatorLogin, PendingEnrollment,
    PendingLogin, Session, Via, ENROLL_TTL, LOGIN_TTL, MAX_ENROLLED, MAX_ENROLL_ATTEMPTS,
    MAX_PENDING_ENROLLMENTS, MAX_PENDING_LOGINS, MAX_SESSIONS, SESSION_TTL,
};
use crate::dashboard::oidc;
use crate::dashboard::server::AppState;
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use openidconnect::{
    core::CoreAuthenticationFlow, AuthorizationCode, CsrfToken, Nonce, PkceCodeChallenge, Scope,
    TokenResponse,
};
use serde::Deserialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Instant;
use subtle::ConstantTimeEq;

type Failure = (StatusCode, &'static str);

/// Default policy for `/auth/*`: no scripts, no forms, nothing framed.
const AUTH_CSP: &str = "default-src 'none'; style-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";
/// The enrollment page additionally runs its own same-origin script, which
/// submits the token with `fetch`; native form submission stays blocked.
const ENROLL_CSP: &str = "default-src 'none'; style-src 'self'; script-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

/// `/auth/*`. Mounted outside the operator auth layers: these routes are how
/// a browser obtains operator access in the first place.
pub(in crate::dashboard) fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/login", get(begin))
        .route("/auth/callback", get(callback))
        .route("/auth/logout", post(logout))
        .route("/auth/session", get(status))
        .route(
            "/auth/enroll",
            get(enroll_page)
                .post(enroll)
                .layer(DefaultBodyLimit::max(4096)),
        )
        .route("/auth/enroll/cancel", get(enroll_cancel))
        .layer(middleware::from_fn(security_headers))
}

/// `/api/operators*`. Mounted inside the operator write layer, so these need
/// the bearer token or an operator session (plus CSRF proof for changes).
pub(in crate::dashboard) fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/api/operators", get(list_operators))
        .route("/api/operators/{id}/remove", post(remove_operator))
        .layer(middleware::map_response(
            |mut response: Response| async move {
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                response
            },
        ))
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    // Never let a proxy or the browser cache a login redirect or session
    // state, and never leak the callback's code/state in a Referer.
    for (name, value) in [
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
    ] {
        response.headers_mut().insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    // A handler may only widen this for its own page (the enrollment form).
    if !response
        .headers()
        .contains_key(header::CONTENT_SECURITY_POLICY)
    {
        response.headers_mut().insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(AUTH_CSP),
        );
    }
    response
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}

/// Browsers following the redirect chain get a readable page instead of a
/// JSON error body.
fn browser_error(headers: &HeaderMap, (status, message): Failure) -> Response {
    let wants_html = headers
        .get(header::ACCEPT)
        .and_then(|h| h.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"));
    if !wants_html {
        return error(status, message);
    }
    let body = format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><link rel=\"stylesheet\" href=\"/static/share.css\"><title>Sign-in failed · muxa</title><main><h1>Sign-in could not be completed</h1><p>{}.</p><p><a href=\"/\">Back to the dashboard</a></p></main></html>",
        html_escape(message)
    );
    (status, Html(body)).into_response()
}

fn html_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[derive(Clone, Copy)]
enum Cookie {
    Session,
    Flow,
    Enroll,
}

fn with_cookie(
    mut response: Response,
    config: &LoginConfig,
    cookie: Cookie,
    value: &str,
    max_age: u64,
) -> Response {
    let (session_name, flow_name) = config.cookie_names();
    let secure = if config.secure() { "; Secure" } else { "" };
    // The flow cookie must survive the IdP's top-level redirect back to us,
    // which is cross-site, so it is Lax. The enrollment cookie is set on
    // that same cross-site redirect chain and must reach the enrollment page
    // it redirects to, so it is Lax too; its only state change requires the
    // Origin + header proof below. The session cookie is never needed on a
    // cross-site request: the dashboard HTML is public and every API call
    // is a same-origin fetch.
    let (name, same_site) = match cookie {
        Cookie::Session => (session_name, "Strict"),
        Cookie::Flow => (flow_name, "Lax"),
        Cookie::Enroll => (config.enroll_cookie_name(), "Lax"),
    };
    let text = format!(
        "{name}={value}; Path=/; HttpOnly; SameSite={same_site}; Max-Age={max_age}{secure}"
    );
    response
        .headers_mut()
        .append(header::SET_COOKIE, text.parse().expect("generated cookie"));
    response
}

/// Only same-origin absolute paths. `//host` and `/\host` are scheme-relative
/// to browsers, so either would turn sign-in into an open redirect.
pub(super) fn safe_return_to(path: &str) -> bool {
    path.len() <= 1024
        && path.starts_with('/')
        && !path.starts_with("//")
        && path.bytes().all(|b| b.is_ascii_graphic() && b != b'\\')
}

fn login_config(state: &AppState) -> Result<&LoginConfig, Failure> {
    state
        .operator
        .config()
        .ok_or((StatusCode::NOT_FOUND, "operator sign-in is not configured"))
}

fn unix_now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

#[derive(Deserialize)]
struct LoginQuery {
    return_to: Option<String>,
}

async fn begin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Response {
    match begin_inner(&state, query).await {
        Ok(response) => response,
        Err(failure) => browser_error(&headers, failure),
    }
}

async fn begin_inner(state: &AppState, query: LoginQuery) -> Result<Response, Failure> {
    let config = login_config(state)?;
    let return_to = query.return_to.unwrap_or_else(|| "/".into());
    if !safe_return_to(&return_to) {
        return Err((StatusCode::BAD_REQUEST, "invalid return_to path"));
    }
    let login = &state.operator;
    let _permit = login
        .login_slots
        .try_acquire()
        .map_err(|_| (StatusCode::TOO_MANY_REQUESTS, "sign-in is busy; try again"))?;
    let (client, _http) = login
        .provider
        .as_ref()
        .ok_or((StatusCode::NOT_FOUND, "operator sign-in is not configured"))?
        .client::<ExtraClaims>()
        .await
        .map_err(|()| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                "the sign-in provider is unavailable; use the dashboard token instead",
            )
        })?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let mut request = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("email".into()))
        .add_extra_param("prompt", "select_account")
        .set_pkce_challenge(challenge);
    for scope in &config.scopes {
        request = request.add_scope(Scope::new(scope.clone()));
    }
    let (url, csrf, nonce) = request.url();
    let browser = oidc::random_secret();
    {
        let mut registry = login.registry();
        let now = Instant::now();
        registry.logins.retain(|_, flow| flow.expires > now);
        if registry.logins.len() >= MAX_PENDING_LOGINS {
            return Err((StatusCode::TOO_MANY_REQUESTS, "too many pending sign-ins"));
        }
        registry.logins.insert(
            csrf.secret().clone(),
            PendingLogin {
                browser: browser.clone(),
                return_to,
                nonce,
                verifier,
                expires: now + LOGIN_TTL,
            },
        );
    }
    Ok(with_cookie(
        Redirect::to(url.as_str()).into_response(),
        config,
        Cookie::Flow,
        &browser,
        LOGIN_TTL.as_secs(),
    ))
}

#[derive(Deserialize)]
struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    match callback_inner(&state, &headers, query).await {
        Ok(response) => response,
        Err(failure) => browser_error(&headers, failure),
    }
}

async fn callback_inner(
    state: &AppState,
    headers: &HeaderMap,
    query: CallbackQuery,
) -> Result<Response, Failure> {
    let config = login_config(state)?;
    let login = &state.operator;
    let (Some(code), Some(flow_state)) = (query.code, query.state) else {
        return Err((StatusCode::UNAUTHORIZED, "sign-in was cancelled or refused"));
    };
    if code.is_empty() || code.len() > 4096 || flow_state.len() > 256 {
        return Err((StatusCode::BAD_REQUEST, "invalid sign-in response"));
    }
    let browser = oidc::cookie(headers, config.cookie_names().1)
        .ok_or((StatusCode::UNAUTHORIZED, "sign-in expired; start again"))?;
    let _permit = login
        .login_slots
        .try_acquire()
        .map_err(|_| (StatusCode::TOO_MANY_REQUESTS, "sign-in is busy; try again"))?;
    // Consume the flow before contacting the provider: a state value is
    // single-use whether or not the exchange succeeds.
    let pending = {
        let mut registry = login.registry();
        let valid = registry.logins.get(&flow_state).is_some_and(|flow| {
            flow.expires > Instant::now()
                && bool::from(flow.browser.as_bytes().ct_eq(browser.as_bytes()))
        });
        if !valid {
            return Err((StatusCode::UNAUTHORIZED, "invalid sign-in state"));
        }
        registry.logins.remove(&flow_state).expect("checked flow")
    };
    let identity = verify(login, config, &code, &pending).await?;
    let enrolled = login
        .registry()
        .enrolled(config, &identity.issuer, &identity.subject)
        .map(|entry| entry.id.clone());
    if let Some(id) = &enrolled {
        touch(login, id, identity.email.as_deref()).await;
    }
    let via = if identity.in_group {
        Via::Group
    } else if enrolled.is_some() {
        Via::Enrollment
    } else if config.enrollment_enabled() {
        return begin_enrollment(login, config, identity, pending.return_to);
    } else {
        tracing::warn!(subject = %identity.subject, "dashboard sign-in refused: account is not in the operator group");
        return Err((
            StatusCode::FORBIDDEN,
            "this account is not allowed to operate this dashboard",
        ));
    };
    tracing::info!(subject = %identity.subject, ?via, "dashboard operator signed in");
    let secret = start_session(login, config, headers, identity.into_session(via));
    let response = Redirect::to(&pending.return_to).into_response();
    Ok(with_cookie(
        with_cookie(response, config, Cookie::Flow, "", 0),
        config,
        Cookie::Session,
        &secret,
        SESSION_TTL.as_secs(),
    ))
}

/// Record a new operator session for this browser and return its secret.
fn start_session(
    login: &OperatorLogin,
    config: &LoginConfig,
    headers: &HeaderMap,
    session: Session,
) -> String {
    let secret = oidc::random_secret();
    let mut registry = login.registry();
    let now = Instant::now();
    registry.sessions.retain(|_, session| session.expires > now);
    // Replace this browser's previous session rather than keeping a
    // fixation target alive next to the new one.
    if let Some(previous) = oidc::cookie(headers, config.cookie_names().0) {
        registry.sessions.remove(&previous);
    }
    // Only operators get here, so evicting the oldest session keeps the
    // owner able to sign in instead of locking them out.
    while registry.sessions.len() >= MAX_SESSIONS {
        let oldest = registry
            .sessions
            .iter()
            .min_by_key(|(_, session)| session.expires)
            .map(|(key, _)| key.clone())
            .expect("non-empty sessions");
        registry.sessions.remove(&oldest);
    }
    registry.sessions.insert(secret.clone(), session);
    secret
}

/// Refresh an enrolled account's display email and last sign-in. Best
/// effort: failing to record it must not block a legitimate sign-in.
async fn touch(login: &OperatorLogin, id: &str, email: Option<&str>) {
    let at = unix_now();
    {
        let mut registry = login.registry();
        if let Some(entry) = registry.enrolled.iter_mut().find(|entry| entry.id == id) {
            entry.last_seen_at = at;
            if email.is_some() {
                entry.email = email.map(str::to_owned);
            }
        }
    }
    let Some(storage) = login.storage.clone() else {
        return;
    };
    let (id, email) = (id.to_owned(), email.map(str::to_owned));
    let result = tokio::task::spawn_blocking(move || storage.touch(&id, email.as_deref(), at))
        .await
        .map_err(std::io::Error::other)
        .and_then(|result| result);
    if let Err(error) = result {
        tracing::warn!(%error, "operator storage: could not record the last sign-in");
    }
}

/// A verified account.
struct Identity {
    issuer: String,
    subject: String,
    email: Option<String>,
    in_group: bool,
}

impl Identity {
    fn into_session(self, via: Via) -> Session {
        Session {
            issuer: self.issuer,
            subject: self.subject,
            email: self.email,
            via,
            expires: Instant::now() + SESSION_TTL,
        }
    }
}

async fn verify(
    login: &OperatorLogin,
    config: &LoginConfig,
    code: &str,
    flow: &PendingLogin,
) -> Result<Identity, Failure> {
    const INVALID: Failure = (StatusCode::UNAUTHORIZED, "sign-in verification failed");
    let (client, http) = login
        .provider
        .as_ref()
        .ok_or(INVALID)?
        .client::<ExtraClaims>()
        .await
        .map_err(|()| INVALID)?;
    let tokens = client
        .exchange_code(AuthorizationCode::new(code.to_owned()))
        .map_err(|_| INVALID)?
        .set_pkce_verifier(openidconnect::PkceCodeVerifier::new(
            flow.verifier.secret().clone(),
        ))
        .request_async(&http)
        .await
        .map_err(|_| INVALID)?;
    let token = tokens.id_token().ok_or(INVALID)?;
    let verifier = client.id_token_verifier();
    // Signature, issuer, audience, expiry and nonce.
    let claims = token.claims(&verifier, &flow.nonce).map_err(|_| INVALID)?;
    oidc::check_access_token_hash(&tokens, token, claims.access_token_hash(), &verifier)
        .map_err(|()| INVALID)?;
    let subject = claims.subject().as_str().to_owned();
    if subject.is_empty() || subject.len() > 255 || subject.chars().any(char::is_control) {
        return Err(INVALID);
    }
    // Authorization is group membership or enrollment by (issuer, subject);
    // email is only for display, so it need not be verified.
    let groups = claim_groups(claims.additional_claims().claims.get(config.groups_claim()));
    let in_group = config
        .required_group
        .as_deref()
        .is_some_and(|required| groups.contains(&required));
    let email = claims
        .email()
        .map(|email| email.as_str().to_owned())
        .filter(|email| email.len() <= 254 && !email.chars().any(char::is_control));
    Ok(Identity {
        issuer: claims.issuer().as_str().to_owned(),
        subject,
        email,
        in_group,
    })
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let config = match login_config(&state) {
        Ok(config) => config,
        Err((status, message)) => return error(status, message),
    };
    if !state.operator.csrf_ok(&Method::POST, &headers) {
        return error(StatusCode::FORBIDDEN, "same-origin request required");
    }
    if let Some(secret) = oidc::cookie(&headers, config.cookie_names().0) {
        state.operator.registry().sessions.remove(&secret);
    }
    with_cookie(
        StatusCode::NO_CONTENT.into_response(),
        config,
        Cookie::Session,
        "",
        0,
    )
}

/// Unauthenticated: a signed-out browser needs to learn that it can sign in.
async fn status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    Json(super::super::server::login_status(&state, &headers)).into_response()
}

// ── Enrollment ─────────────────────────────────────────────────────

/// A verified account that is not an operator: hold it for the token
/// instead of refusing outright.
fn begin_enrollment(
    login: &OperatorLogin,
    config: &LoginConfig,
    identity: Identity,
    return_to: String,
) -> Result<Response, Failure> {
    let secret = oidc::random_secret();
    {
        let mut registry = login.registry();
        let now = Instant::now();
        registry
            .enrollments
            .retain(|_, pending| pending.expires > now);
        if registry.enrollments.len() >= MAX_PENDING_ENROLLMENTS {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                "too many sign-ins are waiting for enrollment; try again later",
            ));
        }
        tracing::info!(subject = %identity.subject, "dashboard sign-in needs enrollment");
        registry.enrollments.insert(
            secret.clone(),
            PendingEnrollment {
                issuer: identity.issuer,
                subject: identity.subject,
                email: identity.email,
                return_to,
                attempts: 0,
                expires: now + ENROLL_TTL,
            },
        );
    }
    let response = Redirect::to("/auth/enroll").into_response();
    Ok(with_cookie(
        with_cookie(response, config, Cookie::Flow, "", 0),
        config,
        Cookie::Enroll,
        &secret,
        ENROLL_TTL.as_secs(),
    ))
}

fn enrollment_config(state: &AppState) -> Result<&LoginConfig, Failure> {
    login_config(state).and_then(|config| {
        if config.enrollment_enabled() {
            Ok(config)
        } else {
            Err((StatusCode::NOT_FOUND, "operator enrollment is disabled"))
        }
    })
}

const NO_PENDING: Failure = (
    StatusCode::UNAUTHORIZED,
    "no sign-in is waiting for enrollment; sign in again",
);

/// `(email, subject)` of this browser's live pending enrollment.
fn pending_account(
    login: &OperatorLogin,
    config: &LoginConfig,
    headers: &HeaderMap,
) -> Option<(Option<String>, String)> {
    let secret = oidc::cookie(headers, config.enroll_cookie_name())?;
    let mut registry = login.registry();
    let now = Instant::now();
    registry
        .enrollments
        .retain(|_, pending| pending.expires > now);
    registry
        .enrollments
        .get(&secret)
        .map(|pending| (pending.email.clone(), pending.subject.clone()))
}

async fn enroll_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let config = match enrollment_config(&state) {
        Ok(config) => config,
        Err(failure) => return browser_error(&headers, failure),
    };
    let Some((email, subject)) = pending_account(&state.operator, config, &headers) else {
        return browser_error(&headers, NO_PENDING);
    };
    let account = match email {
        Some(email) => format!(
            "<strong>{}</strong> (subject <code>{}</code>)",
            html_escape(&email),
            html_escape(&subject)
        ),
        None => format!("subject <code>{}</code>", html_escape(&subject)),
    };
    let body = format!(
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><link rel=\"stylesheet\" href=\"/static/share.css\"><title>Register operator · muxa</title><main class=\"enroll\"><h1>Register this account as an operator</h1><p>Signed in as {account}. This account is not yet allowed to operate this dashboard.</p><p>Enter this dashboard's access token to register this account as an operator. It can then sign in without the token until it is removed under <em>operators</em> in the dashboard. The token is checked once and is not stored in this browser.</p><form id=\"enroll\" method=\"post\" action=\"/auth/enroll\"><label for=\"token\">Dashboard access token</label><input id=\"token\" name=\"token\" type=\"password\" autocomplete=\"off\" maxlength=\"1024\" required autofocus><button type=\"submit\">Register and sign in</button></form><p id=\"status\" role=\"status\" aria-live=\"polite\"></p><p><a href=\"/auth/enroll/cancel\">Cancel</a></p></main><script type=\"module\" src=\"/static/operator-enroll.mjs\"></script></html>"
    );
    ([(header::CONTENT_SECURITY_POLICY, ENROLL_CSP)], Html(body)).into_response()
}

#[derive(Deserialize)]
struct EnrollBody {
    token: String,
}

/// JSON error for the enrollment script. `restart` means the pending
/// enrollment is gone and only a new sign-in can continue.
fn enroll_error(status: StatusCode, message: &str, restart: bool) -> Response {
    (status, Json(json!({"error": message, "restart": restart}))).into_response()
}

/// `POST /auth/enroll`: exchange the dashboard token plus this browser's
/// pending enrollment for a durable enrollment and an operator session.
///
/// CSRF: the pending cookie is `SameSite=Lax`, which a cross-site POST never
/// carries, and the request must also bear the exact public `Origin` and
/// `X-Muxa-Operator: 1` (a cross-site page cannot set the header without a
/// preflight this server never grants). The body is JSON, never a form, so
/// the token never lands in a URL or a form-encoded access log.
async fn enroll(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let config = match enrollment_config(&state) {
        Ok(config) => config,
        Err((status, message)) => return error(status, message),
    };
    let login = &state.operator;
    if !login.csrf_ok(&Method::POST, &headers) {
        return error(StatusCode::FORBIDDEN, "same-origin request required");
    }
    let Some(secret) = oidc::cookie(&headers, config.enroll_cookie_name()) else {
        return enroll_error(NO_PENDING.0, NO_PENDING.1, true);
    };
    let Some(expected) = state.config.token.as_deref() else {
        return error(StatusCode::NOT_FOUND, "operator enrollment is unavailable");
    };
    let Ok(EnrollBody { token }) = serde_json::from_slice::<EnrollBody>(&body) else {
        return error(StatusCode::BAD_REQUEST, "expected {\"token\": \"...\"}");
    };
    let pending = {
        let mut registry = login.registry();
        let now = Instant::now();
        registry
            .enrollments
            .retain(|_, pending| pending.expires > now);
        if !registry.enrollments.contains_key(&secret) {
            return enroll_error(NO_PENDING.0, NO_PENDING.1, true);
        }
        if let Some(retry_at) = registry.backoff.retry_at().filter(|at| *at > now) {
            let wait = retry_at.duration_since(now).as_secs().max(1);
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, wait.to_string())],
                Json(json!({
                    "error": format!("too many wrong tokens; wait {wait} seconds"),
                    "restart": false,
                })),
            )
                .into_response();
        }
        // The same constant-time comparison as the Authorization header.
        if !crate::dashboard::auth::check_bearer(Some(&format!("Bearer {token}")), expected) {
            registry.backoff.fail();
            let pending = registry
                .enrollments
                .get_mut(&secret)
                .expect("checked enrollment");
            pending.attempts += 1;
            let attempts = pending.attempts;
            tracing::warn!(subject = %pending.subject, attempts, "dashboard operator enrollment refused: wrong token");
            if attempts >= MAX_ENROLL_ATTEMPTS {
                registry.enrollments.remove(&secret);
                return with_cookie(
                    enroll_error(
                        StatusCode::UNAUTHORIZED,
                        "too many wrong tokens; sign in again to retry",
                        true,
                    ),
                    config,
                    Cookie::Enroll,
                    "",
                    0,
                );
            }
            return enroll_error(
                StatusCode::UNAUTHORIZED,
                "that is not this dashboard's access token",
                false,
            );
        }
        registry.backoff = super::Backoff::default();
        registry
            .enrollments
            .remove(&secret)
            .expect("checked enrollment")
    };
    let cleared = |response: Response| with_cookie(response, config, Cookie::Enroll, "", 0);
    if let Err((status, message)) = record_enrollment(login, config, &pending).await {
        return cleared(enroll_error(status, message, true));
    }
    tracing::info!(subject = %pending.subject, "dashboard operator enrolled");
    let session = Session {
        issuer: pending.issuer,
        subject: pending.subject,
        email: pending.email,
        via: Via::Enrollment,
        expires: Instant::now() + SESSION_TTL,
    };
    let secret = start_session(login, config, &headers, session);
    with_cookie(
        cleared(Json(json!({"redirect": pending.return_to})).into_response()),
        config,
        Cookie::Session,
        &secret,
        SESSION_TTL.as_secs(),
    )
}

/// Durably record `pending` as an enrolled operator (a no-op when it already
/// is one). The session is created only after this succeeds.
async fn record_enrollment(
    login: &OperatorLogin,
    config: &LoginConfig,
    pending: &PendingEnrollment,
) -> Result<(), Failure> {
    let _writes = login.enroll_writes.lock().await;
    let entry = {
        let registry = login.registry();
        if registry
            .enrolled(config, &pending.issuer, &pending.subject)
            .is_some()
        {
            return Ok(());
        }
        if registry.enrolled.len() >= MAX_ENROLLED {
            return Err((
                StatusCode::CONFLICT,
                "the enrolled-operator limit is reached; remove one first",
            ));
        }
        let now = unix_now();
        Enrolled {
            id: uuid::Uuid::new_v4().simple().to_string(),
            issuer: pending.issuer.clone(),
            subject: pending.subject.clone(),
            email: pending.email.clone(),
            created_at: now,
            last_seen_at: now,
        }
    };
    if let Some(storage) = login.storage.clone() {
        let record = entry.clone();
        tokio::task::spawn_blocking(move || storage.insert(&record))
            .await
            .map_err(std::io::Error::other)
            .and_then(|result| result)
            .map_err(|error| {
                tracing::error!(%error, "operator storage write failed");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "operator storage unavailable; the account was not registered",
                )
            })?;
    }
    login.registry().enrolled.push(entry);
    Ok(())
}

/// Abandon this browser's pending enrollment.
async fn enroll_cancel(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let config = match login_config(&state) {
        Ok(config) => config,
        Err(failure) => return browser_error(&headers, failure),
    };
    if let Some(secret) = oidc::cookie(&headers, config.enroll_cookie_name()) {
        state.operator.registry().enrollments.remove(&secret);
    }
    with_cookie(
        Redirect::to("/").into_response(),
        config,
        Cookie::Enroll,
        "",
        0,
    )
}

// ── Enrolled-operator management ───────────────────────────────────

async fn list_operators(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let config = match login_config(&state) {
        Ok(config) => config,
        Err((status, message)) => return error(status, message),
    };
    let login = &state.operator;
    let current = login
        .session(&headers)
        .map(|session| (session.issuer, session.subject));
    let registry = login.registry();
    let operators: Vec<_> = registry
        .enrolled
        .iter()
        .map(|entry| {
            json!({
                "id": entry.id,
                "email": entry.email,
                "subject": entry.subject,
                "issuer": entry.issuer,
                "created_at": entry.created_at,
                "last_seen_at": entry.last_seen_at,
                // Entries for another issuer are kept but never match.
                "active": entry.issuer == config.issuer_url,
                "current": current.as_ref().is_some_and(|(issuer, subject)| {
                    *issuer == entry.issuer && *subject == entry.subject
                }),
            })
        })
        .collect();
    Json(json!({
        "enrollment": config.enrollment_enabled(),
        "max": MAX_ENROLLED,
        "operators": operators,
    }))
    .into_response()
}

/// Remove an enrolled account and end every live session it holds.
async fn remove_operator(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if let Err((status, message)) = login_config(&state) {
        return error(status, message);
    }
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return error(StatusCode::NOT_FOUND, "no such operator");
    }
    let login: &Arc<OperatorLogin> = &state.operator;
    let _writes = login.enroll_writes.lock().await;
    let Some(entry) = login
        .registry()
        .enrolled
        .iter()
        .find(|entry| entry.id == id)
        .cloned()
    else {
        return error(StatusCode::NOT_FOUND, "no such operator");
    };
    if let Some(storage) = login.storage.clone() {
        let id = id.clone();
        let result = tokio::task::spawn_blocking(move || storage.remove(&id))
            .await
            .map_err(std::io::Error::other)
            .and_then(|result| result);
        if let Err(error) = result {
            tracing::error!(%error, "operator storage write failed");
            return self::error(
                StatusCode::SERVICE_UNAVAILABLE,
                "operator storage unavailable; the account was not removed",
            );
        }
    }
    let ended = {
        let mut registry = login.registry();
        registry.enrolled.retain(|other| other.id != id);
        let before = registry.sessions.len();
        registry.sessions.retain(|_, session| {
            !(session.issuer == entry.issuer && session.subject == entry.subject)
        });
        before - registry.sessions.len()
    };
    tracing::info!(subject = %entry.subject, ended, "dashboard operator removed");
    Json(json!({"removed": true, "sessions_ended": ended})).into_response()
}
