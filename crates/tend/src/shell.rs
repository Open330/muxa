//! The shell a multiplexer hands command strings to, and the names programs go
//! by on this platform.
//!
//! tmux runs `new-window`, `split-window`, `display-popup` and `run-shell`
//! commands through `/bin/sh`. rmux on Windows runs them through its
//! `default-shell`, `PowerShell` by default, where POSIX quoting (`'\''`),
//! `exec`, `2>/dev/null` and a leading quoted program are all errors. Every
//! command line tend builds for a pane goes through [`Dialect`] so the two
//! never mix.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// The quoting and invocation rules of the shell that will read a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// `/bin/sh` and its relatives.
    Posix,
    /// `PowerShell` (`pwsh` or Windows `PowerShell`).
    PowerShell,
}

impl Dialect {
    /// The dialect of the shell the local multiplexer runs commands through:
    /// `PowerShell` under rmux on Windows, `/bin/sh` everywhere else.
    #[must_use]
    pub fn native() -> Self {
        if cfg!(windows) {
            Self::PowerShell
        } else {
            Self::Posix
        }
    }

    /// One argument, always quoted.
    #[must_use]
    pub fn quote(self, word: &str) -> String {
        match self {
            Self::Posix => format!("'{}'", word.replace('\'', r"'\''")),
            // Single quotes are literal in PowerShell; a quote escapes by
            // doubling. Typographic quotes count as quotes too.
            Self::PowerShell => format!(
                "'{}'",
                word.replace('\'', "''")
                    .replace('\u{2018}', "\u{2018}\u{2018}")
                    .replace('\u{2019}', "\u{2019}\u{2019}")
            ),
        }
    }

    /// One argument, bare when it is plainly a word, quoted otherwise, so a
    /// command line stays readable where it is shown to a person.
    #[must_use]
    pub fn word(self, word: &str) -> String {
        let plain = !word.is_empty()
            && word
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_-./:=@%+,".contains(c))
            // PowerShell reads `,` as its array operator and a leading `@` or
            // `%` as a sigil/alias; quoting those words is always safe.
            && !(self == Self::PowerShell && (word.contains(',') || word.starts_with(['@', '%'])));
        if plain {
            word.to_owned()
        } else {
            self.quote(word)
        }
    }

    /// A program and its arguments as one command line. `PowerShell` reads a
    /// leading quoted string as an expression, so a program that needs quoting
    /// goes through the call operator there; a plain name stays bare, which
    /// also keeps it as the window name a multiplexer derives from the line.
    #[must_use]
    pub fn command_line<S: AsRef<str>>(self, argv: &[S]) -> String {
        let mut words = argv.iter().map(|word| self.word(word.as_ref()));
        match self {
            Self::Posix => words.collect::<Vec<_>>().join(" "),
            Self::PowerShell => {
                let Some(program) = argv.first() else {
                    return String::new();
                };
                let program = program.as_ref();
                let first = words.next().unwrap_or_default();
                let head = if first == program && !program.starts_with('-') {
                    first
                } else {
                    format!("& {}", self.quote(program))
                };
                std::iter::once(head)
                    .chain(words)
                    .collect::<Vec<_>>()
                    .join(" ")
            }
        }
    }

    /// A command line that replaces the shell where the dialect can (`exec`
    /// in POSIX shells, so the pane closes with the program), and simply runs
    /// it where it cannot.
    #[must_use]
    pub fn exec_line<S: AsRef<str>>(self, argv: &[S]) -> String {
        match self {
            Self::Posix => format!("exec {}", self.command_line(argv)),
            Self::PowerShell => self.command_line(argv),
        }
    }

    /// Export explicit values into the new execution environment, replacing inherited values.
    pub fn exec_line_with_env<S: AsRef<str>>(
        self,
        argv: &[S],
        env: &[(&str, &str)],
    ) -> Result<String, String> {
        let mut lines = Vec::new();
        for (key, value) in env {
            if key.is_empty()
                || !key.chars().enumerate().all(|(i, c)| {
                    c == '_' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit())
                })
                || value.contains('\0')
            {
                return Err("invalid execution environment".into());
            }
            lines.push(match self {
                Self::Posix => format!("export {key}={}", self.quote(value)),
                Self::PowerShell => format!("$env:{key}={}", self.quote(value)),
            });
        }
        lines.push(self.exec_line(argv));
        Ok(lines.join("; "))
    }

    /// `cd` into `path`.
    #[must_use]
    pub fn cd(self, path: &str) -> String {
        match self {
            Self::Posix => format!("cd {}", self.quote(path)),
            Self::PowerShell => format!("Set-Location -LiteralPath {}", self.quote(path)),
        }
    }
}

