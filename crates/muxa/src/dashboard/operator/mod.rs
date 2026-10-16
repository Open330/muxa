//! Operator sign-in through a generic `OpenID` Connect provider.
//!
//! A browser that completes the authorization-code flow (PKCE S256, state,
//! nonce) receives an in-memory operator session cookie when the verified
//! account is an operator: its ID token's group claim contains
//! `required_group`, or its `(issuer, subject)` was enrolled. That session is
//! equivalent to the dashboard bearer token for the browser that holds it;
//! the token itself keeps working for CLI/API clients and as the
//! break-glass path when the provider is unavailable.
//!
//! Enrollment lets the token holder link an account without touching the
//! provider's group configuration: a verified account that is not yet an
//! operator gets a short-lived pending enrollment bound to its browser, and
//! presenting the dashboard token on the enrollment page records the account
//! durably. Enrollment therefore grants nothing the token did not already
//! grant, and a pending enrollment authorizes no API by itself.
//!
//! Sign-in is deny-by-default and has two roles. An **operator** (group
//! member or enrolled account) has full access. A **viewer** matches
//! `viewer_group` or a viewer rule (an exact verified email, a verified email
//! domain, or an issuer-pinned subject) and may only read what an anonymous
//! `public_read` visitor could. Any other verified account gets no session at
//! all. Operators manage viewer rules from the dashboard; rule-based viewer
//! sessions are re-checked on every request, like enrolled operators.
//!
//! Operator sessions live in their own store. Pane-sharing recipient
//! sessions never appear here, so a share invitation cannot be turned into
//! operator access, and this cookie is never consulted by recipient routes.
mod routes;
mod storage;
#[cfg(test)]
mod tests;
mod viewers;

use super::oidc;
use axum::http::{header, HeaderMap, Method};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

pub(super) use routes::{admin_routes, routes};

/// `[dashboard.login]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginConfig {
    /// Canonical externally reachable origin, with no path or credentials.
    /// The provider must allow `{public_url}/auth/callback` as a redirect URI.
    pub public_url: String,
    pub issuer_url: String,
    pub client_id: String,
    /// Environment variable name; the secret itself is never serialized.
    /// Omit for a public client (PKCE only).
    pub client_secret_env: Option<String>,
    /// Group whose members may operate the dashboard. Optional when
    /// enrollment is on: an issuer alone authenticates everyone it knows, so
    /// without a group only accounts enrolled with the token are operators.
    pub required_group: Option<String>,
    /// ID token claim holding group names. Defaults to `groups`.
    pub groups_claim: Option<String>,
    /// Scopes requested in addition to `openid email`, e.g. `["groups"]`
    /// for providers that only release the group claim on request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// Let a holder of the dashboard token enroll a signed-in account that
    /// is not in `required_group`. Defaults to `true`; `false` restores
    /// group-only sign-in and stops honoring enrolled accounts.
    pub enrollment: Option<bool>,
    /// Group whose members may view the dashboard read-only.
    pub viewer_group: Option<String>,
    /// Viewer rules from configuration, shown read-only next to the rules
    /// operators add in the dashboard: `email:<address>`,
    /// `email:*@<domain>` or `sub:<subject>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub viewer_rules: Vec<String>,
}

const DEFAULT_GROUPS_CLAIM: &str = "groups";

/// Enrolled operators, relative to the XDG data directory; a sibling of the
/// pane-sharing store.
const DEFAULT_STORAGE_PATH: &str = "muxa/dashboard-operators/operators.sqlite3";

/// Claims that the ID token parser consumes itself; a group claim with one of
/// these names could never be read as extra claims.
const STANDARD_CLAIMS: &[&str] = &[
    "iss",
    "sub",
    "aud",
    "exp",
    "iat",
    "auth_time",
    "nonce",
    "acr",
    "amr",
    "azp",
    "at_hash",
    "c_hash",
    "name",
    "given_name",
    "family_name",
    "middle_name",
    "nickname",
    "preferred_username",
    "profile",
    "picture",
    "website",
    "email",
    "email_verified",
    "gender",
    "birthdate",
    "zoneinfo",
    "locale",
    "phone_number",
    "phone_number_verified",
    "address",
    "updated_at",
];

fn is_token(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\')
}

