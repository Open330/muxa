//! Core types for muxa.
//!
//! This crate is I/O-free. It owns the canonical `AgentEvent` schema, the
//! in-memory `Store` that applies events, the configuration model, and a
//! small typed error hierarchy. Higher layers (`muxa-runtime`,
//! `muxa-adapters`, the binaries) depend on it.

pub mod config;
pub mod error;
pub mod event;
pub mod paths;
pub mod state;

pub use config::Config;
pub use error::{CoreError, Result};
pub use event::{AgentEvent, AgentId, AgentKind, AgentState, NotificationLevel, PROTOCOL_VERSION};
pub use state::{Agent, SharedStore, Store};
