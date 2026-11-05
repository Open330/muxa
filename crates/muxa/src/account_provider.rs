//! Where a provider's accounts come from, and which one an agent should use.
//!
//! Account routing (`[agent.<program>].account`) and swaps (`muxa swap`, the
//! `swap_account` automation action) need two things from a provider's
//! accounts: their state (which still have quota) and how to start the CLI on
//! one. Three backends answer that, in this order of preference:
//!
//! 1. **Configured accounts**: `[[accounts.<provider>]]` in `config.toml`,
//!    each the environment that selects it (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
//!    a token file). muxa ranks them itself from the rate-limit state it
//!    already observes: hook and statusline reports, and Codex rollout files.
//!    No external tool is involved.
//! 2. **`aas`**: when no accounts are configured and the `aas` binary is
//!    installed (or `[swap].aas` names one), `aas candidates` ranks its own
//!    accounts and `aas e <name>` starts the CLI.
//! 3. **None**: launches start the provider CLI as it is, and a swap has no
//!    account to move to. A capped agent then waits for its reset, which the
//!    `resume-after-limit` automation rule resumes; that rule needs no
//!    accounts at all.
//!
//! An account's limits outlive its agents: a capped account stays capped
//! after its last agent is stopped or swapped away. They are kept in an
//! [`AccountLedger`] (its own file, outside the config), with the
//! [`AccountBinding`]s that say which account muxa put each agent on.
//!
//! An agent's quota readings count for an account only when muxa knows the
//! agent runs on it, deterministically:
//!
//! - muxa launched or swapped the agent onto it ([`AccountBinding`]), and the
//!   reading was taken after that binding; a reading from before a swap is
//!   the previous account's and counts for neither;
//! - otherwise, for Claude only, the agent's own `sessions/<pid>.json` names
//!   its session and that process's environment selects the account.
//!
//! Anything else (a pane's first process, a command line naming a session)
//! is not a binding: the agent's account is unknown, and its readings say
//! nothing about any account.

use crate::account_swap::Candidate;
use crate::accounts::{AccountProfile, AccountsConfig, ResolvedAccounts};
use crate::event::AgentKind;
use crate::state::Agent;
use crate::AgentState;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use time::OffsetDateTime;

/// How long an account is skipped after a limit whose reset time was never
/// reported. Most limits do report one; this only covers those that do not.
pub const UNKNOWN_RESET_COOLDOWN: time::Duration = time::Duration::hours(1);

/// How long the ledger keeps an account nobody has reported on.
const LEDGER_RETENTION: time::Duration = time::Duration::days(7);

/// How many bindings the ledger keeps; the oldest go first.
const BINDINGS_KEPT: usize = 256;

/// The ledger's on-disk format. A different version is read as empty.
const LEDGER_VERSION: u32 = 1;

/// The backend that serves one provider's accounts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Backend {
    /// `[[accounts.<provider>]]`, in config order.
    Accounts(Vec<AccountProfile>),
    /// The resolved `aas` binary.
    Aas(String),
    /// No backend; the reason says how to get one.
    Unavailable(String),
}

/// The backend for `provider`: its configured accounts when it lists any,
/// else `aas` when the binary named by `aas` (`[swap].aas`) can be found,
/// else none.
#[must_use]
pub fn backend(provider: &str, accounts: &AccountsConfig, aas: &str) -> Backend {
    backend_with(provider, accounts, aas, crate::account_swap::find_aas)
}

fn backend_with(
    provider: &str,
    accounts: &AccountsConfig,
    aas: &str,
    find_aas: impl FnOnce(&str) -> Option<String>,
) -> Backend {
    let profiles = accounts.for_provider(provider);
    if !profiles.is_empty() {
        return Backend::Accounts(profiles.to_vec());
    }
    match find_aas(aas) {
        Some(path) => Backend::Aas(path),
        None => Backend::Unavailable(no_accounts(provider)),
    }
}

/// Why `provider` has no account to route to, and what still works.
#[must_use]
pub fn no_accounts(provider: &str) -> String {
    format!(
        "no {provider} accounts are configured: list them as [[accounts.{provider}]] in \
         config.toml (or install aas); a capped agent still resumes after its reset \
         with the resume-after-limit rule"
    )
}

/// The configured account a Claude agent runs on, by Claude's own record:
/// its `sessions/<pid>.json` names the agent's session, and that process's
/// environment selects the account. `None` for any other provider, or when
/// the record or the environment cannot be read.
#[must_use]
pub fn claude_record(resolved: &ResolvedAccounts, agent: &Agent) -> Option<String> {
    if agent.kind != AgentKind::ClaudeCode {
        return None;
    }
    let pid = agent.workload.primary_pid?;
    if !crate::account_swap::process_runs_session(pid, agent.kind, &agent.session_id) {
        return None;
    }
    let environ = crate::accounts::process_environ(pid);
    resolved.identify(&environ).map(str::to_owned)
}

