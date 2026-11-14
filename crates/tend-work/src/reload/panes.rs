//! Bringing each pane back, several at once.
//!
//! Once the layout and tend's marks stand, every pane goes through the same
//! steps on its own: wait for its shell, `cd` to its directory, type its
//! command and see the program take the pane ([`launch_pane`]); then watch
//! the agent come up — walking Claude past its folder-trust prompt, seeing
//! any other agent reach its prompt, a startup dialog or a startup error —
//! and tell a mid-turn agent to carry on ([`watch_pane`]).
//!
//! A pane's steps run in order; panes run side by side. At most `jobs` panes
//! are being launched at a time: that is the burst of process start-up the
//! limit keeps a 50-pane restore from firing all at once. Watching a
//! launched agent afterwards is waiting, not work, so it happens on a thread
//! of its own and the slot goes to the next pane. A workspace then takes
//! about as long as its slowest pane instead of the sum of every wait.
//!
//! Each pane ends in one result, reported as it lands: restored, waiting on
//! a startup dialog (nothing more is typed into it), or failed / left for a
//! person, with the reason.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tend::agent_tui;
use tend::BackendEndpoint;

use super::{
    agent, foreground, launch, mux_control, plan, relaunch_command, resume_hint, runs_claude,
    saved_dirs, wait_for_shell, Apply, PaneShape, RestoreReport, Snapshot, CONTINUE_READY_QUIET,
    CONTINUE_READY_TIMEOUT, CONTINUE_SUBMIT_GRACE,
};

/// How many panes a restore launches at once unless told otherwise: one per
/// CPU core, at most 8. A slot is held from the pane's shell coming up until
/// its program holds the pane, so this bounds how many agents are being
/// exec'd and loaded at the same instant. Measured on a 10-core Mac with 49
/// panes: 8 restores in 8.7s, 16 in 8.5s and 64 in 7.2s, while 47 Claude
/// processes started together each took about three times as long to draw
/// as one alone. 8 keeps most of the gain without the spike.
pub(super) fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map_or(4, std::num::NonZeroUsize::get)
        .clamp(2, 8)
}

/// How long a relaunched agent other than Claude gets to show its own
/// prompt, a startup dialog or an error before it is taken as up.
const AGENT_READY_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a dialog must stay on screen to count as the agent waiting on it,
/// not as a frame of its startup.
const DIALOG_HOLD: Duration = Duration::from_millis(1500);

const POLL: Duration = Duration::from_millis(250);

/// Run `work` on every item, at most `jobs` at a time, each worker under the
/// calling thread's [`crate::subprocess`] scope. No item is started once the
/// scope is abandoned; items already running stop at their next check.
pub(super) fn for_each_bounded<'a, T: Sync>(
    items: &'a [T],
    jobs: usize,
    work: impl Fn(&'a T) + Sync,
) {
    let scope = crate::subprocess::current_scope();
    let next = AtomicUsize::new(0);
    let workers = jobs.clamp(1, items.len().max(1));
    std::thread::scope(|threads| {
        for _ in 0..workers {
            let scope = scope.clone();
            let (next, work) = (&next, &work);
            threads.spawn(move || {
                crate::subprocess::within(scope, || loop {
                    if crate::subprocess::abandoned() {
                        break;
                    }
                    let Some(item) = items.get(next.fetch_add(1, Ordering::SeqCst)) else {
                        break;
                    };
                    work(item);
                });
            });
        }
    });
}

/// Where one pane ended up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Ending {
    /// A shell in its directory, as recorded.
    Shell,
    /// Its program runs; `continued` when it was also told to carry on.
    Relaunched {
        continued: bool,
    },
    /// The command went out, but the pane stayed at its shell.
    Unconfirmed,
    /// Left for a person, with why.
    Left(String),
    /// Its agent stopped on a startup dialog or an unrecognized screen.
    Waiting(String),
    Failed(String),
    /// The restore was abandoned before this pane finished.
    Abandoned,
}

/// What a relaunched agent's screen settled on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum AgentScreen {
    /// Its own prompt.
    Ready,
    /// A dialog that held for [`DIALOG_HOLD`], with its question.
    Dialog(String),
    /// A startup error that means it cannot take input.
    Blocked(&'static str),
    /// None of these before the deadline.
    Unseen,
}