/// One argument for the multiplexer's own command parser — the one that reads
/// a hook, a key binding's command, or an `if-shell`/`run-shell` argument —
/// always quoted.
///
/// This is not a shell's quoting: tmux and rmux read `'…'` literally and a
/// backslash-escaped `\'` outside quotes, on every platform, whatever shell
/// they later hand a `run-shell` line to. A `PowerShell` line quoted with
/// [`Dialect::quote`] doubles its `'`, which this parser reads as two adjacent
/// strings, so the inner quotes are lost. Quote each layer for the parser
/// that will read it: [`Dialect`] for the shell, this for the multiplexer.
#[must_use]
pub fn mux_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// A process that runs `script` through the local shell: `sh -c` on Unix,
/// `PowerShell`'s `-Command` on Windows.
#[must_use]
pub fn script_command(script: &str) -> Command {
    if cfg!(windows) {
        let mut command = Command::new(powershell());
        command.args(["-NoLogo", "-NoProfile", "-Command", script]);
        command
    } else {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]);
        command
    }
}

/// `pwsh` when `PowerShell` 7 is installed, else the Windows `PowerShell` every
/// Windows has.
#[must_use]
pub fn powershell() -> &'static Path {
    static RESOLVED: OnceLock<PathBuf> = OnceLock::new();
    RESOLVED.get_or_init(|| find_program("pwsh").unwrap_or_else(|| PathBuf::from("powershell")))
}

/// A program's bare name: the last path component, without the executable
/// extensions Windows adds (`C:\…\pwsh.exe` → `pwsh`, `/usr/bin/zsh` → `zsh`,
/// `-zsh` stays `-zsh`).
#[must_use]
pub fn program_name(argv0: &str) -> &str {
    let base = argv0.rsplit(['/', '\\']).next().unwrap_or(argv0);
    for extension in [".exe", ".cmd", ".bat", ".com", ".ps1"] {
        if base.len() > extension.len() {
            let (stem, tail) = base.split_at(base.len() - extension.len());
            if tail.eq_ignore_ascii_case(extension) {
                return stem;
            }
        }
    }
    base
}