/// muxa put an agent on an account: launched it there, or swapped it there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountBinding {
    pub provider: String,
    pub account: String,
    /// The conversation, when muxa knew it (a swap resumes a known one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Where the agent was started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    /// Just before the agent was started on the account: readings taken
    /// earlier are about another account.
    #[serde(with = "time::serde::rfc3339")]
    pub bound_at: OffsetDateTime,
}

impl AccountBinding {
    /// Whether `agent` was started where this binding put one.
    fn placed(&self, agent: &Agent) -> bool {
        if self.pane.is_none() || agent.pane != self.pane {
            return false;
        }
        match (self.socket.as_deref(), agent.tmux_socket.as_deref()) {
            (Some(bound), Some(row)) => {
                crate::backend::pane_endpoints_match(agent.pane.as_deref(), bound, row)
            }
            _ => true,
        }
    }
}

/// What one account's agents have reported, kept after they are gone.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountLimit {
    pub provider: String,
    pub account: String,
    /// The account has no quota until then.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub capped_until: Option<OffsetDateTime>,
    /// When a limit window was first reported full with no reset time.
    /// The account is skipped for [`UNKNOWN_RESET_COOLDOWN`] from then.
    #[serde(default, with = "time::serde::rfc3339::option")]
    pub full_since: Option<OffsetDateTime>,
    /// The highest share of a limit window last reported used.
    #[serde(default)]
    pub used_pct: Option<f32>,
    #[serde(with = "time::serde::rfc3339")]
    pub observed_at: OffsetDateTime,
}

impl AccountLimit {
    /// Until when the account is capped, if it still is at `now`.
    fn capped_at(&self, now: OffsetDateTime) -> Option<OffsetDateTime> {
        let unknown = self.full_since.map(|since| since + UNKNOWN_RESET_COOLDOWN);
        [self.capped_until, unknown]
            .into_iter()
            .flatten()
            .filter(|end| *end > now)
            .max()
    }
}

/// Account limits that outlive the agents that reported them, and which
/// account muxa put each agent on. Its own file (see
/// [`crate::paths::default_account_ledger_file`]); no configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountLedger {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    accounts: Vec<AccountLimit>,
    #[serde(default)]
    bindings: Vec<AccountBinding>,
}

/// What the agents on one account report now.
#[derive(Debug, Default)]
struct Live {
    cap_end: Option<OffsetDateTime>,
    full_without_reset: bool,
    used_pct: Option<f32>,
}

impl Live {
    fn observe(&mut self, agent: &Agent) {
        let mut ends = Vec::new();
        if crate::account_swap::is_rate_limited(agent) {
            ends.push(
                agent
                    .rate_limited_until
                    .unwrap_or(agent.state_entered_at + UNKNOWN_RESET_COOLDOWN),
            );
        }
        // A window reported full is spent until it resets, whether or not the
        // agent has hit it yet; with no reset time it is waited out.
        for (pct, resets) in [
            (agent.rate_limit_5h_pct, agent.rate_limit_5h_resets_at),
            (agent.rate_limit_7d_pct, agent.rate_limit_7d_resets_at),
        ] {
            if let Some(pct) = pct {
                if pct >= 100.0 {
                    match resets {
                        Some(resets) => ends.push(resets),
                        None => self.full_without_reset = true,
                    }
                }
                self.used_pct = Some(self.used_pct.map_or(pct, |known| known.max(pct)));
            }
        }
        if let Some(end) = ends.into_iter().max() {
            self.cap_end = Some(self.cap_end.map_or(end, |known| known.max(end)));
        }
    }
}

