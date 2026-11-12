//! "Needs you": the decision queue and its history on the web dashboard.
//!
//! The same daemon reads the Mac app and `tend review queue` use
//! ([`crate::decision_queue`]): human requests, agents waiting on input and
//! open review items, in one ordered list, and past decisions.
//!
//! # Authorization
//!
//! These routes are operator-only, reads included. A queue entry carries
//! what agents asked the person (question text, options, evidence paths),
//! which the redacted public and viewer views never show, so they sit
//! behind the same gate as pane output:
//!
//! - the dashboard bearer token, or a live OIDC **operator** session;
//! - a viewer session gets 403, an anonymous visitor 401, in every auth mode
//!   (`public_read` and `auth = "none"` included: with no token configured
//!   the gate refuses everyone);
//! - reads are GETs and need no CSRF proof; anything that changes state
//!   (added separately) additionally needs the session's CSRF proof
//!   (`Origin` = the public URL and `X-Tend-Operator: 1`), see
//!   [`super::operator`].
//!
//! The queue covers this daemon's host only; the response says so.

use axum::{
    extract::{Query, State},
    response::{IntoResponse, Json, Response},
};
use serde::Deserialize;
use serde_json::json;

use super::server::AppState;

const DEFAULT_LIMIT: usize = 100;

#[derive(Debug, Deserialize)]
pub(super) struct DecisionsQuery {
    #[serde(default)]
    limit: Option<usize>,
}

/// `GET /api/decisions`: what is waiting on the person, in queue order.
pub(super) async fn queue_handler(
    State(state): State<AppState>,
    Query(query): Query<DecisionsQuery>,
) -> Response {
    let agents = state.store.snapshot().await;
    let queue = crate::decision_queue::read_pending(
        &state.reviews,
        &state.collaboration,
        &agents,
        query.limit.unwrap_or(DEFAULT_LIMIT),
        true,
    )
    .await;
    Json(json!({ "covers": "this host", "queue": queue })).into_response()
}

/// `GET /api/decisions/history`: past decisions, newest first.
pub(super) async fn history_handler(
    State(state): State<AppState>,
    Query(query): Query<DecisionsQuery>,
) -> Response {
    let history = crate::decision_queue::read_history(
        &state.reviews,
        &state.collaboration,
        query.limit.unwrap_or(DEFAULT_LIMIT),
        true,
    )
    .await;
    Json(json!({ "covers": "this host", "history": history })).into_response()
}

/// `GET /needs`: the phone-first page that renders the two reads above.
pub(super) async fn page_handler() -> Response {
    super::assets::serve_page("needs.html")
}