/// The decision behind [`wait_for_agent`], fed one capture at a time.
#[derive(Debug, Default)]
pub(super) struct AgentWatch {
    dialog_since: Option<(Duration, String)>,
}

impl AgentWatch {
    pub fn observe(&mut self, elapsed: Duration, screen: &str) -> Option<AgentScreen> {
        if let Some(reason) = agent_tui::blocked_screen(screen) {
            return Some(AgentScreen::Blocked(reason));
        }
        if let Some(question) = startup_dialog(screen) {
            let (since, _) = self
                .dialog_since
                .get_or_insert_with(|| (elapsed, question.clone()));
            if elapsed.saturating_sub(*since) >= DIALOG_HOLD {
                return Some(AgentScreen::Dialog(question));
            }
            return None;
        }
        self.dialog_since = None;
        agent_tui::agent_prompt_visible(screen).then_some(AgentScreen::Ready)
    }
}

/// The dialog `screen` ends in, named by what it shows, if any.
pub(super) fn startup_dialog(screen: &str) -> Option<String> {
    agent_tui::pending_dialog(screen, agent_tui::STARTUP_DIALOGS, &[])
        // Named by what it shows (its label says how a swap answers it).
        .map(|dialog| {
            dialog
                .needles
                .first()
                .copied()
                .unwrap_or(dialog.label)
                .to_owned()
        })
        .or_else(|| {
            agent_tui::dialog_is_active(screen)
                .then(|| agent_tui::open_question(screen))
                .flatten()
        })
}

/// A startup dialog, or anything that might be a trust prompt.
fn held_screen(screen: &str) -> Option<String> {
    startup_dialog(screen)
        .or_else(|| launch::may_be_trust_prompt(screen).then(|| "a trust prompt".to_owned()))
}

/// Waiting for a resumed agent's screen to hold still before the follow-up,
/// fed one capture at a time. A dialog can appear after the agent's prompt
/// first showed (a late update menu, a trust prompt), so every capture is
/// checked again: a dialog never counts as settled, and one that holds for
/// [`DIALOG_HOLD`] ends the pane as waiting on it. A Claude pane must also
/// show its own screen (banner or composer footer) first.
#[derive(Debug)]
pub(super) struct SettleWatch {
    claude: bool,
    last: String,
    quiet_since: Duration,
    dialog_since: Option<Duration>,
}

/// What [`SettleWatch`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Settle {
    /// Still and showing the agent: the follow-up can go in.
    Settled,
    /// Nothing may be typed: the pane ends this way.
    Stop(Ending),
}

impl SettleWatch {
    pub fn new(claude: bool) -> Self {
        Self {
            claude,
            last: String::new(),
            quiet_since: Duration::ZERO,
            dialog_since: None,
        }
    }

    pub fn observe(&mut self, elapsed: Duration, screen: &str) -> Option<Settle> {
        if let Some(reason) = agent_tui::blocked_screen(screen) {
            return Some(Settle::Stop(Ending::Failed(reason.to_owned())));
        }
        if let Some(question) = held_screen(screen) {
            let since = *self.dialog_since.get_or_insert(elapsed);
            self.last.clear();
            if elapsed.saturating_sub(since) >= DIALOG_HOLD {
                return Some(Settle::Stop(Ending::Waiting(waiting_note(&question))));
            }
            return None;
        }
        self.dialog_since = None;
        if screen != self.last {
            screen.clone_into(&mut self.last);
            self.quiet_since = elapsed;
            return None;
        }
        let quiet = elapsed.saturating_sub(self.quiet_since) >= CONTINUE_READY_QUIET;
        (!self.last.trim().is_empty()
            && quiet
            && (!self.claude || launch::claude_started(&self.last)))
        .then_some(Settle::Settled)
    }
}

