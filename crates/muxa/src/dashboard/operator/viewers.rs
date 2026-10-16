//! Viewer rules: which signed-in accounts may read the dashboard.
//!
//! A rule is one of `email:<address>`, `email:*@<domain>` or
//! `sub:<subject>`. Email rules match only an ID token whose `email_verified`
//! is `true`, compare case-insensitively, and a domain rule matches that exact
//! domain (never a subdomain). Every rule is pinned to the configured issuer.
//! There are no other wildcards.
use super::routes::{error, login_config, unix_now};
use super::{LoginConfig, Session, Via, MAX_VIEWER_RULES};
use crate::dashboard::server::AppState;
use axum::{
    body::Bytes,
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ViewerRule {
    /// Exact address, lowercased.
    Email(String),
    /// Exact domain, lowercased.
    Domain(String),
    Subject(String),
}

/// A DNS name of at least two labels: letters, digits and inner hyphens.
fn valid_domain(domain: &str) -> bool {
    domain.len() <= 253
        && domain.contains('.')
        && domain.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

impl ViewerRule {
    pub(super) fn parse(text: &str) -> Result<Self, &'static str> {
        if let Some(address) = text.strip_prefix("email:") {
            let address = address.to_ascii_lowercase();
            if let Some(domain) = address.strip_prefix("*@") {
                return if valid_domain(domain) {
                    Ok(Self::Domain(domain.to_owned()))
                } else {
                    Err("expected email:*@<domain>")
                };
            }
            let Some((local, domain)) = address.split_once('@') else {
                return Err("expected email:<address> or email:*@<domain>");
            };
            let local_ok = !local.is_empty()
                && local.len() <= 64
                && local
                    .bytes()
                    .all(|b| b.is_ascii_graphic() && !b"*@\"\\(),:;<>[]".contains(&b));
            if !local_ok || !valid_domain(domain) {
                return Err("expected email:<address> or email:*@<domain>");
            }
            return Ok(Self::Email(address));
        }
        if let Some(subject) = text.strip_prefix("sub:") {
            if subject.is_empty()
                || subject.len() > 255
                || subject.trim() != subject
                || subject.chars().any(char::is_control)
            {
                return Err("expected sub:<subject>");
            }
            return Ok(Self::Subject(subject.to_owned()));
        }
        Err("a rule starts with email: or sub:")
    }

    /// The canonical text form, which [`Self::parse`] accepts back.
    pub(super) fn text(&self) -> String {
        match self {
            Self::Email(address) => format!("email:{address}"),
            Self::Domain(domain) => format!("email:*@{domain}"),
            Self::Subject(subject) => format!("sub:{subject}"),
        }
    }

    fn matches(&self, account: Account<'_>) -> bool {
        let verified_email = account
            .email
            .filter(|_| account.email_verified)
            .map(str::to_ascii_lowercase);
        match self {
            Self::Email(address) => verified_email.as_deref() == Some(address.as_str()),
            Self::Domain(domain) => verified_email
                .as_deref()
                .and_then(|email| email.rsplit_once('@'))
                .is_some_and(|(_, host)| host == domain),
            Self::Subject(subject) => account.subject == subject,
        }
    }
}

/// A rule added from the dashboard.
#[derive(Debug, Clone)]
pub(super) struct StoredRule {
    /// Opaque handle for the management API.
    pub(super) id: String,
    /// The issuer configured when the rule was added; the rule never
    /// matches while another issuer is configured.
    pub(super) issuer: String,
    pub(super) rule: ViewerRule,
    pub(super) created_at: i64,
}

/// The verified facts a rule is evaluated against.
#[derive(Clone, Copy)]
pub(super) struct Account<'a> {
    pub(super) issuer: &'a str,
    pub(super) subject: &'a str,
    pub(super) email: Option<&'a str>,
    pub(super) email_verified: bool,
}

impl<'a> Account<'a> {
    pub(super) fn of(session: &'a Session) -> Self {
        Self {
            issuer: &session.issuer,
            subject: &session.subject,
            email: session.email.as_deref(),
            email_verified: session.email_verified,
        }
    }
}

/// Does any configured or stored rule admit `account` as a viewer?
pub(super) fn matches(
    config_rules: &[ViewerRule],
    stored: &[StoredRule],
    config: &LoginConfig,
    account: Account<'_>,
) -> bool {
    if account.issuer != config.issuer_url {
        return false;
    }
    config_rules.iter().any(|rule| rule.matches(account))
        || stored
            .iter()
            .filter(|entry| entry.issuer == account.issuer)
            .any(|entry| entry.rule.matches(account))
}

// ── Management API (operator only) ─────────────────────────────────