/// The program at the head of a command line: its first word, except that
/// a quoted program, or an unquoted Windows path with spaces in it
/// (`C:\Program Files\PowerShell\pwsh.exe -NoLogo`), runs to its closing
/// quote or executable extension. Arguments never count, so `vim ~/fish`
/// is `vim`.
#[must_use]
pub fn program_token(line: &str) -> &str {
    let line = line.trim();
    if let Some(quoted) = line.strip_prefix('"') {
        return quoted.split('"').next().unwrap_or(quoted);
    }
    let bytes = line.as_bytes();
    let windows_path = line.starts_with(r"\\")
        || (bytes.len() > 2 && bytes[0].is_ascii_alphabetic() && &bytes[1..3] == br":\");
    if windows_path {
        let lower = line.to_ascii_lowercase();
        let end = [".exe", ".cmd", ".bat", ".com", ".ps1"]
            .iter()
            .flat_map(|extension| {
                lower
                    .match_indices(extension)
                    .map(|(at, _)| at + extension.len())
            })
            .filter(|&end| lower[end..].chars().next().is_none_or(char::is_whitespace))
            .min();
        if let Some(end) = end {
            return &line[..end];
        }
    }
    line.split_whitespace().next().unwrap_or_default()
}

/// Whether the program `command` runs is an interactive shell (login-shell
/// `-` prefix and Windows extensions allowed). Only the program is looked
/// at — see [`program_token`] — never its arguments.
#[must_use]
pub fn is_shell_program(command: &str) -> bool {
    let name = program_name(program_token(command)).trim_start_matches('-');
    [
        "sh",
        "bash",
        "zsh",
        "fish",
        "dash",
        "ksh",
        "nu",
        "pwsh",
        "powershell",
        "cmd",
    ]
    .iter()
    .any(|shell| name.eq_ignore_ascii_case(shell))
}

/// Find `name` on `PATH`, trying Windows' executable extensions (`PATHEXT`)
/// there. `std::process::Command` only appends `.exe` on its own, so an npm
/// shim such as `codex.cmd` is invisible to it without this.
#[must_use]
pub fn find_program(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
            .split(';')
            .filter(|ext| !ext.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    } else {
        Vec::new()
    };
    std::env::split_paths(&path).find_map(|dir| {
        if cfg!(windows) {
            extensions
                .iter()
                .map(|ext| dir.join(format!("{name}{ext}")))
                .find(|candidate| candidate.is_file())
        } else {
            let candidate = dir.join(name);
            candidate.is_file().then_some(candidate)
        }
    })
}

/// A [`Command`] for `program` that also finds Windows script shims
/// (`.cmd`/`.bat`) on `PATH`, which `Command::new` alone does not.
#[must_use]
pub fn program_command(program: &str) -> Command {
    if cfg!(windows) && !program.contains(['/', '\\']) {
        if let Some(path) = find_program(program) {
            return Command::new(path);
        }
    }
    Command::new(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_names_drop_paths_and_windows_extensions() {
        assert_eq!(
            program_name(r"C:\Program Files\PowerShell\7\pwsh.exe"),
            "pwsh"
        );
        assert_eq!(program_name("/usr/bin/zsh"), "zsh");
        assert_eq!(program_name("-zsh"), "-zsh");
        assert_eq!(program_name(r"C:\Users\j\.local\bin\claude.EXE"), "claude");
        assert_eq!(program_name("codex.cmd"), "codex");
        assert_eq!(program_name(".exe"), ".exe");
    }

    #[test]
    fn shells_are_recognised_on_every_platform() {
        for shell in [
            "zsh",
            "-bash",
            "/bin/sh",
            r"C:\Program Files\PowerShell\7\pwsh.exe",
            "powershell.exe",
            "cmd.exe",
            "zsh -l",
            r"C:\Program Files\PowerShell\pwsh.exe -NoLogo -NoExit",
            r#""C:\Program Files\PowerShell\pwsh.exe" -NoLogo"#,
        ] {
            assert!(is_shell_program(shell), "{shell}");
        }
        for program in [
            "claude",
            "claude.exe",
            "codex.cmd",
            "node",
            "vim /home/u/.config/fish",
            r"C:\tools\vim.exe C:\y\fish",
        ] {
            assert!(!is_shell_program(program), "{program}");
        }
    }

    #[test]
    fn posix_lines_quote_like_sh() {
        let line = Dialect::Posix.command_line(&["a path/tend", "it's", "plain-word"]);
        assert_eq!(line, r"'a path/tend' 'it'\''s' plain-word");
        assert_eq!(Dialect::Posix.exec_line(&["claude"]), "exec claude");
        assert_eq!(Dialect::Posix.cd("/tmp/a b"), "cd '/tmp/a b'");
    }

    #[test]
    fn powershell_lines_call_the_program_and_double_quotes() {
        let line = Dialect::PowerShell.command_line(&[
            r"C:\Program Files\tend.exe",
            "watch",
            "it's $HOME",
            "%4",
        ]);
        assert_eq!(
            line,
            r"& 'C:\Program Files\tend.exe' watch 'it''s $HOME' '%4'"
        );
        // No `exec` in PowerShell: the program simply runs, bare when its
        // name needs no quoting.
        assert_eq!(
            Dialect::PowerShell.exec_line(&["claude", "--resume", "x"]),
            "claude --resume x"
        );
        assert_eq!(
            Dialect::PowerShell.cd(r"F:\work's"),
            r"Set-Location -LiteralPath 'F:\work''s'"
        );
    }
}

#[cfg(test)]
mod launch_env_tests {
    use super::*;
    #[test]
    fn exported_value_replaces_inheritance_and_quotes_are_literal() {
        let value = "fresh ' $HOME `id`\nnext line";
        let arg = "prompt '\n$(id)";
        let line = Dialect::Posix
            .exec_line_with_env(
                &[
                    "sh",
                    "-c",
                    "printf '%s\\0%s' \"$TEND_LAUNCH_ID\" \"$1\"",
                    "unused",
                    arg,
                ],
                &[("TEND_LAUNCH_ID", value)],
            )
            .unwrap();
        #[cfg(unix)]
        {
            let output = std::process::Command::new("sh")
                .args(["-c", &line])
                .env("TEND_LAUNCH_ID", "inherited")
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(output.stdout, format!("{value}\0{arg}").as_bytes());
        }
        assert!(Dialect::Posix
            .exec_line_with_env(&["true"], &[("BAD; id", "x")])
            .is_err());
        assert!(!Dialect::PowerShell
            .exec_line_with_env(&["claude"], &[("TEND_LAUNCH_ID", "new")])
            .unwrap()
            .contains("exec "));
    }
}
