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
//! after its last agent is stopped or swapped away. The daemon folds what
//! live agents report into an [`AccountLedger`] (its own file, outside the
//! config) as limits are reported, and ranking reads it back. An agent's
//! account is only ever read from a process provably running that agent's
//! session; an agent whose binding is uncertain says nothing about any
//! account.

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

/// The configured account `agent` runs on, read from the environment of a
/// process provably running its session. `None` when there is no such
/// process, it cannot be read, or it matches no account.
#[must_use]
pub fn identify(resolved: &ResolvedAccounts, agent: &Agent) -> Option<String> {
    identify_with(
        resolved,
        agent,
        crate::account_swap::process_runs_session,
        crate::accounts::process_environ,
    )
}

/// [`identify`] with the session check (`runs(pid, kind, session)`) and the
/// environment read supplied.
fn identify_with(
    resolved: &ResolvedAccounts,
    agent: &Agent,
    runs: impl Fn(u32, AgentKind, &str) -> bool,
    environ: impl Fn(u32) -> Vec<(String, String)>,
) -> Option<String> {
    // The pane's first agent process, shared by every row in the pane: it is
    // this agent's only when it provably runs this agent's session. A nested
    // agent on another account must not be credited to its parent's.
    let pid = agent.workload.primary_pid?;
    if !runs(pid, agent.kind, &agent.session_id) {
        return None;
    }
    let environ = environ(pid);
    if environ.is_empty() {
        return None;
    }
    resolved.identify(&environ).map(str::to_owned)
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

/// Account limits that outlive the agents that reported them. Its own file
/// (see [`crate::paths::default_account_ledger_file`]); no configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AccountLedger {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    accounts: Vec<AccountLimit>,
}

