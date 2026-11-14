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

/// The config keys `args` (Codex's own arguments) already sets with
/// `-c`/`--config`. A leading `--` (an `aas exec … --` wrapper) is skipped;
/// a later one ends the options.
fn keys_set(args: &[String]) -> Vec<String> {
    let args = match args.first().map(String::as_str) {
        Some("--") => &args[1..],
        _ => args,
    };
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

/// `args` (all of Codex's own arguments, after the program) with the
/// overrides first: before every option, its value, a subcommand, `--` and
/// a prompt, so none of them can swallow or shift. After a leading `--`
/// (`aas exec <account> -- …`), where Codex's arguments begin. Idempotent.
#[must_use]
pub fn with_startup_overrides(args: &[String]) -> Vec<String> {
    let extra = startup_args(args);
    if extra.is_empty() {
        return args.to_vec();
    }
    let at = usize::from(args.first().map(String::as_str) == Some("--"));
    let mut out = args.to_vec();
    out.splice(at..at, extra);
    out
}

/// The same for a whole command line already split into words, when that
/// line starts Codex: `codex …` (any path, `.exe`/`.cmd`), behind `env
/// K=V …`, through an interpreter (`node …/bin/codex …`), or as `aas
/// e|exec <account>@codex [--] …` / `aas e <account> codex …`. Only the
/// program position counts: a later word that happens to be `codex` is an
/// argument. Any other line comes back unchanged.
#[must_use]
pub fn with_startup_overrides_in_line(words: &[String]) -> Vec<String> {
    let Some(program) = codex_program_index(words) else {
        return words.to_vec();
    };
    let (head, tail) = words.split_at(program + 1);
    let mut out = head.to_vec();
    out.extend(with_startup_overrides(tail));
    out
}

/// The same for a line known only as shell text: the overrides are spliced
/// in right after the Codex program (or its `aas` account word), found
/// among the line's leading plain words. Plain means no quote, escape,
/// `$`, `#` or glob: none can hide a word boundary there. A line whose start
/// is not plain, or that does not start Codex, comes back unchanged. The
/// overrides come first, so a `-c` for the same key later on the line
/// (even one quoted, which is not read here) still wins.
#[must_use]
pub fn with_startup_overrides_in_text(line: &str, render: impl Fn(&[String]) -> String) -> String {
    let plain = |word: &str| {
        !word.chars().any(|c| {
            matches!(
                c,
                '\'' | '"' | '\\' | '`' | '$' | '#' | '*' | '?' | '[' | ';' | '&' | '|'
            )
        })
    };
    let words: Vec<&str> = line.split_whitespace().take_while(|w| plain(w)).collect();
    let owned: Vec<String> = words.iter().map(|w| (*w).to_owned()).collect();
    let Some(program) = codex_program_index(&owned) else {
        return line.to_owned();
    };
    let mut after = program;
    if owned.get(program + 1).map(String::as_str) == Some("--") {
        after += 1;
    }
    let extra = startup_args(&owned[after + 1..]);
    if extra.is_empty() {
        return line.to_owned();
    }
    let anchor = words[after];
    // `words` borrow from `line`: the anchor's end is its own offset.
    let end = anchor.as_ptr() as usize - line.as_ptr() as usize + anchor.len();
    format!("{} {}{}", &line[..end], render(&extra), &line[end..])
}

fn base(word: &str) -> String {
    let name = word
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .to_ascii_lowercase();
    for ext in [".exe", ".cmd", ".js"] {
        if let Some(stem) = name.strip_suffix(ext) {
            return stem.to_owned();
        }
    }
    name
}

/// Where the word that starts Codex's own arguments sits: the Codex
/// program, or the account (or frontend) word of an `aas` wrapper.
fn codex_program_index(words: &[String]) -> Option<usize> {
    let mut i = 0;
    if words.first().is_some_and(|w| base(w) == "env") {
        i = 1;
        while let Some(word) = words.get(i) {
            if word == "-u" || word == "--unset" {
                i += 2;
            } else if word.contains('=') && !word.starts_with('-') {
                i += 1;
            } else {
                break;
            }
        }
    }
    let program = words.get(i)?;
    match base(program).as_str() {
        "codex" => Some(i),
        "node" | "bun" | "deno" => words
            .get(i + 1)
            .is_some_and(|script| base(script) == "codex")
            .then_some(i + 1),
        "aas" => {
            let verb = words.get(i + 1)?;
            let account = words.get(i + 2)?;
            if !matches!(verb.as_str(), "e" | "exec") {
                return None;
            }
            if account.ends_with("@codex") {
                Some(i + 2)
            } else if words.get(i + 3).is_some_and(|front| front == "codex") {
                Some(i + 3)
            } else {
                None
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| (*w).to_owned()).collect()
    }

    const UPDATE: &str = "check_for_update_on_startup=false";
    const CWD: &str = "tui.resume_cwd=\"session\"";

    fn with(rest: &[&str]) -> Vec<String> {
        let mut v = s(&["-c", UPDATE, "-c", CWD]);
        v.extend(s(rest));
        v
    }

    #[test]
    fn overrides_lead_codexs_own_arguments_so_nothing_shifts() {
        assert_eq!(
            with_startup_overrides(&s(&["--yolo", "resume", "id"])),
            with(&["--yolo", "resume", "id"])
        );
        // An option's value that reads like a subcommand keeps its option.
        assert_eq!(
            with_startup_overrides(&s(&["--profile", "resume", "do it"])),
            with(&["--profile", "resume", "do it"])
        );
        // Never after `--`, where they would be prompt text.
        assert_eq!(
            with_startup_overrides(&s(&["--yolo", "--", "do it"])),
            with(&["--yolo", "--", "do it"])
        );
        assert_eq!(with_startup_overrides(&s(&["--", "resume"])), {
            let mut v = s(&["--"]);
            v.extend(with(&["resume"]));
            v
        });
        let once = with_startup_overrides(&s(&["resume", "id"]));
        assert_eq!(with_startup_overrides(&once), once);
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
            let mut want = s(&["-c", CWD]);
            want.extend(own);
            want.extend(s(&["resume", "id"]));
            assert_eq!(with_startup_overrides(&args), want);
        }
        // A prompt that mentions a key sets nothing.
        assert_eq!(
            startup_args(&s(&["resume", "id", "explain check_for_update_on_startup"])).len(),
            4
        );
        // After `--` nothing is an option.
        assert_eq!(
            startup_args(&s(&[
                "--",
                "-c",
                "x",
                "--",
                "-c",
                "check_for_update_on_startup=true"
            ]))
            .len(),
            4
        );
    }

    #[test]
    fn a_line_gets_them_only_when_its_program_is_codex() {
        let line = |w: &[&str]| with_startup_overrides_in_line(&s(w));
        let mut want = s(&["aas", "e", "chatgpt@codex"]);
        want.extend(with(&["--yolo", "--no-daemon", "resume", "id"]));
        assert_eq!(
            line(&[
                "aas",
                "e",
                "chatgpt@codex",
                "--yolo",
                "--no-daemon",
                "resume",
                "id"
            ]),
            want
        );
        let mut want = s(&["env", "CODEX_HOME=/h", "/opt/bin/codex"]);
        want.extend(with(&["resume", "id"]));
        assert_eq!(
            line(&["env", "CODEX_HOME=/h", "/opt/bin/codex", "resume", "id"]),
            want
        );
        let mut want = s(&["aas", "exec", "b@codex", "--"]);
        want.extend(with(&["resume", "id"]));
        assert_eq!(
            line(&["aas", "exec", "b@codex", "--", "resume", "id"]),
            want
        );
        let mut want = s(&["node", "/n/bin/codex"]);
        want.extend(with(&["--yolo"]));
        assert_eq!(line(&["node", "/n/bin/codex", "--yolo"]), want);
        let mut want = s(&[r"C:\npm\codex.cmd"]);
        want.extend(with(&["resume", "id"]));
        assert_eq!(line(&[r"C:\npm\codex.cmd", "resume", "id"]), want);
        // Not Codex, or `codex` only as an argument: byte for byte the same.
        for words in [
            s(&["claude", "--resume", "id"]),
            s(&["aas", "e", "w@claude", "--resume", "id"]),
            s(&["node", "server.js", "codex"]),
            s(&["vim", "codex"]),
            s(&["env", "A=1", "claude"]),
        ] {
            assert_eq!(with_startup_overrides_in_line(&words), words);
        }
    }

    #[test]
    fn a_text_line_gets_them_after_its_program_and_never_inside_quotes() {
        let render = |w: &[String]| w.join(" ");
        let text = |line: &str| with_startup_overrides_in_text(line, render);
        let o = format!("-c {UPDATE} -c {CWD}");
        assert_eq!(text("codex resume id"), format!("codex {o} resume id"));
        assert_eq!(
            text("codex resume id 'explain check_for_update_on_startup'"),
            format!("codex {o} resume id 'explain check_for_update_on_startup'")
        );
        assert_eq!(
            text("codex -c 'developer_instructions=\"please resume the session\"' resume id"),
            format!(
                "codex {o} -c 'developer_instructions=\"please resume the session\"' resume id"
            )
        );
        assert_eq!(
            text("aas e x@codex --yolo resume id"),
            format!("aas e x@codex {o} --yolo resume id")
        );
        assert_eq!(
            text("aas exec x@codex -- resume id"),
            format!("aas exec x@codex -- {o} resume id")
        );
        // A user's own plain -c for a key leaves that key alone.
        assert_eq!(
            text("codex -c check_for_update_on_startup=true resume id"),
            format!("codex -c {CWD} -c check_for_update_on_startup=true resume id")
        );
        // Not plain where the program would be, or not Codex: unchanged.
        for line in [
            "'codex' resume id",
            "FOO=$X codex",
            "claude --resume id",
            "node server.js codex",
        ] {
            assert_eq!(text(line), line);
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