impl AccountLedger {
    /// The ledger at `path`; empty when it is missing, unreadable, or of
    /// another version.
    #[must_use]
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .filter(|ledger| ledger.version == LEDGER_VERSION)
            .unwrap_or_default()
    }

    /// Write the ledger to `path` atomically, readable by its owner only.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut ledger = self.clone();
        ledger.version = LEDGER_VERSION;
        let bytes = serde_json::to_vec_pretty(&ledger).map_err(std::io::Error::other)?;
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        {
            use std::io::Write;
            let mut file = private_file(&tmp, true)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        std::fs::rename(&tmp, path)
    }

    /// What is known about `account`.
    #[must_use]
    pub fn get(&self, provider: &str, account: &str) -> Option<&AccountLimit> {
        self.accounts
            .iter()
            .find(|limit| limit.provider == provider && limit.account == account)
    }

    /// The binding that says which account `agent` runs on, if muxa put it
    /// on one: the latest that names its conversation, or that started it in
    /// its pane. A pane binding (a launch, whose conversation is not known
    /// yet) is the first agent of its provider to start in that pane after
    /// it, never one started later: a pane can host nested agents.
    #[must_use]
    pub fn binding_for(&self, agent: &Agent, agents: &[Agent]) -> Option<&AccountBinding> {
        let provider = crate::account_swap::provider_name(agent.kind)?;
        let first_in_pane = |binding: &AccountBinding| {
            agents
                .iter()
                .filter(|other| other.kind == agent.kind && binding.placed(other))
                .filter(|other| !crate::state::is_synthetic(&other.session_id))
                .filter(|other| other.started_at >= binding.bound_at)
                .min_by(|a, b| {
                    a.started_at
                        .cmp(&b.started_at)
                        .then_with(|| a.session_id.cmp(&b.session_id))
                })
                .is_some_and(|first| first.session_id == agent.session_id)
        };
        self.bindings
            .iter()
            .filter(|binding| binding.provider == provider)
            .filter(|binding| match &binding.session_id {
                Some(session) => *session == agent.session_id,
                None => {
                    binding.placed(agent)
                        && agent.started_at >= binding.bound_at
                        && first_in_pane(binding)
                }
            })
            .max_by_key(|binding| binding.bound_at)
    }

    /// The account `agent`'s quota readings count for: its binding's, when
    /// they were taken after it; otherwise `record`'s answer (Claude's own
    /// session record) when muxa never bound it. `None` when its readings
    /// cannot be attributed to any account.
    pub fn attribute(
        &self,
        agent: &Agent,
        agents: &[Agent],
        record: impl FnOnce(&Agent) -> Option<String>,
    ) -> Option<String> {
        match self.binding_for(agent, agents) {
            Some(binding) => agent
                .rate_limits_at
                .is_some_and(|at| at >= binding.bound_at)
                .then(|| binding.account.clone()),
            None => record(agent),
        }
    }

    /// Fold what `agents` report into the ledger: each agent's account is
    /// `account_of`'s answer. Accounts with no reporting agent keep what they
    /// had. Returns whether anything changed.
    pub fn observe(
        &mut self,
        provider: &str,
        profiles: &[AccountProfile],
        agents: &[Agent],
        now: OffsetDateTime,
        account_of: impl Fn(&Agent) -> Option<String>,
    ) -> bool {
        let mut live: Vec<Option<Live>> = profiles.iter().map(|_| None).collect();
        for agent in agents
            .iter()
            .filter(|agent| agent.state != AgentState::Stopped)
            .filter(|agent| crate::account_swap::provider_name(agent.kind) == Some(provider))
        {
            let Some(account) = account_of(agent) else {
                continue;
            };
            if let Some(index) = profiles.iter().position(|p| p.name == account) {
                live[index].get_or_insert_with(Live::default).observe(agent);
            }
        }
        let mut changed = false;
        for (profile, live) in profiles.iter().zip(live) {
            if let Some(live) = live {
                changed |= self.fold(provider, &profile.name, &live, now);
            }
        }
        changed | self.expire(now)
    }

    /// Fold what `account`'s agents report now into its entry.
    fn fold(&mut self, provider: &str, account: &str, live: &Live, now: OffsetDateTime) -> bool {
        let found = self
            .accounts
            .iter()
            .position(|l| l.provider == provider && l.account == account);
        let index = found.unwrap_or_else(|| {
            self.accounts.push(AccountLimit {
                provider: provider.to_owned(),
                account: account.to_owned(),
                capped_until: None,
                full_since: None,
                used_pct: None,
                observed_at: now,
            });
            self.accounts.len() - 1
        });
        let entry = &mut self.accounts[index];
        let before = entry.clone();
        let still_capped = entry.capped_until.filter(|end| *end > now);
        entry.capped_until = match (still_capped, live.cap_end.filter(|end| *end > now)) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        entry.full_since = if live.full_without_reset {
            entry.full_since.or(Some(now))
        } else {
            None
        };
        if live.used_pct.is_some() {
            entry.used_pct = live.used_pct;
        }
        entry.observed_at = now;
        *entry != before
    }

    fn expire(&mut self, now: OffsetDateTime) -> bool {
        let before = self.accounts.len();
        self.accounts.retain(|limit| {
            limit.capped_at(now).is_some() || now - limit.observed_at < LEDGER_RETENTION
        });
        self.accounts.len() != before
    }

    /// Record `binding`. It replaces any binding of the same conversation,
    /// or of the same pane that no conversation was pinned to yet.
    pub fn bind(&mut self, binding: AccountBinding) {
        self.bindings.retain(|kept| {
            kept.provider != binding.provider
                || match (&binding.session_id, &kept.session_id) {
                    (Some(new), Some(old)) => new != old,
                    (None, None) => kept.pane != binding.pane || kept.socket != binding.socket,
                    _ => true,
                }
        });
        self.bindings.push(binding);
        self.bindings.sort_by_key(|kept| kept.bound_at);
        let excess = self.bindings.len().saturating_sub(BINDINGS_KEPT);
        self.bindings.drain(..excess);
    }

    /// Pin each pane binding to the conversation it attributes: once the
    /// launched agent is known, a later agent in that pane is not it.
    fn pin(&mut self, agents: &[Agent]) -> bool {
        let pinned: Vec<(usize, String)> = self
            .bindings
            .iter()
            .enumerate()
            .filter(|(_, binding)| binding.session_id.is_none())
            .filter_map(|(index, binding)| {
                agents
                    .iter()
                    .find(|agent| {
                        self.binding_for(agent, agents)
                            .is_some_and(|found| found == binding)
                    })
                    .map(|agent| (index, agent.session_id.clone()))
            })
            .collect();
        let changed = !pinned.is_empty();
        for (index, session) in pinned {
            self.bindings[index].session_id = Some(session);
        }
        changed
    }

    /// `agent` leaves `account` in a swap: what it reported is folded in,
    /// whether or not it is still running a moment later. Only when that is
    /// certain: muxa bound it to `account` and the reading is from after
    /// that, or (never bound) `proven` says the process whose environment
    /// named `account` provably runs this agent's conversation.
    pub fn leave(
        &mut self,
        provider: &str,
        account: &str,
        agent: &Agent,
        proven: bool,
        now: OffsetDateTime,
    ) -> bool {
        let attributed = match self.binding_for(agent, std::slice::from_ref(agent)) {
            Some(binding) => {
                binding.account == account
                    && agent
                        .rate_limits_at
                        .is_some_and(|at| at >= binding.bound_at)
            }
            None => proven,
        };
        if !attributed {
            return false;
        }
        let mut live = Live::default();
        live.observe(agent);
        self.fold(provider, account, &live, now)
    }
}