/// What the live agents on one account report now.
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
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp)?;
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

    /// Fold what `agents` report into the ledger: each agent's account is
    /// `identify`'s answer. Accounts with no reporting agent keep what they
    /// had. Returns whether anything changed.
    pub fn observe(
        &mut self,
        provider: &str,
        profiles: &[AccountProfile],
        agents: &[Agent],
        now: OffsetDateTime,
        identify: impl Fn(&Agent) -> Option<String>,
    ) -> bool {
        let mut live: Vec<Option<Live>> = profiles.iter().map(|_| None).collect();
        for agent in agents
            .iter()
            .filter(|agent| agent.state != AgentState::Stopped)
            .filter(|agent| crate::account_swap::provider_name(agent.kind) == Some(provider))
        {
            let Some(account) = identify(agent) else {
                continue;
            };
            if let Some(index) = profiles.iter().position(|p| p.name == account) {
                live[index].get_or_insert_with(Live::default).observe(agent);
            }
        }
        let mut changed = false;
        for (profile, live) in profiles.iter().zip(live) {
            let Some(live) = live else { continue };
            let found = self
                .accounts
                .iter()
                .position(|l| l.provider == provider && l.account == profile.name);
            let index = found.unwrap_or_else(|| {
                self.accounts.push(AccountLimit {
                    provider: provider.to_owned(),
                    account: profile.name.clone(),
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
            changed |= *entry != before;
        }
        let before = self.accounts.len();
        self.accounts.retain(|limit| {
            limit.capped_at(now).is_some() || now - limit.observed_at < LEDGER_RETENTION
        });
        changed || self.accounts.len() != before
    }
}

/// Fold what live `agents` report about every configured account into the
/// ledger at `path`. The daemon calls this as limits are reported, so an
/// account stays capped after its agents are gone. Returns whether the
/// file changed.
pub fn record(
    path: &Path,
    accounts: &AccountsConfig,
    agents: &[Agent],
    now: OffsetDateTime,
) -> std::io::Result<bool> {
    // One writer at a time within this process.
    static WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _write = WRITE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut ledger = AccountLedger::load(path);
    let mut changed = false;
    for provider in crate::account_route::PROVIDERS {
        let profiles = accounts.for_provider(provider);
        if profiles.is_empty() {
            continue;
        }
        let resolved = ResolvedAccounts::new(profiles);
        changed |= ledger.observe(provider, profiles, agents, now, |agent| {
            identify(&resolved, agent)
        });
    }
    if changed {
        ledger.save(path)?;
    }
    Ok(changed)
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

/// `profiles` as ranked candidates, from `ledger` with what `agents` report
/// now folded in (`identify` names each agent's account). Accounts with
/// quota come first, the least used first, ties in config order; capped
/// ones follow, the soonest to reset first. Deterministic for the same
/// observations.
#[must_use]
pub fn rank(
    provider: &str,
    profiles: &[AccountProfile],
    agents: &[Agent],
    now: OffsetDateTime,
    identify: impl Fn(&Agent) -> Option<String>,
    ledger: &AccountLedger,
) -> Vec<Candidate> {
    let mut ledger = ledger.clone();
    ledger.observe(provider, profiles, agents, now, identify);
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

/// `profiles` ranked from the daemon's ledger and live agents, each
/// identified by a process provably running its session.
#[must_use]
pub fn rank_observed(
    provider: &str,
    profiles: &[AccountProfile],
    agents: &[Agent],
    now: OffsetDateTime,
) -> Vec<Candidate> {
    let resolved = ResolvedAccounts::new(profiles);
    let ledger = default_ledger_path()
        .map(|path| AccountLedger::load(&path))
        .unwrap_or_default();
    rank(
        provider,
        profiles,
        agents,
        now,
        |agent| identify(&resolved, agent),
        &ledger,
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
        let mut ledger = AccountLedger::default();
        let on_account = |account: &'static str| move |_: &Agent| Some(account.to_owned());
        assert!(ledger.observe("claude", &profiles, &[on_a], NOW, on_account("a")));

        // Stopped: no live agent on `a` at all.
        let later = NOW + time::Duration::minutes(10);
        let ranked = rank("claude", &profiles, &[], later, by_session, &ledger);
        assert_eq!(names(&ranked), ["b", "a"]);
        assert!(!ranked[1].eligible);

        // Swapped a -> b: the same conversation now runs on `b`, freshly
        // started (the registry clears its cap and usage on a new session).
        let mut on_b = agent("conv", AgentKind::ClaudeCode);
        on_b.state = AgentState::Working;
        let ranked = rank(
            "claude",
            &profiles,
            &[on_b],
            later,
            on_account("b"),
            &ledger,
        );
        assert_eq!(names(&ranked), ["b", "a"]);
        assert!(ranked[0].eligible);
        assert_eq!(ranked[0].reason, "no limit observed");
        assert!(!ranked[1].eligible, "a stays capped after the swap");

        // A new launch before the reset must not pick `a`.
        let only_a = [profile("a")];
        let ranked = rank("claude", &only_a, &[], later, by_session, &ledger);
        assert!(crate::account_route::choose(&ranked, "claude").is_err());
        // After the reset it can.
        let after = reset + time::Duration::seconds(1);
        let ranked = rank("claude", &only_a, &[], after, by_session, &ledger);
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

    /// Two rows in one pane share the pane's first agent process. Only the
    /// row whose session that process provably runs is attributed to its
    /// account; the nested agent, on another account, is attributed to none.
    #[test]
    fn a_shared_pane_process_is_attributed_only_to_the_session_it_runs() {
        let profiles = [profile("a"), profile("b")];
        let resolved = ResolvedAccounts::new(&profiles);
        let mut parent = agent("parent-session", AgentKind::ClaudeCode);
        parent.workload.primary_pid = Some(100);
        let mut nested = capped("nested-session", Some(NOW + time::Duration::hours(2)));
        nested.workload.primary_pid = Some(100);
        let runs =
            |pid: u32, _: AgentKind, session: &str| pid == 100 && session == "parent-session";
        let environ = |_: u32| vec![("CLAUDE_CONFIG_DIR".to_owned(), "/p/a".to_owned())];
        assert_eq!(
            identify_with(&resolved, &parent, runs, environ).as_deref(),
            Some("a")
        );
        assert_eq!(identify_with(&resolved, &nested, runs, environ), None);
        let ranked = rank(
            "claude",
            &profiles,
            &[parent, nested],
            NOW,
            |agent| identify_with(&resolved, agent, runs, environ),
            &AccountLedger::default(),
        );
        assert!(
            ranked.iter().all(|c| c.eligible),
            "the nested agent's cap must not be credited to the parent's account"
        );
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
