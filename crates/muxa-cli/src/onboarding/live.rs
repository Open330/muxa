//! The live tour: onboarding stops simulating muxa and becomes it.
//!
//! Everything the learner sees is real — a real tmux server, a real `muxad`, a
//! real `muxa watch` over a real mailbox. Only the agents are scripted, and the
//! tour says so on screen rather than hoping nobody notices.
//!
//! Three decisions shape the whole module.
//!
//! **The sandbox.** `scripts/muxa-sandbox.sh`, embedded so it ships with every
//! install, redirects every muxa surface at once — socket, config, data
//! directory, tmux server, and `tmux` on `PATH`. Nothing here can reach the
//! fleet the learner actually runs.
//!
//! **Narration through tmux's own status bar.** The alternatives fight the
//! lesson: a narration *pane* gets split and zoomed by the very exercises the
//! tour teaches, and `display-popup` is modal — it covers the watch the learner
//! is meant to be reading, and does not expand `#{}` formats. Status rows
//! survive every pane operation, expand formats, and are themselves real tmux.
//!
//! **Observation, not interception.** Because the narration never owns the
//! keyboard, no step can wait on a keypress. Each one polls real tmux and real
//! muxa state and advances when the learner has actually done the thing, which
//! is what lets the tour be driven with real commands instead of a quiz.

use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::{tr, UiLanguage};

/// Embedded rather than located on disk: a release tarball, a `cargo install`
/// and a brew bottle all ship the binary and none of them ship the repository.
/// `scripts/muxa-sandbox.sh` stays the single source, tested by
/// `scripts/sandbox-smoke.sh`.
const SANDBOX_SCRIPT: &str = include_str!("../../../../scripts/muxa-sandbox.sh");

const SANDBOX_NAME_PREFIX: &str = "muxa-onboarding";
const POLL: Duration = Duration::from_millis(250);

/// The block printed when nobody is attached to a status bar.
const RULE_WIDTH: usize = 64;
const BAR_WIDTH: usize = 24;
/// Commands longer than this take their own line in the closing summary.
const ALIGN_LIMIT: usize = 32;
/// What the learner's `PS1` renders, so a printed block can end with the same
/// thing bash would have drawn on the line it replaced.
const PROMPT: &str = " muxa-onboarding $ ";

/// Long enough that nobody reading the screen feels rushed, short enough that
/// an abandoned terminal does not hold a sandbox open all afternoon.
const STEP_TIMEOUT: Duration = Duration::from_secs(900);

/// How long a step waits before it offers a way past itself.
///
/// Issue #76 was an onboarding with one gate and no way around it, and a live
/// tour can strand someone just as easily — a terminal that swallows a key, a
/// step that will not register. Long enough not to invite skipping, short
/// enough that nobody sits there wondering.
const SKIP_AFTER: Duration = Duration::from_secs(45);

/// Set by the skip key, read by the poll loop. A tmux user option rather than a
/// file: the narration already lives in tmux, and this keeps the escape hatch
/// in the same place.
const SKIP_OPTION: &str = "@muxa-onboarding-skip";

/// Same mechanism for the language toggle the simulation had on `F2`.
const LANGUAGE_OPTION: &str = "@muxa-onboarding-language";

const SANDBOX_CONFIG: &str = "\
[discovery]
enabled = false

[collaboration]
enabled = true
# The tour must never type into a pane the learner is reading.
wake = 'never'
scope = 'host'

[watch]
view = 'window'
sort = ['state']
";

/// Set by the signal handler, read by the poll loop.
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// Without this, Ctrl-C tears the process down without running `Drop`, and the
/// learner is left with a daemon and a tmux server they did not ask for and
/// have no idea how to find. Watching on a thread rather than installing a raw
/// handler keeps the workspace's `unsafe_code = "forbid"` intact; the teardown
/// itself happens on the main thread once the poll loop sees the flag.
///
/// SIGKILL cannot be caught. Each run has a PID-scoped directory and sandbox,
/// so even an unclean exit cannot make a later or concurrent tour adopt its
/// daemon, tmux server, command history, or scripted-agent transcripts.
fn trap_signals() {
    std::thread::spawn(|| {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        runtime.block_on(async {
            use tokio::signal::unix::{signal, SignalKind};

            let (Ok(mut interrupt), Ok(mut terminate), Ok(mut hangup)) = (
                signal(SignalKind::interrupt()),
                signal(SignalKind::terminate()),
                signal(SignalKind::hangup()),
            ) else {
                return;
            };
            tokio::select! {
                _ = interrupt.recv() => {}
                _ = terminate.recv() => {}
                _ = hangup.recv() => {}
            }
            INTERRUPTED.store(true, Ordering::SeqCst);
        });
    });
}

// ---------------------------------------------------------------------------
// Sandbox
// ---------------------------------------------------------------------------

/// Owns the sandbox while it is alive and takes it down on the way out —
/// including on a panic, which is the path that would otherwise leave a daemon
/// and a tmux server behind on someone's machine.
struct Sandbox {
    name: String,
    dir: PathBuf,
    script: PathBuf,
    config: PathBuf,
    rcfile: PathBuf,
    /// What the shell prompt shows when nobody is attached to a status bar.
    cue: PathBuf,
    /// Every command the learner runs, appended by their own shell.
    ///
    /// Without it a step whose action is a shell command — `tmux ls`,
    /// `muxa msg list` — has nothing to detect, and the only way to teach one
    /// was to bolt it onto the end of another step's cue. That is what put two
    /// actions on one line and left the learner unsure which of them had
    /// registered.
    history: PathBuf,
    /// `HOME` for everything the learner runs, so `cd ~` lands here too.
    home: PathBuf,
    /// Where their shell starts, and what `ls` shows.
    project: PathBuf,
    tmux: PathBuf,
    exe: PathBuf,
    env: BTreeMap<String, String>,
}

impl Sandbox {
    fn create() -> Result<Self> {
        let tmux = which("tmux").context("tmux is required for the live tour")?;
        let exe = std::env::current_exe().context("locating the running muxa binary")?;
        let (name, dir) = allocate_tour_dir()?;
        let script = dir.join("sandbox.sh");
        let config = dir.join("config.src.toml");
        let rcfile = dir.join("bashrc");
        let cue = dir.join("cue");
        let history = dir.join("history");
        let home = dir.join("home");
        let project = home.join("checkout-service");

        let mut sandbox = Self {
            name,
            dir,
            script,
            config,
            rcfile,
            cue,
            history,
            home,
            project,
            tmux,
            exe,
            env: BTreeMap::new(),
        };
        // Construct the owner before staging anything: if a later write or
        // command fails, normal local-drop cleanup removes this run's unique
        // directory and sandbox instead of leaving a half-built fixture.
        write_executable(&sandbox.script, SANDBOX_SCRIPT).context("staging the sandbox script")?;
        std::fs::write(&sandbox.config, SANDBOX_CONFIG).context("staging the sandbox config")?;
        // The shell appends to this. A run that died before its teardown — a
        // crash, a SIGKILL — leaves commands behind that would satisfy this
        // run's steps before the learner has typed anything.
        std::fs::write(&sandbox.history, "").context("clearing the command log")?;
        write_workspace(&sandbox.project).context("staging the practice workspace")?;
        sandbox.script_command(&["up"])?;
        sandbox.env = sandbox.read_env()?;
        sandbox.write_rcfile()?;
        sandbox.prepare_shell()?;
        Ok(sandbox)
    }

