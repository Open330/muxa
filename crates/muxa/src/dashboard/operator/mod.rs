//! Operator sign-in through a generic `OpenID` Connect provider.
//!
//! A browser that completes the authorization-code flow (PKCE S256, state,
//! nonce) and presents an ID token whose group claim contains
//! `required_group` receives an in-memory operator session cookie. That
//! session is equivalent to the dashboard bearer token for the browser that
//! holds it; the token itself keeps working for CLI/API clients and as the
//! break-glass path when the provider is unavailable.
//!
//! Operator sessions live in their own store. Pane-sharing recipient
//! sessions never appear here, so a share invitation cannot be turned into
//! operator access, and this cookie is never consulted by recipient routes.
mod routes;
#[cfg(test)]
mod tests;

use super::oidc;
use axum::http::{header, HeaderMap, Method};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;

pub(super) use routes::routes;

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
    /// Group whose members may operate the dashboard. Required: an issuer
    /// alone authenticates everyone it knows, not just the owner.
    pub required_group: String,
    /// ID token claim holding group names. Defaults to `groups`.
    pub groups_claim: Option<String>,
    /// Scopes requested in addition to `openid email`, e.g. `["groups"]`
    /// for providers that only release the group claim on request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
}

const DEFAULT_GROUPS_CLAIM: &str = "groups";

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
        if !is_token(&self.required_group, 256) {
            return Err("required_group must be a non-empty group name without spaces".into());
        }
        if !is_token(self.groups_claim(), 128) || STANDARD_CLAIMS.contains(&self.groups_claim()) {
            return Err("groups_claim must name a non-standard ID token claim".into());
        }
        if self.scopes.len() > 16 || !self.scopes.iter().all(|scope| is_token(scope, 128)) {
            return Err("scopes must be at most 16 names without spaces".into());
        }
        Ok(())
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
}

/// Operator session lifetime. Group membership is checked at sign-in only,
/// so this bounds how long a removed member keeps access.
const SESSION_TTL: Duration = Duration::from_secs(8 * 3600);
const LOGIN_TTL: Duration = Duration::from_secs(300);
const MAX_SESSIONS: usize = 64;
const MAX_PENDING_LOGINS: usize = 32;

#[derive(Clone)]
struct Session {
    subject: String,
    email: Option<String>,
    expires: Instant,
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
}

/// Operator login state. A std mutex (never held across `.await`) keeps the
/// per-request session lookup synchronous for the auth middleware.
pub struct OperatorLogin {
    config: Option<LoginConfig>,
    provider: Option<oidc::Provider>,
    registry: std::sync::Mutex<Registry>,
    login_slots: Semaphore,
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
        Arc::new(Self {
            config,
            provider,
            registry: std::sync::Mutex::new(Registry::default()),
            login_slots: Semaphore::new(4),
        })
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

    /// The live operator session named by this request's cookie, if any.
    fn session(&self, headers: &HeaderMap) -> Option<Session> {
        let config = self.config.as_ref()?;
        let secret = oidc::cookie(headers, config.cookie_names().0)?;
        let mut registry = self.registry();
        let session = registry.sessions.get(&secret)?;
        if session.expires > Instant::now() {
            return Some(session.clone());
        }
        registry.sessions.remove(&secret);
        None
    }

    /// `(signed in, email)` for display. The email may be absent: it is not
    /// what authorizes the session.
    pub(in crate::dashboard) fn signed_in(&self, headers: &HeaderMap) -> (bool, Option<String>) {
        self.session(headers)
            .map_or((false, None), |session| (true, session.email))
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
/// no token is configured (control disabled).
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
    login.session(headers).map(|_| OperatorAuth::Session)
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