/// Whether the follow-up typed into a pane may be submitted, from one last
/// capture of it: not when it cannot be read, shows a startup error, or
/// shows a dialog. Fails closed; the text then stays unsent in the input.
pub(super) fn may_submit(screen: Result<String, String>) -> Result<(), Ending> {
    let screen = screen.map_err(|error| {
        Ending::Failed(format!(
            "was mid-turn; its screen could not be read before Enter ({error}), so the \
             follow-up was typed but not submitted"
        ))
    })?;
    if let Some(reason) = agent_tui::blocked_screen(&screen) {
        return Err(Ending::Failed(reason.to_owned()));
    }
    if let Some(question) = held_screen(&screen) {
        return Err(Ending::Waiting(waiting_note(&question)));
    }
    Ok(())
}

/// The note for a pane held at a dialog.
pub(super) fn waiting_note(question: &str) -> String {
    let question = question.trim();
    let question: String = question.chars().take(120).collect();
    format!("waiting on a startup dialog ({question}); answer it in the pane")
}

/// Bring back every pane of `snapshot`, `how.jobs` at a time, and record
/// each one's result in `report` as it lands.
pub(super) fn restore_panes(
    endpoint: &BackendEndpoint,
    snapshot: &Snapshot,
    how: &Apply<'_>,
    report: &mut RestoreReport,
) {
    let total = snapshot.panes.len();
    let shared = Mutex::new(std::mem::take(report));
    let finished = Mutex::new(BTreeMap::new());
    let finish = |target: String, started: Instant, ending: Ending| {
        if ending == Ending::Abandoned {
            return;
        }
        let seconds = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
        let mut report = shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        record(&mut report, &target, &ending, seconds);
        report.tell("panes", total);
        drop(report);
        finished
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(target, ending);
    };
    let scope = crate::subprocess::current_scope();
    std::thread::scope(|watchers| {
        // `jobs` panes at a time are got as far as their program starting —
        // the burst of process start-up the limit is for. Watching an agent
        // come up after that is waiting, not work, so each launched pane is
        // watched on its own thread and its slot goes to the next pane.
        for_each_bounded(&snapshot.panes, how.jobs, |pane| {
            let started = Instant::now();
            let target = super::pane_target(pane);
            match launch_pane(endpoint, pane, how) {
                Err(ending) => finish(target, started, ending),
                Ok(launched) => {
                    let (scope, shared, finish) = (scope.clone(), &shared, &finish);
                    watchers.spawn(move || {
                        crate::subprocess::within(scope, || {
                            let ending =
                                watch_pane(endpoint, pane, &target, &launched, how, shared);
                            finish(target, started, ending);
                        });
                    });
                }
            }
        });
    });
    *report = shared
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let finished = finished
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // A pane the abandoned restore never got to is not one it relaunched.
    for pane in &snapshot.panes {
        let target = super::pane_target(pane);
        if !finished.contains_key(&target) {
            report.pane_failed(
                target,
                "not attempted: the restore was stopped first".to_owned(),
            );
        }
    }
    if how.layout_only {
        return;
    }
    let relaunched = finished
        .values()
        .filter(|ending| matches!(ending, Ending::Relaunched { .. } | Ending::Waiting(_)))
        .count();
    report.say(&plan::relaunched_line(relaunched));
    let continued = finished
        .values()
        .filter(|ending| matches!(ending, Ending::Relaunched { continued: true }))
        .count();
    if continued > 0 {
        if let Some(text) = how.follow_up {
            report.say(&format!(
                "sent {text:?} to {continued} agent(s) the reload interrupted mid-turn"
            ));
        }
    }
}

/// Put one pane's ending into the report, printing its line.
fn record(report: &mut RestoreReport, target: &str, ending: &Ending, seconds: f64) {
    report.pane_seconds.insert(target.to_owned(), seconds);
    match ending {
        Ending::Abandoned => {}
        Ending::Shell => report.say(&format!("  {target}: shell ready ({seconds:.1}s)")),
        Ending::Relaunched { .. } => report.say(&format!("  {target}: restored ({seconds:.1}s)")),
        Ending::Unconfirmed => {
            report.say(&plan::manual_line(target, launch::UNCONFIRMED_NOTE));
            report.pane_unconfirmed(target.to_owned(), launch::UNCONFIRMED_NOTE.to_owned());
        }
        Ending::Left(note) => {
            report.say(&plan::manual_line(target, note));
            report.pane_left(target.to_owned(), note.clone());
        }
        Ending::Waiting(note) => {
            report.say(&plan::manual_line(target, note));
            report.pane_waiting_dialog(target.to_owned(), note.clone());
        }
        Ending::Failed(error) => {
            report.say(&plan::manual_line(target, error));
            report.pane_failed(target.to_owned(), error.clone());
        }
    }
}