/// `path` created (or truncated) for its owner only.
fn private_file(path: &Path, truncate: bool) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(truncate);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

/// Read, change and write the ledger at `path` as one step: the daemon and
/// every CLI that swaps or launches write it, one at a time (an advisory
/// lock beside it). `change` says whether it changed anything.
pub fn update(
    path: &Path,
    change: impl FnOnce(&mut AccountLedger) -> bool,
) -> std::io::Result<bool> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock = private_file(&path.with_extension("json.lock"), false)?;
    lock.lock()?;
    let mut ledger = AccountLedger::load(path);
    let changed = change(&mut ledger);
    if changed {
        ledger.save(path)?;
    }
    Ok(changed)
}

/// Fold what `agents` report about every configured account into the
/// ledger at `path`, each agent attributed by [`AccountLedger::attribute`].
/// The daemon calls this as limits are reported, so an account stays capped
/// after its agents are gone. Returns whether the file changed.
pub fn record(
    path: &Path,
    accounts: &AccountsConfig,
    agents: &[Agent],
    now: OffsetDateTime,
) -> std::io::Result<bool> {
    record_with(path, accounts, agents, now, claude_record)
}

fn record_with(
    path: &Path,
    accounts: &AccountsConfig,
    agents: &[Agent],
    now: OffsetDateTime,
    record: impl Fn(&ResolvedAccounts, &Agent) -> Option<String>,
) -> std::io::Result<bool> {
    update(path, |ledger| {
        let mut changed = ledger.pin(agents);
        let bound = ledger.clone();
        for provider in crate::account_route::PROVIDERS {
            let profiles = accounts.for_provider(provider);
            if profiles.is_empty() {
                continue;
            }
            let resolved = ResolvedAccounts::new(profiles);
            changed |= ledger.observe(provider, profiles, agents, now, |agent| {
                bound.attribute(agent, agents, |agent| record(&resolved, agent))
            });
        }
        changed
    })
}

/// The ledger ranking reads by default. Unit tests read none, so they never
/// see this machine's accounts.
#[must_use]
pub fn default_ledger_path() -> Option<PathBuf> {
    if cfg!(test) {
        None
    } else {
        crate::paths::default_account_ledger_file()
    }
}

/// The ledger at [`default_ledger_path`], empty when there is none.
#[must_use]
pub fn default_ledger() -> AccountLedger {
    default_ledger_path()
        .map(|path| AccountLedger::load(&path))
        .unwrap_or_default()
}

/// Record, in the default ledger, that muxa put an agent on `account`.
/// A ledger that cannot be written is reported, never fatal: the agent
/// runs regardless, and is merely not attributed.
pub fn bind_default(binding: AccountBinding) {
    let Some(path) = default_ledger_path() else {
        return;
    };
    if let Err(error) = update(&path, |ledger| {
        ledger.bind(binding);
        true
    }) {
        tracing::warn!(%error, "account ledger: could not record an account binding");
    }
}

