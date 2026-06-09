//! HTTP dashboard for the muxa daemon.
//!
//! The dashboard exposes the daemon's read-only state — agents, panes
//! across every tmux server, and (in a later commit) a live event stream
//! — over a small `axum`-based HTTP API alongside the existing
//! Unix-socket IPC.
//!
//! ## Security model
//!
//! Default-secure: opt-in via `[dashboard] enabled = true`, bound to
//! `127.0.0.1` only. Non-loopback binds require both `allow_public =
//! true` (an explicit acknowledgement) and either a non-empty bearer
//! token or the explicit `auth = "none"` public-API opt-in. Token
//! comparison is constant-time when token auth is enabled.
//!
//! ## Layout
//!
//! - [`config`] — resolved [`DashboardConfig`] + the precedence rules
//!   that turn TOML + env + CLI into one set of values.
//! - [`auth`] — header-level bearer-token check (framework-agnostic).
//! - [`server`] — the `axum::Router`, handlers, and [`serve`] entrypoint.

pub mod assets;
pub mod auth;
pub mod config;
pub mod server;

pub use config::{DashboardConfig, DashboardConfigError, DashboardOverrides};
pub use server::{router, serve, AppState, DashboardRuntimeConfig};