fn say(report: &Mutex<RestoreReport>, line: &str) {
    report
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .say(line);
}

/// A pane whose program was seen starting.
pub(super) struct Launched {
    command: String,
}

/// One pane up to its program starting: its directory, its command, and the
/// program taking the pane. `Err` with the pane's ending when it stops
/// before that (a shell, a pane left alone, a failure).
fn launch_pane(
    endpoint: &BackendEndpoint,
    pane: &PaneShape,
    how: &Apply<'_>,
) -> Result<Launched, Ending> {
    let target = super::pane_target(pane);
    // `select-layout` renumbered the panes by geometry, so the directory is
    // set per final index here rather than inherited from `split-window -c`.
    let shell = wait_for_shell(endpoint, &target);
    if crate::subprocess::abandoned() {
        return Err(Ending::Abandoned);
    }
    if shell {
        let cd = tend::shell::Dialect::native().cd(&pane.path);
        if let Err(error) = mux_control::run(endpoint, &["send-keys", "-t", &target, &cd, "Enter"])
        {
            return Err(Ending::Failed(error));
        }
    }
    if how.layout_only {
        return Err(Ending::Shell);
    }
    let Some(command) = relaunch_command(pane) else {
        return Err(resume_hint(pane).map_or(Ending::Shell, Ending::Left));
    };
    if !pane.replayable {
        return Err(Ending::Left(
            "argv was a process title, not a command — start it by hand".to_owned(),
        ));
    }
    // Something already running there gets nothing typed into it; a shell
    // that just ran the `cd` is waited for again so the command is not typed
    // into the middle of it.
    if !shell || !wait_for_shell(endpoint, &target) {
        if crate::subprocess::abandoned() {
            return Err(Ending::Abandoned);
        }
        return Err(Ending::Left(
            "already running something; left alone".to_owned(),
        ));
    }
    if let Err(error) = mux_control::run(endpoint, &["send-keys", "-t", &target, &command, "Enter"])
    {
        return Err(Ending::Failed(error));
    }
    // Keys sent are not a program started: only a pane whose foreground
    // moved off its shell counts as relaunched.
    if !confirm_launch(endpoint, &target) {
        if crate::subprocess::abandoned() {
            return Err(Ending::Abandoned);
        }
        return Err(Ending::Unconfirmed);
    }
    Ok(Launched { command })
}

/// A launched pane until its agent is up: Claude walked past its folder-trust
/// prompt, any other agent watched for its prompt, a startup dialog or a
/// startup error; then, for one caught mid-turn, the follow-up.
fn watch_pane(
    endpoint: &BackendEndpoint,
    pane: &PaneShape,
    target: &str,
    launched: &Launched,
    how: &Apply<'_>,
    report: &Mutex<RestoreReport>,
) -> Ending {
    let target = target.to_owned();
    let command = &launched.command;
    let claude = runs_claude(pane, command);
    let up = if claude {
        pass_trust_prompt(endpoint, &target, &pane.path, report)
    } else if is_agent(pane, command) {
        match wait_for_agent(endpoint, &target) {
            AgentScreen::Ready | AgentScreen::Unseen => Ok(()),
            AgentScreen::Dialog(question) => Err(Ending::Waiting(waiting_note(&question))),
            AgentScreen::Blocked(reason) => Err(Ending::Failed(reason.to_owned())),
        }
    } else {
        Ok(())
    };
    if let Err(ending) = up {
        return ending;
    }
    if crate::subprocess::abandoned() {
        return Ending::Abandoned;
    }
    let continued = match how.follow_up {
        Some(text) if interrupted(pane) => {
            match continue_pane(endpoint, pane, &target, text, report) {
                Ok(continued) => continued,
                Err(ending) => return ending,
            }
        }
        _ => false,
    };
    Ending::Relaunched { continued }
}

/// Whether the snapshot caught this pane's agent mid-turn, on a conversation
/// that comes back: a fresh agent told "continue" acts on nothing it knows.
pub(super) fn interrupted(pane: &PaneShape) -> bool {
    pane.agent
        .as_ref()
        .is_some_and(|agent| agent.working && agent::resumable(&agent.session_id))
}

