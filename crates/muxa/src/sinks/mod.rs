//! External event sinks.
//!
//! A sink consumes the daemon's in-process broadcasts (today: prompts;
//! later: transitions, heartbeats) and forwards them to a long-lived
//! external system. Sinks are **always opt-in** — the daemon spawns one
//! task per enabled sink at startup and joins them on the shared
//! shutdown broadcast.
//!
//! ## Layout
//!
//! - [`ohmyprompt`] — HTTP POST to `oh-my-prompt`'s `/api/sync/upload`
//!   endpoint with batching, retries, and a bounded ring-buffer.
//! - [`webhook`] — single-shot HTTP POST to a Slack/Discord webhook
//!   on state transitions (default: `WaitingInput`/`Error`). Best-effort,
//!   no queue, in-task per-agent rate-limit.
//!
//! Each sink owns its own runtime contract (wire schema, auth header
//! shape, retry policy) — there is intentionally no `Sink` trait in v1
//! because the only consumer (`muxad`) calls each impl by name and the
//! shapes diverge enough that a uniform interface would be premature.

pub mod ohmyprompt;
pub mod webhook;

pub use ohmyprompt::{OhMyPromptError, OhMyPromptSink};
pub use webhook::{WebhookError, WebhookSink};