impl LoginConfig {
    pub fn validate(&self) -> Result<(), String> {
        oidc::validate_provider(
            &self.public_url,
            &self.issuer_url,
            &self.client_id,
            self.client_secret_env.as_deref(),
        )?;
        match &self.required_group {
            Some(group) if !is_token(group, 256) => {
                return Err("required_group must be a non-empty group name without spaces".into());
            }
            None if !self.enrollment_enabled() => {
                return Err("required_group is required when enrollment = false".into());
            }
            _ => {}
        }
        if !is_token(self.groups_claim(), 128) || STANDARD_CLAIMS.contains(&self.groups_claim()) {
            return Err("groups_claim must name a non-standard ID token claim".into());
        }
        if self.scopes.len() > 16 || !self.scopes.iter().all(|scope| is_token(scope, 128)) {
            return Err("scopes must be at most 16 names without spaces".into());
        }
        if self
            .viewer_group
            .as_ref()
            .is_some_and(|group| !is_token(group, 256))
        {
            return Err("viewer_group must be a non-empty group name without spaces".into());
        }
        if self.viewer_rules.len() > MAX_VIEWER_RULES {
            return Err(format!(
                "viewer_rules allows at most {MAX_VIEWER_RULES} entries"
            ));
        }
        for rule in &self.viewer_rules {
            viewers::ViewerRule::parse(rule)
                .map_err(|message| format!("viewer_rules: {rule:?}: {message}"))?;
        }
        Ok(())
    }

    pub(in crate::dashboard) fn enrollment_enabled(&self) -> bool {
        self.enrollment.unwrap_or(true)
    }

    pub(in crate::dashboard) fn groups_claim(&self) -> &str {
        self.groups_claim.as_deref().unwrap_or(DEFAULT_GROUPS_CLAIM)
    }

    pub(in crate::dashboard) fn origin(&self) -> String {
        oidc::origin(&self.public_url)
    }

    pub(in crate::dashboard) fn matches_host(&self, host: &str) -> bool {
        oidc::matches_host(&self.public_url, host)
    }

    fn secure(&self) -> bool {
        oidc::is_https(&self.public_url)
    }

    /// `(session cookie, login-flow cookie)`. The `__Host-` prefix pins both
    /// to this exact origin with `Path=/` and `Secure`; plain-HTTP loopback
    /// development cannot use it, so it gets distinct, obviously-dev names.
    fn cookie_names(&self) -> (&'static str, &'static str) {
        if self.secure() {
            ("__Host-muxa-op", "__Host-muxa-op-login")
        } else {
            ("muxa-op-dev", "muxa-op-login-dev")
        }
    }

    /// The pending-enrollment cookie, named like the others.
    fn enroll_cookie_name(&self) -> &'static str {
        if self.secure() {
            "__Host-muxa-op-enroll"
        } else {
            "muxa-op-enroll-dev"
        }
    }
}

/// Operator session lifetime. Group membership is checked at sign-in only,
/// so this bounds how long a removed group member keeps access. Enrollment
/// is re-checked on every request.
const SESSION_TTL: Duration = Duration::from_secs(8 * 3600);
const LOGIN_TTL: Duration = Duration::from_secs(300);
const MAX_SESSIONS: usize = 64;
const MAX_PENDING_LOGINS: usize = 32;
/// A verified-but-unauthorized account has this long to present the token.
const ENROLL_TTL: Duration = Duration::from_secs(300);
const MAX_PENDING_ENROLLMENTS: usize = 16;
/// Wrong tokens per pending enrollment before it is discarded; the account
/// must sign in at the provider again to get another.
const MAX_ENROLL_ATTEMPTS: u32 = 5;
/// Every wrong token delays the next enrollment attempt from any browser by
/// one more second, up to this cap.
const MAX_ENROLL_BACKOFF: Duration = Duration::from_secs(30);
/// Quiet period after which the failure count starts over.
const ENROLL_BACKOFF_RESET: Duration = Duration::from_secs(600);
const MAX_ENROLLED: usize = 64;
/// Viewer rules added from the dashboard, and separately from configuration.
const MAX_VIEWER_RULES: usize = 128;

/// Why a session exists, which also fixes its role.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::dashboard) enum Via {
    /// Operator: the ID token carried `required_group` at sign-in.
    Group,
    /// Operator: the account is enrolled; re-checked on every request so
    /// removal takes effect immediately.
    Enrollment,
    /// Viewer: the ID token carried `viewer_group` at sign-in.
    ViewerGroup,
    /// Viewer: a viewer rule matched; re-checked on every request.
    ViewerRule,
}

impl Via {
    pub(in crate::dashboard) fn role(self) -> Role {
        match self {
            Self::Group | Self::Enrollment => Role::Operator,
            Self::ViewerGroup | Self::ViewerRule => Role::Viewer,
        }
    }
}

/// What a request may do on the operator dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::dashboard) enum Role {
    /// Nothing beyond what the deployment serves anonymously.
    None,
    /// Read-only, redacted like an anonymous `public_read` visitor.
    Viewer,
    /// Everything the bearer token allows.
    Operator,
}

#[derive(Clone)]
struct Session {
    issuer: String,
    subject: String,
    email: Option<String>,
    /// The provider's `email_verified`; email viewer rules need it.
    email_verified: bool,
    via: Via,
    /// Wrong tokens presented to upgrade this (viewer) session.
    attempts: u32,
    expires: Instant,
}

