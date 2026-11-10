use crate::dashboard::server::AppState;
use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::json;

pub(super) async fn status(State(state): State<AppState>) -> Response {
    let config = state.sharing.config.as_ref();
    let secret_ready = config.is_some_and(|config| {
        config
            .client_secret_env
            .as_ref()
            .is_none_or(|name| std::env::var(name).is_ok_and(|value| !value.trim().is_empty()))
    });
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "configured":config.is_some(),
            "durable":state.sharing.storage.is_some(),
            "client_secret_ready":secret_ready,
            "callback_url":config.map(|c|format!("{}/share/auth/callback",c.origin())),
            "issuer_url":config.map(|c|&c.issuer_url),
            "sessions_survive_restart":false,
            "max_share_hours":24
        })),
    )
        .into_response()
}

pub(super) async fn check(State(state): State<AppState>) -> Response {
    if state.sharing.config.is_none() {
        return super::routes::error(
            StatusCode::NOT_FOUND,
            "configure dashboard.sharing before checking login",
        );
    }
    let Ok(_permit) = state.sharing.login_slots.try_acquire() else {
        return super::routes::error(StatusCode::TOO_MANY_REQUESTS, "login check busy; try again");
    };
    match state.sharing.client().await {
        Ok(_) => ([(header::CACHE_CONTROL, "no-store")], Json(json!({"ok":true,"message":"Discovery and signing keys are reachable. Complete a test invitation to verify the account claims and callback."}))).into_response(),
        Err(()) => super::routes::error(StatusCode::SERVICE_UNAVAILABLE, "check issuer discovery, HTTPS reachability, client ID and the client secret environment variable"),
    }
}
