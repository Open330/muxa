//! Where a provider's accounts come from, and which one an agent should use.
//!
//! Account routing (`[agent.<program>].account`) and swaps (`tend swap`, the
//! `swap_account` automation action) need two things from a provider's
//! accounts: their state (which still have quota) and how to start the CLI on
//! one. Three backends answer that, in this order of preference:
//!
//! 1. **Configured accounts**: `[[accounts.<provider>]]` in `config.toml`,
//!    each the environment that selects it (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`,
//!    a token file). No external tool is involved. They are used in config
//!    order, by a fixed rule ([`configured`]): tend does not yet rank them by
//!    observed usage, since it cannot yet tell deterministically which
//!    account a running agent's limit reports belong to.
//! 2. **`aas`**: when no accounts are configured and the `aas` binary is
//!    installed (or `[swap].aas` names one), `aas candidates` ranks its own
//!    accounts and `aas e <name>` starts the CLI.
//! 3. **None**: launches start the provider CLI as it is, and a swap has no
//!    account to move to. A capped agent then waits for its reset, which the
//!    `resume-after-limit` automation rule resumes; that rule needs no
//!    accounts at all.

use crate::account_swap::Candidate;
use crate::accounts::{AccountProfile, AccountsConfig};

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

/// `profiles` as candidates, in config order: each account in `capped` (the
/// one an agent being swapped is capped on) is ineligible.
#[must_use]
pub fn configured(provider: &str, profiles: &[AccountProfile], capped: &[&str]) -> Vec<Candidate> {
    profiles
        .iter()
        .map(|profile| {
            let is_capped = capped.contains(&profile.name.as_str());
            Candidate {
                name: profile.name.clone(),
                provider: provider.to_owned(),
                eligible: !is_capped,
                active: false,
                reason: if is_capped {
                    "rate limited here".into()
                } else {
                    "configured".into()
                },
                remaining_pct: None,
            }
        })
        .collect()
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
        let candidates = configured("claude", &profiles, &["a"]);
        assert_eq!(
            candidates
                .iter()
                .map(|c| (c.name.as_str(), c.eligible))
                .collect::<Vec<_>>(),
            [("a", false), ("b", true), ("c", true)]
        );
        assert_eq!(candidates[0].reason, "rate limited here");
        assert!(configured("claude", &profiles, &[])
            .iter()
            .all(|c| c.eligible));
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
