//! Start a new agent on an `aas` account that still has quota.
//!
//! Without this, a muxa launch runs the bare provider CLI and the `aas`
//! shim puts it on whichever account `aas active` names — even while that
//! account is capped, so every new worker starts already stopped.
//! `[agent.<program>].account` lets muxa choose instead: `auto` takes the
//! best-ranked eligible account from `aas candidates`, a name pins one of
//! that provider's accounts.
//!
//! `aas` stays optional. It is never linked, only run, and only when the
//! provider's config asks for it; when it is missing, slow, or has nothing
//! eligible, the agent starts bare exactly as it would without the setting.
//! The decision is made before any pane exists, so a failed query never
//! leaves half-built layout behind.
//!
//! Already-running agents are [`crate::account_swap`]'s business.

use crate::account_swap::{find_aas, query_candidates_within, Candidate};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How long `aas candidates` gets before a launch gives up on it. `aas`
/// answers from its usage cache, so this only trips when it hangs.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// The providers `aas` can run.
pub const PROVIDERS: &[&str] = &["claude", "codex"];

/// Which account a new agent starts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnAccount {
    /// The best-ranked eligible account from `aas candidates`.
    Auto,
    /// This account, by its `aas` name.
    Named(String),
}

impl SpawnAccount {
    /// Read `[agent.<program>].account`. `None` for a blank value.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "" => None,
            "auto" => Some(Self::Auto),
            name => Some(Self::Named(name.to_owned())),
        }
    }
}

/// A provider's routing policy, resolved from config for one launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountRoute {
    /// `claude` or `codex`.
    pub provider: String,
    pub choice: SpawnAccount,
    /// The configured `aas` binary (`[swap].aas`).
    pub aas: String,
}

/// What routing decided, reported with the launch result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountDecision {
    /// The account the agent was started on; `None` when it started bare.
    pub account: Option<String>,
    /// Why it started bare, when it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
}

/// A decided launch: run `aas e <account>` in place of the provider CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutedLaunch {
    /// The `aas` executable, as an absolute path where one was found: the
    /// pane's shell need not have the same `PATH` as this process.
    pub aas: String,
    pub account: String,
}

impl RoutedLaunch {
    /// The argv that starts `provider_args` on this account. `aas e` picks
    /// the provider CLI from the account, so the provider's own name is not
    /// repeated.
    #[must_use]
    pub fn argv(&self, provider_args: impl IntoIterator<Item = String>) -> Vec<String> {
        [self.aas.clone(), "e".into(), self.account.clone()]
            .into_iter()
            .chain(provider_args)
            .collect()
    }
}

/// The first eligible `provider` account, in `aas`'s own ranking.
pub fn choose(candidates: &[Candidate], provider: &str) -> Result<String, String> {
    candidates
        .iter()
        .find(|c| c.eligible && c.provider == provider)
        .map(|c| c.name.clone())
        .ok_or_else(|| {
            let passed: Vec<String> = candidates
                .iter()
                .filter(|c| c.provider == provider)
                .map(|c| format!("{}: {}", c.name, c.reason))
                .collect();
            if passed.is_empty() {
                format!("aas has no {provider} account")
            } else {
                format!(
                    "no {provider} account has quota left ({})",
                    passed.join(", ")
                )
            }
        })
}

/// A pinned account, used whatever its quota, but only for its own
/// provider: `aas e` picks the CLI from the account, so `jelly@claude`
/// under `[agent.codex]` would start Claude with Codex's arguments.
fn pinned(candidates: &[Candidate], name: &str, provider: &str) -> Result<String, String> {
    match candidates.iter().find(|c| c.name == name) {
        Some(c) if c.provider == provider => Ok(name.to_owned()),
        Some(c) => Err(format!(
            "{name} is a {} account, not {provider}",
            c.provider
        )),
        None => Err(format!("aas has no {provider} account named {name}")),
    }
}

/// Decide where a new agent starts. `Err` is the reason it starts bare.
pub fn decide(route: &AccountRoute) -> Result<RoutedLaunch, String> {
    decide_with(route, |aas, provider| {
        query_candidates_within(aas, provider, QUERY_TIMEOUT)
    })
}

fn decide_with(
    route: &AccountRoute,
    query: impl FnOnce(&str, &str) -> Result<Vec<Candidate>, String>,
) -> Result<RoutedLaunch, String> {
    let aas = find_aas(&route.aas).ok_or_else(|| format!("`{}` not found", route.aas))?;
    let candidates = query(&aas, &route.provider)?;
    let account = match &route.choice {
        SpawnAccount::Named(name) => pinned(&candidates, name, &route.provider)?,
        SpawnAccount::Auto => choose(&candidates, &route.provider)?,
    };
    Ok(RoutedLaunch { aas, account })
}

