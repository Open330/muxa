use super::routes::{error, sharing_config};
use super::{
    normalize_email, random_secret, PendingLogin, Session, Sharing, SharingConfig, LOGIN_TTL,
    MAX_SESSIONS, SESSION_TTL,
};
use crate::dashboard::oidc;
use crate::dashboard::server::AppState;
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use openidconnect::{
    core::CoreAuthenticationFlow, AuthorizationCode, CsrfToken, EmptyAdditionalClaims, Nonce,
    PkceCodeChallenge, Scope, TokenResponse,
};
use serde::Deserialize;
use std::time::Instant;
use subtle::ConstantTimeEq;

type Client = oidc::Client<EmptyAdditionalClaims>;

impl Sharing {
    pub(super) async fn client(&self) -> Result<(Client, reqwest::Client), ()> {
        self.provider.as_ref().ok_or(())?.client().await
    }
}

pub(super) fn cookie_name(config: &SharingConfig, flow: bool) -> &'static str {
    match (config.secure(), flow) {
        (true, false) => "__Host-muxa-share",
        (true, true) => "__Host-muxa-login",
        (false, false) => "muxa-share-dev",
        (false, true) => "muxa-login-dev",
    }
}

pub(super) fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    oidc::cookie(headers, name)
}

fn with_cookie(
    mut response: Response,
    config: &SharingConfig,
    flow: bool,
    value: &str,
    max_age: u64,
) -> Response {
    let secure = if config.secure() { "; Secure" } else { "" };
    let text = format!(
        "{}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}",
        cookie_name(config, flow)
    );
    response
        .headers_mut()
        .append(header::SET_COOKIE, text.parse().expect("generated cookie"));
    response
}

#[derive(Clone, Deserialize)]
pub(super) struct LoginQuery {
    share: String,
}

async fn begin_inner(State(state): State<AppState>, Query(query): Query<LoginQuery>) -> Response {
    let config = match sharing_config(&state) {
        Ok(config) => config,
        Err((status, message)) => return error(status, message),
    };
    let Ok(_permit) = state.sharing.login_slots.try_acquire() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "login is busy; try again");
    };
    {
        let grant = state
            .sharing
            .registry
            .lock()
            .await
            .grants
            .get(&query.share)
            .cloned();
        let Some(grant) = grant else {
            return error(StatusCode::NOT_FOUND, "share unavailable");
        };
        let grant = grant.lock().await;
        if grant.revoked || grant.expires <= Instant::now() {
            return error(StatusCode::NOT_FOUND, "share unavailable");
        }
    }
    let Ok((client, _http)) = state.sharing.client().await else {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "login provider unavailable or not configured",
        );
    };
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, csrf, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("email".into()))
        .add_extra_param("prompt", "select_account")
        .set_pkce_challenge(challenge)
        .url();
    let browser = random_secret();
    let mut registry = state.sharing.registry.lock().await;
    registry
        .logins
        .retain(|_, flow| flow.expires > Instant::now());
    if registry.logins.len() >= 128
        || registry
            .logins
            .values()
            .filter(|flow| flow.share == query.share)
            .count()
            >= 8
    {
        return error(StatusCode::TOO_MANY_REQUESTS, "too many pending logins");
    }
    registry.logins.insert(
        csrf.secret().clone(),
        PendingLogin {
            browser: browser.clone(),
            share: query.share,
            nonce,
            verifier,
            expires: Instant::now() + LOGIN_TTL,
        },
    );
    with_cookie(
        Redirect::to(url.as_str()).into_response(),
        config,
        true,
        &browser,
        LOGIN_TTL.as_secs(),
    )
}

#[derive(Deserialize)]
pub(super) struct CallbackQuery {
    code: Option<String>,
    state: String,
}