/// A verified account that is not (yet) an operator, waiting for the
/// dashboard token. Grants no access by itself.
struct PendingEnrollment {
    issuer: String,
    subject: String,
    email: Option<String>,
    email_verified: bool,
    return_to: String,
    attempts: u32,
    expires: Instant,
}

/// One enrolled account. `id` is an opaque handle for the management API so
/// subjects never appear in URLs.
#[derive(Debug, Clone)]
struct Enrolled {
    id: String,
    issuer: String,
    subject: String,
    email: Option<String>,
    created_at: i64,
    last_seen_at: i64,
}

/// Global enrollment failure backoff.
#[derive(Default)]
struct Backoff {
    failures: u32,
    last: Option<Instant>,
}

impl Backoff {
    /// When the next attempt is allowed, if the backoff is still active.
    fn retry_at(&self) -> Option<Instant> {
        let last = self.last?;
        if last.elapsed() >= ENROLL_BACKOFF_RESET {
            return None;
        }
        Some(last + Duration::from_secs(u64::from(self.failures)).min(MAX_ENROLL_BACKOFF))
    }

    fn fail(&mut self) {
        if self.retry_at().is_none() {
            self.failures = 0;
        }
        self.failures = self.failures.saturating_add(1);
        self.last = Some(Instant::now());
    }
}

struct PendingLogin {
    browser: String,
    return_to: String,
    nonce: openidconnect::Nonce,
    verifier: openidconnect::PkceCodeVerifier,
    expires: Instant,
}

#[derive(Default)]
struct Registry {
    sessions: HashMap<String, Session>,
    logins: HashMap<String, PendingLogin>,
    enrollments: HashMap<String, PendingEnrollment>,
    /// Viewer rules added from the dashboard, including ones for another
    /// issuer (listed so they can be removed, but never matched).
    viewer_rules: Vec<viewers::StoredRule>,
    /// Every stored entry, including ones for another issuer (listed so they
    /// can be removed, but never matched).
    enrolled: Vec<Enrolled>,
    backoff: Backoff,
}

impl Registry {
    /// The enrolled entry for this account, pinned to the configured issuer.
    fn enrolled(&self, config: &LoginConfig, issuer: &str, subject: &str) -> Option<&Enrolled> {
        if !config.enrollment_enabled() || issuer != config.issuer_url {
            return None;
        }
        self.enrolled
            .iter()
            .find(|entry| entry.issuer == issuer && entry.subject == subject)
    }
}

/// Operator login state. A std mutex (never held across `.await`) keeps the
/// per-request session lookup synchronous for the auth middleware.
pub struct OperatorLogin {
    config: Option<LoginConfig>,
    provider: Option<oidc::Provider>,
    registry: std::sync::Mutex<Registry>,
    login_slots: Semaphore,
    /// `viewer_rules` from configuration, validated at startup.
    config_rules: Vec<viewers::ViewerRule>,
    /// Durable enrolled operators. `None` keeps enrollments in memory only
    /// (embedding callers and tests that do not go through [`Self::open`]).
    storage: Option<Arc<storage::Storage>>,
    /// Serializes enrolled-operator writes so the bound and the store agree.
    enroll_writes: tokio::sync::Mutex<()>,
}

/// What a request authenticated as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::dashboard) enum OperatorAuth {
    /// `Authorization: Bearer <token>`: not ambient, no CSRF exposure.
    Bearer,
    /// Operator session cookie: ambient, so state changes need CSRF proof.
    Session,
}

impl OperatorLogin {
    pub(in crate::dashboard) fn new(config: Option<LoginConfig>) -> Arc<Self> {
        let provider = config.as_ref().map(|config| {
            oidc::Provider::new(
                &config.issuer_url,
                &config.client_id,
                config.client_secret_env.as_deref(),
                format!("{}/auth/callback", config.origin()),
            )
        });
        let config_rules = config
            .as_ref()
            .map(|config| {
                config
                    .viewer_rules
                    .iter()
                    .filter_map(|rule| viewers::ViewerRule::parse(rule).ok())
                    .collect()
            })
            .unwrap_or_default();
        Arc::new(Self {
            config,
            provider,
            registry: std::sync::Mutex::new(Registry::default()),
            login_slots: Semaphore::new(4),
            config_rules,
            storage: None,
            enroll_writes: tokio::sync::Mutex::new(()),
        })
    }