    fn script_command(&self, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("bash");
        cmd.arg(&self.script).arg(args[0]);
        cmd.args(["--name", &self.name]);
        cmd.arg("--tmux").arg(&self.tmux);
        cmd.arg("--muxa").arg(&self.exe);
        if let Some(muxad) = muxad_beside(&self.exe) {
            cmd.arg("--muxad").arg(muxad);
        }
        if args[0] == "up" {
            cmd.arg("--config").arg(&self.config);
            // Without this the sandbox server reads the learner's own
            // `~/.tmux.conf`. Somebody who rebound their prefix to `C-a` would
            // then be told to press `Ctrl-b c`, and step 2 would be a dead end
            // with no explanation — exactly the shape of issue #76.
            cmd.args(["--tmux-config", "/dev/null"]);
            if let Some(dir) = self.exe.parent() {
                cmd.arg("--extra-path").arg(dir);
            }
        }
        cmd.args(&args[1..]);
        let out = cmd.output().context("running the sandbox script")?;
        if !out.status.success() {
            bail!(
                "sandbox {} failed:\n{}",
                args[0],
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn read_env(&self) -> Result<BTreeMap<String, String>> {
        let raw = self.script_command(&["env"])?;
        let mut env = BTreeMap::new();
        for line in raw.lines() {
            let Some(rest) = line.strip_prefix("export ") else {
                continue;
            };
            let Some((key, value)) = rest.split_once('=') else {
                continue;
            };
            // `env` single-quotes every value, and appends an unexpanded
            // `:"$PATH"` that only a shell should resolve.
            let value = value
                .split(":\"$PATH\"")
                .next()
                .unwrap_or_default()
                .trim_matches('\'');
            env.insert(key.to_string(), value.to_string());
        }
        if !env.contains_key("MUXA_SOCKET") {
            bail!("the sandbox did not report its environment");
        }
        Ok(env)
    }

    fn env_value(&self, key: &str) -> String {
        self.env.get(key).cloned().unwrap_or_default()
    }

    /// The learner's shell. It carries the sandbox environment explicitly
    /// rather than relying on inheritance, and a bare prompt so the tour does
    /// not drag anyone's Starship setup into the recording of their own screen.
    ///
    /// The prompt also carries the current step. Printing it instead put the
    /// instruction *below* the prompt bash had already drawn, so whatever the
    /// learner typed next had nothing in front of it and the shell looked like
    /// it had gone away. As part of the prompt it sits where every shell puts
    /// context, and refreshes on every command — only outside tmux, since
    /// inside the status bar already says it.
    fn write_rcfile(&self) -> Result<()> {
        use std::fmt::Write as _;

        let mut body = String::from("unset PROMPT_COMMAND\n");
        // The reminder goes through `PROMPT_COMMAND` rather than `PS1`:
        // `$(cat …)` strips trailing newlines, so a reminder rendered in the
        // prompt ran straight into `muxa-onboarding $` on the same line.
        // Inside tmux the status bar already says all this.
        let _ = writeln!(
            body,
            "export PROMPT_COMMAND='history 1 | sed \"s/^ *[0-9]* *//\" >> {}; \
             [ -z \"$TMUX\" ] && cat {} 2>/dev/null'",
            self.history.display(),
            self.cue.display()
        );
        let _ = writeln!(body, "export PS1='{PROMPT}'");
        // Every pane, not just the first: a window the learner opens at step 2
        // would otherwise land back in their own home directory.
        let _ = writeln!(body, "export HOME='{}'", self.home.display());
        let _ = writeln!(body, "cd '{}' 2>/dev/null || true", self.project.display());
        for key in [
            "MUXA_SOCKET",
            "MUXA_CONFIG",
            "XDG_DATA_HOME",
            "MUXA_TMUX_SOCKET",
        ] {
            let _ = writeln!(body, "export {key}='{}'", self.env_value(key));
        }
        let _ = writeln!(
            body,
            "export PATH='{}':\"$PATH\"",
            self.env_value("MUXA_SANDBOX_SHIM")
        );
        if let Some(dir) = self.exe.parent() {
            let _ = writeln!(body, "export PATH='{}':\"$PATH\"", dir.display());
        }
        std::fs::write(&self.rcfile, body).context("staging the learner's shell profile")?;
        Ok(())
    }

    /// Make every pane the learner opens use the tour's shell.
    ///
    /// The rcfile only covers the shell this process starts. The moment they
    /// run `tmux new-session`, tmux opens their *login* shell instead — which
    /// never reads that file, and which does read their own dotfiles against a
    /// redirected `XDG_DATA_HOME`, so a plugin manager greets the learner with
    /// an installation error on step one. Pinning `default-command` fixes the
    /// prompt, the `PATH`, and that first impression in one line.
    ///
    /// `PATH` goes into the server environment as well, for anything that ends
    /// up running outside a login shell.
    fn prepare_shell(&self) -> Result<()> {
        let mut path = self.env_value("MUXA_SANDBOX_SHIM");
        if let Some(dir) = self.exe.parent() {
            path.push(':');
            path.push_str(&dir.to_string_lossy());
        }
        if let Some(inherited) = std::env::var_os("PATH") {
            path.push(':');
            path.push_str(&inherited.to_string_lossy());
        }
        self.tmux_command(&["set-environment", "-g", "PATH", &path])?;
        // Panes created with an explicit command — the agents — are unaffected.
        self.tmux_command(&[
            "set-option",
            "-g",
            "default-command",
            &format!("bash --rcfile {}", self.rcfile.display()),
        ])?;
        Ok(())
    }

    fn tmux_command(&self, args: &[&str]) -> Result<String> {
        let socket = self.env_value("MUXA_TMUX_SOCKET");
        // `-f` so no call of the tour's can start a server that reads the
        // learner's `~/.tmux.conf`. Their hooks running inside the sandbox is
        // what detached them the moment they attached.
        let config = self.env_value("MUXA_SANDBOX_TMUX_CONFIG");
        let config = if config.is_empty() {
            "/dev/null".to_string()
        } else {
            config
        };
        let out = Command::new(&self.tmux)
            .args(["-u", "-f", &config, "-S", &socket])
            .args(args)
            .output()
            .context("running tmux against the sandbox")?;
        if !out.status.success() {
            bail!(
                "tmux {} failed ({}):\n{}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    }

    /// Best-effort tmux, for the many polls whose failure is not interesting —
    /// asking about a session that has already gone away, mostly.
    fn tmux_quiet(&self, args: &[&str]) -> String {
        self.tmux_command(args).unwrap_or_default()
    }

    /// muxa, speaking as one of the sandbox's panes. Every collaboration call
    /// needs this: the daemon refuses an origin it cannot correlate to a
    /// tracked pane, and an unstamped call would carry the learner's *real*
    /// `$TMUX_PANE` instead.
    fn muxa_as(&self, pane: &str, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new(&self.exe);
        cmd.args(args);
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        cmd.env("TMUX", self.env_value("MUXA_SANDBOX_TMUX_ENV"));
        cmd.env("TMUX_PANE", pane);
        let out = cmd.output().context("running muxa against the sandbox")?;
        if !out.status.success() {
            bail!(
                "muxa {} failed ({}):\n{}",
                args.join(" "),
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Answer whatever the learner just asked claude.
    ///
    /// Their request is real and sits in claude's mailbox; leaving it there
    /// would teach that messages go nowhere. A reply they can find with
    /// `muxa msg list` is the point of a durable mailbox — a line in a
    /// transcript is a drawing of one. Failure is fatal because the next step
    /// explicitly asks the learner to read this reply.
    fn reply_as_claude(&self, fleet: &Fleet, language: UiLanguage) -> Result<()> {
        let raw = self.muxa_as(&fleet.claude, &["msg", "inbox", "--json"])?;
        let requests = serde_json::from_str::<serde_json::Value>(&raw)
            .context("reading claude's live-tour inbox")?;
        let id = requests
            .as_array()
            .and_then(|items| items.first())
            .and_then(|item| item.get("id"))
            .and_then(serde_json::Value::as_str)
            .context("claude's live-tour inbox had no request to answer")?;
        let body = tr(
            language,
            "auth path is covered; writing the regression test now",
            "인증 경로는 끝났고, 지금 회귀 테스트를 쓰는 중입니다",
        );
        self.muxa_as(&fleet.claude, &["msg", "reply", id, body])?;
        Ok(())
    }

    /// Feed the daemon the payload a real agent CLI would send, so the states
    /// the tour teaches arrive through the real pipeline rather than being
    /// drawn. Only valid once the daemon is up — events sent before it starts
    /// are simply lost.
    fn hook(&self, pane: &str, kind: &str, event: &str, body: &str) -> Result<()> {
        let mut cmd = Command::new(&self.exe);
        cmd.args(["hook", kind, "--event", event]);
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        cmd.env("TMUX", self.env_value("MUXA_SANDBOX_TMUX_ENV"));
        cmd.env("TMUX_PANE", pane);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn().context("spawning a hook")?;
        child
            .stdin
            .as_mut()
            .context("hook stdin")?
            .write_all(body.as_bytes())?;
        let output = child.wait_with_output().context("waiting for a hook")?;
        if !output.status.success() {
            bail!(
                "muxa hook {kind} --event {event} failed ({}):\n{}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(())
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.script_command(&["down"]);
        let temp = std::env::temp_dir();
        // Exact equality to the directory this instance allocated, plus the
        // validated prefix, keeps recursive cleanup away from every caller path.
        if self.name.starts_with(SANDBOX_NAME_PREFIX) && self.dir == temp.join(&self.name) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

fn allocate_tour_dir() -> Result<(String, PathBuf)> {
    let temp = std::env::temp_dir();
    let pid = std::process::id();
    for attempt in 0..100_u8 {
        let name = if attempt == 0 {
            format!("{SANDBOX_NAME_PREFIX}-{pid}")
        } else {
            format!("{SANDBOX_NAME_PREFIX}-{pid}-{attempt}")
        };
        let dir = temp.join(&name);
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&dir) {
            Ok(()) => return Ok((name, dir)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("creating the live-tour directory"),
        }
    }
    bail!("could not allocate a unique live-tour directory")
}

fn write_workspace(project: &Path) -> Result<()> {
    for (relative, body) in WORKSPACE_FILES {
        let path = project.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, body)?;
    }
    Ok(())
}

fn write_executable(path: &Path, body: &str) -> Result<()> {
    std::fs::write(path, body)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))?;
    }
    Ok(())
}

fn which(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(binary))
        .find(|candidate| candidate.is_file())
}

/// `muxad` normally sits beside `muxa`; the `PATH` fallback covers a
/// development tree where only one of them is on it.
fn muxad_beside(exe: &Path) -> Option<PathBuf> {
    let sibling = exe.parent()?.join("muxad");
    if sibling.is_file() {
        Some(sibling)
    } else {
        which("muxad")
    }
}

// ---------------------------------------------------------------------------
// Narration
// ---------------------------------------------------------------------------

impl Sandbox {
    fn prepare_status(&self) -> Result<()> {
        // Four rows, and row 0 is left as tmux's own window list.
        // Overwriting it meant a learner who pressed `Ctrl-b c` had no way
        // to see the window they had just made — the tour was hiding its
        // own evidence.
        self.tmux_command(&["set", "-g", "status", "4"])?;
        self.tmux_command(&["set", "-g", "status-position", "top"])?;
        self.tmux_command(&["set", "-g", "status-style", "bg=#0b1220,fg=#c9d1d9"])?;
        self.tmux_command(&["set", "-g", "status-interval", "2"])?;
        // tmux renames a window after whatever is running in it, which turns
        // the topology into a list of processes — `bash`, and then `muxa` the
        // moment they start watch. A window is a Work, and a Work keeps its
        // name.
        self.tmux_command(&["set", "-g", "automatic-rename", "off"])?;
        // A labelled border over every pane. Without it the mailbox steps ask
        // the learner to believe that a message left one pane and arrived in
        // another, while the screen shows three anonymous boxes — the whole
        // point of the act is which pane did what, so each one says who it is
        // and the border carries what just happened to it.
        self.tmux_command(&["set", "-g", "pane-border-status", "top"])?;
        self.tmux_command(&["set", "-g", "pane-border-format", " #{pane_title} "])?;
        self.tmux_command(&["set", "-g", "pane-border-style", "fg=#30363d"])?;
        self.tmux_command(&["set", "-g", "pane-active-border-style", "fg=#1f6feb,bold"])?;
        // Root table, so they need no prefix and cannot be confused with typing.
        self.tmux_command(&["bind-key", "-n", "F12", "set", "-g", SKIP_OPTION, "1"])?;
        self.tmux_command(&["bind-key", "-n", "F2", "set", "-g", LANGUAGE_OPTION, "1"])?;
        Ok(())
    }

    /// Reads the flag and clears it, so one keypress is one request.
    ///
    /// A server that has gone away is not an error here. `exit` in the
    /// learner's pane takes the pane, and with it the last window, the session
    /// and the server; the poll loop notices that on its own turn. Raising
    /// here instead printed tmux's own `no server running on …` at somebody
    /// who had just typed `exit`.
    fn consume_flag(&self, option: &str) -> bool {
        let Ok(value) = self.tmux_command(&["show", "-gvq", option]) else {
            return false;
        };
        if value != "1" {
            return false;
        }
        let _ = self.tmux_command(&["set", "-gu", option]);
        true
    }

    /// What the learner's prompt shows above itself, until the step changes.
    fn write_cue(&self, line: &str) -> Result<()> {
        std::fs::write(&self.cue, line).context("writing the prompt's step reminder")
    }

    /// Whether the sandbox server is still there.
    fn server_alive(&self) -> bool {
        self.tmux_command(&["display-message", "-p", "#{pid}"])
            .is_ok_and(|pid| !pid.trim().is_empty())
    }

    /// Has the learner run a command starting with this?
    fn ran_command(&self, prefix: &str) -> bool {
        std::fs::read_to_string(&self.history)
            .is_ok_and(|log| log.lines().any(|line| line.trim().starts_with(prefix)))
    }

    fn skip_requested(&self) -> bool {
        self.consume_flag(SKIP_OPTION)
    }

    fn language_toggled(&self) -> bool {
        self.consume_flag(LANGUAGE_OPTION)
    }

    #[allow(clippy::too_many_arguments)]
    fn narrate(
        &self,
        index: usize,
        total: usize,
        achieved: &str,
        title: &str,
        cue: &str,
        escape: Option<&str>,
        other_language: &str,
    ) -> Result<()> {
        // What just happened comes first. A tour that only ever says what to do
        // next leaves the learner typing commands and guessing whether any of
        // them landed.
        let banner = format!(
            "#[align=left bg=#1f6feb,fg=#0b1220,bold] muxa onboarding · {index}/{total} \
             #[default]  #[fg=#3fb950]{achieved}#[default]#[fg=#8b949e]    F2 {other_language}"
        );
        let title = format!("#[align=left]  {title}");
        let cue = match escape {
            Some(hint) => {
                format!("#[align=left fg=#d29922,bold]  {cue}#[default]#[fg=#8b949e]    {hint}")
            }
            None => format!("#[align=left fg=#d29922,bold]  {cue}"),
        };
        // Row 0 stays tmux's own window list.
        self.tmux_command(&["set", "-g", "status-format[1]", &banner])?;
        self.tmux_command(&["set", "-g", "status-format[2]", &title])?;
        self.tmux_command(&["set", "-g", "status-format[3]", &cue])?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The learner's workspace
// ---------------------------------------------------------------------------

/// A checkout service that does not exist, laid out where the tour can point
/// at it.
///
/// Without this the learner's shell sits in whatever directory they launched
/// from, so `ls` shows their own repository and `muxa watch` prints their real
/// path in the inspector — the tour claiming to be a sandbox while showing
/// them their own machine. The files are the ones the scripted agents say they
/// are reading, so `cat crates/checkout/src/auth.rs` answers.
///
/// This is a convincing workspace, not a jail. `cd /` still works: a real
/// filesystem confinement needs bubblewrap or a mount namespace, neither of
/// which is available unprivileged on every platform muxa runs on.
const WORKSPACE_FILES: &[(&str, &str)] = &[
    (
        "Cargo.toml",
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/api\", \"crates/checkout\"]\n",
    ),
    (
        "README.md",
        "# checkout-service\n\nPayments and the public read API.\n\n\
         - `crates/checkout` — auth, capture, refunds\n\
         - `crates/api` — the public-read boundary\n",
    ),
    (
        "crates/checkout/src/auth.rs",
        "use crate::Session;\n\n\
         /// Extracts the bearer token from an Authorization header.\n\
         pub fn bearer_token(header: &str) -> Option<&str> {\n\
         \x20   header.strip_prefix(\"Bearer \")\n\
         }\n\n\
         /// TODO: this stores the raw bearer token on the session.\n\
         pub fn attach(session: &mut Session, bearer: &str) {\n\
         \x20   session.bearer = bearer.to_string();\n\
         }\n\n\
         pub fn is_bearer(scheme: &str) -> bool {\n\
         \x20   scheme.eq_ignore_ascii_case(\"bearer\")\n\
         }\n",
    ),
    (
        "crates/checkout/src/lib.rs",
        "pub mod auth;\n\n\
         pub struct Session {\n\
         \x20   pub bearer: String,\n\
         }\n",
    ),
    (
        "crates/api/src/public.rs",
        "/// Everything reachable without a session.\n\
         ///\n\
         /// Anything added here is public forever, so the boundary is reviewed\n\
         /// before it moves.\n\
         pub const PUBLIC_READ: &[&str] = &[\n\
         \x20   \"/health\",\n\
         \x20   \"/v1/catalog\",\n\
         \x20   \"/v1/prices\",\n\
         ];\n",
    ),
    ("crates/api/src/lib.rs", "pub mod public;\n"),
    (
        "tests/auth.rs",
        "#[test]\n\
         fn rejects_raw_bearer() {\n\
         \x20   // pending: the regression test claude is writing\n\
         }\n",
    ),
];

// ---------------------------------------------------------------------------
// Agent transcripts
// ---------------------------------------------------------------------------

/// One scripted agent's screen.
///
/// The pane runs `tail -f` over this file and the tour appends to it, so the
/// session *grows* the way a real one does. A painted frame would have been
/// less code, but `muxa watch`'s inspector and preview both render the selected
/// pane's live screen — a fleet parked on a single static line makes those
/// features look broken, and makes the tour's claim that this is what the work
/// looks like ring hollow.
///
/// Nothing here shells out to an agent CLI. The transcript is a fiction; what
/// muxa does around it is not, and the tour says so rather than letting anyone
/// believe they watched Claude answer.
struct Transcript {
    path: PathBuf,
}

impl Transcript {
    /// An existing transcript, left as it is.
    fn open(dir: &Path, name: &str) -> Self {
        Self {
            path: dir.join(format!("muxa-onboarding-{name}.log")),
        }
    }

    fn new(dir: &Path, name: &str) -> Result<Self> {
        let path = dir.join(format!("muxa-onboarding-{name}.log"));
        std::fs::write(&path, "").with_context(|| format!("staging {name}'s transcript"))?;
        Ok(Self { path })
    }

    fn append(&self, lines: &[String]) {
        use std::io::Write as _;
        let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&self.path) else {
            return;
        };
        for line in lines {
            let _ = writeln!(file, "{line}");
        }
    }

    /// `tail` rather than `cat`: it never reaches EOF, so the pane stays open
    /// and shows whatever arrives next.
    fn pane_command(&self) -> String {
        format!("exec tail -n +1 -f {}", self.path.display())
    }

    /// A pane that shows the transcript *and* answers its own approval prompt.
    ///
    /// A prompt reading `[y] yes  [n] no` that swallows the keystroke is worse
    /// than no prompt: it invites the learner to do the one thing the tour has
    /// made impossible. Pressing `y` here appends the tool output and fires the
    /// hook a resuming agent fires, so the row in `muxa watch` goes back to
    /// `working` — the real attend-and-clear loop rather than a picture of it.
    fn answerable_pane_command(
        &self,
        dir: &Path,
        approved: &Path,
        declined: &Path,
    ) -> Result<String> {
        let runner = dir.join("muxa-onboarding-codex-pane.sh");
        let body = format!(
            r#"#!/usr/bin/env bash
# codex's screen, plus the keypress that answers its approval prompt.
# Written by `muxa onboard`; removed with the sandbox.
log={log}
tail -n +1 -f "$log" &
while IFS= read -rsn1 key; do
  case "$key" in
    y|Y|a|A)
      cat {approved} >> "$log"
      # `pre_tool_use` is what a resuming agent sends, and what takes the row
      # back out of `waiting` in watch.
      muxa hook codex --event pre_tool_use \
        <<< '{{"session_id":"onboarding-codex","tool_name":"shell"}}' \
        >/dev/null 2>&1
      break ;;
    n|N)
      cat {declined} >> "$log"
      break ;;
  esac
done
wait
"#,
            log = self.path.display(),
            approved = approved.display(),
            declined = declined.display(),
        );
        write_executable(&runner, &body)?;
        Ok(format!("exec bash {}", runner.display()))
    }
}

const DIM: &str = "\u{1b}[2m";
const BOLD: &str = "\u{1b}[1m";
const RESET: &str = "\u{1b}[0m";
const GREEN: &str = "\u{1b}[1;32m";
const YELLOW: &str = "\u{1b}[1;33m";
const BLUE: &str = "\u{1b}[1;34m";
const MAGENTA: &str = "\u{1b}[1;35m";
const CYAN: &str = "\u{1b}[1;36m";

/// The shape of the CLI a pane is standing in for.
///
/// A generic "agent frame" is what made the first version read as a mock: the
/// learner knows what these two look like, and a screen that matches nothing
/// they recognise is a screen they discount. Claude Code marks its turns with
/// `⏺` and folds tool results under `⎿`; Codex prefixes its own with `•` and
/// `└`. Close enough to be recognised, never close enough to be mistaken for a
/// real session — the first line says so outright.
#[derive(Clone, Copy)]
enum Cli {
    ClaudeCode,
    Codex,
}

impl Cli {
    fn colour(self) -> &'static str {
        match self {
            Self::ClaudeCode => MAGENTA,
            Self::Codex => CYAN,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
        }
    }

    /// The glyph each CLI puts in front of its own turns.
    fn turn(self) -> &'static str {
        match self {
            Self::ClaudeCode => "⏺",
            Self::Codex => "•",
        }
    }

    /// And in front of the result folded underneath.
    fn result(self) -> &'static str {
        match self {
            Self::ClaudeCode => "⎿ ",
            Self::Codex => "└ ",
        }
    }
}

fn header(cli: Cli, language: UiLanguage) -> Vec<String> {
    let colour = cli.colour();
    let name = cli.name();
    vec![
        format!(
            "{DIM}⟨{}⟩{RESET}",
            tr(
                language,
                "simulated session — no agent CLI is running",
                "시뮬레이션 세션 — 실제 agent CLI는 실행되지 않습니다",
            )
        ),
        String::new(),
        format!("{colour}▐{RESET} {BOLD}{name}{RESET} {DIM}· ~/checkout-service{RESET}"),
        String::new(),
    ]
}

fn prompt_line(text: &str) -> Vec<String> {
    vec![format!("{GREEN}>{RESET} {text}"), String::new()]
}

/// One turn: what the agent did, and what came back.
fn turn(cli: Cli, action: &str, result: &str) -> Vec<String> {
    let mut lines = vec![format!("{}{}{RESET} {action}", cli.colour(), cli.turn())];
    if !result.is_empty() {
        lines.push(format!("  {DIM}{}{result}{RESET}", cli.result()));
    }
    lines.push(String::new());
    lines
}

/// The input box Claude Code and Codex both park at the bottom of the screen.
/// Without it the pane reads as a log rather than as a session waiting on you.
/// Columns a string occupies in a terminal.
///
/// Padding by character count leaves the box's right edge ragged the moment any
/// CJK text is in it, because those glyphs are two cells wide. Close enough for
/// a fixed-width box without taking a dependency for it.
fn display_width(text: &str) -> usize {
    text.chars()
        .map(|ch| match ch as u32 {
            0x1100..=0x115F
            | 0x2E80..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1F64F
            | 0x20000..=0x3FFFD => 2,
            _ => 1,
        })
        .sum()
}

/// How wide an agent's pane has to be for its own frames to render.
///
/// The layout reads this, so narrowing the mock screens widens the learner's
/// pane rather than leaving a box clipped down the middle.
const AGENT_FRAME_COLUMNS: usize = 60;

fn composer(cli: Cli, language: UiLanguage) -> Vec<String> {
    let width: usize = AGENT_FRAME_COLUMNS - 2;
    // Each CLI's own idle hint. Inviting the learner to type here would be a
    // lie — they talk to these agents through `muxa msg`, not this box.
    let hint = match cli {
        Cli::ClaudeCode => tr(language, "? for shortcuts", "? 로 단축키 보기"),
        Cli::Codex => tr(language, "Ctrl+C to quit", "Ctrl+C 로 종료"),
    };
    let padding = (width - 4).saturating_sub(display_width(hint));
    vec![
        format!("{DIM}╭{}╮{RESET}", "─".repeat(width)),
        format!(
            "{DIM}│{RESET} {}>{RESET}  {DIM}{hint}{}{RESET}{DIM}│{RESET}",
            cli.colour(),
            " ".repeat(padding),
        ),
        format!("{DIM}╰{}╯{RESET}", "─".repeat(width)),
    ]
}

/// What codex's pane shows once it is blocked. Mirrors the `approval` frame in
/// `docs/demo-paint.sh`.
fn approval_block(language: UiLanguage) -> Vec<String> {
    vec![
        String::new(),
        format!(
            "  {YELLOW}⏸  {}{RESET}",
            tr(language, "Approval required", "승인이 필요합니다")
        ),
        format!("     {BOLD}$ rg -n \"public_read\" --type rust{RESET}"),
        String::new(),
        format!(
            "     {GREEN}[y]{RESET} {}   {YELLOW}[n]{RESET} {}   {DIM}[a]{RESET} {}",
            tr(language, "yes", "예"),
            tr(language, "no", "아니오"),
            tr(language, "yes, and don't ask again", "예, 다시 묻지 않기"),
        ),
    ]
}

fn approved_block(language: UiLanguage) -> Vec<String> {
    let mut lines = turn(
        Cli::Codex,
        "Ran rg -n \"public_read\" --type rust",
        tr(language, "3 matches", "3건 일치"),
    );
    lines.push(working(language));
    lines.push(String::new());
    lines
}

fn declined_block(language: UiLanguage) -> Vec<String> {
    vec![
        String::new(),
        format!(
            "  {DIM}✗ {}{RESET}",
            tr(
                language,
                "declined — leaving it alone",
                "거절됨 — 그대로 둡니다"
            )
        ),
    ]
}

fn incoming_question(language: UiLanguage) -> Vec<String> {
    let mut lines = turn(
        Cli::ClaudeCode,
        &format!(
            "muxa_inbox()  {DIM}·  {}{RESET}",
            tr(language, "1 request", "요청 1건")
        ),
        tr(language, "how far along?", "어디까지 됐나요?"),
    );
    lines.extend(turn(
        Cli::ClaudeCode,
        tr(
            language,
            "Replied: auth path is covered; writing the regression test now",
            "답장함: 인증 경로는 끝났고, 지금 회귀 테스트를 쓰는 중입니다",
        ),
        "",
    ));
    lines
}

fn outgoing_request(language: UiLanguage) -> Vec<String> {
    turn(
        Cli::Codex,
        &format!(
            "muxa_send_message(to: \"you\")  {DIM}·  {}{RESET}",
            tr(language, "waiting on a decision", "결정 대기 중")
        ),
        tr(
            language,
            "the public-read boundary needs a decision before I continue",
            "public-read 경계는 계속하기 전에 결정이 필요합니다",
        ),
    )
}

fn finished(language: UiLanguage) -> Vec<String> {
    let mut lines = turn(
        Cli::ClaudeCode,
        "Bash(cargo test -p checkout auth)",
        tr(language, "1 passed", "1건 통과"),
    );
    lines.push(format!(
        "{GREEN}⏺{RESET} {}",
        tr(language, "Done.", "완료했습니다.")
    ));
    lines.push(String::new());
    lines
}

fn working(language: UiLanguage) -> String {
    format!("  {BLUE}▶ {}{RESET}", tr(language, "working…", "작업 중…"))
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// The learner's pane and the two scripted agents.
///
/// All three live in one window on purpose. A collaboration room *is* a tmux
/// window — `@alias` targets resolve inside it and nowhere else — so putting
/// the learner anywhere else would force them to address peers by raw pane id,
/// which teaches the wrong thing about the model.
struct Fleet {
    learner: String,
    claude: String,
    codex: String,
    claude_screen: Transcript,
    codex_screen: Transcript,
}

/// The first screenful each agent shows, in the shape of the CLI it stands in
/// for.
fn write_opening_screens(claude: &Transcript, codex: &Transcript, language: UiLanguage) {
    let claude_prompt = tr(
        language,
        "harden the checkout auth path",
        "checkout 인증 경로를 강화해줘",
    );
    let codex_prompt = tr(
        language,
        "review the public-read boundary",
        "public-read 경계를 검토해줘",
    );

    let mut opening = header(Cli::ClaudeCode, language);
    opening.extend(prompt_line(claude_prompt));
    opening.extend(turn(
        Cli::ClaudeCode,
        tr(
            language,
            "I'll start with the token handling in auth.rs.",
            "auth.rs의 토큰 처리부터 보겠습니다.",
        ),
        "",
    ));
    opening.extend(turn(
        Cli::ClaudeCode,
        "Read(crates/checkout/src/auth.rs)",
        tr(language, "Read 42 lines", "42줄 읽음"),
    ));
    opening.extend(turn(
        Cli::ClaudeCode,
        "Search(pattern: \"bearer\")",
        tr(language, "Found 12 matches", "12건 일치"),
    ));
    opening.push(working(language));
    opening.push(String::new());
    opening.extend(composer(Cli::ClaudeCode, language));
    claude.append(&opening);

    let mut opening = header(Cli::Codex, language);
    opening.extend(prompt_line(codex_prompt));
    opening.extend(turn(
        Cli::Codex,
        "Read crates/api/src/public.rs",
        tr(language, "3 routes listed", "route 3개 확인"),
    ));
    opening.push(working(language));
    opening.push(String::new());
    opening.extend(composer(Cli::Codex, language));
    codex.append(&opening);
}

impl Sandbox {
    /// Stage the agent screens before the step that brings them up.
    fn prepare_agents(&self, language: UiLanguage) -> Result<()> {
        let claude_screen = Transcript::new(&self.dir, "claude")?;
        let codex_screen = Transcript::new(&self.dir, "codex")?;
        write_opening_screens(&claude_screen, &codex_screen, language);
        Ok(())
    }

    /// How many times the learner's prompt has been drawn.
    ///
    /// Their `PROMPT_COMMAND` appends a line each time, so this counts
    /// keystrokes at the prompt — including the bare Enter that a step can ask
    /// for without asking for a command.
    fn prompt_count(&self) -> usize {
        std::fs::read_to_string(&self.history).map_or(0, |log| log.lines().count())
    }

    /// What a pane's border says it is.
    ///
    /// `select-pane -T` rather than anything drawn: tmux renders the title in
    /// the border it is already drawing, so the label cannot drift out of
    /// alignment with the pane it names.
    fn label_pane(&self, pane: &str, label: &str) {
        let _ = self.tmux_command(&["select-pane", "-t", pane, "-T", label]);
    }

    /// Learner on the left, agents stacked on the right.
    ///
    /// Laid out rather than zoomed. Zooming the learner's pane gave
    /// `muxa watch` the whole screen and hid the two agents that had just
    /// arrived — so the step confirmed an arrival the learner could not see,
    /// above an instruction to look at the whole Work, over a blank screen.
    /// Half the width is still plenty for watch, and the fleet is what the
    /// next four steps are about.
    fn lay_out_fleet(&self, window: &str, learner: &str) -> Result<()> {
        // `main-vertical` makes the first pane the wide one, and the learner's
        // is first only because it predates the split. Not worth assuming.
        let first = self
            .tmux_command(&["list-panes", "-t", window, "-F", "#{pane_id}"])?
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        if !first.is_empty() && first != learner {
            self.tmux_command(&["swap-pane", "-s", &first, "-t", learner])?;
        }
        // A column count rather than `50%`, which older tmux rejects here.
        // Half each suits a wide terminal. On a narrow one, half would put the
        // agents below the width their own frames need, so they keep that and
        // the learner takes what is left — down to a floor, because a pane too
        // narrow for `muxa watch` is its own kind of broken.
        let width = self
            .tmux_command(&["display-message", "-p", "-t", window, "#{window_width}"])?
            .trim()
            .parse::<usize>()
            .unwrap_or(160);
        let main = (width / 2)
            .min(width.saturating_sub(AGENT_FRAME_COLUMNS + 1))
            .max(60)
            .min(width.saturating_sub(20))
            .max(20);
        self.tmux_command(&[
            "set-window-option",
            "-t",
            window,
            "main-pane-width",
            &main.to_string(),
        ])?;
        self.tmux_command(&["select-layout", "-t", window, "main-vertical"])?;
        self.tmux_command(&["select-pane", "-t", learner])?;
        Ok(())
    }

    /// codex joins whatever window the learner started claude in.
    ///
    /// Joining *their* window rather than a prepared one is the point: the room
    /// they can address peers in is the window they are already in, and
    /// watching a pane arrive beside their own is what makes "a pane is an
    /// agent" land.
    fn add_agents(
        &self,
        session: &str,
        before_split: &[String],
        language: UiLanguage,
    ) -> Result<Fleet> {
        // Resolved through the session rather than the attached client: a
        // learner who skipped their way here may not be attached to anything,
        // and the agents still have to land in a real window.
        let target = format!("{session}:");
        let window =
            self.tmux_command(&["display-message", "-p", "-t", &target, "#{window_id}"])?;
        if window.is_empty() {
            bail!("could not find a window to put the agents in");
        }

        // The pane the learner just split off becomes claude, and the one they
        // were sitting in stays theirs. Watching an agent move into the pane
        // *they* made is what turns "a pane is an agent" from a sentence into
        // something they did.
        let panes: Vec<String> = self
            .tmux_command(&["list-panes", "-t", &window, "-F", "#{pane_id}"])?
            .lines()
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .collect();
        let fresh = panes
            .iter()
            .find(|id| !before_split.contains(id))
            .cloned()
            .unwrap_or_default();
        let learner = panes
            .iter()
            .find(|id| **id != fresh)
            .cloned()
            .unwrap_or_default();
        if learner.is_empty() {
            bail!("could not tell your pane from the one you split off");
        }
        // Already written by `prepare_agents`; opening them again would blank
        // the screen the learner is looking at.
        let claude_screen = Transcript::open(&self.dir, "claude");
        let codex_screen = Transcript::open(&self.dir, "codex");

        let approved = self.dir.join("muxa-onboarding-approved.txt");
        let declined = self.dir.join("muxa-onboarding-declined.txt");
        std::fs::write(&approved, approved_block(language).join("\n") + "\n")?;
        std::fs::write(&declined, declined_block(language).join("\n") + "\n")?;

        // A window is a Work, and in a real fleet it is named after one.
        self.tmux_command(&["rename-window", "-t", &window, "checkout"])?;
        // Whatever they made at step 2 is a Work too — an unstaffed one. Left
        // called `bash`, the fleet looks half-built rather than like a
        // workspace with a job nobody has picked up yet.
        for other in self
            .tmux_quiet(&["list-windows", "-a", "-F", "#{window_id}"])
            .lines()
            .filter(|id| !id.is_empty() && *id != window)
            .map(str::to_string)
            .collect::<Vec<_>>()
        {
            self.tmux_command(&["rename-window", "-t", &other, "release-checks"])?;
        }

        // The pane they split becomes claude. `respawn-pane -k` keeps the pane
        // id, so the hook registers the pane the learner is looking at — and
        // watching an agent arrive in the pane *they* made is what turns "a
        // pane is an agent" from a sentence into something they did.
        let claude = if fresh.is_empty() {
            learner.clone()
        } else {
            fresh.clone()
        };
        self.tmux_command(&[
            "respawn-pane",
            "-k",
            "-c",
            &self.project.to_string_lossy(),
            "-t",
            &claude,
            &claude_screen.pane_command(),
        ])?;
        let codex = self.split(
            &window,
            &codex_screen.answerable_pane_command(&self.dir, &approved, &declined)?,
        )?;

        self.lay_out_fleet(&window, &learner)?;
        self.label_pane(
            &learner,
            tr(language, " you · your shell", " you · 당신의 셸"),
        );
        self.label_pane(&claude, " @claude");
        self.label_pane(&codex, " @codex");

        let fleet = Fleet {
            learner,
            claude,
            codex,
            claude_screen,
            codex_screen,
        };
        self.seed_fleet(&fleet)?;
        Ok(fleet)
    }

    fn split(&self, window: &str, command: &str) -> Result<String> {
        // `-c` is not optional here. Invoked from outside any client,
        // `split-window` takes this process's working directory — the repo the
        // learner launched from — and `muxa watch` then prints that real path
        // as the agent's cwd, in a tour that just told them nothing outside the
        // sandbox is involved.
        let id = self.tmux_command(&[
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
            "-c",
            &self.project.to_string_lossy(),
            "-t",
            window,
            command,
        ])?;
        if id.is_empty() {
            bail!("could not create an agent pane");
        }
        Ok(id)
    }

    /// Hooks, not fabricated rows: the daemon has to be listening for these, so
    /// they run well after `daemon`, and the states they produce arrive through
    /// the same pipeline a real agent CLI uses.
    fn seed_fleet(&self, fleet: &Fleet) -> Result<()> {
        self.hook(
            &fleet.learner,
            "claude",
            "session_start",
            r#"{"session_id":"onboarding-you"}"#,
        )?;
        self.hook(
            &fleet.claude,
            "claude",
            "user_prompt_submit",
            r#"{"session_id":"onboarding-claude","prompt":"harden the checkout auth path"}"#,
        )?;
        self.hook(
            &fleet.codex,
            "codex",
            "user_prompt_submit",
            r#"{"session_id":"onboarding-codex","prompt":"review the public-read boundary"}"#,
        )?;
        // The daemon ingests hooks asynchronously, and both `identity` and `msg`
        // refuse an origin it has not correlated yet. Setting the aliases
        // immediately looks like it works and leaves the tour unable to advance
        // past `muxa msg send @claude`, so wait for the panes to actually land.
        if !self.wait_until_tracked(fleet) {
            bail!("the daemon did not pick up the tour's panes");
        }

        // Aliases are what make `muxa msg send @claude` teachable; without them
        // the learner would have to name a peer by raw pane id.
        // `session_start` classifies the learner pane through the Claude
        // adapter, so automatic pane handles may provision `claude` there
        // before the scripted peer arrives. Move the learner to `you` first;
        // that releases the room-local namespace before assigning peer names.
        for (pane, alias) in [
            (&fleet.learner, "you"),
            (&fleet.claude, "claude"),
            (&fleet.codex, "codex"),
        ] {
            self.muxa_as(pane, &["identity", "set", "--alias", alias])?;
        }
        Ok(())
    }

    fn wait_until_tracked(&self, fleet: &Fleet) -> bool {
        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline {
            let all_known = [&fleet.learner, &fleet.claude, &fleet.codex]
                .iter()
                .all(|pane| {
                    self.muxa_as(pane, &["peers"])
                        .is_ok_and(|out| out.contains("self"))
                });
            if all_known {
                return true;
            }
            std::thread::sleep(POLL);
        }
        false
    }
}

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

/// What a step is waiting for. Every variant reads real tmux or real muxa
/// state; none of them reads a keystroke.
enum Detect {
    /// The learner made a session of their own.
    SessionCreated,
    /// That session has more than the window it started with.
    SecondWindow,
    /// They opened the session tree and closed it again — one gesture.
    TreeModeClosed,
    /// Nobody is attached — they detached, and the work kept running.
    NoClient,
    /// They ran a command starting with this.
    TypedCommand(&'static str),
    /// Somebody is attached again.
    ClientAttached,
    /// Their window has more than the pane it started with.
    PaneSplit,
    /// The learner pressed Enter at their own shell.
    ///
    /// Their prompt appends a line every time it is drawn, so a bare Enter is
    /// one line more than the step opened with. Asking them to *type* `claude`
    /// asked for a command that is neither tmux nor muxa — the two things the
    /// tour is here to teach — and that the sandbox only pretends to have.
    PressedEnter,
    /// Some pane on the sandbox server is running this command.
    PaneRunning(&'static str),
    /// They opened watch and came back out of it.
    ///
    /// Watch owns the keyboard while it runs, so the tour cannot see `j`, `k`
    /// or `?` go by. Leaving it is the one transition that is visible from
    /// outside, and it is the one worth gating on: it means they were in there
    /// long enough to look.
    LeftWatch,
    /// The active pane is the one `muxa attend` should have jumped to.
    ActivePaneIsCodex,
    /// They came back to their own pane.
    ActivePaneIsLearner,
    /// The learner sent a request.
    SentMessage,
    /// The learner claimed what was waiting for them.
    ClaimedInbox,
}

struct Step {
    /// What the learner's last action actually did. Shown as this step opens,
    /// because "did that work?" is the question they are holding.
    achieved_en: &'static str,
    achieved_ko: &'static str,
    title_en: &'static str,
    title_ko: &'static str,
    cue_en: &'static str,
    cue_ko: &'static str,
    detect: Detect,
}

/// Fifteen steps in two acts, one action each.
///
/// Act I keeps only the one tmux idea muxa is built on — a session outlives the
/// terminal attached to it. Zoom, copy mode and pane-navigation drills are
/// deliberately absent: they are tmux trivia, and the pane concept lands in Act
/// II when the agents themselves arrive as panes.
/// An earlier pass fit this into nine by putting two actions on some cues —
/// "see both: Ctrl-b s · then leave: Ctrl-b d". That reads as one instruction
/// and leaves the learner unsure which half registered, which is the opposite
/// of what a confirmation line is for. Fewer steps is not the goal; knowing
/// where you are is.
const STEPS: &[Step] = &[
    Step {
        achieved_en: "ready — you are at a plain shell, outside tmux",
        achieved_ko: "준비 완료 — 지금은 tmux 밖의 평범한 셸입니다",
        title_en: "A tmux session is a workspace that keeps running without you.",
        title_ko: "tmux session은 당신 없이도 계속 도는 작업 공간입니다.",
        cue_en: "type:   tmux new-session -s muxa-onboarding",
        cue_ko: "입력:   tmux new-session -s muxa-onboarding",
        detect: Detect::SessionCreated,
    },
    Step {
        achieved_en: "✓ session created, and you are inside it — these rows are tmux's",
        achieved_ko: "✓ session을 만들고 들어왔습니다 — 이 줄들이 tmux입니다",
        title_en: "One window is one Work. Make a second one.",
        title_ko: "window 하나가 Work 하나입니다. 두 번째를 만들어 보세요.",
        cue_en: "press   Ctrl-b   then   c",
        cue_ko: "Ctrl-b 를 눌렀다 떼고   c",
        detect: Detect::SecondWindow,
    },
    Step {
        achieved_en: "✓ second window created — the top row lists both",
        achieved_ko: "✓ 두 번째 window가 생겼습니다 — 맨 윗줄에 둘 다 보입니다",
        title_en: "The tree is the readable view of that: sessions, and the windows in them.",
        title_ko: "그걸 제대로 보는 화면이 tree입니다: session과 그 안의 window들.",
        cue_en: "press   Ctrl-b   then   s      ·   q closes it",
        cue_ko: "Ctrl-b 를 눌렀다 떼고   s      ·   q 로 닫습니다",
        detect: Detect::TreeModeClosed,
    },
    Step {
        achieved_en: "✓ you saw both windows in the tree",
        achieved_ko: "✓ tree에서 window 두 개를 확인했습니다",
        title_en: "Now leave. Detaching removes you, not the work.",
        title_ko: "이제 나가 보세요. detach는 당신만 빠지고 작업은 그대로입니다.",
        cue_en: "press   Ctrl-b   then   d",
        cue_ko: "Ctrl-b 를 눌렀다 떼고   d",
        detect: Detect::NoClient,
    },
    Step {
        achieved_en: "✓ detached — you are back at your own shell",
        achieved_ko: "✓ detach했습니다 — 당신의 셸로 돌아왔습니다",
        title_en: "The session is still there. Ask tmux yourself.",
        title_ko: "session은 여전히 있습니다. tmux에게 직접 물어보세요.",
        cue_en: "type:   tmux ls",
        cue_ko: "입력:   tmux ls",
        detect: Detect::TypedCommand("tmux ls"),
    },
    Step {
        achieved_en: "✓ still listed, still running — that is the whole reason muxa lives in tmux",
        achieved_ko: "✓ 여전히 목록에 있고 여전히 돌고 있습니다 — muxa가 tmux에 사는 이유입니다",
        title_en: "So going back is just attaching to it again.",
        title_ko: "그러니 돌아가는 건 다시 붙기만 하면 됩니다.",
        cue_en: "type:   tmux attach -t muxa-onboarding",
        cue_ko: "입력:   tmux attach -t muxa-onboarding",
        detect: Detect::ClientAttached,
    },
    Step {
        achieved_en: "✓ back in, with every window and pane as you left it",
        achieved_ko: "✓ 다시 들어왔습니다 — window도 pane도 떠날 때 그대로입니다",
        title_en: "A pane is an agent. Split this window and one will move in.",
        title_ko: "pane 하나가 agent 하나입니다. 이 window를 나누면 거기에 agent가 들어옵니다.",
        cue_en: "press   Ctrl-b   then   %      ·   \" splits top and bottom instead",
        cue_ko: "Ctrl-b 를 눌렀다 떼고   %      ·   \" 는 상하로 나눕니다",
        detect: Detect::PaneSplit,
    },
    Step {
        achieved_en: "✓ split — you are in the new pane, at its own shell",
        achieved_ko: "✓ 나눴습니다 — 지금 새 pane의 셸에 있습니다",
        title_en: "A pane becomes an agent when one runs in it. The tour will start two.",
        title_ko: "pane은 거기서 agent가 돌면 agent가 됩니다. tour가 둘을 띄우겠습니다.",
        cue_en: "press   Enter                  ·   sets up the practice agents",
        cue_ko: "Enter 를 누르세요               ·   연습용 agent를 준비합니다",
        detect: Detect::PressedEnter,
    },
    Step {
        achieved_en: "✓ that pane is claude now, codex joined it, and your cursor is back in yours",
        achieved_ko: "✓ 그 pane이 claude가 됐고 codex도 합류했습니다 — 커서는 당신 pane에 있습니다",
        title_en: "`muxa watch` is the way in: every session, window and agent, live.",
        title_ko: "`muxa watch`가 진입점입니다: session·window·agent를 살아있는 채로 봅니다.",
        cue_en: "run:   muxa watch",
        cue_ko: "입력:   muxa watch",
        detect: Detect::PaneRunning("muxa"),
    },
    Step {
        achieved_en:
            "✓ watch is open — the tree is your fleet, and the row colour is each agent's state",
        achieved_ko:
            "✓ watch가 열렸습니다 — 트리가 당신의 fleet이고, 줄 색이 각 agent의 상태입니다",
        title_en: "j/k move · h/l fold · Enter jumps into the selected pane · ? lists every key.",
        title_ko: "j/k 이동 · h/l 접기 · Enter 로 선택한 pane에 진입 · ? 로 전체 키 목록.",
        cue_en: "look around, then   q   to come back here",
        cue_ko: "둘러본 뒤   q   로 돌아오세요",
        detect: Detect::LeftWatch,
    },
    Step {
        achieved_en: "✓ back at your shell — and codex has stopped, waiting on you",
        achieved_ko: "✓ 셸로 돌아왔습니다 — 그리고 codex가 멈춰서 당신을 기다립니다",
        title_en: "`attend` goes to whichever agent has been blocked longest — codex, here.",
        title_ko: "`attend`는 가장 오래 막힌 agent로 갑니다 — 여기서는 codex입니다.",
        cue_en: "run:   muxa attend",
        cue_ko: "입력:   muxa attend",
        detect: Detect::ActivePaneIsCodex,
    },
    Step {
        achieved_en: "✓ attend moved you into codex's pane — press y to approve it if you like",
        achieved_ko: "✓ attend가 codex의 pane으로 데려왔습니다 — 원하면 y로 승인해 보세요",
        title_en: "This pane has no shell of yours. `Ctrl-b ;` returns to the pane you were in.",
        title_ko: "이 pane에는 당신의 셸이 없습니다. `Ctrl-b ;`는 직전에 있던 pane으로 돌아갑니다.",
        cue_en: "press   Ctrl-b   then   ;      ·   Ctrl-b o cycles instead",
        cue_ko: "Ctrl-b 를 눌렀다 떼고   ;      ·   Ctrl-b o 는 순서대로 순환합니다",
        detect: Detect::ActivePaneIsLearner,
    },
    Step {
        achieved_en: "✓ back in your own pane",
        achieved_ko: "✓ 당신의 pane으로 돌아왔습니다",
        title_en: "You can ask an agent something without attaching to it.",
        title_ko: "attach하지 않고도 agent에게 물어볼 수 있습니다.",
        cue_en: "run:   muxa msg send @claude \"how far along?\"",
        cue_ko: "입력:   muxa msg send @claude \"어디까지 됐나요?\"",
        detect: Detect::SentMessage,
    },
    Step {
        achieved_en: "✓ the message reached claude, and it answered into your mailbox",
        achieved_ko: "✓ 메시지가 claude에게 닿았고, mailbox로 답이 왔습니다",
        title_en: "`list` shows what you sent, and what came back.",
        title_ko: "`list`는 보낸 것과 돌아온 답을 보여줍니다.",
        cue_en: "run:   muxa msg list",
        cue_ko: "입력:   muxa msg list",
        detect: Detect::TypedCommand("muxa msg list"),
    },
    Step {
        achieved_en: "✓ you read claude's reply — and codex has asked you something of its own",
        achieved_ko: "✓ claude의 답장을 봤습니다 — 그리고 codex가 당신에게 물어왔습니다",
        title_en: "`inbox` claims the requests addressed to you. Agents use muxa too.",
        title_ko: "`inbox`는 당신 앞으로 온 요청을 가져옵니다. agent도 muxa를 씁니다.",
        cue_en: "run:   muxa msg inbox",
        cue_ko: "입력:   muxa msg inbox",
        detect: Detect::ClaimedInbox,
    },
    Step {
        achieved_en: "✓ you claimed codex's request — the mailbox is yours to work through",
        achieved_ko: "✓ codex의 요청을 확인했습니다 — mailbox는 당신이 처리하는 곳입니다",
        title_en: "session is a workspace · window is a work · pane is an agent",
        title_ko: "session은 workspace · window는 work · pane은 agent",
        cue_en: "press   Ctrl-b   then   d      ·   finishes and deletes the sandbox",
        cue_ko: "Ctrl-b 를 눌렀다 떼고   d      ·   마치고 sandbox를 지웁니다",
        detect: Detect::NoClient,
    },
];
/// The step that opens once the learner has started claude: the pane they ran
/// it in becomes claude's, and codex lands beside it.
/// One line of the current step, for above the shell prompt.
///
/// The full block is printed once, when the step changes. Running a command
/// scrolls it away, so the prompt carries a short reminder of what was asked —
/// otherwise the learner is left with a bare prompt and the last command's
/// output, and the instruction somewhere above.
fn reminder_line(index: usize, cue: &str) -> String {
    format!(
        "{DIM}▸ {}/{}{RESET}  {YELLOW}{cue}{RESET}\n",
        index + 1,
        STEPS.len()
    )
}

const AGENTS_ARRIVE: usize = 8;

/// The step that asks them to split — one before the one that asks for Enter,
/// which is one before the agents land.
const SPLIT_STEP: usize = AGENTS_ARRIVE - 2;

/// The beats that fire scripted events rather than only reading tmux.
///
/// Named because they were plain integers twice and a step inserted upstream
/// silently moved what they pointed at: the send-on-skip fixup kept firing on
/// the step before the one that sends, so the escape hatch reached an empty
/// inbox and the tour exited. `beats_line_up_with_what_they_fire_on` holds
/// each of these against the detect it belongs to.
const BLOCK_CODEX_STEP: usize = 10;
const SEND_STEP: usize = 12;
const CLAUDE_ANSWERS_STEP: usize = 13;
const CODEX_ASKS_STEP: usize = 14;
const CLAUDE_FINISHES_STEP: usize = 15;

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

/// One-way facts about the run so far.
#[derive(Default)]
struct Seen {
    /// They opened the session tree.
    tree: bool,
    /// They opened `muxa watch`.
    watch: bool,
    /// Their shell has drawn a prompt, so printing must make room for one.
    prompt: bool,
}

struct Tour<'a> {
    sandbox: &'a Sandbox,
    /// Mutable: `F2` switches it mid-tour, the way the simulation's footer did.
    language: UiLanguage,
    fleet: Option<Fleet>,
    /// The tree opens and closes; the step is the whole gesture.
    /// One-way facts about the run so far: the tree was opened, watch was
    /// opened, the shell has drawn a prompt. Separate `bool` fields said the
    /// same thing four times over.
    seen: Seen,
    /// Panes present before the learner split, so the new one is identifiable.
    before_split: Vec<String>,
    /// Prompt draws at the moment the current step opened, so a step can ask
    /// for a bare Enter and know when it arrives.
    prompts_at_entry: usize,

    /// `--no-quiz` does not remove the steps — there is nothing to remove, the
    /// tour is the real thing — so it offers the way past from the start
    /// instead of after a wait.
    no_quiz: bool,
}

impl Tour<'_> {
    fn clients(&self) -> String {
        self.sandbox
            .tmux_quiet(&["list-clients", "-F", "#{client_name}"])
    }

    /// Sessions the learner made, as opposed to the placeholder that kept the
    /// server alive before they had one.
    fn own_sessions(&self) -> Vec<String> {
        let holder = self.sandbox.env_value("MUXA_SANDBOX_HOLDER");
        self.sandbox
            .tmux_quiet(&["list-sessions", "-F", "#{session_name}"])
            .lines()
            .filter(|name| !name.is_empty() && *name != holder)
            .map(str::to_string)
            .collect()
    }

    /// The learner's current window, named explicitly.
    ///
    /// `list-panes` with no target picks whichever window tmux considers
    /// current, and the tour runs outside any client — so after step 2 leaves a
    /// second window behind, a bare `list-panes` can report the window the
    /// learner is *not* in. That is a split that never registers.
    fn session_target(&self) -> String {
        let session = self
            .own_sessions()
            .first()
            .cloned()
            .unwrap_or_else(|| "muxa-onboarding".to_string());
        format!("{session}:")
    }

    fn satisfied(&mut self, detect: &Detect) -> bool {
        match detect {
            Detect::SessionCreated => !self.own_sessions().is_empty(),
            Detect::SecondWindow => {
                let holder = self.sandbox.env_value("MUXA_SANDBOX_HOLDER");
                self.sandbox
                    .tmux_quiet(&["list-windows", "-a", "-F", "#{session_name}"])
                    .lines()
                    .filter(|name| !name.is_empty() && *name != holder)
                    .count()
                    >= 2
            }
            // Opened *and* closed: one gesture, so one step.
            Detect::TreeModeClosed => {
                // Across every pane, not `display-message` on "the current"
                // one: run from outside a client that resolves to whichever
                // pane tmux feels like, which is rarely the one the learner is
                // looking at.
                let in_tree = self
                    .sandbox
                    .tmux_quiet(&["list-panes", "-a", "-F", "#{pane_mode}"])
                    .contains("tree");
                if in_tree {
                    self.seen.tree = true;
                    return false;
                }
                self.seen.tree
            }
            Detect::NoClient => self.clients().is_empty(),
            Detect::TypedCommand(prefix) => self.sandbox.ran_command(prefix),
            Detect::ClientAttached => !self.clients().is_empty(),
            Detect::PaneSplit => {
                self.sandbox
                    .tmux_quiet(&[
                        "list-panes",
                        "-t",
                        &self.session_target(),
                        "-F",
                        "#{pane_id}",
                    ])
                    .lines()
                    .filter(|id| !id.is_empty())
                    .count()
                    >= 2
            }
            Detect::PressedEnter => self.sandbox.prompt_count() > self.prompts_at_entry,
            Detect::LeftWatch => {
                let running = self
                    .sandbox
                    .tmux_quiet(&["list-panes", "-a", "-F", "#{pane_current_command}"])
                    .lines()
                    .any(|line| line.trim() == "muxa");
                if running {
                    self.seen.watch = true;
                    return false;
                }
                self.seen.watch
            }
            Detect::PaneRunning(command) => self
                .sandbox
                .tmux_quiet(&["list-panes", "-a", "-F", "#{pane_current_command}"])
                .lines()
                .any(|line| line.trim() == *command),
            Detect::ActivePaneIsCodex => self.fleet.as_ref().is_some_and(|fleet| {
                self.sandbox
                    .tmux_quiet(&["display-message", "-p", "#{pane_id}"])
                    == fleet.codex
            }),
            Detect::ActivePaneIsLearner => self.fleet.as_ref().is_some_and(|fleet| {
                self.sandbox
                    .tmux_quiet(&["display-message", "-p", "#{pane_id}"])
                    == fleet.learner
            }),
            Detect::SentMessage => self.fleet.as_ref().is_some_and(|fleet| {
                self.sandbox
                    .muxa_as(
                        &fleet.learner,
                        &["msg", "list", "--mailbox", "sent", "--json"],
                    )
                    .is_ok_and(|out| out.contains("\"id\""))
            }),
            // A claimed `--no-reply` request leaves `queued` behind, and that is
            // the only transition this step needs to see.
            Detect::ClaimedInbox => self.fleet.as_ref().is_some_and(|fleet| {
                self.sandbox
                    .muxa_as(
                        &fleet.learner,
                        &["msg", "list", "--mailbox", "incoming", "--json"],
                    )
                    .is_ok_and(|out| out.contains("\"status\"") && !out.contains("\"queued\""))
            }),
        }
    }

    /// What a skipped step would have done to the world.
    ///
    /// Skipping has to leave the tour consistent, not just further along: the
    /// agents cannot move into a pane that was never split.
    fn perform(&self, index: usize) -> Result<()> {
        match index {
            0 => {
                self.sandbox.tmux_command(&[
                    "new-session",
                    "-d",
                    "-s",
                    "muxa-onboarding",
                    "-x",
                    "200",
                    "-y",
                    "50",
                ])?;
            }
            1 => {
                if let Some(session) = self.own_sessions().first() {
                    self.sandbox.tmux_command(&[
                        "new-window",
                        "-d",
                        "-t",
                        &format!("{session}:"),
                    ])?;
                }
            }
            SPLIT_STEP => {
                if let Some(session) = self.own_sessions().first() {
                    self.sandbox
                        .tmux_command(&["split-window", "-t", &format!("{session}:")])?;
                }
            }
            SEND_STEP => {
                // Entering the next step makes scripted claude answer the
                // learner's request. A skipped send still needs to create that
                // request, or the escape hatch exits on an empty inbox instead
                // of leaving the later mailbox steps coherent.
                if let Some(fleet) = self.fleet.as_ref() {
                    let body = tr(self.language, "how far along?", "어디까지 됐나요?");
                    self.sandbox
                        .muxa_as(&fleet.learner, &["msg", "send", "@claude", body])?;
                }
            }
            // The rest are about where the learner is looking, which the tour
            // will not fake, or about state it has already produced.
            _ => {}
        }
        Ok(())
    }

    /// Fired as a step opens, so the world moves on its own rather than only in
    /// response to the learner.
    fn on_enter(&mut self, index: usize) -> Result<()> {
        // Rebased each step, so a step asking for Enter counts only the ones
        // pressed after it opened.
        self.prompts_at_entry = self.sandbox.prompt_count();
        // The placeholder only had to keep the server alive until the learner
        // made a session of their own. Step 5 asks them to run `tmux ls`, and a
        // mysterious `_sandbox` in that output is the tour's own plumbing
        // showing through the lesson.
        if index == 1 {
            let holder = self.sandbox.env_value("MUXA_SANDBOX_HOLDER");
            if !holder.is_empty() {
                self.sandbox
                    .tmux_command(&["kill-session", "-t", &holder])?;
            }
        }
        // Their pane, recorded before the split so the new one can be told
        // apart from it afterwards.
        if index == SPLIT_STEP {
            self.before_split = self
                .sandbox
                .tmux_command(&[
                    "list-panes",
                    "-t",
                    &self.session_target(),
                    "-F",
                    "#{pane_id}",
                ])?
                .lines()
                .map(str::to_string)
                .collect();
        }
        // `claude` has to be on their PATH before the step that asks for it.
        if index == AGENTS_ARRIVE - 1 {
            return self.sandbox.prepare_agents(self.language);
        }
        if index == AGENTS_ARRIVE {
            let session = self
                .own_sessions()
                .first()
                .cloned()
                .unwrap_or_else(|| "muxa-onboarding".to_string());
            self.fleet = Some(self.sandbox.add_agents(
                &session,
                &self.before_split,
                self.language,
            )?);
            return Ok(());
        }
        let Some(fleet) = self.fleet.as_ref() else {
            return Ok(());
        };
        match index {
            BLOCK_CODEX_STEP => {
                // The row in watch flips to `waiting` because of the hook; the
                // pane has to show *why*, or the state is a claim rather than
                // something the learner can see for themselves.
                fleet.codex_screen.append(&approval_block(self.language));
                self.sandbox.label_pane(
                    &fleet.codex,
                    tr(
                        self.language,
                        " @codex  ⏸ blocked · needs approval",
                        " @codex  ⏸ 멈춤 · 승인 필요",
                    ),
                );
                self.sandbox.hook(
                    &fleet.codex,
                    "codex",
                    "permission_request",
                    r#"{"session_id":"onboarding-codex","tool_name":"shell"}"#,
                )
            }
            CLAUDE_ANSWERS_STEP => self.claude_answers(fleet),
            CODEX_ASKS_STEP => self.codex_asks(fleet),
            CLAUDE_FINISHES_STEP => {
                fleet.claude_screen.append(&finished(self.language));
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// claude takes the learner's question and answers it.
    ///
    /// The borders carry it: a message that "reached claude" is otherwise a
    /// claim the narration makes over three anonymous boxes.
    fn claude_answers(&self, fleet: &Fleet) -> Result<()> {
        fleet
            .claude_screen
            .append(&incoming_question(self.language));
        self.sandbox.label_pane(
            &fleet.claude,
            tr(
                self.language,
                " @claude  ← your question   → replied",
                " @claude  ← 당신의 질문   → 답장함",
            ),
        );
        self.sandbox.label_pane(
            &fleet.learner,
            tr(
                self.language,
                " you · your shell  ← claude's reply is in your mailbox",
                " you · 당신의 셸  ← claude의 답장이 mailbox에 있습니다",
            ),
        );
        // And claude answers through muxa, not only on its own screen.
        self.sandbox.reply_as_claude(fleet, self.language)
    }

    /// codex asks the learner for a decision, the other direction.
    fn codex_asks(&self, fleet: &Fleet) -> Result<()> {
        let body = tr(
            self.language,
            "the public-read boundary needs a decision before I continue",
            "public-read 경계는 계속하기 전에 결정이 필요합니다",
        );
        fleet.codex_screen.append(&outgoing_request(self.language));
        self.sandbox.label_pane(
            &fleet.codex,
            tr(
                self.language,
                " @codex  → asked you for a decision",
                " @codex  → 당신에게 결정을 요청함",
            ),
        );
        self.sandbox.label_pane(
            &fleet.learner,
            tr(
                self.language,
                " you · your shell  ← codex is waiting on you",
                " you · 당신의 셸  ← codex가 당신을 기다립니다",
            ),
        );
        self.sandbox
            .muxa_as(&fleet.codex, &["msg", "send", "@you", body, "--no-reply"])
            .map(|_| ())
    }

    /// The step, for the beats where nobody is attached to a status bar.
    ///
    /// Bash has usually drawn its prompt on this line already, and writing
    /// beside it is how a doubled `muxa-onboarding $` and a half-eaten Korean
    /// line reached the learner. Erase that line first, and re-issue the
    /// prompt afterwards — bash will not draw a second one for a line it has
    /// finished. Before the shell exists there is nothing to erase and nothing
    /// to replace: bash draws the first prompt itself.
    fn print_step(&self, index: usize, achieved: &str, title: &str, cue: &str) -> Result<()> {
        let rule = "─".repeat(RULE_WIDTH);
        let done = (index + 1) * BAR_WIDTH / STEPS.len();
        let bar = format!(
            "{GREEN}{}{RESET}{DIM}{}{RESET}",
            "━".repeat(done),
            "━".repeat(BAR_WIDTH - done)
        );
        // Erase whatever bash left on this line first. Writing beside a prompt
        // it had already drawn is where the doubled `muxa-onboarding $` and the
        // half-eaten Korean line came from. Before the shell exists there is
        // nothing to erase, and bash draws the first prompt itself.
        let clear = if self.seen.prompt { "\r\x1b[2K" } else { "" };
        let tail = if self.seen.prompt {
            format!("{}{PROMPT}", reminder_line(index, cue))
        } else {
            String::new()
        };
        print!(
            "{clear}\n{DIM}{rule}{RESET}\n\
             {BOLD}muxa onboarding{RESET}  {DIM}{}/{}{RESET}  {bar}\n\
             {GREEN}{achieved}{RESET}\n\
             {title}\n\
             {YELLOW}{BOLD}{cue}{RESET}\n\
             {DIM}{rule}{RESET}\n{tail}",
            index + 1,
            STEPS.len()
        );
        std::io::stdout()
            .flush()
            .context("printing live-tour narration")?;
        Ok(())
    }

    fn narrate(&self, index: usize, escape: bool) -> Result<()> {
        let step = &STEPS[index];
        let achieved = tr(self.language, step.achieved_en, step.achieved_ko);
        let title = tr(self.language, step.title_en, step.title_ko);
        let cue = tr(self.language, step.cue_en, step.cue_ko);
        let hint = escape.then(|| {
            tr(
                self.language,
                "stuck?  F12  skips this step",
                "막혔나요?  F12  로 이 단계를 건너뜁니다",
            )
        });

        // Their prompt reads this on every redraw, so running a command does
        // not scroll the instruction away — the block above is printed once,
        // when the step changes, and this line comes back with every prompt.
        self.sandbox.write_cue(&reminder_line(index, cue))?;

        self.sandbox.narrate(
            index + 1,
            STEPS.len(),
            achieved,
            title,
            cue,
            hint,
            // Named in the language it switches *to*, so it reads as an offer
            // rather than a label for where you already are.
            match self.language {
                UiLanguage::En => "한국어",
                UiLanguage::Ko => "English",
            },
        )?;

        // The status bar only exists for someone attached to the server, and a
        // few steps are the ones where they are not.
        //
        // Printed rather than left for the prompt to render: bash will not
        // re-draw a prompt it has already put on screen — SIGWINCH redraws the
        // *saved* prompt string rather than re-evaluating `PS1` — so a step
        // change reached the learner only once they pressed a key. Ruled and
        // coloured, because a plain paragraph above a shell prompt reads as the
        // last command's output rather than as the thing to do next; and it
        // ends with the prompt, so whatever they type next has one in front of
        // it.
        if self.clients().is_empty() {
            self.print_step(index, achieved, title, cue)?;
        }
        Ok(())
    }

    fn drive(&mut self) -> Result<usize> {
        // Prints rather than paints: nobody is attached yet.
        self.narrate(0, self.no_quiz)?;

        // The learner's own shell, in their own terminal. They are not attached
        // to anything yet — Act I is where they attach themselves, which is the
        // lesson. The tour polls alongside rather than taking the keyboard.
        let mut shell = Command::new("bash")
            .arg("--rcfile")
            .arg(&self.sandbox.rcfile)
            .spawn()
            .context("starting your shell")?;
        // From here on bash owns the line, so a printed block has to clear it
        // and put the prompt back.
        self.seen.prompt = true;

        let mut index = 0usize;
        let mut entered = Instant::now();
        let mut offered_escape = self.no_quiz;

        while index < STEPS.len() {
            if INTERRUPTED.load(Ordering::SeqCst) {
                break;
            }
            if shell.try_wait().context("watching your shell")?.is_some() {
                break;
            }
            // `exit` inside the last pane is a legitimate way to leave, and the
            // tour should end the way `Ctrl-b d` ends it.
            if !self.sandbox.server_alive() {
                break;
            }
            if entered.elapsed() > STEP_TIMEOUT {
                break;
            }
            if self.satisfied(&STEPS[index].detect) {
                index += 1;
                entered = Instant::now();
                offered_escape = self.no_quiz;
                if index < STEPS.len() {
                    self.on_enter(index)?;
                    self.narrate(index, offered_escape)?;
                }
                continue;
            }
            if self.sandbox.language_toggled() {
                self.language = match self.language {
                    UiLanguage::En => UiLanguage::Ko,
                    UiLanguage::Ko => UiLanguage::En,
                };
                self.narrate(index, offered_escape)?;
                continue;
            }
            if self.sandbox.skip_requested() {
                self.perform(index)?;
                index += 1;
                entered = Instant::now();
                offered_escape = self.no_quiz;
                if index < STEPS.len() {
                    self.on_enter(index)?;
                    self.narrate(index, offered_escape)?;
                }
                continue;
            }
            if !offered_escape && entered.elapsed() > SKIP_AFTER {
                offered_escape = true;
                self.narrate(index, true)?;
            }
            std::thread::sleep(POLL);
        }

        let _ = self.sandbox.tmux_command(&["kill-server"]);
        let _ = shell.kill();
        let _ = shell.wait();
        Ok(index)
    }
}

pub(super) fn run(language: UiLanguage, no_quiz: bool) -> Result<()> {
    preflight(language)?;
    trap_signals();

    eprintln!(
        "{}",
        tr(
            language,
            "Building a throwaway muxa — its own tmux server, its own daemon. Nothing is installed.",
            "일회용 muxa를 만듭니다 — 전용 tmux 서버, 전용 daemon. 설치는 없습니다.",
        )
    );

    // The sandbox is kept alive past `drive` on purpose. Tearing it down first
    // meant the learner watched a blank terminal for the several seconds it
    // takes to stop a daemon and wait for it to actually exit, and only then
    // got the part worth reading. Everything that does not depend on the
    // teardown is printed before it runs; the one line that claims the sandbox
    // is gone waits until it is.
    let (completed, sandbox) = {
        let sandbox = Sandbox::create()?;
        // The daemon comes up before the learner has anything, because a hook
        // sent while it is down is dropped rather than queued.
        sandbox.script_command(&["daemon"])?;
        sandbox.prepare_status()?;
        let mut tour = Tour {
            sandbox: &sandbox,
            language,
            fleet: None,
            seen: Seen::default(),
            before_split: Vec::new(),
            prompts_at_entry: 0,
            no_quiz,
        };
        (tour.drive()?, sandbox)
    };

    outcome_line(language, completed);
    next_steps(language);
    // `Drop` tears the sandbox down here, on every path including a panic.
    drop(sandbox);
    println!();
    println!(
        "  {}",
        tr(
            language,
            "The sandbox is gone — no daemon, no config, nothing left on disk.",
            "sandbox는 사라졌습니다 — daemon도 config도 남지 않았습니다.",
        )
    );
    Ok(())
}

fn preflight(language: UiLanguage) -> Result<()> {
    if std::env::var_os("TMUX").is_some() {
        bail!(tr(
            language,
            "The live tour runs its own tmux server, and nesting it inside yours makes the prefix ambiguous.\nDetach with your prefix + d, or open a terminal outside tmux, then run this again.",
            "라이브 tour는 자체 tmux 서버를 띄웁니다. 지금 tmux 안이라 prefix가 모호해집니다.\nprefix + d로 detach하거나 tmux 밖 터미널에서 다시 실행하세요.",
        ));
    }
    if which("tmux").is_none() {
        bail!(tr(
            language,
            "The live tour needs tmux. Install it, or run `muxa onboard --print` for the written guide.",
            "라이브 tour에는 tmux가 필요합니다. 설치하거나 `muxa onboard --print`로 문서 가이드를 보세요.",
        ));
    }
    Ok(())
}

/// What the learner can do on their own machine, once the sandbox is gone.
const OWN_FLEET: &[(&str, &str, &str)] = &[
    (
        "tmux new-session -s <work>",
        "a session is a workspace",
        "session은 workspace",
    ),
    (
        "muxa watch",
        "the way in — j/k moves, Enter jumps, ? lists the keys",
        "진입점 — j/k 이동, Enter 진입, ? 로 키 목록",
    ),
    (
        "muxa attend",
        "jump to whichever agent has been blocked longest",
        "가장 오래 막힌 agent로 이동",
    ),
    (
        "muxa msg send @<alias> \"…\"",
        "ask an agent without attaching",
        "attach 없이 agent에게 묻기",
    ),
    (
        "muxa msg inbox",
        "claim what was sent to you",
        "당신 앞으로 온 요청 가져오기",
    ),
];

/// The parts of muxa the tour did not have room for.
const WORTH_KNOWING: &[(&str, &str, &str)] = &[
    (
        "muxa peek",
        "every pane in this window at once, with a digit to jump by",
        "이 window의 모든 pane을 한눈에, 숫자로 이동",
    ),
    (
        "muxa stats",
        "prompt history, live agents, and how long the session has run",
        "프롬프트 이력·활성 agent·세션 지속 시간",
    ),
    (
        "muxa timeline",
        "when each agent worked, waited, or failed",
        "각 agent가 언제 일했고 기다렸고 실패했는지",
    ),
    (
        "muxa work up",
        "staff a whole Work at once instead of a pane at a time",
        "pane 하나씩이 아니라 Work 전체를 한 번에 구성",
    ),
    (
        "muxa doctor",
        "check the setup end to end",
        "설치 상태를 처음부터 끝까지 점검",
    ),
];

/// The tour installs nothing, so it has to say how to get a real muxa.
const INSTALLING: &[(&str, &str, &str)] = &[
    (
        "curl -fsSL https://raw.githubusercontent.com/Open330/muxa/main/scripts/install.sh | sh",
        "builds muxa and muxad, then runs `muxa init`",
        "muxa와 muxad를 빌드하고 `muxa init`까지 실행합니다",
    ),
    (
        "muxa init",
        "wire tmux, agent hooks, and the dashboard",
        "tmux·agent hook·대시보드 연결",
    ),
];

/// How the tour ended, said before anything slow happens.
fn outcome_line(language: UiLanguage, completed: usize) {
    println!();
    println!(
        "{BOLD}{}{RESET}",
        if completed >= STEPS.len() {
            tr(
                language,
                "Done — all sixteen steps.",
                "끝났습니다 — 16단계 전부.",
            )
        } else {
            tr(language, "Stopped early.", "중간에 종료했습니다.")
        }
    );
}

/// Everything below is about the learner's own machine. A tour that ends
/// without saying how to keep what you just learned to use is a demo.
fn next_steps(language: UiLanguage) {
    section(
        language,
        "The same commands, on your own fleet",
        "직접 쓰실 때도 같은 명령입니다",
        OWN_FLEET,
    );
    section(
        language,
        "Worth knowing next",
        "다음으로 볼 만한 것",
        WORTH_KNOWING,
    );
    section(
        language,
        "Installing it for real",
        "실제로 설치하기",
        INSTALLING,
    );

    println!();
    for line in [
        tr(
            language,
            "Docs:  https://github.com/Open330/muxa#readme",
            "문서:  https://github.com/Open330/muxa/blob/main/README.ko.md",
        ),
        tr(
            language,
            "Install guide:  https://github.com/Open330/muxa/blob/main/docs/INSTALL.md",
            "설치 안내:  https://github.com/Open330/muxa/blob/main/docs/INSTALL.ko.md",
        ),
    ] {
        println!("  {line}");
    }
    println!();
    println!(
        "  {}",
        tr(
            language,
            "Run this tour again any time:  muxa onboard   ·   the written guide:  muxa onboard --print",
            "이 tour는 언제든 다시:  muxa onboard   ·   문서로 보기:  muxa onboard --print",
        )
    );
}

/// A titled block of `command — what it is for`, aligned on the command.
fn section(
    language: UiLanguage,
    title_en: &'static str,
    title_ko: &'static str,
    rows: &[(&'static str, &'static str, &'static str)],
) {
    println!();
    println!("{BOLD}{}{RESET}", tr(language, title_en, title_ko));
    // Aligned on the commands that fit. One long command — the install
    // one-liner — would otherwise push every gloss beside it off the screen,
    // so it takes a line of its own and its gloss goes underneath.
    let width = rows
        .iter()
        .map(|(cmd, ..)| cmd.chars().count())
        .filter(|len| *len <= ALIGN_LIMIT)
        .max()
        .unwrap_or(0);
    for (command, gloss_en, gloss_ko) in rows {
        let gloss = tr(language, gloss_en, gloss_ko);
        if command.chars().count() > ALIGN_LIMIT {
            println!("  {command}");
            println!("  {:width$}   {DIM}{gloss}{RESET}", "");
        } else {
            println!("  {command:<width$}   {DIM}{gloss}{RESET}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each scripted beat fires on the step whose action it belongs to.
    ///
    /// These were plain integers, and inserting a step upstream moved what
    /// every one of them pointed at without failing anything: the send-on-skip
    /// fixup kept firing one step early, so the escape hatch reached claude's
    /// inbox before anything was in it and the tour exited on the error. Pin
    /// them to their detects, so the next insertion fails here instead.
    #[test]
    fn beats_line_up_with_what_they_fire_on() {
        assert!(matches!(STEPS[SPLIT_STEP].detect, Detect::PaneSplit));
        assert!(matches!(
            STEPS[AGENTS_ARRIVE - 1].detect,
            Detect::PressedEnter
        ));
        assert!(matches!(
            STEPS[AGENTS_ARRIVE].detect,
            Detect::PaneRunning("muxa")
        ));
        assert!(matches!(STEPS[SEND_STEP].detect, Detect::SentMessage));
        assert!(matches!(
            STEPS[CLAUDE_ANSWERS_STEP].detect,
            Detect::TypedCommand("muxa msg list")
        ));
        assert!(matches!(
            STEPS[CODEX_ASKS_STEP].detect,
            Detect::ClaimedInbox
        ));
        assert!(matches!(
            STEPS[BLOCK_CODEX_STEP].detect,
            Detect::ActivePaneIsCodex
        ));
        assert_eq!(CLAUDE_FINISHES_STEP, STEPS.len() - 1);
    }

    /// Every step says what the last action accomplished, in both languages.
    #[test]
    fn every_step_confirms_the_action_before_it() {
        for (index, step) in STEPS.iter().enumerate() {
            assert!(
                !step.achieved_en.is_empty(),
                "step {index} has no English confirmation"
            );
            assert!(
                !step.achieved_ko.is_empty(),
                "step {index} has no Korean confirmation"
            );
            // Status rows render no markdown, so emphasis markers print as
            // themselves.
            for text in [step.title_en, step.title_ko, step.cue_en, step.cue_ko] {
                assert!(
                    !text.contains("**"),
                    "step {index} carries literal asterisks: {text}"
                );
            }
        }
    }
}