/// Whether the pane runs an agent tend can read the screen of: one it
/// tracked there, or a command line that starts Codex.
fn is_agent(pane: &PaneShape, command: &str) -> bool {
    pane.agent.is_some()
        || command
            .split_whitespace()
            .any(|word| word.rsplit('/').next() == Some("codex") || word.ends_with("@codex"))
}

/// Watch `target` until its program holds the pane (two polls in a row on
/// the same non-shell command), up to [`launch::CONFIRM_TIMEOUT`].
pub(super) fn confirm_launch(endpoint: &BackendEndpoint, target: &str) -> bool {
    let started = Instant::now();
    let mut watch = launch::LaunchWatch::new([target.to_owned()]);
    while started.elapsed() < launch::CONFIRM_TIMEOUT && !crate::subprocess::abandoned() {
        watch.observe(target, foreground(endpoint, target).as_deref());
        if watch.is_settled() {
            return true;
        }
        std::thread::sleep(POLL);
    }
    false
}

/// Walk a relaunched Claude past its folder-trust prompt, but only for the
/// directory the pane was saved in. `Err` with the pane's ending when it is
/// left on a prompt or never shows Claude's own screen.
fn pass_trust_prompt(
    endpoint: &BackendEndpoint,
    target: &str,
    path: &str,
    report: &Mutex<RestoreReport>,
) -> Result<(), Ending> {
    let mut watch = launch::TrustWatch::new([(target.to_owned(), saved_dirs(path))]);
    let started = Instant::now();
    while !watch.is_settled()
        && started.elapsed() < launch::TRUST_TIMEOUT
        && !crate::subprocess::abandoned()
    {
        if let Ok(screen) = mux_control::capture(endpoint, &["capture-pane", "-p", "-t", target]) {
            if let Some(reason) = agent_tui::blocked_screen(&screen) {
                return Err(Ending::Failed(reason.to_owned()));
            }
            if let Some(key) = watch.observe(target, started.elapsed(), &screen) {
                let name = match key {
                    launch::TrustKey::Down => "Down",
                    launch::TrustKey::Enter => "Enter",
                };
                match mux_control::run(endpoint, &["send-keys", "-t", target, name]) {
                    Ok(()) if key == launch::TrustKey::Enter => say(
                        report,
                        &format!(
                            "  {target}: answered Claude's folder-trust prompt for the folder it \
                             was saved in"
                        ),
                    ),
                    Ok(()) => {}
                    Err(_) => watch.leave(target, launch::TRUST_UNRESOLVED_NOTE),
                }
            }
        }
        if !watch.is_settled() {
            std::thread::sleep(POLL);
        }
    }
    if crate::subprocess::abandoned() {
        return Err(Ending::Abandoned);
    }
    // A pane left on the prompt is not up: nothing more may be typed into
    // it, the follow-up included.
    match watch.unresolved().remove(target) {
        Some(note) => Err(Ending::Waiting(note.to_owned())),
        None => Ok(()),
    }
}

/// Watch a relaunched agent's screen until it shows its prompt, a startup
/// dialog or a startup error.
fn wait_for_agent(endpoint: &BackendEndpoint, target: &str) -> AgentScreen {
    let started = Instant::now();
    let mut watch = AgentWatch::default();
    while started.elapsed() < AGENT_READY_TIMEOUT && !crate::subprocess::abandoned() {
        if let Ok(screen) = mux_control::capture(endpoint, &["capture-pane", "-p", "-t", target]) {
            if let Some(seen) = watch.observe(started.elapsed(), &screen) {
                return seen;
            }
        }
        std::thread::sleep(POLL);
    }
    AgentScreen::Unseen
}

