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

use crate::account_swap::Candidate;
use crate::accounts::{AccountProfile, AccountsConfig, ResolvedAccounts};
use crate::state::Agent;
use crate::AgentState;
use time::OffsetDateTime;

/// How long an account is skipped after a limit whose reset time was never
/// reported. Most limits do report one; this only covers those that do not.
pub const UNKNOWN_RESET_COOLDOWN: time::Duration = time::Duration::hours(1);

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

/// The configured account `agent` runs on, read from its process
/// environment. `None` when the process cannot be read or matches none.
#[must_use]
pub fn identify(resolved: &ResolvedAccounts, agent: &Agent) -> Option<String> {
    let pid = agent.pid.or(agent.workload.primary_pid)?;
    let environ = crate::accounts::process_environ(pid);
    if environ.is_empty() {
        return None;
    }
    resolved.identify(&environ).map(str::to_owned)
}

/// What the agents on one account say about it.
#[derive(Debug, Clone, Default)]
struct Usage {
    /// The account has no quota until then.
    capped_until: Option<OffsetDateTime>,
    /// The highest share of a limit window any of its agents reports used.
    used_pct: Option<f32>,
}

impl Usage {
    fn observe(&mut self, agent: &Agent, now: OffsetDateTime) {
        if let Some(until) = cap_end(agent, now) {
            self.capped_until = Some(self.capped_until.map_or(until, |known| known.max(until)));
        }
        for pct in [agent.rate_limit_5h_pct, agent.rate_limit_7d_pct]
            .into_iter()
            .flatten()
        {
            self.used_pct = Some(self.used_pct.map_or(pct, |known| known.max(pct)));
        }
    }
}

/// When the limit `agent` reports ends, if it is still in force at `now`.
fn cap_end(agent: &Agent, now: OffsetDateTime) -> Option<OffsetDateTime> {
    let mut ends = Vec::new();
    if crate::account_swap::is_rate_limited(agent) {
        ends.push(
            agent
                .rate_limited_until
                .unwrap_or(agent.state_entered_at + UNKNOWN_RESET_COOLDOWN),
        );
    }
    // A window reported full is spent until it resets, whether or not the
    // agent has hit it yet.
    for (pct, resets) in [
        (agent.rate_limit_5h_pct, agent.rate_limit_5h_resets_at),
        (agent.rate_limit_7d_pct, agent.rate_limit_7d_resets_at),
    ] {
        if let (Some(pct), Some(resets)) = (pct, resets) {
            if pct >= 100.0 {
                ends.push(resets);
            }
        }
    }
    ends.into_iter().filter(|end| *end > now).max()
}

/// `profiles` as ranked candidates, from what the agents in `agents` report
/// about the account each runs on (`identify` names it). Accounts with
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
) -> Vec<Candidate> {
    let mut usage: Vec<Usage> = vec![Usage::default(); profiles.len()];
    for agent in agents
        .iter()
        .filter(|agent| agent.state != AgentState::Stopped)
        .filter(|agent| crate::account_swap::provider_name(agent.kind) == Some(provider))
    {
        let Some(account) = identify(agent) else {
            continue;
        };
        if let Some(index) = profiles.iter().position(|p| p.name == account) {
            usage[index].observe(agent, now);
        }
    }
    let mut ranked: Vec<(usize, Candidate, Option<OffsetDateTime>, f32)> = profiles
        .iter()
        .zip(usage)
        .enumerate()
        .map(|(index, (profile, usage))| {
            let reason = match (usage.capped_until, usage.used_pct) {
                (Some(until), _) => format!("rate limited until {}", rfc3339(until)),
                (None, Some(pct)) => format!("quota available ({pct:.0}% used)"),
                (None, None) => "no limit observed".to_owned(),
            };
            let candidate = Candidate {
                name: profile.name.clone(),
                provider: provider.to_owned(),
                eligible: usage.capped_until.is_none(),
                active: false,
                reason,
                remaining_pct: usage
                    .used_pct
                    .map(|pct| f64::from((100.0 - pct).clamp(0.0, 100.0))),
            };
            (
                index,
                candidate,
                usage.capped_until,
                usage.used_pct.unwrap_or(0.0),
            )
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

/// `profiles` ranked from live agents, each identified by its process
/// environment.
#[must_use]
pub fn rank_observed(
    provider: &str,
    profiles: &[AccountProfile],
    agents: &[Agent],
    now: OffsetDateTime,
) -> Vec<Candidate> {
    let resolved = ResolvedAccounts::new(profiles);
    rank(provider, profiles, agents, now, |agent| {
        identify(&resolved, agent)
    })
}

fn rfc3339(at: OffsetDateTime) -> String {
    at.format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| at.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::RateLimitScope;
    use crate::AgentKind;
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

    #[test]
    fn an_account_capped_now_goes_last_and_ineligible_until_its_reset() {
        let profiles = [profile("work"), profile("home"), profile("spare")];
        let agents = [capped("work/1", Some(NOW + time::Duration::hours(2)))];
        let ranked = rank("claude", &profiles, &agents, NOW, by_session);
        assert_eq!(names(&ranked), ["home", "spare", "work"]);
        assert!(!ranked[2].eligible);
        assert!(
            ranked[2].reason.contains("2026-11-05T14:00:00Z"),
            "{}",
            ranked[2].reason
        );
        // Once the reset has passed the account is usable again, in config order.
        let later = rank(
            "claude",
            &profiles,
            &agents,
            NOW + time::Duration::hours(3),
            by_session,
        );
        assert_eq!(names(&later), ["work", "home", "spare"]);
        assert!(later.iter().all(|c| c.eligible));
    }

    #[test]
    fn an_unreported_reset_is_waited_out_for_the_cooldown() {
        let profiles = [profile("work"), profile("home")];
        let mut agent = capped("work/1", None);
        agent.state_entered_at = NOW - time::Duration::minutes(10);
        let ranked = rank("claude", &profiles, &[agent.clone()], NOW, by_session);
        assert_eq!(names(&ranked), ["home", "work"]);
        assert!(!ranked[1].eligible);
        let after = NOW + UNKNOWN_RESET_COOLDOWN;
        let ranked = rank("claude", &profiles, &[agent], after, by_session);
        assert!(ranked.iter().all(|c| c.eligible));
    }

    #[test]
    fn a_full_window_caps_an_account_whose_agent_has_not_hit_it_yet() {
        let profiles = [profile("work"), profile("home")];
        let mut idle = agent("work/1", AgentKind::ClaudeCode);
        idle.rate_limit_5h_pct = Some(100.0);
        idle.rate_limit_5h_resets_at = Some(NOW + time::Duration::minutes(30));
        let ranked = rank("claude", &profiles, &[idle], NOW, by_session);
        assert_eq!(names(&ranked), ["home", "work"]);
        assert!(!ranked[1].eligible);
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
        let ranked = rank("claude", &profiles, &[busy, light, codex], NOW, by_session);
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
        let ranked = rank("claude", &profiles, &[stopped, stranger], NOW, by_session);
        assert_eq!(names(&ranked), ["work", "home"]);
        assert!(ranked.iter().all(|c| c.eligible));
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