pub(super) async fn list(State(state): State<AppState>) -> Response {
    let config = match login_config(&state) {
        Ok(config) => config,
        Err((status, message)) => return error(status, message),
    };
    let login = &state.operator;
    let configured = login.config_rules.iter().map(|rule| {
        json!({
            "id": null,
            "rule": rule.text(),
            "source": "config",
            "active": true,
            "created_at": null,
        })
    });
    let registry = login.registry();
    let stored = registry.viewer_rules.iter().map(|entry| {
        json!({
            "id": entry.id,
            "rule": entry.rule.text(),
            "source": "dashboard",
            "active": entry.issuer == config.issuer_url,
            "created_at": entry.created_at,
        })
    });
    let rules: Vec<_> = configured.chain(stored).collect();
    Json(json!({
        "viewer_group": config.viewer_group,
        "max": MAX_VIEWER_RULES,
        "rules": rules,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct AddBody {
    rule: String,
}

pub(super) async fn add(State(state): State<AppState>, body: Bytes) -> Response {
    let config = match login_config(&state) {
        Ok(config) => config,
        Err((status, message)) => return error(status, message),
    };
    let Ok(AddBody { rule }) = serde_json::from_slice::<AddBody>(&body) else {
        return error(StatusCode::BAD_REQUEST, "expected {\"rule\": \"...\"}");
    };
    let rule = match ViewerRule::parse(rule.trim()) {
        Ok(rule) => rule,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let login = &state.operator;
    let _writes = login.enroll_writes.lock().await;
    let entry = {
        let registry = login.registry();
        let duplicate = login.config_rules.contains(&rule)
            || registry
                .viewer_rules
                .iter()
                .any(|entry| entry.issuer == config.issuer_url && entry.rule == rule);
        if duplicate {
            return error(StatusCode::CONFLICT, "that viewer rule already exists");
        }
        if registry.viewer_rules.len() >= MAX_VIEWER_RULES {
            return error(
                StatusCode::CONFLICT,
                "the viewer rule limit is reached; remove one first",
            );
        }
        StoredRule {
            id: uuid::Uuid::new_v4().simple().to_string(),
            issuer: config.issuer_url.clone(),
            rule,
            created_at: unix_now(),
        }
    };
    if let Some(storage) = login.storage.clone() {
        let record = entry.clone();
        let result = tokio::task::spawn_blocking(move || storage.insert_rule(&record))
            .await
            .map_err(std::io::Error::other)
            .and_then(|result| result);
        if let Err(error) = result {
            tracing::error!(%error, "operator storage write failed");
            return self::error(
                StatusCode::SERVICE_UNAVAILABLE,
                "operator storage unavailable; the rule was not added",
            );
        }
    }
    tracing::info!(rule = %entry.rule.text(), "dashboard viewer rule added");
    let body = json!({"id": entry.id, "rule": entry.rule.text()});
    login.registry().viewer_rules.push(entry);
    (StatusCode::CREATED, Json(body)).into_response()
}

/// Remove a rule and end every viewer session that no longer matches any.
pub(super) async fn remove(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let config = match login_config(&state) {
        Ok(config) => config,
        Err((status, message)) => return error(status, message),
    };
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return error(StatusCode::NOT_FOUND, "no such viewer rule");
    }
    let login = &state.operator;
    let _writes = login.enroll_writes.lock().await;
    let Some(entry) = login
        .registry()
        .viewer_rules
        .iter()
        .find(|entry| entry.id == id)
        .cloned()
    else {
        return error(StatusCode::NOT_FOUND, "no such viewer rule");
    };
    if let Some(storage) = login.storage.clone() {
        let id = id.clone();
        let result = tokio::task::spawn_blocking(move || storage.remove_rule(&id))
            .await
            .map_err(std::io::Error::other)
            .and_then(|result| result);
        if let Err(error) = result {
            tracing::error!(%error, "operator storage write failed");
            return self::error(
                StatusCode::SERVICE_UNAVAILABLE,
                "operator storage unavailable; the rule was not removed",
            );
        }
    }
    let ended = {
        let mut guard = login.registry();
        let registry = &mut *guard;
        registry.viewer_rules.retain(|other| other.id != id);
        let before = registry.sessions.len();
        let stored = &registry.viewer_rules;
        registry.sessions.retain(|_, session| {
            session.via != Via::ViewerRule
                || matches(&login.config_rules, stored, config, Account::of(session))
        });
        before - registry.sessions.len()
    };
    tracing::info!(rule = %entry.rule.text(), ended, "dashboard viewer rule removed");
    Json(json!({"removed": true, "sessions_ended": ended})).into_response()
}