/// `profiles` as ranked candidates, from `ledger` with what `agents` report
/// now folded in (`account_of` names each agent's account). Accounts with
/// quota come first, the least used first, ties in config order; capped
/// ones follow, the soonest to reset first. Deterministic for the same
/// observations.
#[must_use]
pub fn rank(
    provider: &str,
    profiles: &[AccountProfile],
    agents: &[Agent],
    now: OffsetDateTime,
    account_of: impl Fn(&Agent) -> Option<String>,
    ledger: &AccountLedger,
) -> Vec<Candidate> {
    let mut ledger = ledger.clone();
    ledger.observe(provider, profiles, agents, now, account_of);
    let mut ranked: Vec<(usize, Candidate, Option<OffsetDateTime>, f32)> = profiles
        .iter()
        .enumerate()
        .map(|(index, profile)| {
            let limit = ledger.get(provider, &profile.name);
            let capped = limit.and_then(|limit| limit.capped_at(now));
            let used = limit.and_then(|limit| limit.used_pct);
            let reason = match (capped, used) {
                (Some(until), _) => format!("rate limited until {}", rfc3339(until)),
                (None, Some(pct)) => format!("quota available ({pct:.0}% used)"),
                (None, None) => "no limit observed".to_owned(),
            };
            let candidate = Candidate {
                name: profile.name.clone(),
                provider: provider.to_owned(),
                eligible: capped.is_none(),
                active: false,
                reason,
                remaining_pct: used.map(|pct| f64::from((100.0 - pct).clamp(0.0, 100.0))),
            };
            (index, candidate, capped, used.unwrap_or(0.0))
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.1.eligible
            .cmp(&a.1.eligible)
            .then_with(|| a.2.cmp(&b.2))
            .then_with(|| a.3.total_cmp(&b.3))
            .then_with(|| a.0.cmp(&b.0))
    });
    ranked
        .into_iter()
        .map(|(_, candidate, _, _)| candidate)
        .collect()
}

