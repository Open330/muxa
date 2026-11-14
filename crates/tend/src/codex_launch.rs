//! The Codex config overrides every Codex start tend composes carries, so a
//! launched or resumed Codex comes up on its composer rather than on a
//! startup menu nobody is watching:
//!
//! - `check_for_update_on_startup=false`: no "Update available" menu (its
//!   default row installs the update). A restore of many panes otherwise
//!   leaves every one of them on it.
//! - `tui.resume_cwd="session"`: a resumed conversation starts in its own
//!   directory without asking which one.
//!
//! Codex ignores a `-c` key it does not know, so an older release starts as
//! before. A key the user already sets on the command line (`-c key=…`,
//! `--config key=…`, `-ckey=…`, `--config=key=…`) is left to the user.
//!
//! One builder for every launch path: worker and agent starts
//! (`tend agent start`, work up, MCP, Fleet-relayed and pipeline launches
//! all go through `tend_work::agent_launch::start`), snapshot restore and
//! reload (`tend_work::reload`), and the account swap's relaunch. A test
//! there checks no Codex launch path builds its arguments around it.

/// The overrides, as `key=value` for `-c`.
pub const STARTUP_OVERRIDES: [&str; 2] = [
    "check_for_update_on_startup=false",
    "tui.resume_cwd=\"session\"",
];

fn key_of(assignment: &str) -> &str {
    assignment
        .split_once('=')
        .map_or(assignment, |(key, _)| key)
        .trim()
}

/// The config keys `args` already sets with `-c`/`--config`.
fn keys_set(args: &[String]) -> Vec<String> {
    let mut keys = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            break;
        }
        let value = match arg {
            "-c" | "--config" => {
                i += 1;
                args.get(i).map(String::as_str)
            }
            _ => arg
                .strip_prefix("--config=")
                .or_else(|| arg.strip_prefix("-c").filter(|rest| !rest.is_empty())),
        };
        if let Some(value) = value {
            keys.push(key_of(value).to_owned());
        }
        i += 1;
    }
    keys
}

/// `-c <override>` pairs for each override `args` does not already set.
#[must_use]
pub fn startup_args(args: &[String]) -> Vec<String> {
    let set = keys_set(args);
    STARTUP_OVERRIDES
        .iter()
        .filter(|assignment| !set.iter().any(|key| key == key_of(assignment)))
        .flat_map(|assignment| ["-c".to_owned(), (*assignment).to_owned()])
        .collect()
}

/// `args` (a Codex argument list, after the program) with the overrides
/// added: before a `resume` subcommand when there is one, else before a
/// trailing positional prompt (`prompt_at`, its index) or `--`, else at the
/// end. Idempotent.
#[must_use]
pub fn with_startup_overrides(args: &[String], prompt_at: Option<usize>) -> Vec<String> {
    let extra = startup_args(args);
    if extra.is_empty() {
        return args.to_vec();
    }
    let at = args
        .iter()
        .position(|arg| arg == "resume")
        .or(prompt_at)
        .or_else(|| args.iter().position(|arg| arg == "--"))
        .unwrap_or(args.len())
        .min(args.len());
    let mut out = args.to_vec();
    out.splice(at..at, extra);
    out
}

/// The same for a whole command line already rendered as words: the
/// Codex program (`codex`, a path ending in `/codex`, or an `aas e …@codex`
/// wrapper) is found, and the overrides go into its arguments. Words that
/// are not a Codex launch come back unchanged.
#[must_use]
pub fn with_startup_overrides_in_line(words: &[String]) -> Vec<String> {
    let Some(program) = codex_program_index(words) else {
        return words.to_vec();
    };
    let (head, tail) = words.split_at(program + 1);
    let mut out = head.to_vec();
    out.extend(with_startup_overrides(tail, None));
    out
}

