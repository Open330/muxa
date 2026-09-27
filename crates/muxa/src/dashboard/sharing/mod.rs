//! Temporary pane shares. Recipient sessions never authorize operator APIs.
mod login;
mod routes;
#[cfg(test)]
mod tests;

use crate::PaneKey;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Semaphore};

pub(super) use routes::{admin_routes, recipient_routes};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharingConfig {
    /// Canonical externally reachable origin, with no path or credentials.
    pub public_url: String,
    pub issuer_url: String,
    pub client_id: String,
    /// Environment variable name; the secret itself is never serialized.
    pub client_secret_env: Option<String>,
}

impl SharingConfig {
    pub fn validate(&self) -> Result<(), String> {
        let public = url::Url::parse(&self.public_url).map_err(|_| "invalid public_url")?;
        let issuer = url::Url::parse(&self.issuer_url).map_err(|_| "invalid issuer_url")?;
        for url in [&public, &issuer] {
            if !secure_url(url)
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err("sharing URLs require HTTPS (HTTP is allowed only on loopback), without credentials/query/fragment".into());
            }
        }
        if public.path() != "/" || self.client_id.trim().is_empty() {
            return Err("public_url must be an origin; client_id is required".into());
        }
        if self.client_secret_env.as_ref().is_some_and(|name| {
            name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        }) {
            return Err("client_secret_env must name an environment variable".into());
        }
        Ok(())
    }

    fn origin(&self) -> String {
        // Validated at configuration resolution, including for test fixtures.
        url::Url::parse(&self.public_url)
            .expect("validated public_url")
            .origin()
            .ascii_serialization()
    }

    fn secure(&self) -> bool {
        url::Url::parse(&self.public_url).is_ok_and(|url| url.scheme() == "https")
    }

    pub(super) fn matches_host(&self, host: &str) -> bool {
        let Ok(public) = url::Url::parse(&self.public_url) else {
            return false;
        };
        let Ok(request) = url::Url::parse(&format!("{}://{host}", public.scheme())) else {
            return false;
        };
        request.origin() == public.origin()
            && request.path() == "/"
            && request.username().is_empty()
            && request.password().is_none()
    }
}

fn secure_url(url: &url::Url) -> bool {
    url.scheme() == "https"
        || (url.scheme() == "http"
            && matches!(
                url.host_str(),
                Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
            ))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    View,
    Prompt,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    socket: String,
    key: PaneKey,
    pid: u32,
    tty: String,
    process_stamp: String,
}

struct Grant {
    id: String,
    target: Target,
    email: String,
    subject: Option<String>,
    permission: Permission,
    expires: Instant,
    expires_at: i64,
    revoked: bool,
    last_prompt: Option<Instant>,
    capture: Option<(Instant, String)>,
}

#[derive(Clone)]
struct Session {
    subject: String,
    email: String,
    expires: Instant,
}

struct PendingLogin {
    browser: String,
    share: String,
    nonce: openidconnect::Nonce,
    verifier: openidconnect::PkceCodeVerifier,
    expires: Instant,
}

#[derive(Default)]
struct Registry {
    grants: HashMap<String, Arc<Mutex<Grant>>>,
    sessions: HashMap<String, Session>,
    logins: HashMap<String, PendingLogin>,
}

pub struct Sharing {
    config: Option<SharingConfig>,
    registry: Mutex<Registry>,
    metadata: Mutex<Option<(Instant, openidconnect::core::CoreProviderMetadata)>>,
    login_slots: Semaphore,
    pane_slots: Arc<Semaphore>,
}

impl Sharing {
    pub(super) fn new(config: Option<SharingConfig>) -> Arc<Self> {
        Arc::new(Self {
            config,
            registry: Mutex::new(Registry::default()),
            metadata: Mutex::new(None),
            login_slots: Semaphore::new(4),
            pane_slots: Arc::new(Semaphore::new(8)),
        })
    }
}

fn random_secret() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}

fn normalize_email(email: &str) -> Result<String, &'static str> {
    let email = email.trim();
    if email.len() > 254
        || !email.is_ascii()
        || email
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        || !email.contains('@')
        || email.starts_with('@')
        || email.ends_with('@')
    {
        return Err("a valid recipient email is required");
    }
    Ok(email.to_ascii_lowercase())
}

const LOGIN_TTL: Duration = Duration::from_secs(300);
const SESSION_TTL: Duration = Duration::from_secs(8 * 3600);
const MAX_GRANTS: usize = 256;
const MAX_SESSIONS: usize = 1024;