    /// Like [`Self::new`], with enrolled operators and viewer rules persisted
    /// at `path` (default: the XDG data directory). Nothing is opened when
    /// sign-in is off.
    pub(in crate::dashboard) async fn open(
        config: Option<LoginConfig>,
        path: Option<std::path::PathBuf>,
    ) -> std::io::Result<Arc<Self>> {
        let mut login = Self::new(config);
        if login.config.is_none() {
            return Ok(login);
        }
        let path = path
            .or_else(|| dirs::data_dir().map(|d| d.join(DEFAULT_STORAGE_PATH)))
            .ok_or_else(|| std::io::Error::other("operator sign-in needs an XDG data directory"))?;
        let (storage, enrolled, viewer_rules) =
            tokio::task::spawn_blocking(move || storage::Storage::open(&path))
                .await
                .map_err(std::io::Error::other)??;
        let inner = Arc::get_mut(&mut login).expect("new operator login state");
        inner.storage = Some(Arc::new(storage));
        let mut registry = inner.registry();
        registry.enrolled = enrolled;
        registry.viewer_rules = viewer_rules;
        drop(registry);
        Ok(login)
    }

    pub(in crate::dashboard) fn config(&self) -> Option<&LoginConfig> {
        self.config.as_ref()
    }

    fn registry(&self) -> std::sync::MutexGuard<'_, Registry> {
        // A panic while holding the lock cannot leave a half-written session.
        self.registry
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether a rule-based viewer session still matches a viewer rule.
    fn viewer_rule_matches(
        &self,
        registry: &Registry,
        config: &LoginConfig,
        session: &Session,
    ) -> bool {
        viewers::matches(
            &self.config_rules,
            &registry.viewer_rules,
            config,
            viewers::Account::of(session),
        )
    }

    /// The live sign-in session (operator or viewer) named by this
    /// request's cookie, if any.
    fn session(&self, headers: &HeaderMap) -> Option<Session> {
        let config = self.config.as_ref()?;
        let secret = oidc::cookie(headers, config.cookie_names().0)?;
        let mut registry = self.registry();
        let session = registry.sessions.get(&secret)?;
        let live = session.expires > Instant::now()
            && match session.via {
                Via::Group | Via::ViewerGroup => true,
                Via::Enrollment => registry
                    .enrolled(config, &session.issuer, &session.subject)
                    .is_some(),
                Via::ViewerRule => self.viewer_rule_matches(&registry, config, session),
            };
        if live {
            return Some(session.clone());
        }
        registry.sessions.remove(&secret);
        None
    }

    /// Does this request carry a live viewer (not operator) session?
    pub(in crate::dashboard) fn viewer(&self, headers: &HeaderMap) -> bool {
        self.session(headers)
            .is_some_and(|session| session.via.role() == Role::Viewer)
    }

    /// `(email, via)` of the signed-in account, for display. The email may
    /// be absent: it is not what authorizes the session.
    pub(in crate::dashboard) fn signed_in(
        &self,
        headers: &HeaderMap,
    ) -> Option<(Option<String>, Via)> {
        self.session(headers)
            .map(|session| (session.email, session.via))
    }

    /// Cookie-authorized state changes must prove they come from the
    /// dashboard page itself: an exact `Origin` (a cross-site page cannot
    /// forge it) plus a custom header (a cross-site form cannot set it
    /// without a CORS preflight this server never grants).
    pub(in crate::dashboard) fn csrf_ok(&self, method: &Method, headers: &HeaderMap) -> bool {
        if matches!(*method, Method::GET | Method::HEAD) {
            return true;
        }
        let Some(config) = self.config.as_ref() else {
            return false;
        };
        headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok())
            == Some(config.origin().as_str())
            && headers
                .get("x-muxa-operator")
                .and_then(|value| value.to_str().ok())
                == Some("1")
    }
}

/// Authenticate a dashboard operator request: the configured bearer token,
/// or else a live operator session cookie. `None` when neither is present or
/// no token is configured (control disabled). A viewer session is never
/// operator access.
pub(in crate::dashboard) fn authenticate(
    token: Option<&str>,
    login: &OperatorLogin,
    headers: &HeaderMap,
) -> Option<OperatorAuth> {
    let expected = token?;
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    if super::auth::check_bearer(bearer, expected) {
        return Some(OperatorAuth::Bearer);
    }
    login
        .session(headers)
        .filter(|session| session.via.role() == Role::Operator)
        .map(|_| OperatorAuth::Session)
}

/// Group names from the configured claim. Providers disagree on the shape:
/// absent means no groups, a lone string is one group, and an array keeps
/// only its string members.
fn claim_groups(value: Option<&serde_json::Value>) -> Vec<&str> {
    match value {
        Some(serde_json::Value::String(group)) => vec![group.as_str()],
        Some(serde_json::Value::Array(items)) => {
            items.iter().filter_map(serde_json::Value::as_str).collect()
        }
        _ => Vec::new(),
    }
}

/// Every non-standard ID token claim, so the group claim name can be chosen
/// in configuration rather than at compile time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ExtraClaims {
    #[serde(flatten)]
    claims: serde_json::Map<String, serde_json::Value>,
}

impl openidconnect::AdditionalClaims for ExtraClaims {}