/// Tell an agent the snapshot caught mid-turn to carry on, once its resumed
/// screen has stopped redrawing. `Ok` with whether the text went out; `Err`
/// with the pane's ending when a dialog or a startup error took the screen,
/// in which case nothing, or no Enter, was typed.
pub(super) fn continue_pane(
    endpoint: &BackendEndpoint,
    pane: &PaneShape,
    target: &str,
    text: &str,
    report: &Mutex<RestoreReport>,
) -> Result<bool, Ending> {
    let claude = pane
        .agent
        .as_ref()
        .is_some_and(|agent| agent.kind == "claude_code");
    let capture = || mux_control::capture(endpoint, &["capture-pane", "-p", "-t", target]);
    let started = Instant::now();
    let mut watch = SettleWatch::new(claude);
    let mut settled = false;
    while started.elapsed() < CONTINUE_READY_TIMEOUT && !crate::subprocess::abandoned() {
        match watch.observe(started.elapsed(), &capture().unwrap_or_default()) {
            Some(Settle::Settled) => {
                settled = true;
                break;
            }
            Some(Settle::Stop(ending)) => return Err(ending),
            None => std::thread::sleep(POLL),
        }
    }
    if crate::subprocess::abandoned() {
        return Err(Ending::Abandoned);
    }
    if !settled {
        say(
            report,
            &plan::manual_line(
                target,
                "was mid-turn; resumed screen never settled, so nothing was sent",
            ),
        );
        return Ok(false);
    }
    let failed = |error: String| {
        say(
            report,
            &plan::manual_line(
                target,
                &format!("was mid-turn; could not send {text:?}: {error}"),
            ),
        );
        Ok(false)
    };
    if let Err(error) = mux_control::run(endpoint, &["send-keys", "-t", target, "-l", "--", text]) {
        return failed(error);
    }
    std::thread::sleep(CONTINUE_SUBMIT_GRACE);
    // Enter answers whatever holds the screen now: it is read once more,
    // and only a screen read and still the agent's gets it.
    may_submit(capture())?;
    match mux_control::run(endpoint, &["send-keys", "-t", target, "Enter"]) {
        Ok(()) => Ok(true),
        Err(error) => failed(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn work_runs_at_most_jobs_at_a_time_and_covers_every_item() {
        let items: Vec<usize> = (0..40).collect();
        let running = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let seen = Mutex::new(Vec::new());
        for_each_bounded(&items, 6, |item| {
            let now = running.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(5));
            seen.lock().unwrap().push(*item);
            running.fetch_sub(1, Ordering::SeqCst);
        });
        let mut seen = seen.into_inner().unwrap();
        seen.sort_unstable();
        assert_eq!(seen, items);
        assert!(peak.load(Ordering::SeqCst) <= 6);
        assert!(peak.load(Ordering::SeqCst) > 1, "ran one at a time");
    }

    #[test]
    fn an_abandoned_scope_starts_no_more_items_and_workers_inherit_it() {
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let scope = crate::subprocess::Scope {
            socket: "/nonexistent".into(),
            deadline: Instant::now() + Duration::from_secs(60),
            cancel: Some(cancel.clone()),
        };
        let items: Vec<usize> = (0..50).collect();
        let done = AtomicUsize::new(0);
        crate::subprocess::with_scope(scope, || {
            for_each_bounded(&items, 4, |_| {
                // Each worker sees the caller's scope.
                assert!(crate::subprocess::current_scope().is_some());
                if done.fetch_add(1, Ordering::SeqCst) == 5 {
                    cancel.store(true, Ordering::SeqCst);
                }
            });
        });
        assert!(done.load(Ordering::SeqCst) < 10, "{done:?}");
    }

    #[test]
    fn the_default_limit_stays_between_two_and_eight() {
        assert!((2..=8).contains(&default_jobs()));
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn an_agent_is_up_once_its_prompt_shows() {
        let mut watch = AgentWatch::default();
        assert_eq!(watch.observe(ms(0), "loading…"), None);
        assert_eq!(
            watch.observe(ms(250), ">_ OpenAI Codex\n\n› \n  ? for shortcuts"),
            Some(AgentScreen::Ready)
        );
    }

    #[test]
    fn a_dialog_that_holds_is_waited_on_and_one_that_passes_is_not() {
        // Codex's update prompt as 0.164.0 drew it.
        let dialog = "  Update available · 0.164.0 → 0.166.0\n› 1. Update now (runs `npm i`)\n  \
            2. Skip\n  3. Skip until next version\n  enter continue · esc skip\n";
        let mut watch = AgentWatch::default();
        assert_eq!(watch.observe(ms(0), dialog), None);
        assert_eq!(watch.observe(ms(500), dialog), None);
        assert_eq!(
            watch.observe(ms(1600), dialog),
            Some(AgentScreen::Dialog("Update available".to_owned()))
        );
        // A dialog drawn for a moment during startup, then the prompt.
        let mut watch = AgentWatch::default();
        assert_eq!(watch.observe(ms(0), dialog), None);
        assert_eq!(watch.observe(ms(300), "loading"), None);
        assert_eq!(watch.observe(ms(1700), dialog), None);
        assert_eq!(
            watch.observe(ms(2000), "› \n  ? for shortcuts"),
            Some(AgentScreen::Ready)
        );
    }

    const CODEX_UPDATE: &str =
        "  Update available · 0.164.0 → 0.166.0\n› 1. Update now (runs `npm i`)\n  \
        2. Skip\n  3. Skip until next version\n  enter continue · esc skip\n";

    #[test]
    fn a_dialog_drawn_after_the_prompt_stops_the_follow_up() {
        let prompt = "› Ask Codex to do anything\n  ? for shortcuts";
        // The agent watch lets the pane go at its first prompt…
        assert_eq!(
            AgentWatch::default().observe(ms(0), prompt),
            Some(AgentScreen::Ready)
        );
        // …and the follow-up's own watch then sees the update menu replace
        // it: never settled, and once it holds, the pane waits on it.
        let mut settle = SettleWatch::new(false);
        assert_eq!(settle.observe(ms(0), prompt), None);
        assert_eq!(settle.observe(ms(250), CODEX_UPDATE), None);
        assert_eq!(settle.observe(ms(1000), CODEX_UPDATE), None);
        assert_eq!(settle.observe(ms(1600), CODEX_UPDATE), None);
        assert_eq!(
            settle.observe(ms(1800), CODEX_UPDATE),
            Some(Settle::Stop(Ending::Waiting(waiting_note(
                "Update available"
            ))))
        );
        // A quiet prompt settles; a Claude pane also needs Claude's screen.
        let mut settle = SettleWatch::new(false);
        assert_eq!(settle.observe(ms(0), prompt), None);
        assert_eq!(settle.observe(ms(1600), prompt), Some(Settle::Settled));
        let mut claude = SettleWatch::new(true);
        assert_eq!(claude.observe(ms(0), "> \n"), None);
        assert_eq!(claude.observe(ms(2000), "> \n"), None);
        // A trust prompt is never a settled screen.
        let trust = "Do you trust the files in this folder?\n❯ 1. Yes, proceed\n  2. No, exit\nEnter to confirm";
        let mut settle = SettleWatch::new(true);
        assert_eq!(settle.observe(ms(0), trust), None);
        assert!(matches!(
            settle.observe(ms(2000), trust),
            Some(Settle::Stop(Ending::Waiting(_)))
        ));
    }

    #[test]
    fn enter_goes_only_to_a_screen_read_and_still_the_agents() {
        assert_eq!(
            may_submit(Ok("› continue\n  ? for shortcuts".into())),
            Ok(())
        );
        assert!(matches!(
            may_submit(Err("server busy".into())),
            Err(Ending::Failed(note)) if note.contains("not submitted")
        ));
        assert_eq!(
            may_submit(Ok("Error: No conversation found with id abc\n$ ".into())),
            Err(Ending::Failed(
                "the agent could not find the conversation to resume".into()
            ))
        );
        assert_eq!(
            may_submit(Ok(CODEX_UPDATE.into())),
            Err(Ending::Waiting(waiting_note("Update available")))
        );
    }

    #[test]
    fn a_startup_error_fails_the_pane() {
        let mut watch = AgentWatch::default();
        assert_eq!(
            watch.observe(
                ms(0),
                "$ codex resume abc\nError: No conversation found with id abc\n$ "
            ),
            Some(AgentScreen::Blocked(
                "the agent could not find the conversation to resume"
            ))
        );
    }

    #[test]
    fn a_waiting_note_names_the_question_briefly() {
        let note = waiting_note(&"x".repeat(500));
        assert!(note.starts_with("waiting on a startup dialog (xxx"));
        assert!(note.len() < 200);
    }
}
