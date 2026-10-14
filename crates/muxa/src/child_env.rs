//! Environment a muxa-spawned process must not inherit from whichever agent
//! session happened to start it.
//!
//! An agent CLI exports variables that describe *its own* session: its id,
//! its private messaging socket and token, its pid, and the account it runs
//! as. muxad is often started from inside such a session (an agent's shell
//! running `muxa daemon start`, or a pane's shell startup hook), and then
//! every headless turn it spawns would run as that session: on its account,
//! talking to its socket. A daemon-wide process has no business with any of
//! it.

/// Variables an agent CLI sets for its own session. Never meaningful for a
/// process muxa starts.
pub const SESSION_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "CODEX_THREAD_ID",
    "CODEX_SANDBOX",
    "CODEX_SANDBOX_NETWORK_DISABLED",
];

/// Account selectors, each paired with the marker that proves it was
/// inherited from inside that agent's session rather than chosen for muxa.
/// An operator who exports `CLAUDE_CONFIG_DIR` in a plain shell keeps it.
const SESSION_ACCOUNT_VARS: &[(&str, &str)] = &[
    ("CLAUDE_CONFIG_DIR", "CLAUDECODE"),
    ("CODEX_HOME", "CODEX_THREAD_ID"),
];

/// Names to remove from a child's environment, given a lookup into the
/// environment it would otherwise inherit.
pub fn inherited_session_vars(is_set: impl Fn(&str) -> bool) -> Vec<&'static str> {
    let mut names: Vec<&'static str> = SESSION_VARS.to_vec();
    names.extend(
        SESSION_ACCOUNT_VARS
            .iter()
            .filter(|(_, marker)| is_set(marker))
            .map(|(name, _)| *name),
    );
    names
}

/// [`inherited_session_vars`] for this process's own environment.
pub fn inherited_session_vars_here() -> Vec<&'static str> {
    inherited_session_vars(|name| std::env::var_os(name).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_vars_always_go_and_accounts_only_with_their_session() {
        let plain = inherited_session_vars(|_| false);
        assert!(plain.contains(&"CLAUDE_CODE_MESSAGING_TOKEN"));
        assert!(plain.contains(&"CODEX_THREAD_ID"));
        assert!(
            !plain.contains(&"CLAUDE_CONFIG_DIR"),
            "a plain shell's choice stays"
        );
        assert!(!plain.contains(&"CODEX_HOME"));

        let in_claude = inherited_session_vars(|name| name == "CLAUDECODE");
        assert!(in_claude.contains(&"CLAUDE_CONFIG_DIR"));
        assert!(!in_claude.contains(&"CODEX_HOME"));

        let in_codex = inherited_session_vars(|name| name == "CODEX_THREAD_ID");
        assert!(in_codex.contains(&"CODEX_HOME"));
        assert!(!in_codex.contains(&"CLAUDE_CONFIG_DIR"));
    }
}