/// `profiles` ranked from `ledger` and live `agents`, each attributed by
/// [`AccountLedger::attribute`].
#[must_use]
pub fn rank_observed(
    provider: &str,
    profiles: &[AccountProfile],
    agents: &[Agent],
    now: OffsetDateTime,
    ledger: &AccountLedger,
) -> Vec<Candidate> {
    let resolved = ResolvedAccounts::new(profiles);
    rank(
        provider,
        profiles,
        agents,
        now,
        |agent| ledger.attribute(agent, agents, |agent| claude_record(&resolved, agent)),
        ledger,
    )
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| at.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::RateLimitScope;
    use time::macros::datetime;

    const NOW: OffsetDateTime = datetime!(2026-11-05 12:00:00 UTC);

    fn profile(name: &str) -> AccountProfile {
        AccountProfile {
            name: name.into(),
            env: [("CLAUDE_CONFIG_DIR".to_owned(), format!("/p/{name}"))].into(),
            env_file: None,
        }
    }

    fn agent(session: &str, kind: AgentKind) -> Agent {
        serde_json::from_value(serde_json::json!({
            "kind": kind,
            "session_id": session,
            "state": "idle",
            "started_at": "2026-11-05T11:00:00Z",
            "last_activity_at": "2026-11-05T11:00:00Z",
            "state_entered_at": "2026-11-05T11:00:00Z"
        }))
        .unwrap()
    }

    fn capped(session: &str, until: Option<OffsetDateTime>) -> Agent {
        let mut agent = agent(session, AgentKind::ClaudeCode);
        agent.state = AgentState::Error;
        agent.rate_limit_scope = Some(RateLimitScope::FiveHour);
        agent.rate_limited_until = until;
        agent
    }

    /// Agents named `<account>/<n>` run on `<account>`.
    fn by_session(agent: &Agent) -> Option<String> {
        agent.session_id.split('/').next().map(str::to_owned)
    }

    /// muxa put the conversation `session` on `account` at `at`.
    fn bound(session: &str, account: &str, at: OffsetDateTime) -> AccountBinding {
        AccountBinding {
            provider: "claude".into(),
            account: account.into(),
            session_id: Some(session.into()),
            pane: Some("%4".into()),
            socket: None,
            bound_at: at,
        }
    }

    fn names(ranked: &[Candidate]) -> Vec<&str> {
        ranked.iter().map(|c| c.name.as_str()).collect()
    }

    fn rank_now(
        profiles: &[AccountProfile],
        agents: &[Agent],
        now: OffsetDateTime,
    ) -> Vec<Candidate> {
        rank(
            "claude",
            profiles,
            agents,
            now,
            by_session,
            &AccountLedger::default(),
        )
    }

    #[test]
    fn an_account_capped_now_goes_last_and_ineligible_until_its_reset() {
        let profiles = [profile("work"), profile("home"), profile("spare")];
        let agents = [capped("work/1", Some(NOW + time::Duration::hours(2)))];
        let ranked = rank_now(&profiles, &agents, NOW);
        assert_eq!(names(&ranked), ["home", "spare", "work"]);
        assert!(!ranked[2].eligible);
        assert!(
            ranked[2].reason.contains("2026-11-05T14:00:00Z"),
            "{}",
            ranked[2].reason
        );
        // Once the reset has passed the account is usable again, in config order.
        let later = rank_now(&profiles, &agents, NOW + time::Duration::hours(3));
        assert_eq!(names(&later), ["work", "home", "spare"]);
        assert!(later.iter().all(|c| c.eligible));
    }

    #[test]
    fn an_unreported_reset_is_waited_out_for_the_cooldown() {
        let profiles = [profile("work"), profile("home")];
        let mut agent = capped("work/1", None);
        agent.state_entered_at = NOW - time::Duration::minutes(10);
        let ranked = rank_now(&profiles, std::slice::from_ref(&agent), NOW);
        assert_eq!(names(&ranked), ["home", "work"]);
        assert!(!ranked[1].eligible);
        let after = NOW + UNKNOWN_RESET_COOLDOWN;
        assert!(rank_now(&profiles, &[agent], after)
            .iter()
            .all(|c| c.eligible));
    }

    #[test]
    fn a_full_window_caps_an_account_whose_agent_has_not_hit_it_yet() {
        let profiles = [profile("work"), profile("home")];
        let mut idle = agent("work/1", AgentKind::ClaudeCode);
        idle.rate_limit_5h_pct = Some(100.0);
        idle.rate_limit_5h_resets_at = Some(NOW + time::Duration::minutes(30));
        let ranked = rank_now(&profiles, &[idle], NOW);
        assert_eq!(names(&ranked), ["home", "work"]);
        assert!(!ranked[1].eligible);
    }

    /// A window reported full with no reset time is not quota left: with no
    /// other account, nothing is eligible, until the cooldown from when it
    /// was first seen full.
    #[test]
    fn a_full_window_without_a_reset_time_is_capped_for_the_cooldown() {
        let profiles = [profile("only")];
        let mut idle = agent("only/1", AgentKind::ClaudeCode);
        idle.rate_limit_5h_pct = Some(100.0);
        let ranked = rank_now(&profiles, std::slice::from_ref(&idle), NOW);
        assert!(!ranked[0].eligible, "{}", ranked[0].reason);
        assert!(crate::account_route::choose(&ranked, "claude").is_err());
        // The cooldown runs from the first report, not from each re-read.
        let mut ledger = AccountLedger::default();
        ledger.observe(
            "claude",
            &profiles,
            std::slice::from_ref(&idle),
            NOW,
            by_session,
        );
        let later = NOW + UNKNOWN_RESET_COOLDOWN;
        let ranked = rank("claude", &profiles, &[idle], later, by_session, &ledger);
        assert!(ranked[0].eligible, "{}", ranked[0].reason);
    }

    #[test]
    fn eligible_accounts_rank_by_reported_usage_then_config_order() {
        let profiles = [profile("a"), profile("b"), profile("c"), profile("d")];
        let mut busy = agent("a/1", AgentKind::ClaudeCode);
        busy.rate_limit_5h_pct = Some(80.0);
        let mut light = agent("b/1", AgentKind::ClaudeCode);
        light.rate_limit_7d_pct = Some(20.0);
        // Another provider's agent says nothing about these accounts.
        let mut codex = agent("c/1", AgentKind::Codex);
        codex.rate_limit_5h_pct = Some(99.0);
        let ranked = rank_now(&profiles, &[busy, light, codex], NOW);
        // Unobserved accounts count as unused.
        assert_eq!(names(&ranked), ["c", "d", "b", "a"]);
        assert_eq!(ranked[3].remaining_pct, Some(20.0));
        assert!(ranked[3].reason.contains("80% used"));
    }

    #[test]
    fn stopped_and_unidentified_agents_are_ignored() {
        let profiles = [profile("work"), profile("home")];
        let mut stopped = capped("work/1", Some(NOW + time::Duration::hours(1)));
        stopped.state = AgentState::Stopped;
        let stranger = capped("elsewhere/1", Some(NOW + time::Duration::hours(1)));
        let ranked = rank_now(&profiles, &[stopped, stranger], NOW);
        assert_eq!(names(&ranked), ["work", "home"]);
        assert!(ranked.iter().all(|c| c.eligible));
    }

    /// The cap is the account's, not its agent's: stopping the agent, or
    /// swapping it onto another account, leaves the account capped until its
    /// reset, and the account it moved to does not inherit its usage.
    #[test]
    fn an_account_stays_capped_after_its_agent_is_stopped_or_swapped_away() {
        let profiles = [profile("a"), profile("b")];
        let reset = NOW + time::Duration::hours(5);
        let mut on_a = capped("conv", Some(reset));
        on_a.rate_limit_5h_pct = Some(100.0);
        on_a.rate_limit_5h_resets_at = Some(reset);
        on_a.rate_limits_at = Some(NOW);
        let mut ledger = AccountLedger::default();
        ledger.bind(bound("conv", "a", NOW - time::Duration::hours(1)));
        let observe = |ledger: &mut AccountLedger, agents: &[Agent], now| {
            let bindings = ledger.clone();
            ledger.observe("claude", &profiles, agents, now, |agent| {
                bindings.attribute(agent, agents, |_| None)
            })
        };
        assert!(observe(&mut ledger, std::slice::from_ref(&on_a), NOW));

        // Stopped: no live agent on `a` at all.
        let later = NOW + time::Duration::minutes(10);
        let ranked = rank_observed("claude", &profiles, &[], later, &ledger);
        assert_eq!(names(&ranked), ["b", "a"]);
        assert!(!ranked[1].eligible);

        // Swapped a -> b: the same row, still showing what it read on `a`,
        // is bound to `b` from the swap on. That reading is `a`'s.
        ledger.bind(bound("conv", "b", later));
        let mut on_b = on_a.clone();
        on_b.state = AgentState::Working;
        let ranked = rank_observed(
            "claude",
            &profiles,
            std::slice::from_ref(&on_b),
            later,
            &ledger,
        );
        assert_eq!(names(&ranked), ["b", "a"]);
        assert!(ranked[0].eligible, "{}", ranked[0].reason);
        assert_eq!(ranked[0].reason, "no limit observed");
        assert!(!ranked[1].eligible, "a stays capped after the swap");

        // A new launch before the reset must not pick `a`.
        let only_a = [profile("a")];
        let ranked = rank_observed("claude", &only_a, &[], later, &ledger);
        assert!(crate::account_route::choose(&ranked, "claude").is_err());
        // After the reset it can.
        let after = reset + time::Duration::seconds(1);
        let ranked = rank_observed("claude", &only_a, &[], after, &ledger);
        assert_eq!(
            crate::account_route::choose(&ranked, "claude").unwrap(),
            "a"
        );
    }

    /// Last-known usage outlives the agents too, so an account that was
    /// nearly spent is not preferred over a fresh one once its agents leave.
    #[test]
    fn last_known_usage_is_kept_when_the_agents_are_gone() {
        let profiles = [profile("a"), profile("b")];
        let mut on_a = agent("a/1", AgentKind::ClaudeCode);
        on_a.rate_limit_5h_pct = Some(90.0);
        let mut ledger = AccountLedger::default();
        ledger.observe("claude", &profiles, &[on_a], NOW, by_session);
        let ranked = rank("claude", &profiles, &[], NOW, by_session, &ledger);
        assert_eq!(names(&ranked), ["b", "a"]);
        assert!(ranked[1].reason.contains("90% used"));
    }

    /// A Codex conversation resumed on `b` by a swap still shows the window
    /// Codex wrote on `a` until it writes another. That reading predates the
    /// binding to `b`, so it counts for neither account; the next one counts
    /// for `b`.
    #[test]
    fn a_reading_from_before_the_binding_counts_for_no_account() {
        let profiles = [profile("a"), profile("b")];
        let mut row = agent("conv", AgentKind::Codex);
        row.rate_limit_5h_pct = Some(100.0);
        row.rate_limit_5h_resets_at = Some(NOW + time::Duration::hours(3));
        row.rate_limits_at = Some(NOW - time::Duration::minutes(5));
        let mut ledger = AccountLedger::default();
        ledger.bind(AccountBinding {
            provider: "codex".into(),
            ..bound("conv", "b", NOW - time::Duration::minutes(1))
        });
        let agents = std::slice::from_ref(&row);
        assert_eq!(ledger.attribute(&row, agents, |_| unreachable!()), None);
        let ranked = rank_observed("codex", &profiles, agents, NOW, &ledger);
        assert!(ranked.iter().all(|c| c.eligible), "{ranked:?}");

        row.rate_limits_at = Some(NOW);
        assert_eq!(
            ledger
                .attribute(&row, std::slice::from_ref(&row), |_| None)
                .as_deref(),
            Some("b")
        );
    }

    /// A launch binds the pane it starts in, before the conversation is
    /// known. Only the first agent of that provider to start there after it
    /// is the launched one: a nested agent started later in the same pane,
    /// on another account, is not. Once recorded, the binding is pinned to
    /// the launched conversation.
    #[test]
    fn a_pane_binding_attributes_only_the_first_agent_started_there() {
        let mut parent = agent("parent", AgentKind::ClaudeCode);
        parent.pane = Some("%4".into());
        parent.started_at = NOW - time::Duration::minutes(30);
        parent.rate_limits_at = Some(NOW);
        let mut nested = capped("nested", Some(NOW + time::Duration::hours(2)));
        nested.pane = Some("%4".into());
        nested.started_at = NOW - time::Duration::minutes(5);
        nested.rate_limits_at = Some(NOW);
        let mut ledger = AccountLedger::default();
        ledger.bind(AccountBinding {
            session_id: None,
            ..bound("", "a", NOW - time::Duration::hours(1))
        });
        let agents = [parent.clone(), nested.clone()];
        assert_eq!(
            ledger.attribute(&parent, &agents, |_| None).as_deref(),
            Some("a")
        );
        assert_eq!(ledger.attribute(&nested, &agents, |_| None), None);
        let profiles = [profile("a"), profile("b")];
        let ranked = rank_observed("claude", &profiles, &agents, NOW, &ledger);
        assert!(
            ranked.iter().all(|c| c.eligible),
            "the nested agent's cap must not be credited to the launched account"
        );

        // Pinned: once the parent is gone from the registry, a later agent
        // in that pane is still not the launched one.
        assert!(ledger.pin(&agents));
        assert_eq!(ledger.attribute(&nested, &[nested.clone()], |_| None), None);
        assert_eq!(
            ledger
                .attribute(&parent, &[parent.clone()], |_| None)
                .as_deref(),
            Some("a")
        );
    }

    /// An agent muxa never bound is attributed only by its provider's own
    /// record; without one its readings say nothing.
    #[test]
    fn an_unbound_agent_is_attributed_only_by_its_provider_record() {
        let row = capped("free", Some(NOW + time::Duration::hours(1)));
        let ledger = AccountLedger::default();
        assert_eq!(ledger.attribute(&row, &[], |_| None), None);
        assert_eq!(
            ledger.attribute(&row, &[], |_| Some("a".into())).as_deref(),
            Some("a")
        );
    }

    /// A swap records the cap its target leaves before stopping it: it is
    /// kept whether or not the agent is still running when the daemon next
    /// records, and only when the account is certain.
    #[test]
    fn a_cap_left_in_a_swap_is_kept_after_its_agent_is_gone() {
        let profiles = [profile("a"), profile("b")];
        let mut leaving = capped("conv", Some(NOW + time::Duration::hours(2)));
        leaving.rate_limits_at = Some(NOW);
        let mut ledger = AccountLedger::default();
        // Not proven, never bound: nothing is known about its account.
        assert!(!ledger.leave("claude", "a", &leaving, false, NOW));
        assert!(ledger.leave("claude", "a", &leaving, true, NOW));
        // The agent is gone; the daemon's next record sees no agent at all.
        let mut stopped = leaving.clone();
        stopped.state = AgentState::Stopped;
        let bindings = ledger.clone();
        let agents = std::slice::from_ref(&stopped);
        ledger.observe("claude", &profiles, agents, NOW, |agent| {
            bindings.attribute(agent, agents, |_| Some("a".into()))
        });
        let ranked = rank_observed("claude", &profiles, &[], NOW, &ledger);
        assert_eq!(names(&ranked), ["b", "a"]);
        assert!(!ranked[1].eligible);

        // Bound to another account, its reading is not `a`'s to leave.
        let mut other = AccountLedger::default();
        other.bind(bound("conv", "b", NOW - time::Duration::hours(1)));
        assert!(!other.leave("claude", "a", &leaving, true, NOW));
    }

    #[test]
    fn updates_go_through_the_ledger_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("muxa").join("account-limits.json");
        assert!(update(&path, |ledger| {
            ledger.bind(bound("conv", "a", NOW));
            true
        })
        .unwrap());
        assert!(!update(&path, |_| false).unwrap());
        let read = AccountLedger::load(&path);
        assert_eq!(read.bindings, vec![bound("conv", "a", NOW)]);
    }

    #[test]
    fn the_ledger_round_trips_and_ignores_other_versions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("muxa").join("account-limits.json");
        let profiles = [profile("a")];
        let mut ledger = AccountLedger::default();
        ledger.observe(
            "claude",
            &profiles,
            &[capped("a/1", Some(NOW + time::Duration::hours(1)))],
            NOW,
            by_session,
        );
        ledger.save(&path).unwrap();
        let read = AccountLedger::load(&path);
        assert!(read
            .get("claude", "a")
            .is_some_and(|l| l.capped_at(NOW).is_some()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        std::fs::write(&path, r#"{"version":99,"accounts":[]}"#).unwrap();
        assert_eq!(AccountLedger::load(&path), AccountLedger::default());
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(AccountLedger::load(&path), AccountLedger::default());
    }

    #[test]
    fn backend_prefers_configured_accounts_then_aas_then_none() {
        let mut accounts = AccountsConfig::default();
        let found = |_: &str| Some("/opt/aas".to_owned());
        let missing = |_: &str| None;
        assert_eq!(
            backend_with("claude", &accounts, "aas", found),
            Backend::Aas("/opt/aas".into())
        );
        let Backend::Unavailable(reason) = backend_with("codex", &accounts, "aas", missing) else {
            panic!("no backend expected");
        };
        assert!(reason.contains("[[accounts.codex]]"), "{reason}");
        assert!(reason.contains("resume-after-limit"), "{reason}");
        accounts.claude.push(profile("work"));
        assert_eq!(
            backend_with("claude", &accounts, "aas", |_: &str| -> Option<String> {
                panic!("configured accounts never look for aas")
            }),
            Backend::Accounts(vec![profile("work")])
        );
        // Codex lists none, so it keeps its own fallback.
        assert_eq!(
            backend_with("codex", &accounts, "aas", found),
            Backend::Aas("/opt/aas".into())
        );
    }
}
