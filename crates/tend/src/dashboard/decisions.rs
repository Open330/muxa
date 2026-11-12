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
//! - reads are GETs and need no CSRF proof; the two writes below are POSTs
//!   behind the control gate, so a cookie session must also send the CSRF
//!   proof (`Origin` = the public URL and `X-Tend-Operator: 1`), see
//!   [`super::operator`].
//!
//! # Writes
//!
//! Deciding goes through the same daemon stores as the Mac app:
//!
//! - `POST /api/decisions/reviews/{id}` records a review disposition
//!   (`accepted`, `returned` or `withdrawn`, with an optional note and the
//!   chosen option). Dispositions are final: repeating the same one is a
//!   no-op, a different one is refused (409).
//! - `POST /api/decisions/requests/{id}` answers a human request as the
//!   operator console. Only requests addressed to the console can be
//!   answered (the store checks the recipient), once: an identical retry is
//!   a no-op, anything else after the answer is refused (409).
//!
//! A replayed request therefore changes nothing. Every write is recorded
//! with who made it: the review disposition's actor label and an audit line
//! name `web:<email>` for a signed-in operator or `web:token` for the bearer
//! token.
//!
//! The queue covers this daemon's host only; the response says so.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

use super::server::AppState;
use crate::collaboration::CollaborationError as E;

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
/// Like every dashboard page it is never framed ([`super::assets`]).
pub(super) async fn page_handler() -> Response {
    super::assets::serve_page("needs.html")
}

const MAX_NOTE_BYTES: usize = 1024;
const MAX_ANSWER_BYTES: usize = 16 * 1024;
const MAX_AUDIT_BYTES: u64 = 4 * 1024 * 1024;

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

/// Who is deciding, as recorded: the signed-in operator's email, else the
/// bearer token.
fn operator_label(state: &AppState, headers: &HeaderMap) -> String {
    match super::operator::authenticate(state.config.token.as_deref(), &state.operator, headers) {
        Some(super::operator::OperatorAuth::Session) => {
            let email = state
                .operator
                .signed_in(headers)
                .and_then(|(email, _)| email)
                .unwrap_or_else(|| "operator".into());
            format!("web:{email}")
        }
        _ => "web:token".into(),
    }
}

