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

const SANDBOX_NAME: &str = "muxa-onboarding";
const POLL: Duration = Duration::from_millis(250);

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
/// SIGKILL cannot be caught. The next `up` clears whatever it leaves behind,
/// and `muxa-sandbox.sh status --name muxa-onboarding` names it meanwhile.
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
    script: PathBuf,
    config: PathBuf,
    rcfile: PathBuf,
    tmux: PathBuf,
    exe: PathBuf,
    env: BTreeMap<String, String>,
}

impl Sandbox {
    fn create() -> Result<Self> {
        let dir = std::env::temp_dir();
        let script = dir.join("muxa-onboarding-sandbox.sh");
        let config = dir.join("muxa-onboarding.src.toml");
        let rcfile = dir.join("muxa-onboarding.bashrc");
        write_executable(&script, SANDBOX_SCRIPT).context("staging the sandbox script")?;
        std::fs::write(&config, SANDBOX_CONFIG).context("staging the sandbox config")?;

        let tmux = which("tmux").context("tmux is required for the live tour")?;
        let exe = std::env::current_exe().context("locating the running muxa binary")?;

        let mut sandbox = Self {
            script,
            config,
            rcfile,
            tmux,
            exe,
            env: BTreeMap::new(),
        };
        sandbox.script_command(&["up"])?;
        sandbox.env = sandbox.read_env()?;
        sandbox.write_rcfile()?;
        sandbox.prepare_shell()?;
        Ok(sandbox)
    }

    fn script_command(&self, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("bash");
        cmd.arg(&self.script).arg(args[0]);
        cmd.args(["--name", SANDBOX_NAME]);
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
    fn write_rcfile(&self) -> Result<()> {
        use std::fmt::Write as _;

        let mut body = String::from("unset PROMPT_COMMAND\nexport PS1='muxa-onboarding $ '\n");
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
        let out = Command::new(&self.tmux)
            .args(["-u", "-L", SANDBOX_NAME])
            .args(args)
            .output()
            .context("running tmux against the sandbox")?;
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
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
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
            .stderr(Stdio::null());
        let mut child = cmd.spawn().context("spawning a hook")?;
        child
            .stdin
            .as_mut()
            .context("hook stdin")?
            .write_all(body.as_bytes())?;
        child.wait().context("waiting for a hook")?;
        Ok(())
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = self.script_command(&["down"]);
        for path in [&self.script, &self.config, &self.rcfile] {
            let _ = std::fs::remove_file(path);
        }
    }
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
        self.tmux_command(&["set", "-g", "status", "3"])?;
        self.tmux_command(&["set", "-g", "status-position", "top"])?;
        self.tmux_command(&["set", "-g", "status-style", "bg=#0b1220,fg=#c9d1d9"])?;
        self.tmux_command(&["set", "-g", "status-interval", "2"])?;
        // Root table, so it needs no prefix and cannot be confused with typing.
        self.tmux_command(&["bind-key", "-n", "F12", "set", "-g", SKIP_OPTION, "1"])?;
        Ok(())
    }

    fn skip_requested(&self) -> bool {
        if self.tmux_quiet(&["show", "-gv", SKIP_OPTION]) != "1" {
            return false;
        }
        let _ = self.tmux_command(&["set", "-gu", SKIP_OPTION]);
        true
    }

    fn narrate(&self, index: usize, total: usize, title: &str, cue: &str, escape: Option<&str>) {
        let banner = format!(
            "#[align=centre bg=#1f6feb,fg=#0b1220,bold] muxa onboarding · {index}/{total} #[default]"
        );
        let title = format!("#[align=centre]{title}");
        let cue = match escape {
            Some(hint) => {
                format!("#[align=centre fg=#d29922,bold]{cue}#[default]#[fg=#8b949e]    {hint}")
            }
            None => format!("#[align=centre fg=#d29922,bold]{cue}"),
        };
        let _ = self.tmux_command(&["set", "-g", "status-format[0]", &banner]);
        let _ = self.tmux_command(&["set", "-g", "status-format[1]", &title]);
        let _ = self.tmux_command(&["set", "-g", "status-format[2]", &cue]);
    }
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
}

