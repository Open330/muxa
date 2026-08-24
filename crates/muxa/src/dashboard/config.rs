//! Resolved dashboard configuration.
//!
//! The TOML schema lives in [`crate::config::DashboardTomlConfig`]; the
//! CLI / env layer in `muxad` collects its own per-field overrides into
//! a [`DashboardOverrides`]. Both flow through [`DashboardConfig::resolve`],
//! which applies precedence (env > flag > toml > default) per-field and
//! validates the security invariants:
//!
//! 1. An enabled dashboard using token or public-read auth requires an
//!    explicit token. In public-read mode that token protects writes only.
//! 2. A non-loopback `bind` is rejected unless `allow_public = true`.
//! 3. A non-loopback `bind` is rejected unless either a non-empty token
//!    is set or `auth = "none"` is explicitly configured. `allow_public`
//!    alone is insufficient; exposing unauthenticated API data requires
//!    a second explicit opt-in.
//!
//! These invariants are enforced once, at startup. The HTTP layer trusts
//! the resolved value.

use crate::config::{DashboardAuthMode, DashboardTomlConfig};
use std::net::{AddrParseError, SocketAddr};
use std::time::Duration;

/// The default port for the dashboard. Picked for low collision risk
/// and a "muxa → 7878" mnemonic.
pub const DEFAULT_PORT: u16 = 7878;

/// The default pane scanner cache TTL.
pub const DEFAULT_PANE_CACHE_TTL: Duration = Duration::from_secs(2);

