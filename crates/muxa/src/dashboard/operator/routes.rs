use super::{
    claim_groups, ExtraClaims, LoginConfig, OperatorLogin, PendingLogin, Session, LOGIN_TTL,
    MAX_PENDING_LOGINS, MAX_SESSIONS, SESSION_TTL,
};
use crate::dashboard::oidc;
use crate::dashboard::server::AppState;
use axum::{
    extract::{Query, Request, State},
    http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode},
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
use std::time::Instant;
use subtle::ConstantTimeEq;

type Failure = (StatusCode, &'static str);

/// `/auth/*`. Mounted outside the operator auth layers: these routes are how
/// a browser obtains operator access in the first place.
pub(in crate::dashboard) fn routes() -> Router<AppState> {
    Router::new()
        .route("/auth/login", get(begin))
        .route("/auth/callback", get(callback))
        .route("/auth/logout", post(logout))
        .route("/auth/session", get(status))
        .layer(middleware::from_fn(security_headers))
}

async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    // Never let a proxy or the browser cache a login redirect or session
    // state, and never leak the callback's code/state in a Referer.
    for (name, value) in [
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        (
            "content-security-policy",
            "default-src 'none'; style-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
        ),
    ] {
        response.headers_mut().insert(
            HeaderName::from_static(name),
            HeaderValue::from_static(value),
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

fn with_cookie(
    mut response: Response,
    config: &LoginConfig,
    flow: bool,
    value: &str,
    max_age: u64,
) -> Response {
    let (session_name, flow_name) = config.cookie_names();
    let secure = if config.secure() { "; Secure" } else { "" };
    // The flow cookie must survive the IdP's top-level redirect back to us,
    // which is cross-site, so it is Lax. The session cookie is never needed
    // on a cross-site request: the dashboard HTML is public and every API
    // call is a same-origin fetch.
    let (name, same_site) = if flow {
        (flow_name, "Lax")
    } else {
        (session_name, "Strict")
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
        true,
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
    let session = verify(login, config, &code, &pending).await?;
    tracing::info!(subject = %session.subject, "dashboard operator signed in");
    let secret = oidc::random_secret();
    {
        let mut registry = login.registry();
        let now = Instant::now();
        registry.sessions.retain(|_, session| session.expires > now);
        // Replace this browser's previous session rather than keeping a
        // fixation target alive next to the new one.
        if let Some(previous) = oidc::cookie(headers, config.cookie_names().0) {
            registry.sessions.remove(&previous);
        }
        // Only group members get here, so evicting the oldest session keeps
        // the owner able to sign in instead of locking them out.
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
    }
    let response = Redirect::to(&pending.return_to).into_response();
    Ok(with_cookie(
        with_cookie(response, config, true, "", 0),
        config,
        false,
        &secret,
        SESSION_TTL.as_secs(),
    ))
}

async fn verify(
    login: &OperatorLogin,
    config: &LoginConfig,
    code: &str,
    flow: &PendingLogin,
) -> Result<Session, Failure> {
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
    // Authorization is group membership alone; email is only for display,
    // so it need not be verified.
    let groups = claim_groups(claims.additional_claims().claims.get(config.groups_claim()));
    if !groups.iter().any(|group| *group == config.required_group) {
        tracing::warn!(%subject, "dashboard sign-in refused: account is not in the operator group");
        return Err((
            StatusCode::FORBIDDEN,
            "this account is not allowed to operate this dashboard",
        ));
    }
    let email = claims
        .email()
        .map(|email| email.as_str().to_owned())
        .filter(|email| email.len() <= 254 && !email.chars().any(char::is_control));
    Ok(Session {
        subject,
        email,
        expires: Instant::now() + SESSION_TTL,
    })
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let config = match login_config(&state) {
        Ok(config) => config,
        Err((status, message)) => return error(status, message),
    };
    if !state.operator.csrf_ok(&axum::http::Method::POST, &headers) {
        return error(StatusCode::FORBIDDEN, "same-origin request required");
    }
    if let Some(secret) = oidc::cookie(&headers, config.cookie_names().0) {
        state.operator.registry().sessions.remove(&secret);
    }
    with_cookie(StatusCode::NO_CONTENT.into_response(), config, false, "", 0)
}

/// Unauthenticated: a signed-out browser needs to learn that it can sign in.
async fn status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    Json(super::super::server::login_status(&state, &headers)).into_response()
}
