//! `tend-aas` — tend's optional account module.
//!
//! Moves running agents between accounts and resumes their conversations
//! (`tend swap`, the `swap_account` automation action, `tend_swap_account`),
//! keeps the daemon's swap jobs (`swap-jobs.sqlite3`), and picks the account
//! a new agent starts on (`[agent.<program>].account`). Accounts come from
//! configured `[[accounts.<provider>]]` profiles or from `aas`.
//!
//! Core (`tend`) never depends on this crate. It speaks to it through
//! [`tend::swap_api`]: the daemon hands the IPC server an
//! `Arc<dyn SwapJobs>`, and the CLI asks [`account_route::decide`] for a
//! launch. Whether the module is used is `[modules] accounts` in
//! `config.toml` ([`tend::Config::accounts_module`]).

pub mod account_provider;
pub mod account_route;
pub mod account_swap;
pub mod codex_hook_trust;
pub(crate) mod swap_job;
pub mod swap_service;
