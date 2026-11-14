//! Where a provider's accounts come from, and which one an agent should use.
//!
//! Account routing (`[agent.<program>].account`) and swaps (`tend swap`, the
//! `swap_account` automation action) need two things from a provider's
//! accounts: their state (which still have quota) and how to start the CLI on
//! one. Three backends answer that, in this order of preference:
//!
//! 1. **Configured accounts**: `[[accounts.<provider>]]` in `config.toml`,
//!    each the environment that selects it (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
//!    a token file). No external tool is involved. They are ranked by the
//!    usage tend has observed on each ([`configured`],
//!    [`crate::account_usage`]): a reading counts for an account only when
//!    the agent process that reported it runs with that account's
//!    environment. With nothing observed, config order decides.
//! 2. **`aas`**: when no accounts are configured and the `aas` binary is
//!    installed (or `[swap].aas` names one), `aas candidates` ranks its own
//!    accounts and `aas e <name>` starts the CLI.
//! 3. **None**: launches start the provider CLI as it is, and a swap has no
//!    account to move to. A capped agent then waits for its reset, which the
//!    `resume-after-limit` automation rule resumes; that rule needs no
//!    accounts at all.

use crate::account_swap::Candidate;
use tend::accounts::{AccountProfile, AccountsConfig};
pub use tend::swap_api::ProviderCandidates;

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
         config.toml (or install aas); a capped agent can still resume after its reset \
         once the resume-after-limit rule is enabled \
         (`tend automation enable resume-after-limit`)"
    )
}

/// `profiles` as candidates, ranked by what `usage` has seen of them at
/// `now`. An account in `capped` (the one an agent being swapped is capped
/// on) or one the ledger holds capped is ineligible. The eligible ones go
/// first, the one with the most quota left first; an account with no
/// current reading counts as unused, and ties keep config order, so an
/// empty ledger ranks exactly in config order.
#[must_use]
pub fn configured(
    provider: &str,
    profiles: &[AccountProfile],
    capped: &[&str],
    usage: &crate::account_usage::Ledger,
    now: time::OffsetDateTime,
) -> Vec<Candidate> {
    let mut ranked: Vec<Candidate> = profiles
        .iter()
        .map(|profile| {
            let standing = usage.standing(provider, &profile.name, now);
            let capped_here = capped.contains(&profile.name.as_str());
            let reason = if capped_here {
                "rate limited here".to_owned()
            } else if let Some(until) = standing.capped_until {
                format!(
                    "capped until {}",
                    until
                        .format(&time::format_description::well_known::Rfc3339)
                        .unwrap_or_default()
                )
            } else if let Some(used) = standing.used_pct {
                format!("configured; {used:.0}% used")
            } else {
                "configured".to_owned()
            };
            Candidate {
                name: profile.name.clone(),
                provider: provider.to_owned(),
                eligible: !capped_here && standing.capped_until.is_none(),
                active: false,
                reason,
                remaining_pct: standing
                    .used_pct
                    .map(|used| f64::from((100.0 - used).clamp(0.0, 100.0))),
            }
        })
        .collect();
    // Stable: equal keys keep config order.
    ranked.sort_by(|a, b| {
        b.eligible.cmp(&a.eligible).then_with(|| {
            let left = |c: &Candidate| c.remaining_pct.unwrap_or(100.0);
            left(b).total_cmp(&left(a))
        })
    });
    ranked
}

