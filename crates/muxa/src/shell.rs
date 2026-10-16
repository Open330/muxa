//! The shell a multiplexer hands command strings to, and the names programs go
//! by on this platform.
//!
//! tmux runs `new-window`, `split-window`, `display-popup` and `run-shell`
//! commands through `/bin/sh`. rmux on Windows runs them through its
//! `default-shell`, `PowerShell` by default, where POSIX quoting (`'\''`),
//! `exec`, `2>/dev/null` and a leading quoted program are all errors. Every
//! command line muxa builds for a pane goes through [`Dialect`] so the two
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

    /// `cd` into `path`.
    #[must_use]
    pub fn cd(self, path: &str) -> String {
        match self {
            Self::Posix => format!("cd {}", self.quote(path)),
            Self::PowerShell => format!("Set-Location -LiteralPath {}", self.quote(path)),
        }
    }
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

/// Whether `argv0` names an interactive shell (login-shell `-` prefix and
/// Windows extensions allowed).
#[must_use]
pub fn is_shell_program(argv0: &str) -> bool {
    let name = program_name(argv0.trim()).trim_start_matches('-');
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
        ] {
            assert!(is_shell_program(shell), "{shell}");
        }
        for program in ["claude", "claude.exe", "codex.cmd", "node"] {
            assert!(!is_shell_program(program), "{program}");
        }
    }

    #[test]
    fn posix_lines_quote_like_sh() {
        let line = Dialect::Posix.command_line(&["a path/muxa", "it's", "plain-word"]);
        assert_eq!(line, r"'a path/muxa' 'it'\''s' plain-word");
        assert_eq!(Dialect::Posix.exec_line(&["claude"]), "exec claude");
        assert_eq!(Dialect::Posix.cd("/tmp/a b"), "cd '/tmp/a b'");
    }

    #[test]
    fn powershell_lines_call_the_program_and_double_quotes() {
        let line = Dialect::PowerShell.command_line(&[
            r"C:\Program Files\muxa.exe",
            "watch",
            "it's $HOME",
            "%4",
        ]);
        assert_eq!(
            line,
            r"& 'C:\Program Files\muxa.exe' watch 'it''s $HOME' '%4'"
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
