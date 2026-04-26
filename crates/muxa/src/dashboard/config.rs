//! Resolved dashboard configuration.
//!
//! The TOML schema lives in [`crate::config::DashboardTomlConfig`]; the
//! CLI / env layer in `muxad` collects its own per-field overrides into
//! a [`DashboardOverrides`]. Both flow through [`DashboardConfig::resolve`],
//! which applies precedence (env > flag > toml > default) per-field and
//! validates the security invariants:
//!
//! 1. A non-loopback `bind` is rejected unless `allow_public = true`.
//! 2. A non-loopback `bind` is rejected unless a non-empty token is set
//!    — `allow_public` alone is insufficient; we want both an explicit
//!    acknowledgement *and* a credential before exposing the socket.
//!
//! These invariants are enforced once, at startup. The HTTP layer trusts
//! the resolved value.

use crate::config::DashboardTomlConfig;
use std::net::{AddrParseError, SocketAddr};
use std::time::Duration;

/// The default port for the dashboard. Picked for low collision risk
/// and a "muxa → 7878" mnemonic.
pub const DEFAULT_PORT: u16 = 7878;
/// The default pane scanner cache TTL.
pub const DEFAULT_PANE_CACHE_TTL: Duration = Duration::from_millis(2000);

/// Fully-resolved dashboard configuration. Constructed via
/// [`DashboardConfig::resolve`]; the HTTP server takes this by reference.
#[derive(Debug, Clone)]
pub struct DashboardConfig {
    pub enabled: bool,
    pub bind: SocketAddr,
    /// `None` = no auth required (only allowed for loopback binds).
    pub token: Option<String>,
    pub allow_public: bool,
    pub pane_cache_ttl: Duration,
}

/// Per-field overrides captured by the binary layer (CLI flags + env
/// vars). Each field is `Option`: `None` means "fall through to the next
/// precedence level"; `Some(_)` wins over the TOML config.
///
/// `muxad` is responsible for collapsing CLI + env into a single
/// `DashboardOverrides` (env-beats-flag, per the user-facing precedence
/// rules) before calling [`DashboardConfig::resolve`].
#[derive(Debug, Clone, Default)]
pub struct DashboardOverrides {
    pub enabled: Option<bool>,
    pub bind: Option<String>,
    pub token: Option<String>,
    pub allow_public: Option<bool>,
}

#[derive(Debug, thiserror::Error)]
pub enum DashboardConfigError {
    #[error("invalid dashboard.bind {addr:?}: {source}")]
    InvalidBind {
        addr: String,
        #[source]
        source: AddrParseError,
    },

    #[error(
        "dashboard.bind={addr} is non-loopback; set allow_public=true (or pass --allow-public) \
         to confirm you want to expose the dashboard beyond this host"
    )]
    NonLoopbackRequiresAllowPublic { addr: SocketAddr },

    #[error(
        "dashboard.bind={addr} is non-loopback; a bearer token is required \
         (set dashboard.token, --dashboard-token, or MUXA_DASHBOARD_TOKEN)"
    )]
    NonLoopbackRequiresToken { addr: SocketAddr },
}

impl DashboardConfig {
    /// Apply precedence (override > toml > default) and enforce the
    /// security invariants. The returned value is the single source of
    /// truth for the HTTP server lifetime.
    pub fn resolve(
        toml: &DashboardTomlConfig,
        ov: &DashboardOverrides,
    ) -> Result<Self, DashboardConfigError> {
        let enabled = ov.enabled.or(toml.enabled).unwrap_or(false);

        let bind_str = ov
            .bind
            .clone()
            .or_else(|| toml.bind.clone())
            .unwrap_or_else(|| format!("127.0.0.1:{DEFAULT_PORT}"));
        let bind: SocketAddr =
            bind_str
                .parse()
                .map_err(|source| DashboardConfigError::InvalidBind {
                    addr: bind_str.clone(),
                    source,
                })?;

        let token = ov
            .token
            .clone()
            .or_else(|| toml.token.clone())
            .filter(|s| !s.is_empty()); // empty string treated as unset

        let allow_public = ov.allow_public.or(toml.allow_public).unwrap_or(false);

        let pane_cache_ttl = toml
            .pane_cache_ttl_ms
            .map_or(DEFAULT_PANE_CACHE_TTL, Duration::from_millis);

        if !bind.ip().is_loopback() {
            if !allow_public {
                return Err(DashboardConfigError::NonLoopbackRequiresAllowPublic { addr: bind });
            }
            if token.is_none() {
                return Err(DashboardConfigError::NonLoopbackRequiresToken { addr: bind });
            }
        }

        Ok(Self {
            enabled,
            bind,
            token,
            allow_public,
            pane_cache_ttl,
        })
    }