/// Resolve the same backend as launches and swaps, preserving per-provider errors.
pub fn candidates(provider: &str, accounts: &AccountsConfig, aas: &str) -> ProviderCandidates {
    let (source, result) = match backend(provider, accounts, aas) {
        Backend::Accounts(profiles) => (
            "configured",
            Ok(configured(
                provider,
                &profiles,
                &[],
                &crate::account_usage::load_default(),
                time::OffsetDateTime::now_utc(),
            )),
        ),
        Backend::Aas(program) => (
            "aas",
            crate::account_swap::query_candidates_within(
                &program,
                provider,
                std::time::Duration::from_secs(10),
            ),
        ),
        Backend::Unavailable(reason) => ("unavailable", Err(reason)),
    };
    let (candidates, reason) = match result {
        Ok(candidates) => (candidates, None),
        Err(reason) => (Vec::new(), Some(reason)),
    };
    ProviderCandidates {
        provider: provider.into(),
        source: source.into(),
        reason,
        candidates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(name: &str) -> AccountProfile {
        AccountProfile {
            name: name.into(),
            env: [("CLAUDE_CONFIG_DIR".to_owned(), format!("/p/{name}"))].into(),
            env_file: None,
        }
    }

    #[test]
    fn configured_accounts_go_in_config_order_and_a_capped_one_is_ineligible() {
        let profiles = [profile("a"), profile("b"), profile("c")];
        let none = crate::account_usage::Ledger::default();
        let now = time::OffsetDateTime::now_utc();
        let candidates = configured("claude", &profiles, &["a"], &none, now);
        assert_eq!(
            candidates
                .iter()
                .map(|c| (c.name.as_str(), c.eligible))
                .collect::<Vec<_>>(),
            [("b", true), ("c", true), ("a", false)]
        );
        assert_eq!(candidates[2].reason, "rate limited here");
        let all = configured("claude", &profiles, &[], &none, now);
        assert!(all.iter().all(|c| c.eligible));
        assert_eq!(
            all.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"],
            "an empty ledger keeps config order"
        );
    }

    #[test]
    fn configured_accounts_rank_by_observed_usage_and_skip_held_caps() {
        use crate::account_usage::{Ledger, Window, WindowReading};
        let now = time::OffsetDateTime::from_unix_timestamp(1_800_000_000).unwrap();
        let hour = time::Duration::hours(1);
        let reading = |used_pct| WindowReading {
            used_pct,
            resets_at: Some(now + hour),
            observed_at: now - hour,
        };
        let mut ledger = Ledger::default();
        ledger
            .accounts
            .entry("claude/a".into())
            .or_default()
            .windows
            .insert(Window::FiveHour, reading(80.0));
        ledger
            .accounts
            .entry("claude/b".into())
            .or_default()
            .hold("test", now + hour);
        ledger
            .accounts
            .entry("claude/c".into())
            .or_default()
            .windows
            .insert(Window::SevenDay, reading(30.0));
        // A reading for another provider's account of the same name.
        ledger
            .accounts
            .entry("codex/d".into())
            .or_default()
            .hold("test", now + hour);
        let profiles = [profile("a"), profile("b"), profile("c"), profile("d")];
        let ranked = configured("claude", &profiles, &[], &ledger, now);
        assert_eq!(
            ranked
                .iter()
                .map(|c| (c.name.as_str(), c.eligible))
                .collect::<Vec<_>>(),
            [("d", true), ("c", true), ("a", true), ("b", false)]
        );
        assert_eq!(ranked[1].remaining_pct, Some(70.0));
        assert!(
            ranked[3].reason.starts_with("capped until "),
            "{}",
            ranked[3].reason
        );
        // Once the cap and the readings lapse, config order is back.
        let later = configured("claude", &profiles, &[], &ledger, now + hour * 2);
        assert_eq!(
            later.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c", "d"]
        );
    }

    #[test]
    fn candidate_listing_prefers_config_and_exposes_no_environment() {
        let mut accounts = AccountsConfig::default();
        accounts.claude.push(profile("work"));
        let listing = candidates("claude", &accounts, "/missing/aas");
        assert_eq!(listing.source, "configured");
        assert!(listing.reason.is_none());
        assert_eq!(listing.candidates[0].name, "work");
        assert!(listing.candidates[0].eligible);
        let json = serde_json::to_string(&listing).unwrap();
        assert!(!json.contains("CLAUDE_CONFIG_DIR"));
        assert!(!json.contains("/p/work"));
        let missing = candidates("codex", &accounts, "/missing/aas");
        assert_eq!(missing.source, "unavailable");
        assert!(missing.candidates.is_empty());
        assert!(missing.reason.unwrap().contains("[[accounts.codex]]"));
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

    #[test]
    fn candidates_carry_no_environment_values() {
        let accounts = AccountsConfig {
            claude: vec![AccountProfile {
                name: "work".into(),
                env: [("SECRET".into(), "private".into())].into(),
                env_file: None,
            }],
            ..Default::default()
        };
        let listed = candidates("claude", &accounts, "/missing/aas");
        let json = serde_json::to_string(&listed).unwrap();
        assert!(json.contains("work"), "{json}");
        assert!(
            !json.contains("private") && !json.contains("SECRET"),
            "{json}"
        );
    }
}