impl Sandbox {
    /// Two agents join whatever window the learner is sitting in.
    ///
    /// Joining *their* window rather than a prepared one is the point: the room
    /// they can address peers in is the window they are already in, and
    /// watching the panes arrive is what makes "a pane is an agent" land.
    fn add_agents(&self, session: &str, language: UiLanguage) -> Result<Fleet> {
        let note = tr(
            language,
            "scripted agent — everything muxa does with it is real",
            "스크립트 agent — muxa가 하는 동작은 전부 진짜입니다",
        );

        // Resolved through the session rather than the attached client: a
        // learner who skipped their way here may not be attached to anything,
        // and the agents still have to land in a real window.
        let target = format!("{session}:");
        let learner = self.tmux_command(&["display-message", "-p", "-t", &target, "#{pane_id}"])?;
        let window =
            self.tmux_command(&["display-message", "-p", "-t", &target, "#{window_id}"])?;
        if learner.is_empty() || window.is_empty() {
            bail!("could not find a window to put the agents in");
        }

        let claude = self.split(
            &window,
            &format!(
                "printf '\\033[1;36m▸ claude\\033[0m  {note}\\n\\n  harden the checkout auth path\\n'; exec cat"
            ),
        )?;
        let codex = self.split(
            &window,
            &format!(
                "printf '\\033[1;35m▸ codex\\033[0m  {note}\\n\\n  review the public-read boundary\\n'; exec cat"
            ),
        )?;

        // Zoomed so `muxa watch` gets the whole screen. The agent panes stay in
        // the window, and `muxa attend` unzooms to reach one — which
        // demonstrates attend better than any description of it.
        self.tmux_command(&["select-pane", "-t", &learner])?;
        self.tmux_command(&["resize-pane", "-Z", "-t", &learner])?;

        let fleet = Fleet {
            learner,
            claude,
            codex,
        };
        self.seed_fleet(&fleet)?;
        Ok(fleet)
    }