/// Where the Codex program (or the account wrapper that starts it) sits in
/// `words`, so its own arguments follow it.
fn codex_program_index(words: &[String]) -> Option<usize> {
    let base = |word: &str| word.rsplit(['/', '\\']).next().unwrap_or(word).to_owned();
    for (i, word) in words.iter().enumerate() {
        let name = base(word);
        let name = name.strip_suffix(".exe").unwrap_or(&name);
        if name == "codex" || name == "codex.cmd" {
            return Some(i);
        }
        // `aas e <account>@codex …` / `aas exec <account>@codex -- …`.
        if name == "aas" {
            let rest = &words[i + 1..];
            if let [verb, account, ..] = rest {
                if matches!(verb.as_str(), "e" | "exec") && account.ends_with("@codex") {
                    let after = i + 2;
                    return Some(if rest.get(2).map(String::as_str) == Some("--") {
                        after + 1
                    } else {
                        after
                    });
                }
            }
            return None;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    const UPDATE: &str = "check_for_update_on_startup=false";
    const CWD: &str = "tui.resume_cwd=\"session\"";

    #[test]
    fn overrides_go_before_resume_or_a_prompt_and_only_once() {
        assert_eq!(
            with_startup_overrides(&s(&["--yolo", "resume", "id"]), None),
            s(&["--yolo", "-c", UPDATE, "-c", CWD, "resume", "id"])
        );
        assert_eq!(
            with_startup_overrides(&s(&["--yolo", "do it"]), Some(1)),
            s(&["--yolo", "-c", UPDATE, "-c", CWD, "do it"])
        );
        assert_eq!(
            with_startup_overrides(&s(&["--yolo"]), None),
            s(&["--yolo", "-c", UPDATE, "-c", CWD])
        );
        let once = with_startup_overrides(&s(&["resume", "id"]), None);
        assert_eq!(with_startup_overrides(&once, None), once);
    }

    #[test]
    fn a_users_own_setting_for_a_key_is_kept() {
        for own in [
            s(&["-c", "check_for_update_on_startup=true"]),
            s(&["--config", "check_for_update_on_startup = true"]),
            s(&["-ccheck_for_update_on_startup=true"]),
            s(&["--config=check_for_update_on_startup=true"]),
        ] {
            let mut args = own.clone();
            args.extend(s(&["resume", "id"]));
            let out = with_startup_overrides(&args, None);
            let mut want = own;
            want.extend(s(&["-c", CWD, "resume", "id"]));
            assert_eq!(out, want);
        }
        // After `--` nothing is an option.
        assert_eq!(
            startup_args(&s(&["--", "-c", "check_for_update_on_startup=true"])).len(),
            4
        );
    }

    #[test]
    fn a_rendered_line_gets_them_after_its_codex_program() {
        assert_eq!(
            with_startup_overrides_in_line(&s(&[
                "aas",
                "e",
                "chatgpt@codex",
                "--yolo",
                "--no-daemon",
                "resume",
                "id"
            ])),
            s(&[
                "aas",
                "e",
                "chatgpt@codex",
                "--yolo",
                "--no-daemon",
                "-c",
                UPDATE,
                "-c",
                CWD,
                "resume",
                "id"
            ])
        );
        assert_eq!(
            with_startup_overrides_in_line(&s(&[
                "env",
                "CODEX_HOME=/h",
                "/opt/bin/codex",
                "resume",
                "id"
            ])),
            s(&[
                "env",
                "CODEX_HOME=/h",
                "/opt/bin/codex",
                "-c",
                UPDATE,
                "-c",
                CWD,
                "resume",
                "id"
            ])
        );
        assert_eq!(
            with_startup_overrides_in_line(&s(&["aas", "exec", "b@codex", "--", "resume", "id"])),
            s(&["aas", "exec", "b@codex", "--", "-c", UPDATE, "-c", CWD, "resume", "id"])
        );
        // Not Codex: unchanged.
        for line in [
            s(&["claude", "--resume", "id"]),
            s(&["aas", "e", "w@claude", "--resume", "id"]),
            s(&["node", "server.js"]),
        ] {
            assert_eq!(with_startup_overrides_in_line(&line), line);
        }
    }

    /// Every place in the workspace that composes a Codex command line
    /// (a `resume` subcommand pushed or formatted, a `--yolo` launch) must
    /// either go through this module or say why it does not need to.
    /// A new launch path that builds its own arguments fails here.
    #[test]
    fn no_codex_launch_path_skips_the_overrides() {
        // file (relative to the workspace) -> why it is allowed
        const COMPOSERS: &[(&str, &str)] = &[
            ("crates/tend-work/src/agent_launch.rs", "uses codex_launch"),
            ("crates/tend-work/src/reload.rs", "uses codex_launch"),
            ("crates/tend-aas/src/account_swap.rs", "uses codex_launch"),
            (
                "crates/tend/src/ask.rs",
                "headless `codex exec resume`: no TUI, so no startup menu",
            ),
        ];
        let markers = [
            "\"resume\".into()",
            "\"resume\".to_owned()",
            "\"resume\".to_string()",
            "push(\"resume\"",
            "\"codex resume ",
            "} resume {",
            "\"--yolo\".into()",
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut found = Vec::new();
        let mut stack = vec![root.join("crates")];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if !path.ends_with("target") && !path.ends_with("tests") {
                        stack.push(path);
                    }
                    continue;
                }
                let name = path.to_string_lossy().into_owned();
                if path.extension().and_then(|e| e.to_str()) != Some("rs")
                    || name.ends_with("_tests.rs")
                    || name.ends_with("codex_launch.rs")
                {
                    continue;
                }
                let text = std::fs::read_to_string(&path).unwrap();
                // Production code only: unit tests sit after the first
                // `#[cfg(test)]` module.
                let code = text
                    .find("#[cfg(test)]\nmod tests")
                    .or_else(|| text.find("#[cfg(test)]\npub(crate) mod tests"))
                    .map_or(text.as_str(), |at| &text[..at]);
                if markers.iter().any(|marker| code.contains(marker)) {
                    let relative = path
                        .strip_prefix(&root)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    found.push((relative, code.contains("codex_launch::")));
                }
            }
        }
        for (file, uses) in &found {
            let allowed = COMPOSERS.iter().find(|(f, _)| f == file);
            assert!(
                allowed.is_some(),
                "{file} composes a Codex command line; route it through tend::codex_launch \
                 (or list it here with the reason it needs no startup overrides)"
            );
            if allowed.is_some_and(|(_, why)| *why == "uses codex_launch") {
                assert!(
                    uses,
                    "{file} is listed as using tend::codex_launch but does not"
                );
            }
        }
        for (file, why) in COMPOSERS {
            if *why == "uses codex_launch" {
                assert!(
                    found.iter().any(|(f, uses)| f == file && *uses),
                    "{file} no longer goes through tend::codex_launch"
                );
            }
        }
    }
}
