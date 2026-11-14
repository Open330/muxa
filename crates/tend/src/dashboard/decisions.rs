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
//! # Fleet
//!
//! The queue is per host. This host contributes everything (questions,
//! waiting agents, review items). A connected Fleet host whose relay
//! answers `fleet_decision_queue_v1` contributes its whole queue, read there
//! as its operator console. An older relay contributes the questions in its
//! operator mailbox (only its newest messages, so a full read is marked
//! possibly incomplete) and its waiting agents from the Fleet snapshot, but
//! no review items. A review item on another host is decided here only when
//! that host is in control mode and its relay applies decisions exactly
//! (`fleet_review_dispose_v1`, the section's `decides_reviews`); otherwise
//! it is shown and decided on its own host. Every host section names what it
//! covers and what could not be read, and a host that is not online is
//! listed as such, so nothing is silently missing.
//!
//! A remote question is answered over Fleet only on a control-mode host,
//! and only after reading the request back from that host and checking it
//! is addressed to the operator console: a Fleet reply to anything else
//! would be sent as the agent in that pane. The history covers this host.
//!
//! A remote review decision names its host; the host's relay checks the
//! option against its own copy of the item, records the person (`by`) and
//! the composed note, and reports what its store holds after: the same
//! decision again is a replay, anything else already recorded a conflict,
//! and an answer about any other item id is refused. Only the dashboard
//! sends it: the IPC `fleet_command` path refuses the operation, so an
//! agent on this host cannot decide a result on another one.

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
};
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;

use super::server::AppState;
use crate::collaboration::CollaborationError as E;

const DEFAULT_LIMIT: usize = 100;

#[derive(Debug, Deserialize)]
pub(super) struct DecisionsQuery {
    #[serde(default)]
    limit: Option<usize>,
}

const FLEET_READ_TIMEOUT: Duration = Duration::from_secs(6);
const FLEET_WRITE_TIMEOUT: Duration = Duration::from_secs(20);

/// The routing envelope for operator-console reads and replies on a Fleet
/// host: not a terminal, so it works after the host's last pane closed.
fn console_route() -> crate::PaneKey {
    crate::PaneKey {
        window: crate::WindowKey {
            session: crate::SessionKey {
                endpoint: crate::BackendEndpoint {
                    host: crate::backend::HostKind::Tmux,
                    socket: "default".into(),
                },
                session_id: "console".into(),
            },
            window_id: "console".into(),
        },
        pane_id: "console".into(),
    }
}

async fn fleet_hosts(state: &AppState) -> Vec<crate::fleet::FleetHostSnapshot> {
    match &state.fleet {
        Some(fleet) => fleet.store.snapshot_selected(None).await.hosts,
        None => Vec::new(),
    }
}

/// This host's Fleet alias, when the Fleet manager knows one.
fn local_alias(hosts: &[crate::fleet::FleetHostSnapshot]) -> String {
    hosts
        .iter()
        .find(|host| host.local)
        .map_or_else(|| "this host".into(), |host| host.alias.clone())
}