    /// Default for tests / callers that don't load any TOML/CLI/env. The
    /// resolver is the canonical path; this is a convenience.
    #[must_use]
    pub fn loopback_default() -> Self {
        Self {
            enabled: false,
            bind: SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)),
            token: None,
            allow_public: false,
            pane_cache_ttl: DEFAULT_PANE_CACHE_TTL,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toml_with_bind(addr: &str) -> DashboardTomlConfig {
        DashboardTomlConfig {
            bind: Some(addr.into()),
            ..DashboardTomlConfig::default()
        }
    }

    #[test]
    fn empty_inputs_resolve_to_loopback_disabled() {
        let cfg = DashboardConfig::resolve(
            &DashboardTomlConfig::default(),
            &DashboardOverrides::default(),
        )
        .unwrap();
        assert!(!cfg.enabled);
        assert!(cfg.bind.ip().is_loopback());
        assert_eq!(cfg.bind.port(), DEFAULT_PORT);
        assert!(cfg.token.is_none());
        assert!(!cfg.allow_public);
        assert_eq!(cfg.pane_cache_ttl, DEFAULT_PANE_CACHE_TTL);
    }

    #[test]
    fn loopback_with_no_token_is_allowed() {
        let cfg = DashboardConfig::resolve(
            &toml_with_bind("127.0.0.1:9999"),
            &DashboardOverrides::default(),
        )
        .unwrap();
        assert_eq!(cfg.bind.port(), 9999);
        assert!(cfg.token.is_none());
    }

    #[test]
    fn non_loopback_without_allow_public_errors() {
        let err = DashboardConfig::resolve(
            &toml_with_bind("0.0.0.0:7878"),
            &DashboardOverrides::default(),
        )
        .unwrap_err();
        assert!(
            matches!(err, DashboardConfigError::NonLoopbackRequiresAllowPublic { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn non_loopback_with_allow_public_but_no_token_errors() {
        let toml = DashboardTomlConfig {
            bind: Some("0.0.0.0:7878".into()),
            allow_public: Some(true),
            ..DashboardTomlConfig::default()
        };
        let err = DashboardConfig::resolve(&toml, &DashboardOverrides::default()).unwrap_err();
        assert!(
            matches!(err, DashboardConfigError::NonLoopbackRequiresToken { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn non_loopback_with_allow_public_and_token_resolves_clean() {
        let toml = DashboardTomlConfig {
            bind: Some("0.0.0.0:7878".into()),
            allow_public: Some(true),
            token: Some("s3cret".into()),
            ..DashboardTomlConfig::default()
        };
        let cfg = DashboardConfig::resolve(&toml, &DashboardOverrides::default()).unwrap();
        assert_eq!(cfg.token.as_deref(), Some("s3cret"));
        assert!(cfg.allow_public);
    }

    #[test]
    fn empty_token_is_treated_as_unset() {
        // Loopback with empty-string token: should resolve clean (no auth).
        let toml = DashboardTomlConfig {
            token: Some(String::new()),
            ..DashboardTomlConfig::default()
        };
        let cfg = DashboardConfig::resolve(&toml, &DashboardOverrides::default()).unwrap();
        assert!(cfg.token.is_none());
    }

    #[test]
    fn override_beats_toml() {
        let toml = DashboardTomlConfig {
            enabled: Some(false),
            bind: Some("127.0.0.1:1111".into()),
            token: Some("from-toml".into()),
            ..DashboardTomlConfig::default()
        };
        let ov = DashboardOverrides {
            enabled: Some(true),
            bind: Some("127.0.0.1:2222".into()),
            token: Some("from-override".into()),
            allow_public: None,
        };
        let cfg = DashboardConfig::resolve(&toml, &ov).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.bind.port(), 2222);
        assert_eq!(cfg.token.as_deref(), Some("from-override"));
    }

    #[test]
    fn toml_used_when_no_override() {
        let toml = DashboardTomlConfig {
            enabled: Some(true),
            bind: Some("127.0.0.1:3333".into()),
            ..DashboardTomlConfig::default()
        };
        let cfg = DashboardConfig::resolve(&toml, &DashboardOverrides::default()).unwrap();
        assert!(cfg.enabled);
        assert_eq!(cfg.bind.port(), 3333);
    }

    #[test]
    fn invalid_bind_string_errors() {
        let toml = toml_with_bind("not-an-address");
        let err = DashboardConfig::resolve(&toml, &DashboardOverrides::default()).unwrap_err();
        assert!(matches!(err, DashboardConfigError::InvalidBind { .. }));
    }

    #[test]
    fn ipv6_loopback_is_allowed_without_token() {
        let toml = toml_with_bind("[::1]:7878");
        let cfg = DashboardConfig::resolve(&toml, &DashboardOverrides::default()).unwrap();
        assert!(cfg.bind.ip().is_loopback());
    }

    #[test]
    fn pane_cache_ttl_from_toml_overrides_default() {
        let toml = DashboardTomlConfig {
            pane_cache_ttl_ms: Some(500),
            ..DashboardTomlConfig::default()
        };
        let cfg = DashboardConfig::resolve(&toml, &DashboardOverrides::default()).unwrap();
        assert_eq!(cfg.pane_cache_ttl, Duration::from_millis(500));
    }
}
