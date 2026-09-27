//! Durable pane shares. Recipient sessions never authorize operator APIs.
mod admin;
mod login;
mod routes;
mod storage;
mod targets;
#[cfg(test)]
mod tests;

use super::oidc::{self, random_secret};
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
    /// Optional private SQLite file; defaults to the XDG data directory.
    pub storage_path: Option<std::path::PathBuf>,
}

impl SharingConfig {
    pub fn validate(&self) -> Result<(), String> {
        oidc::validate_provider(
            &self.public_url,
            &self.issuer_url,
            &self.client_id,
            self.client_secret_env.as_deref(),
        )?;
        if self
            .storage_path
            .as_ref()
            .is_some_and(|path| !path.is_absolute() || path.file_name().is_none())
        {
            return Err("storage_path must be an absolute file path".into());
        }
        Ok(())
    }

    pub(in crate::dashboard) fn origin(&self) -> String {
        oidc::origin(&self.public_url)
    }

    fn secure(&self) -> bool {
        oidc::is_https(&self.public_url)
    }

    pub(in crate::dashboard) fn matches_host(&self, host: &str) -> bool {
        oidc::matches_host(&self.public_url, host)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    View,
    Prompt,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    window: bool,
    peers: Vec<Target>,
    email: String,
    subject: Option<String>,
    permission: Permission,
    expires: Instant,
    expires_at: i64,
    revoked: bool,
    last_prompt: Option<Instant>,
    capture: Option<(Instant, String, String)>,
    deliveries: HashMap<String, Delivery>,
    dirty: bool,
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

#[derive(Clone, Serialize, Deserialize)]
struct Delivery {
    digest: String,
    outcome: String,
}

type ExecutionLocks = HashMap<(String, String), std::sync::Weak<Mutex<()>>>;

pub struct Sharing {
    storage: Option<Arc<storage::Storage>>,
    config: Option<SharingConfig>,
    registry: Mutex<Registry>,
    provider: Option<oidc::Provider>,
    login_slots: Semaphore,
    pane_slots: Arc<Semaphore>,
    execution_locks: std::sync::Mutex<ExecutionLocks>,
}

impl Sharing {
    pub(super) fn new(config: Option<SharingConfig>) -> Arc<Self> {
        let provider = config.as_ref().map(|config| {
            oidc::Provider::new(
                &config.issuer_url,
                &config.client_id,
                config.client_secret_env.as_deref(),
                format!("{}/share/auth/callback", config.origin()),
            )
        });
        Arc::new(Self {
            storage: None,
            config,
            registry: Mutex::new(Registry::default()),
            provider,
            login_slots: Semaphore::new(4),
            pane_slots: Arc::new(Semaphore::new(8)),
            execution_locks: std::sync::Mutex::new(HashMap::new()),
        })
    }
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