/// One audit line per decision written from the web: when, who, what, and
/// how it ended. Appended to `path` (owner-only, rotated once at 4 MiB)
/// and always logged.
async fn audit(path: Option<&PathBuf>, entry: serde_json::Value) {
    tracing::info!(target: "tend::dashboard::decisions", entry = %entry, "web decision");
    let Some(path) = path else {
        return;
    };
    let mut line = entry.to_string();
    line.push('\n');
    let path = path.clone();
    let written = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        use std::io::Write as _;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if std::fs::metadata(&path).is_ok_and(|meta| meta.len() > MAX_AUDIT_BYTES) {
            let mut rotated = path.clone().into_os_string();
            rotated.push(".1");
            std::fs::rename(&path, rotated)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&path)?.write_all(line.as_bytes())
    })
    .await;
    if !matches!(written, Ok(Ok(()))) {
        tracing::warn!(target: "tend::dashboard::decisions", "web decision audit line not written");
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReviewDecision {
    disposition: crate::review::DispositionKind,
    #[serde(default)]
    note: Option<String>,
    /// The option chosen from the item's decision, recorded in the note.
    #[serde(default)]
    option: Option<String>,
}

/// `POST /api/decisions/reviews/{id}`: accept, return or withdraw a review
/// item as the operator.
pub(super) async fn review_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(decision): Json<ReviewDecision>,
) -> Response {
    let by = operator_label(&state, &headers);
    let item = match state.reviews.get(id.clone()).await {
        Ok(item) => item,
        Err(crate::review::ReviewError::NotFound(_)) => {
            return error(StatusCode::NOT_FOUND, format!("no review item {id}"));
        }
        Err(other) => return error(StatusCode::SERVICE_UNAVAILABLE, other.to_string()),
    };
    if let Some(option) = &decision.option {
        let offered = item
            .decision
            .as_ref()
            .is_some_and(|d| d.options.iter().any(|o| o == option));
        if !offered {
            return error(
                StatusCode::BAD_REQUEST,
                "that option is not one this item offers",
            );
        }
    }
    let note = match (
        decision.option.as_deref(),
        decision.note.as_deref().map(str::trim),
    ) {
        (Some(option), Some(note)) if !note.is_empty() => Some(format!("chose: {option}\n{note}")),
        (Some(option), _) => Some(format!("chose: {option}")),
        (None, Some(note)) if !note.is_empty() => Some(note.to_owned()),
        (None, _) => None,
    };
    if note
        .as_ref()
        .is_some_and(|note| note.len() > MAX_NOTE_BYTES)
    {
        return error(
            StatusCode::BAD_REQUEST,
            format!("note exceeds {MAX_NOTE_BYTES} bytes"),
        );
    }
    let result = state
        .reviews
        .dispose(
            id.clone(),
            decision.disposition,
            crate::review::ReviewActor {
                kind: crate::review::ActorKind::User,
                label: Some(by.clone()),
            },
            note,
        )
        .await;
    let (status, outcome) = match &result {
        Ok(_) => (StatusCode::OK, "recorded".to_owned()),
        Err(crate::review::ReviewError::Conflict(message)) => {
            (StatusCode::CONFLICT, message.clone())
        }
        Err(crate::review::ReviewError::Invalid(message)) => {
            (StatusCode::BAD_REQUEST, message.clone())
        }
        Err(other) => (StatusCode::SERVICE_UNAVAILABLE, other.to_string()),
    };
    audit(
        state.decision_audit.as_ref(),
        json!({
            "at": time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).ok(),
            "by": by, "action": "review", "id": id,
            "disposition": decision.disposition, "option": decision.option,
            "outcome": outcome, "status": status.as_u16(),
        }),
    )
    .await;
    match result {
        Ok(item) => Json(json!({ "review": item })).into_response(),
        Err(_) => error(status, outcome),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HumanAnswer {
    answer: String,
    /// `completed` (the default) for an answer, `declined` for a refusal.
    #[serde(default)]
    declined: bool,
}

/// `POST /api/decisions/requests/{id}`: answer a human request as the
/// operator console.
pub(super) async fn answer_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(answer): Json<HumanAnswer>,
) -> Response {
    let by = operator_label(&state, &headers);
    if answer.answer.len() > MAX_ANSWER_BYTES {
        return error(
            StatusCode::BAD_REQUEST,
            format!("answer exceeds {MAX_ANSWER_BYTES} bytes"),
        );
    }
    let status = if answer.declined {
        crate::collaboration::RequestStatus::Declined
    } else {
        crate::collaboration::RequestStatus::Completed
    };
    // The console of any room is the operator: the store checks that the
    // request is addressed to the console at all.
    let console = crate::collaboration::Participant::console(crate::collaboration::RoomId {
        host: "web".into(),
        socket: None,
        window_id: "dashboard".into(),
    });
    let result = state
        .collaboration
        .reply(&console, &id, status, answer.answer, Vec::new())
        .await;
    let (code, outcome) = match &result {
        Ok(_) => (StatusCode::OK, "recorded".to_owned()),
        Err(e @ E::NotFound(_)) => (StatusCode::NOT_FOUND, e.to_string()),
        Err(e @ E::NotParticipant(_)) => (StatusCode::FORBIDDEN, e.to_string()),
        Err(e @ E::AlreadyTerminal(_)) => (StatusCode::CONFLICT, e.to_string()),
        Err(e @ (E::EmptyMessage | E::MessageTooLarge(_) | E::InvalidReplyStatus)) => {
            (StatusCode::BAD_REQUEST, e.to_string())
        }
        Err(other) => (StatusCode::SERVICE_UNAVAILABLE, other.to_string()),
    };
    audit(
        state.decision_audit.as_ref(),
        json!({
            "at": time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).ok(),
            "by": by, "action": "answer", "id": id, "declined": answer.declined,
            "outcome": outcome, "status": code.as_u16(),
        }),
    )
    .await;
    match result {
        Ok(request) => Json(json!({
            "request_id": request.id,
            "status": request.status,
        }))
        .into_response(),
        Err(_) => error(code, outcome),
    }
}