async fn callback_inner(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let config = match sharing_config(&state) {
        Ok(config) => config,
        Err((status, message)) => return error(status, message),
    };
    if query
        .code
        .as_ref()
        .is_none_or(|code| code.is_empty() || code.len() > 4096)
        || query.state.len() > 256
    {
        return error(StatusCode::BAD_REQUEST, "invalid login response");
    }
    let Some(browser) = cookie(&headers, cookie_name(config, true)) else {
        return error(StatusCode::UNAUTHORIZED, "login expired; start again");
    };
    let Ok(_permit) = state.sharing.login_slots.try_acquire() else {
        return error(StatusCode::TOO_MANY_REQUESTS, "login is busy; try again");
    };
    let pending = {
        let mut registry = state.sharing.registry.lock().await;
        let valid = registry.logins.get(&query.state).is_some_and(|flow| {
            flow.expires > Instant::now()
                && bool::from(flow.browser.as_bytes().ct_eq(browser.as_bytes()))
        });
        if !valid {
            return error(StatusCode::UNAUTHORIZED, "invalid login state");
        }
        registry.logins.remove(&query.state).expect("checked flow")
    };
    let Ok(session) = authenticate(
        &state.sharing,
        query.code.as_deref().expect("checked code"),
        &pending,
    )
    .await
    else {
        return error(StatusCode::UNAUTHORIZED, "login verification failed");
    };
    let grant = state
        .sharing
        .registry
        .lock()
        .await
        .grants
        .get(&pending.share)
        .cloned();
    let Some(grant) = grant else {
        return error(StatusCode::NOT_FOUND, "share unavailable");
    };
    let mut grant = grant.lock_owned().await;
    if let Err((status, message)) = super::routes::authorize(&mut grant, &session, false) {
        return error(status, message);
    }
    match state.sharing.persist(grant).await {
        Ok(guard) => drop(guard),
        Err((status, message)) => return error(status, message),
    }
    let mut registry = state.sharing.registry.lock().await;
    registry
        .sessions
        .retain(|_, session| session.expires > Instant::now());
    if registry.sessions.len() >= MAX_SESSIONS {
        return error(StatusCode::TOO_MANY_REQUESTS, "too many active sessions");
    }
    // Replace the browser's previous session, rather than retaining a fixation target.
    if let Some(previous) = cookie(&headers, cookie_name(config, false)) {
        registry.sessions.remove(&previous);
    }
    let secret = random_secret();
    registry.sessions.insert(secret.clone(), session);
    let response = Redirect::to(&format!("/share/{}", pending.share)).into_response();
    with_cookie(
        with_cookie(response, config, true, "", 0),
        config,
        false,
        &secret,
        SESSION_TTL.as_secs(),
    )
}

async fn authenticate(sharing: &Sharing, code: &str, flow: &PendingLogin) -> Result<Session, ()> {
    let (client, http) = sharing.client().await?;
    let tokens = client
        .exchange_code(AuthorizationCode::new(code.to_owned()))
        .map_err(|_| ())?
        .set_pkce_verifier(openidconnect::PkceCodeVerifier::new(
            flow.verifier.secret().clone(),
        ))
        .request_async(&http)
        .await
        .map_err(|_| ())?;
    let token = tokens.id_token().ok_or(())?;
    let verifier = client.id_token_verifier();
    let claims = token.claims(&verifier, &flow.nonce).map_err(|_| ())?;
    oidc::check_access_token_hash(&tokens, token, claims.access_token_hash(), &verifier)?;
    if claims.email_verified() != Some(true) {
        return Err(());
    }
    let email = normalize_email(claims.email().ok_or(())?.as_str()).map_err(|_| ())?;
    Ok(Session {
        subject: claims.subject().as_str().to_owned(),
        email,
        expires: Instant::now() + SESSION_TTL,
    })
}

pub(super) async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let config = match sharing_config(&state) {
        Ok(config) => config,
        Err((status, message)) => return error(status, message),
    };
    if let Some(secret) = cookie(&headers, cookie_name(config, false)) {
        state.sharing.registry.lock().await.sessions.remove(&secret);
    }
    with_cookie(StatusCode::NO_CONTENT.into_response(), config, false, "", 0)
}

pub(super) async fn logout_all(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let config = match sharing_config(&state) {
        Ok(config) => config,
        Err((status, message)) => return error(status, message),
    };
    let mut registry = state.sharing.registry.lock().await;
    if let Some(secret) = cookie(&headers, cookie_name(config, false)) {
        if let Some(current) = registry
            .sessions
            .get(&secret)
            .filter(|s| s.expires > Instant::now())
            .cloned()
        {
            registry
                .sessions
                .retain(|_, session| session.subject != current.subject);
        }
    }
    with_cookie(StatusCode::NO_CONTENT.into_response(), config, false, "", 0)
}

fn browser_result(response: Response, headers: &HeaderMap, share: Option<&str>) -> Response {
    if response.status().is_success()
        || response.status().is_redirection()
        || !headers
            .get(header::ACCEPT)
            .and_then(|h| h.to_str().ok())
            .is_some_and(|accept| accept.contains("text/html"))
    {
        return response;
    }
    if let Some(share) = share.filter(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Redirect::to(&format!("/share/{share}#login-error")).into_response();
    }
    (response.status(), axum::response::Html("<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><link rel=\"stylesheet\" href=\"/static/share.css\"><title>Sign-in failed · muxa</title><main><h1>Sign-in could not be completed</h1><p>The login may have expired or been cancelled. Open your invitation link again and sign in with the invited account.</p></main></html>")).into_response()
}

pub(super) async fn begin(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<LoginQuery>,
) -> Response {
    let share = query.share.clone();
    let response = begin_inner(State(state), Query(query)).await;
    browser_result(response, &headers, Some(&share))
}

pub(super) async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let share = state
        .sharing
        .registry
        .lock()
        .await
        .logins
        .get(&query.state)
        .map(|flow| flow.share.clone());
    let response = callback_inner(State(state), headers.clone(), Query(query)).await;
    browser_result(response, &headers, share.as_deref())
}
