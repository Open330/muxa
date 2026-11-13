//! Start a new agent on a chosen account.
//!
//! Without this, a tend launch runs the bare provider CLI on whichever
//! account it happens to be logged into, even while that account is capped,
//! so every new worker starts already stopped. `[agent.<program>].account`
//! lets tend choose instead: `auto` takes the first configured account (or
//! `aas`'s best-ranked one with quota left), a name pins one of that
//! provider's accounts.
//!
//! The accounts come from [`crate::account_provider`]: configured
//! `[[accounts.<provider>]]` in config order, else `aas` when it is
//! installed. Neither is required. When there is no backend,
//! or it is slow or has nothing eligible, the agent starts bare exactly as it
//! would without the setting. The decision is made before any pane exists,
//! so a failed query never leaves half-built layout behind.
//!
//! Already-running agents are [`crate::account_swap`]'s business.

use crate::account_swap::{find_aas, query_candidates_within, Candidate};
use std::time::Duration;
pub use tend::swap_api::{
    report, AccountDecision, AccountRoute, RoutedLaunch, SpawnAccount, PROVIDERS,
};

/// How long `aas candidates` gets before a launch gives up on it. `aas`
/// answers from its usage cache, so this only trips when it hangs.
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// A configured account, by name or the first one for `auto`. Configured
/// accounts are not ranked by quota yet; a swap moves the agent on when it
/// is capped.
fn from_profiles(route: &AccountRoute) -> Result<RoutedLaunch, String> {
    let provider = route.provider.as_str();
    if cfg!(windows) {
        return Err("configured accounts launch through `env`/`sh`, which this shell lacks".into());
    }
    let profile = match &route.choice {
        SpawnAccount::Named(name) => tend::accounts::find_profile(&route.profiles, provider, name)
            .ok_or_else(|| {
                format!("no {provider} account named {name} in [[accounts.{provider}]]")
            })?,
        SpawnAccount::Auto => route
            .profiles
            .first()
            .ok_or_else(|| format!("[[accounts.{provider}]] lists no account"))?,
    };
    Ok(RoutedLaunch {
        account: profile.name.clone(),
        launcher: profile.launch_prefix(provider),
    })
}

/// The first eligible `provider` account, in the backend's ranking.
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
                format!("no {provider} account is available")
            } else {
                format!(
                    "no {provider} account has quota left ({})",
                    passed.join(", ")
                )
            }
        })
}

/// A pinned account, used whatever its quota, but only for its own
/// provider: `aas e` picks the CLI from the account, so `work@claude`
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
    if route.module_off {
        return Err(tend::swap_api::module_unavailable(true));
    }
    let routed = decide_with(route, |aas, provider| {
        query_candidates_within(aas, provider, QUERY_TIMEOUT)
    })?;
    // A Codex launch on an account home meets the same "Hooks need review"
    // menu a swap would: carry over trust the user gave elsewhere first.
    if route.provider == "codex" {
        if let Some(note) =
            crate::codex_hook_trust::prepare_launch(&route.profiles, &routed.account, &route.aas)
        {
            eprintln!("tend: {note}");
        }
    }
    Ok(routed)
}

fn decide_with(
    route: &AccountRoute,
    query: impl FnOnce(&str, &str) -> Result<Vec<Candidate>, String>,
) -> Result<RoutedLaunch, String> {
    decide_found(route, find_aas, query)
}

fn decide_found(
    route: &AccountRoute,
    find: impl FnOnce(&str) -> Option<String>,
    query: impl FnOnce(&str, &str) -> Result<Vec<Candidate>, String>,
) -> Result<RoutedLaunch, String> {
    if !route.profiles.is_empty() {
        return from_profiles(route);
    }
    let aas = find(&route.aas).ok_or_else(|| {
        if route.aas == crate::account_swap::SwapConfig::default().aas {
            crate::account_provider::no_accounts(&route.provider)
        } else {
            format!("`{}` not found", route.aas)
        }
    })?;
    let candidates = query(&aas, &route.provider)?;
    let account = match &route.choice {
        SpawnAccount::Named(name) => pinned(&candidates, name, &route.provider)?,
        SpawnAccount::Auto => choose(&candidates, &route.provider)?,
    };
    Ok(RoutedLaunch::via_aas(aas, account))
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
            profiles: Vec::new(),
            module_off: false,
        }
    }

    #[test]
    fn parse_reads_auto_names_and_blank() {
        assert_eq!(SpawnAccount::parse(" auto "), Some(SpawnAccount::Auto));
        assert_eq!(
            SpawnAccount::parse("spare@claude"),
            Some(SpawnAccount::Named("spare@claude".into()))
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
            "no codex account is available"
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
        assert_eq!(routed.launcher, [route.aas.as_str(), "e", "ok@claude"]);
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
    fn no_accounts_and_no_aas_starts_bare_and_says_how_to_configure_one() {
        let mut bare = route(SpawnAccount::Auto);
        // The default `aas`, not installed: tend needs no external tool.
        bare.aas = crate::account_swap::SwapConfig::default().aas;
        let reason = decide_found(&bare, |_| None, |_, _| unreachable!()).unwrap_err();
        assert!(reason.contains("[[accounts.claude]]"), "{reason}");
        assert!(reason.contains("resume-after-limit"), "{reason}");
    }

    #[test]
    fn argv_runs_the_provider_args_through_aas_unchanged() {
        let routed = RoutedLaunch::via_aas("/opt/aas".into(), "ok@codex".into());
        assert_eq!(
            routed.argv(["--yolo".into(), "say \"hi\"\nthere".into()]),
            ["/opt/aas", "e", "ok@codex", "--yolo", "say \"hi\"\nthere"]
        );
    }

    fn profile(name: &str) -> tend::accounts::AccountProfile {
        tend::accounts::AccountProfile {
            name: name.into(),
            env: [("CLAUDE_CONFIG_DIR".to_owned(), format!("/p/{name}"))].into(),
            env_file: None,
        }
    }

    #[test]
    fn configured_accounts_route_without_aas() {
        let mut route = route(SpawnAccount::Auto);
        route.aas = "/nonexistent/aas".into();
        route.profiles = vec![profile("work"), profile("home")];
        let never = |_: &str, _: &str| -> Result<Vec<Candidate>, String> {
            panic!("aas must not be queried")
        };
        let auto = decide_with(&route, never).unwrap();
        assert_eq!(auto.account, "work");
        assert_eq!(
            auto.argv(["--resume".into()]),
            ["env", "CLAUDE_CONFIG_DIR=/p/work", "claude", "--resume"]
        );
        route.choice = SpawnAccount::Named("home@claude".into());
        assert_eq!(decide_with(&route, never).unwrap().account, "home");
        route.choice = SpawnAccount::Named("nope".into());
        assert!(decide_with(&route, never)
            .unwrap_err()
            .contains("no claude account named nope"));
    }
}