fn state_label(state: crate::fleet::FleetHostState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

/// One Fleet host's part of the queue.
async fn remote_section(
    fleet: &crate::fleet::FleetRuntime,
    host: crate::fleet::FleetHostSnapshot,
    limit: usize,
) -> serde_json::Value {
    use crate::fleet::{FleetHostState, FleetOperation, HostAccessMode};
    let control = host.mode == HostAccessMode::Control;
    let reviews = decides_reviews(&host);
    if !matches!(
        host.state,
        FleetHostState::Online | FleetHostState::Degraded
    ) {
        return json!({
            "alias": host.alias, "local": false, "control": control,
            "covers": [], "unavailable": [format!("host is {}", state_label(host.state))],
            "queue": crate::decision_queue::assemble_queue(&[], &[], Vec::new(), limit),
        });
    }
    // A current relay sends its host's whole queue, read there as the
    // operator console: questions, waiting agents and review items.
    if host
        .capabilities
        .iter()
        .any(|c| c == crate::fleet::FLEET_DECISION_QUEUE_CAPABILITY)
    {
        return match fleet
            .execute(
                host.alias.clone(),
                FleetOperation::DecisionQueue { limit },
                FLEET_READ_TIMEOUT,
            )
            .await
        {
            Ok(result) => {
                let queue = result.decision_queue.map_or_else(
                    || crate::decision_queue::assemble_queue(&[], &[], Vec::new(), limit),
                    |queue| *queue,
                );
                json!({
                    "alias": host.alias, "local": false, "control": control,
                    "decides_reviews": reviews,
                    "covers": ["questions", "waiting agents", "review items"],
                    "unavailable": queue.unavailable.clone(), "queue": queue,
                })
            }
            Err(error) => json!({
                "alias": host.alias, "local": false, "control": control,
                "covers": [], "unavailable": [format!("decision queue: {error}")],
                "queue": crate::decision_queue::assemble_queue(&[], &[], Vec::new(), limit),
            }),
        };
    }
    // An older relay: questions from its mailbox, which carries only the
    // newest messages, and agents from the Fleet snapshot.
    let agents = host
        .remote
        .as_ref()
        .map(|remote| remote.agents.clone())
        .unwrap_or_default();
    let mut covers = vec!["waiting agents"];
    let mut unavailable = vec!["review items: not read over Fleet".to_owned()];
    let human = match fleet
        .execute(
            host.alias.clone(),
            FleetOperation::CollaborationMailbox {
                pane: console_route(),
                operator_only: true,
                if_unchanged: None,
            },
            FLEET_READ_TIMEOUT,
        )
        .await
    {
        Ok(result) => {
            covers.insert(0, "questions");
            // The mailbox read stops at the newest messages, answered ones
            // included: when it is full, an older open question may be
            // past the cut, so the section says it may be incomplete.
            if result.collaboration_incoming.len() >= crate::fleet::FLEET_MAILBOX_REQUEST_LIMIT {
                unavailable.insert(
                    0,
                    "older questions: this host's tend sends only its newest messages; upgrade it for the full queue".to_owned(),
                );
            }
            // Only what is addressed to the operator console counts.
            result
                .collaboration_incoming
                .into_iter()
                .filter(|request| request.to.console)
                .collect()
        }
        Err(error) => {
            unavailable.insert(0, format!("questions: {error}"));
            Vec::new()
        }
    };
    let mut queue = crate::decision_queue::assemble_queue(&human, &agents, Vec::new(), limit);
    queue.truncated |= unavailable
        .iter()
        .any(|note| note.starts_with("older questions"));
    json!({
        "alias": host.alias, "local": false, "control": control,
        "covers": covers, "unavailable": unavailable, "queue": queue,
    })
}

/// `GET /api/decisions`: what is waiting on the person, per host, each in
/// queue order.
pub(super) async fn queue_handler(
    State(state): State<AppState>,
    Query(query): Query<DecisionsQuery>,
) -> Response {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    let agents = state.store.snapshot().await;
    let local = crate::decision_queue::read_pending(
        &state.reviews,
        &state.collaboration,
        &agents,
        limit,
        true,
    )
    .await;
    let fleet_hosts = fleet_hosts(&state).await;
    let mut hosts = vec![json!({
        "alias": local_alias(&fleet_hosts), "local": true, "control": true,
        "decides_reviews": true,
        "covers": ["questions", "waiting agents", "review items"],
        "unavailable": local.unavailable.clone(), "queue": local,
    })];
    if let Some(fleet) = &state.fleet {
        let sections = futures::future::join_all(
            fleet_hosts
                .into_iter()
                .filter(|host| !host.local)
                .map(|host| remote_section(fleet, host, limit)),
        )
        .await;
        hosts.extend(sections);
    }
    Json(json!({ "hosts": hosts })).into_response()
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
    let covers = local_alias(&fleet_hosts(&state).await);
    Json(json!({ "covers": covers, "history": history })).into_response()
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
/// The display label (`web:<email>`, `web:token`) and, for a session, the
/// identity that authorized it (issuer and subject) for the audit line: an
/// email is shown, not trusted, and may be absent.
fn operator_identity(state: &AppState, headers: &HeaderMap) -> (String, serde_json::Value) {
    match super::operator::authenticate(state.config.token.as_deref(), &state.operator, headers) {
        Some(super::operator::OperatorAuth::Session) => match state.operator.account(headers) {
            Some((issuer, subject, email)) => (
                format!("web:{}", email.as_deref().unwrap_or(&subject)),
                json!({ "issuer": issuer, "subject": subject, "email": email }),
            ),
            None => ("web:operator".into(), serde_json::Value::Null),
        },
        _ => ("web:token".into(), json!({ "credential": "token" })),
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
    /// The Fleet host the item is on; absent (or this host's alias) for
    /// one filed here.
    #[serde(default)]
    host: Option<String>,
}

/// `POST /api/decisions/reviews/{id}`: accept, return or withdraw a review
/// item as the operator, here or on the Fleet host that holds it.
pub(super) async fn review_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(decision): Json<ReviewDecision>,
) -> Response {
    let (by, account) = operator_identity(&state, &headers);
    let note = crate::review::decision_note(decision.option.as_deref(), decision.note.as_deref());
    if note
        .as_ref()
        .is_some_and(|note| note.len() > MAX_NOTE_BYTES)
    {
        return error(
            StatusCode::BAD_REQUEST,
            format!("note exceeds {MAX_NOTE_BYTES} bytes"),
        );
    }
    let hosts = fleet_hosts(&state).await;
    let remote = decision
        .host
        .as_deref()
        .filter(|alias| *alias != local_alias(&hosts) && *alias != "this host")
        .map(str::to_owned);
    let (status, outcome, item) = match &remote {
        Some(alias) => review_remote(&state, &hosts, alias, &id, &decision, &by).await,
        None => review_local(&state, &id, &decision, note, &by).await,
    };
    audit(
        state.decision_audit.as_ref(),
        json!({
            "at": time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).ok(),
            "by": by, "account": account, "action": "review", "host": remote, "id": id,
            "disposition": decision.disposition, "option": decision.option,
            "outcome": outcome, "status": status.as_u16(),
        }),
    )
    .await;
    match item {
        Some(item) if status == StatusCode::OK => {
            Json(json!({ "review": item, "host": remote })).into_response()
        }
        _ => error(status, outcome),
    }
}

type ReviewResult = (StatusCode, String, Option<Box<crate::review::ReviewItem>>);

/// A review item filed on this host, decided in its store.
async fn review_local(
    state: &AppState,
    id: &str,
    decision: &ReviewDecision,
    note: Option<String>,
    by: &str,
) -> ReviewResult {
    let item = match state.reviews.get(id.to_owned()).await {
        Ok(item) => item,
        Err(crate::review::ReviewError::NotFound(_)) => {
            return (StatusCode::NOT_FOUND, format!("no review item {id}"), None);
        }
        Err(other) => return (StatusCode::SERVICE_UNAVAILABLE, other.to_string(), None),
    };
    if decision
        .option
        .as_ref()
        .is_some_and(|option| !item.offers(option))
    {
        return (
            StatusCode::BAD_REQUEST,
            "that option is not one this item offers".into(),
            None,
        );
    }
    let result = state
        .reviews
        .dispose_exact(
            id.to_owned(),
            decision.disposition,
            crate::review::ReviewActor {
                kind: crate::review::ActorKind::User,
                label: Some(by.to_owned()),
            },
            note,
        )
        .await;
    match result {
        Ok((item, false)) => (StatusCode::OK, "recorded".into(), Some(Box::new(item))),
        // The same decision again: nothing changed, and the audit says so.
        Ok((item, true)) => (
            StatusCode::OK,
            "already recorded".into(),
            Some(Box::new(item)),
        ),
        Err(crate::review::ReviewError::Conflict(message)) => (StatusCode::CONFLICT, message, None),
        Err(crate::review::ReviewError::Invalid(message)) => {
            (StatusCode::BAD_REQUEST, message, None)
        }
        Err(other) => (StatusCode::SERVICE_UNAVAILABLE, other.to_string(), None),
    }
}

/// A review item on another Fleet host, decided there over Fleet
/// (`fleet_review_dispose_v1`): only on a control-mode host, the option
/// checked and the note composed by that host's relay against its own copy
/// of the item, with the same final, exact semantics as here.
async fn review_remote(
    state: &AppState,
    hosts: &[crate::fleet::FleetHostSnapshot],
    alias: &str,
    id: &str,
    decision: &ReviewDecision,
    by: &str,
) -> ReviewResult {
    use crate::fleet::{FleetOperation, FleetReviewDispose, FleetReviewOutcome, HostAccessMode};
    let Some(fleet) = &state.fleet else {
        return (
            StatusCode::NOT_FOUND,
            "Fleet is not enabled here".into(),
            None,
        );
    };
    let Some(host) = hosts.iter().find(|host| host.alias == alias && !host.local) else {
        return (
            StatusCode::NOT_FOUND,
            format!("no Fleet host {alias}"),
            None,
        );
    };
    if host.mode != HostAccessMode::Control {
        return (
            StatusCode::FORBIDDEN,
            format!("{alias} is observe-only; decide it there, or set mode = \"control\""),
            None,
        );
    }
    if !decides_reviews(host) {
        return (
            StatusCode::CONFLICT,
            format!("{alias} cannot decide review items over Fleet; upgrade tend there, or decide it on that host"),
            None,
        );
    }
    let operation = FleetOperation::ReviewDispose {
        dispose: FleetReviewDispose {
            id: id.to_owned(),
            disposition: decision.disposition,
            by: by.to_owned(),
            option: decision.option.clone(),
            note: decision.note.clone(),
        },
    };
    let outcome = match fleet
        .execute(alias.to_owned(), operation, FLEET_WRITE_TIMEOUT)
        .await
    {
        Ok(result) => result.review_dispose,
        Err(error) => return (StatusCode::BAD_GATEWAY, format!("{alias}: {error}"), None),
    };
    match outcome {
        // The host must report the item it was asked about; anything else
        // is not taken as this decision.
        Some(FleetReviewOutcome::Recorded { item, .. }) if item.id != id => (
            StatusCode::BAD_GATEWAY,
            format!("{alias} answered for review {} instead of {id}", item.id),
            None,
        ),
        // Nor an item that does not hold this decision, or a new
        // recording made in someone else's name.
        Some(FleetReviewOutcome::Recorded { item, replay }) => {
            let note =
                crate::review::decision_note(decision.option.as_deref(), decision.note.as_deref());
            let ours = item.disposition.as_ref().is_some_and(|d| {
                d.by.kind == crate::review::ActorKind::User && d.by.label.as_deref() == Some(by)
            });
            if !item.decided_as(decision.disposition, note.as_deref()) || (!replay && !ours) {
                (
                    StatusCode::BAD_GATEWAY,
                    format!("{alias} reported review {id} without this decision"),
                    None,
                )
            } else {
                (
                    StatusCode::OK,
                    if replay {
                        "already recorded"
                    } else {
                        "recorded"
                    }
                    .into(),
                    Some(item),
                )
            }
        }
        Some(FleetReviewOutcome::NotFound { message }) => (StatusCode::NOT_FOUND, message, None),
        Some(FleetReviewOutcome::Conflict { message }) => (StatusCode::CONFLICT, message, None),
        Some(FleetReviewOutcome::Invalid { message }) => (StatusCode::BAD_REQUEST, message, None),
        None => (
            StatusCode::BAD_GATEWAY,
            format!("{alias} did not say how the decision ended"),
            None,
        ),
    }
}

/// A Fleet host whose review items can be decided from here: control mode
/// and a relay that applies the decision exactly.
fn decides_reviews(host: &crate::fleet::FleetHostSnapshot) -> bool {
    host.mode == crate::fleet::HostAccessMode::Control
        && host
            .capabilities
            .iter()
            .any(|c| c == crate::fleet::FLEET_REVIEW_DISPOSE_CAPABILITY)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct HumanAnswer {
    answer: String,
    /// The Fleet host the question is on; absent (or this host's alias)
    /// for one asked here.
    #[serde(default)]
    host: Option<String>,
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
    let (by, account) = operator_identity(&state, &headers);
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
    let hosts = fleet_hosts(&state).await;
    if let Some(remote) = answer
        .host
        .as_deref()
        .filter(|alias| *alias != local_alias(&hosts) && *alias != "this host")
    {
        let remote = remote.to_owned();
        let (code, outcome) =
            answer_remote(&state, &hosts, &remote, &id, status, &answer.answer).await;
        audit(
            state.decision_audit.as_ref(),
            json!({
                "at": time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).ok(),
                "by": by, "account": account, "action": "answer", "host": remote, "id": id, "declined": answer.declined,
                "outcome": outcome, "status": code.as_u16(),
            }),
        )
        .await;
        return if code == StatusCode::OK {
            Json(json!({ "request_id": id, "host": remote, "status": status })).into_response()
        } else {
            error(code, outcome)
        };
    }
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
            "by": by, "account": account, "action": "answer", "id": id, "declined": answer.declined,
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

/// Answer a question on a Fleet host as its operator console: refused for
/// an unknown or observe-only host, and for any request that is not
/// addressed to the console there (a Fleet reply to an agent's request
/// would be sent as that agent).
async fn answer_remote(
    state: &AppState,
    hosts: &[crate::fleet::FleetHostSnapshot],
    alias: &str,
    id: &str,
    status: crate::collaboration::RequestStatus,
    body: &str,
) -> (StatusCode, String) {
    use crate::fleet::{FleetOperation, HostAccessMode};
    let Some(fleet) = &state.fleet else {
        return (StatusCode::NOT_FOUND, "Fleet is not enabled here".into());
    };
    let Some(host) = hosts.iter().find(|host| host.alias == alias && !host.local) else {
        return (StatusCode::NOT_FOUND, format!("no Fleet host {alias}"));
    };
    if host.mode != HostAccessMode::Control {
        return (
            StatusCode::FORBIDDEN,
            format!("{alias} is observe-only; answer it there, or set mode = \"control\""),
        );
    }
    let stored = match fleet
        .execute(
            alias.to_owned(),
            FleetOperation::CollaborationGet {
                pane: console_route(),
                request_id: id.to_owned(),
            },
            FLEET_WRITE_TIMEOUT,
        )
        .await
    {
        Ok(result) => result.collaboration_request,
        Err(error) => return (StatusCode::BAD_GATEWAY, format!("{alias}: {error}")),
    };
    let Some(stored) = stored else {
        return (
            StatusCode::NOT_FOUND,
            format!("{alias} has no request {id}"),
        );
    };
    if !stored.to.console {
        return (
            StatusCode::FORBIDDEN,
            "that request is not addressed to you".into(),
        );
    }
    if stored.status.is_terminal() {
        let same = stored
            .reply
            .as_ref()
            .is_some_and(|reply| reply.status == status && reply.body == body.trim());
        return if same {
            (StatusCode::OK, "recorded".into())
        } else {
            (
                StatusCode::CONFLICT,
                format!("request {id} is already answered"),
            )
        };
    }
    match fleet
        .execute(
            alias.to_owned(),
            FleetOperation::CollaborationReply {
                pane: console_route(),
                request_id: id.to_owned(),
                status,
                body: body.to_owned(),
                report: None,
            },
            FLEET_WRITE_TIMEOUT,
        )
        .await
    {
        Ok(_) => (StatusCode::OK, "recorded".into()),
        Err(error) if error.contains("already") => (StatusCode::CONFLICT, error),
        Err(error) => (StatusCode::BAD_GATEWAY, format!("{alias}: {error}")),
    }
}