    fn split(&self, window: &str, command: &str) -> Result<String> {
        let id = self.tmux_command(&[
            "split-window",
            "-d",
            "-P",
            "-F",
            "#{pane_id}",
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
        for (pane, alias) in [
            (&fleet.claude, "claude"),
            (&fleet.codex, "codex"),
            (&fleet.learner, "you"),
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
    /// Nobody is attached — they detached, and the work kept running.
    NoClient,
    /// Somebody is attached again.
    ClientAttached,
    /// Some pane on the sandbox server is running this command.
    PaneRunning(&'static str),
    /// The active pane is the one `muxa attend` should have jumped to.
    ActivePaneIsCodex,
    /// The learner sent a request.
    SentMessage,
    /// The learner claimed what was waiting for them.
    ClaimedInbox,
}

struct Step {
    title_en: &'static str,
    title_ko: &'static str,
    cue_en: &'static str,
    cue_ko: &'static str,
    detect: Detect,
}

/// Nine steps in two acts.
///
/// Act I keeps only the one tmux idea muxa is built on — a session outlives the
/// terminal attached to it. Zoom, copy mode and pane-navigation drills are
/// deliberately absent: they are tmux trivia, and the pane concept lands in Act
/// II when the agents themselves arrive as panes.
const STEPS: &[Step] = &[
    // ---- Act I: tmux ------------------------------------------------------
    Step {
        title_en: "A tmux session is a workspace that keeps running without you.",
        title_ko: "tmux session은 당신 없이도 계속 도는 작업 공간입니다.",
        cue_en: "type:   tmux new-session -s muxa-onboarding",
        cue_ko: "입력:   tmux new-session -s muxa-onboarding",
        detect: Detect::SessionCreated,
    },
    Step {
        title_en: "One window is one Work. Make a second one.",
        title_ko: "window 하나가 Work 하나입니다. 두 번째를 만들어 보세요.",
        cue_en: "press   Ctrl-b   then   c",
        cue_ko: "Ctrl-b 를 눌렀다 떼고   c",
        detect: Detect::SecondWindow,
    },
    Step {
        title_en: "Now leave. Detaching removes you, not the work.",
        title_ko: "이제 나가 보세요. detach는 당신만 빠지고 작업은 그대로입니다.",
        cue_en: "press   Ctrl-b   then   d       ·   then try:   tmux ls",
        cue_ko: "Ctrl-b 를 눌렀다 떼고   d       ·   그다음:   tmux ls",
        detect: Detect::NoClient,
    },
    Step {
        title_en: "Still listed, still running. That is the whole reason muxa lives in tmux.",
        title_ko: "여전히 목록에 있고 여전히 돌고 있습니다. muxa가 tmux에 사는 이유입니다.",
        cue_en: "type:   tmux attach -t muxa-onboarding",
        cue_ko: "입력:   tmux attach -t muxa-onboarding",
        detect: Detect::ClientAttached,
    },
    // ---- Act II: muxa -----------------------------------------------------
    Step {
        title_en: "Two agents just joined this window. A pane is an agent. See them all at once.",
        title_ko:
            "이 window에 agent 둘이 합류했습니다. pane 하나가 agent 하나입니다. 한눈에 보세요.",
        cue_en: "run:   muxa watch             ·   q leaves it",
        cue_ko: "입력:   muxa watch             ·   q로 나갑니다",
        detect: Detect::PaneRunning("muxa"),
    },
    Step {
        title_en: "codex stopped and is waiting on you. State is how you find that.",
        title_ko: "codex가 멈춰서 당신을 기다립니다. 그걸 찾는 방법이 state입니다.",
        cue_en: "leave watch with q, then run:   muxa attend",
        cue_ko: "q로 watch를 나간 뒤 입력:   muxa attend",
        detect: Detect::ActivePaneIsCodex,
    },
    Step {
        // attend left them sitting in codex's pane, which has no shell to
        // type into. Getting back is a real tmux key, so the step teaches it
        // rather than having the tour move the cursor on their behalf.
        title_en: "That is codex's own pane. Go back to yours, and ask from there.",
        title_ko: "여기는 codex의 pane입니다. 당신 pane으로 돌아가서 물어보세요.",
        cue_en: "press   Ctrl-b   then   ;      then run:   muxa msg send @claude \"how far along?\"",
        cue_ko: "Ctrl-b 를 눌렀다 떼고   ;      그 뒤 입력:   muxa msg send @claude \"어디까지 됐나요?\"",
        detect: Detect::SentMessage,
    },
    Step {
        title_en: "Agents use muxa too — codex just sent you a request of its own.",
        title_ko: "agent도 muxa를 씁니다 — codex가 방금 당신에게 요청을 보냈습니다.",
        cue_en: "run:   muxa msg inbox",
        cue_ko: "입력:   muxa msg inbox",
        detect: Detect::ClaimedInbox,
    },
    Step {
        title_en: "session is a workspace · window is a work · pane is an agent",
        title_ko: "session은 workspace · window는 work · pane은 agent",
        cue_en: "press   Ctrl-b   then   d       ·   finishes and deletes the sandbox",
        cue_ko: "Ctrl-b 를 눌렀다 떼고   d       ·   마치고 sandbox를 지웁니다",
        detect: Detect::NoClient,
    },
];

/// The step Act II begins at, and so the point the agents arrive.
const ACT_TWO_START: usize = 4;

// ---------------------------------------------------------------------------
// Run
// ---------------------------------------------------------------------------

struct Tour<'a> {
    sandbox: &'a Sandbox,
    language: UiLanguage,
    fleet: Option<Fleet>,
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

    fn satisfied(&self, detect: &Detect) -> bool {
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
            Detect::NoClient => self.clients().is_empty(),
            Detect::ClientAttached => !self.clients().is_empty(),
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
    /// Skipping has to leave the tour consistent, not just further along: Act II
    /// cannot introduce agents into a session that was never created. Anything
    /// the learner would have done that the tour can do for them, it does.
    fn perform(&self, index: usize) {
        match index {
            0 => {
                let _ = self.sandbox.tmux_command(&[
                    "new-session",
                    "-d",
                    "-s",
                    "muxa-onboarding",
                    "-x",
                    "200",
                    "-y",
                    "50",
                ]);
            }
            1 => {
                if let Some(session) = self.own_sessions().first() {
                    let _ = self.sandbox.tmux_command(&[
                        "new-window",
                        "-d",
                        "-t",
                        &format!("{session}:"),
                    ]);
                }
            }
            // The rest are either about where the learner is looking, which the
            // tour will not fake, or about state it has already produced.
            _ => {}
        }
    }

    /// Fired as a step opens, so the world moves on its own rather than only in
    /// response to the learner.
    fn on_enter(&mut self, index: usize) -> Result<()> {
        if index == ACT_TWO_START {
            // The placeholder has done its job; left alive it shows up in watch
            // as a session with no agents in it.
            let holder = self.sandbox.env_value("MUXA_SANDBOX_HOLDER");
            if !holder.is_empty() {
                let _ = self.sandbox.tmux_command(&["kill-session", "-t", &holder]);
            }
            let session = self
                .own_sessions()
                .first()
                .cloned()
                .unwrap_or_else(|| "muxa-onboarding".to_string());
            self.fleet = Some(self.sandbox.add_agents(&session, self.language)?);
            return Ok(());
        }
        let Some(fleet) = self.fleet.as_ref() else {
            return Ok(());
        };
        match index {
            5 => self.sandbox.hook(
                &fleet.codex,
                "codex",
                "permission_request",
                r#"{"session_id":"onboarding-codex","tool_name":"shell"}"#,
            ),
            7 => {
                let body = tr(
                    self.language,
                    "the public-read boundary needs a decision before I continue",
                    "public-read 경계는 계속하기 전에 결정이 필요합니다",
                );
                self.sandbox
                    .muxa_as(&fleet.codex, &["msg", "send", "@you", body, "--no-reply"])
                    .map(|_| ())
            }
            _ => Ok(()),
        }
    }

    fn narrate(&self, index: usize, escape: bool) {
        let step = &STEPS[index];
        let title = tr(self.language, step.title_en, step.title_ko);
        let cue = tr(self.language, step.cue_en, step.cue_ko);
        let hint = escape.then(|| {
            tr(
                self.language,
                "stuck?  F12  skips this step",
                "막혔나요?  F12  로 이 단계를 건너뜁니다",
            )
        });

        self.sandbox
            .narrate(index + 1, STEPS.len(), title, cue, hint);

        // The status bar only exists for someone attached to the server, and
        // the first step is the one where they are not — they are at a bare
        // shell being asked to create the session. Narrating only through tmux
        // would leave that instruction invisible, which is the same dead end as
        // a gate with no way past it, just earlier.
        if self.clients().is_empty() {
            eprintln!();
            eprintln!("  muxa onboarding · {}/{}", index + 1, STEPS.len());
            eprintln!("  {title}");
            eprintln!("  {cue}");
            eprintln!();
        }
    }

    fn drive(&mut self) -> Result<usize> {
        // Prints rather than paints: nobody is attached yet.
        self.narrate(0, self.no_quiz);

        // The learner's own shell, in their own terminal. They are not attached
        // to anything yet — Act I is where they attach themselves, which is the
        // lesson. The tour polls alongside rather than taking the keyboard.
        let mut shell = Command::new("bash")
            .arg("--rcfile")
            .arg(&self.sandbox.rcfile)
            .spawn()
            .context("starting your shell")?;

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
            if entered.elapsed() > STEP_TIMEOUT {
                break;
            }
            if self.satisfied(&STEPS[index].detect) {
                index += 1;
                entered = Instant::now();
                offered_escape = self.no_quiz;
                if index < STEPS.len() {
                    self.on_enter(index)?;
                    self.narrate(index, offered_escape);
                }
                continue;
            }
            if self.sandbox.skip_requested() {
                self.perform(index);
                index += 1;
                entered = Instant::now();
                offered_escape = self.no_quiz;
                if index < STEPS.len() {
                    self.on_enter(index)?;
                    self.narrate(index, offered_escape);
                }
                continue;
            }
            if !offered_escape && entered.elapsed() > SKIP_AFTER {
                offered_escape = true;
                self.narrate(index, true);
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

    let completed = {
        let sandbox = Sandbox::create()?;
        // The daemon comes up before the learner has anything, because a hook
        // sent while it is down is dropped rather than queued.
        sandbox.script_command(&["daemon"])?;
        sandbox.prepare_status()?;
        let mut tour = Tour {
            sandbox: &sandbox,
            language,
            fleet: None,
            no_quiz,
        };
        tour.drive()?
        // `Drop` tears the sandbox down here, on every path including a panic.
    };

    summary(language, completed);
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

fn summary(language: UiLanguage, completed: usize) {
    println!();
    println!(
        "{}",
        if completed >= STEPS.len() {
            tr(
                language,
                "Done. The sandbox is gone — no daemon, no config, nothing left on disk.",
                "끝났습니다. sandbox는 사라졌습니다 — daemon도 config도 남지 않았습니다.",
            )
        } else {
            tr(
                language,
                "Stopped early. The sandbox is gone — no daemon, no config, nothing left on disk.",
                "중간에 종료했습니다. sandbox는 사라졌습니다 — daemon도 config도 남지 않았습니다.",
            )
        }
    );
    println!();
    println!(
        "{}",
        tr(
            language,
            "The same commands work on your own fleet:",
            "직접 쓰실 때도 같은 명령입니다:",
        )
    );
    for line in [
        "  tmux new-session -s <work>",
        "  muxa watch",
        "  muxa attend",
        "  muxa msg send @<alias> \"…\"",
        "  muxa msg inbox",
    ] {
        println!("{line}");
    }
}