/// Fully-resolved dashboard configuration. Constructed via
/// [`DashboardConfig::resolve`]; the HTTP server takes this by reference.
#[derive(Debug, Clone)]
pub struct DashboardConfig {
    pub enabled: bool,
    pub bind: SocketAddr,
    pub auth: DashboardAuthMode,
    /// `None` = control routes are disabled (`auth = "none"`).
    pub token: Option<String>,
    pub allow_public: bool,
    pub pane_cache_ttl: Duration,
    /// Whether `POST /api/work-control/up` may launch agents. See
    /// [`crate::config::DashboardTomlConfig::allow_work_start`].
    pub allow_work_start: bool,
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
    pub auth: Option<DashboardAuthMode>,
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
        "dashboard is enabled with an authentication mode that requires a control token, \
         but no bearer token is configured; \
         set dashboard.token, --dashboard-token, or MUXA_DASHBOARD_TOKEN; \
         or set dashboard.auth=\"none\" (or --dashboard-auth none) to expose reads and disable control"
    )]
    MissingToken,

    #[error(
        "dashboard.bind={addr} is non-loopback; set allow_public=true (or pass --allow-public) \
         to confirm you want to expose the dashboard beyond this host"
    )]
    NonLoopbackRequiresAllowPublic { addr: SocketAddr },

    #[error(
        "dashboard.bind={addr} is non-loopback; a bearer token is required \
         (set dashboard.token, --dashboard-token, or MUXA_DASHBOARD_TOKEN; \
          or set dashboard.auth=\"none\" to intentionally expose the read-only API)"
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

        let auth = ov.auth.or(toml.auth).unwrap_or(DashboardAuthMode::Token);

        let token = ov
            .token
            .clone()
            .or_else(|| toml.token.clone())
            .filter(|s| !s.trim().is_empty()); // empty/whitespace treated as unset
        let token = if matches!(auth, DashboardAuthMode::None) {
            // Explicit read-only opt-out: run the read API with no token and
            // leave control routes disabled.
            None
        } else {
            token
        };

        let allow_public = ov.allow_public.or(toml.allow_public).unwrap_or(false);
        let allow_work_start = toml.allow_work_start.unwrap_or(false);

        let pane_cache_ttl = toml
            .pane_cache_ttl_ms
            .map_or(DEFAULT_PANE_CACHE_TTL, Duration::from_millis);

        if enabled && !matches!(auth, DashboardAuthMode::None) && token.is_none() {
            return Err(DashboardConfigError::MissingToken);
        }

        if !bind.ip().is_loopback() {
            if !allow_public {
                return Err(DashboardConfigError::NonLoopbackRequiresAllowPublic { addr: bind });
            }
            if !matches!(auth, DashboardAuthMode::None) && token.is_none() {
                return Err(DashboardConfigError::NonLoopbackRequiresToken { addr: bind });
            }
        }

        Ok(Self {
            enabled,
            bind,
            auth,
            token,
            allow_public,
            pane_cache_ttl,
            allow_work_start,
        })
    }

    /// Default for tests / callers that don't load any TOML/CLI/env. The
    /// resolver is the canonical path; this is a convenience.
    #[must_use]
    pub fn loopback_default() -> Self {
        Self {
            enabled: false,
            bind: SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)),
            auth: DashboardAuthMode::Token,
            token: None,
            allow_public: false,
            pane_cache_ttl: DEFAULT_PANE_CACHE_TTL,
            allow_work_start: false,
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
    fn disabled_loopback_with_no_token_is_allowed() {
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
            matches!(
                err,
                DashboardConfigError::NonLoopbackRequiresAllowPublic { .. }
            ),
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
    fn non_loopback_with_allow_public_and_auth_none_resolves_without_token() {
        let toml = DashboardTomlConfig {
            bind: Some("0.0.0.0:7878".into()),
            allow_public: Some(true),
            auth: Some(DashboardAuthMode::None),
            ..DashboardTomlConfig::default()
        };
        let cfg = DashboardConfig::resolve(&toml, &DashboardOverrides::default()).unwrap();
        assert!(cfg.allow_public);
        assert_eq!(cfg.auth, DashboardAuthMode::None);
        assert!(cfg.token.is_none());
    }

    #[test]
    fn non_loopback_public_read_requires_and_preserves_control_token() {
        let toml = DashboardTomlConfig {
            bind: Some("0.0.0.0:7878".into()),
            allow_public: Some(true),
            auth: Some(DashboardAuthMode::PublicRead),
            token: Some("edit-pat".into()),
            ..DashboardTomlConfig::default()
        };
        let cfg = DashboardConfig::resolve(&toml, &DashboardOverrides::default()).unwrap();
        assert_eq!(cfg.auth, DashboardAuthMode::PublicRead);
        assert_eq!(cfg.token.as_deref(), Some("edit-pat"));
        assert!(cfg.allow_public);
    }

    #[test]
    fn enabled_public_read_without_control_token_errors() {
        let toml = DashboardTomlConfig {
            enabled: Some(true),
            auth: Some(DashboardAuthMode::PublicRead),
            ..DashboardTomlConfig::default()
        };
        let err = DashboardConfig::resolve(&toml, &DashboardOverrides::default()).unwrap_err();
        assert!(matches!(err, DashboardConfigError::MissingToken));
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
        assert_eq!(cfg.auth, DashboardAuthMode::Token);
        assert!(cfg.allow_public);
    }

    #[test]
    fn empty_token_is_treated_as_unset() {
        // A disabled dashboard does not require a token, and empty strings
        // must not become unusable bearer credentials.
        let toml = DashboardTomlConfig {
            token: Some(String::new()),
            ..DashboardTomlConfig::default()
        };
        let cfg = DashboardConfig::resolve(&toml, &DashboardOverrides::default()).unwrap();
        assert!(cfg.token.is_none());
    }

    #[test]
    fn enabled_token_auth_without_explicit_token_errors() {
        let toml = DashboardTomlConfig {
            enabled: Some(true),
            ..DashboardTomlConfig::default()
        };
        let err = DashboardConfig::resolve(&toml, &DashboardOverrides::default()).unwrap_err();
        assert!(matches!(&err, DashboardConfigError::MissingToken));
        let message = err.to_string();
        assert!(message.contains("--dashboard-token"), "{message}");
        assert!(message.contains("dashboard.auth=\"none\""), "{message}");
    }

    #[test]
    fn enabled_whitespace_token_is_rejected_as_missing() {
        let toml = DashboardTomlConfig {
            enabled: Some(true),
            token: Some("   ".into()),
            ..DashboardTomlConfig::default()
        };
        let err = DashboardConfig::resolve(&toml, &DashboardOverrides::default()).unwrap_err();
        assert!(matches!(err, DashboardConfigError::MissingToken));
    }

    #[test]
    fn enabled_auth_none_opts_out_of_token() {
        // Explicit escape hatch: auth = "none" runs with no token even
        // when enabled.
        let toml = DashboardTomlConfig {
            enabled: Some(true),
            auth: Some(DashboardAuthMode::None),
            ..DashboardTomlConfig::default()
        };
        let cfg = DashboardConfig::resolve(&toml, &DashboardOverrides::default()).unwrap();
        assert_eq!(cfg.auth, DashboardAuthMode::None);
        assert!(cfg.token.is_none());
    }

    #[test]
    fn enabled_explicit_token_is_not_overwritten() {
        let toml = DashboardTomlConfig {
            enabled: Some(true),
            token: Some("explicit-token".into()),
            ..DashboardTomlConfig::default()
        };
        let cfg = DashboardConfig::resolve(&toml, &DashboardOverrides::default()).unwrap();
        assert_eq!(cfg.token.as_deref(), Some("explicit-token"));
    }

    #[test]
    fn disabled_default_stays_tokenless() {
        // A disabled dashboard never binds, so there's nothing to guard;
        // don't require a token that would never be used.
        let cfg = DashboardConfig::resolve(
            &DashboardTomlConfig::default(),
            &DashboardOverrides::default(),
        )
        .unwrap();
        assert!(!cfg.enabled);
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
            auth: None,
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
            token: Some("from-toml".into()),
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