/// [`decide`], folded into what the launch result reports.
#[must_use]
pub fn report(outcome: &Result<RoutedLaunch, String>) -> AccountDecision {
    match outcome {
        Ok(routed) => AccountDecision {
            account: Some(routed.account.clone()),
            skipped: None,
        },
        Err(reason) => AccountDecision {
            account: None,
            skipped: Some(reason.clone()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(name: &str, provider: &str, eligible: bool) -> Candidate {
        Candidate {
            name: name.into(),
            provider: provider.into(),
            eligible,
            active: false,
            reason: if eligible {
                "quota_available"
            } else {
                "usage_rate_limited"
            }
            .into(),
            remaining_pct: None,
        }
    }

    fn route(choice: SpawnAccount) -> AccountRoute {
        AccountRoute {
            provider: "claude".into(),
            choice,
            // Any file that exists stands in for the binary.
            aas: std::env::current_exe().unwrap().display().to_string(),
        }
    }

    #[test]
    fn parse_reads_auto_names_and_blank() {
        assert_eq!(SpawnAccount::parse(" auto "), Some(SpawnAccount::Auto));
        assert_eq!(
            SpawnAccount::parse("jelly@claude"),
            Some(SpawnAccount::Named("jelly@claude".into()))
        );
        assert_eq!(SpawnAccount::parse("  "), None);
    }

    #[test]
    fn choose_keeps_aas_ranking_and_skips_capped_and_other_providers() {
        let list = [
            candidate("cx@codex", "codex", true),
            candidate("capped@claude", "claude", false),
            candidate("second@claude", "claude", true),
            candidate("third@claude", "claude", true),
        ];
        assert_eq!(choose(&list, "claude").unwrap(), "second@claude");
        assert_eq!(choose(&list, "codex").unwrap(), "cx@codex");
    }

    #[test]
    fn choose_names_every_passed_over_account() {
        let list = [
            candidate("a@claude", "claude", false),
            candidate("cx@codex", "codex", true),
        ];
        let reason = choose(&list, "claude").unwrap_err();
        assert!(reason.contains("a@claude: usage_rate_limited"), "{reason}");
        assert!(!reason.contains("cx@codex"), "{reason}");
        assert_eq!(
            choose(&[], "codex").unwrap_err(),
            "aas has no codex account"
        );
    }

    #[test]
    fn auto_queries_the_resolved_binary_for_its_provider() {
        let route = route(SpawnAccount::Auto);
        let routed = decide_with(&route, |aas, provider| {
            assert_eq!(aas, route.aas);
            assert_eq!(provider, "claude");
            Ok(vec![candidate("ok@claude", "claude", true)])
        })
        .unwrap();
        assert_eq!(routed.account, "ok@claude");
        assert_eq!(routed.aas, route.aas);
    }

    #[test]
    fn a_named_account_is_used_capped_or_not_but_only_for_its_provider() {
        let list = || {
            Ok(vec![
                candidate("pin@claude", "claude", false),
                candidate("cx@codex", "codex", true),
            ])
        };
        let routed = decide_with(&route(SpawnAccount::Named("pin@claude".into())), |_, _| {
            list()
        })
        .unwrap();
        assert_eq!(routed.account, "pin@claude");
        let other = decide_with(&route(SpawnAccount::Named("cx@codex".into())), |_, _| {
            list()
        });
        assert_eq!(
            other.unwrap_err(),
            "cx@codex is a codex account, not claude"
        );
        let missing = decide_with(&route(SpawnAccount::Named("nope".into())), |_, _| list());
        assert_eq!(missing.unwrap_err(), "aas has no claude account named nope");
    }

    #[test]
    fn a_missing_binary_or_failed_query_starts_bare_with_a_reason() {
        let mut missing = route(SpawnAccount::Named("pin@claude".into()));
        missing.aas = "/nonexistent/aas".into();
        let outcome = decide_with(&missing, |_, _| unreachable!());
        assert_eq!(
            report(&outcome),
            AccountDecision {
                account: None,
                skipped: Some("`/nonexistent/aas` not found".into()),
            }
        );
        let outcome = decide_with(&route(SpawnAccount::Auto), |_, _| Err("timed out".into()));
        assert_eq!(report(&outcome).skipped.as_deref(), Some("timed out"));
    }

    #[test]
    fn argv_runs_the_provider_args_through_aas_unchanged() {
        let routed = RoutedLaunch {
            aas: "/opt/aas".into(),
            account: "ok@codex".into(),
        };
        assert_eq!(
            routed.argv(["--yolo".into(), "say \"hi\"\nthere".into()]),
            ["/opt/aas", "e", "ok@codex", "--yolo", "say \"hi\"\nthere"]
        );
    }
}
