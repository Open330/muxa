//! What a restore will do, per session, window and pane — and afterwards what
//! it did. The same document is printed for a person (`muxa restore`) and as
//! JSON for Muxa.app (`muxa restore --json`), so the app never has to work out
//! resume commands or skip decisions on its own.

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

use super::{relaunch_command, resume_hint, PaneShape, Snapshot, SnapshotOrigin};

/// What happens to one recorded session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SessionAction {
    /// Missing from the server; it is created.
    Create,
    /// Already on the server and `--only-missing` leaves it alone.
    Skip,
    /// Already on the server and, without `--only-missing`, gets the windows
    /// and panes it lacks; what it has is kept, but its idle shells are
    /// still given their `cd` and recorded command.
    FillMissing,
}

/// What a restored pane is given to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PaneAction {
    /// A tracked agent, relaunched on its own conversation.
    Resume,
    /// Its own command line, typed back in.
    Replay,
    /// Left as an empty shell.
    Shell,
    /// The argv was a process title; a person has to start it.
    Manual,
    /// A tracked agent whose command line was never captured: its
    /// conversation can be resumed, but only by hand (`note` says how).
    ResumeByHand,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SessionResult {
    Created,
    Skipped,
    /// An existing session that got what it was missing.
    Filled,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum PaneResult {
    Relaunched,
    Shell,
    Manual,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct SessionPlan {
    pub name: String,
    pub action: SessionAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<SessionResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub windows: Vec<WindowPlan>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct WindowPlan {
    pub index: String,
    pub name: String,
    pub layout: String,
    pub panes: Vec<PanePlan>,
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct PanePlan {
    pub index: String,
    pub path: String,
    pub action: PaneAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_kind: Option<String>,
    /// What a person has to do, for a pane muxa cannot relaunch itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<PaneResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// `=session:window.pane`, the key the report records failures under.
    #[serde(skip)]
    pub target: String,
}

/// Counts shown beside a snapshot in a listing and at the top of a plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct Summary {
    pub taken_at: String,
    pub origin: SnapshotOrigin,
    pub host: String,
    pub socket: String,
    pub sessions: usize,
    pub windows: usize,
    pub panes: usize,
    /// Panes muxa had tracked an agent in.
    pub agents: usize,
    /// Panes that come back on their own agent conversation.
    pub resumable: usize,
    /// Panes whose argv was a process title rather than a command.
    pub not_replayable: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub(super) struct Totals {
    pub sessions_created: usize,
    pub sessions_skipped: usize,
    pub sessions_failed: usize,
    pub panes: usize,
    pub relaunched: usize,
    pub shell: usize,
    pub manual: usize,
    pub failed: usize,
}

/// The whole `muxa restore --json` document: the plan, and with `--run` the
/// per-item results and totals.
#[derive(Debug, Serialize)]
#[allow(clippy::struct_excessive_bools)] // a wire document of independent flags
pub(super) struct RestoreDocument {
    pub id: String,
    pub dir: String,
    pub summary: Summary,
    pub socket: String,
    pub only_missing: bool,
    pub layout_only: bool,
    /// False when the server did not answer, in which case every session is
    /// planned as missing.
    pub server_reachable: bool,
    pub run: bool,
    pub sessions: Vec<SessionPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub totals: Option<Totals>,
}

/// What `apply` did, collected as it goes. With `echo` it also prints the
/// same lines `muxa restore --run` always printed, as it goes, so the text
/// output is unchanged; `--json` turns the echo off and reads the fields.
#[derive(Debug, Default)]
pub(super) struct RestoreReport {
    echo: bool,
    /// Sessions whose windows were all created or found.
    pub reached: BTreeSet<String>,
    /// The session whose creation stopped the restore, and why.
    pub failed: Option<(String, String)>,
    /// Per-pane failures, keyed by `=session:window.pane`.
    pub pane_failures: BTreeMap<String, String>,
    /// Panes left for a person — a process title, a busy pane, an agent whose
    /// command line was never captured — with the reason.
    pub panes_left: BTreeMap<String, String>,
}

impl RestoreReport {
    pub fn new(echo: bool) -> Self {
        Self {
            echo,
            ..Self::default()
        }
    }

    pub fn say(&self, line: &str) {
        if self.echo {
            println!("{line}");
        }
    }

    pub fn pane_failed(&mut self, target: String, error: String) {
        self.pane_failures.entry(target).or_insert(error);
    }

    pub fn pane_left(&mut self, target: String, note: String) {
        self.panes_left.entry(target).or_insert(note);
    }
}

pub(super) fn rebuilt_line(created: usize, panes: usize, kept: usize) -> String {
    format!("rebuilt {created} session(s)/window(s), {panes} pane(s) ({kept} already there)")
}

pub(super) fn split_failure_line(target: &str, error: &str) -> String {
    format!("  {target}: could not split — {error}")
}

pub(super) fn relaunched_line(relaunched: usize) -> String {
    format!("relaunched {relaunched} pane(s)")
}

pub(super) fn manual_line(target: &str, note: &str) -> String {
    format!("  {target}: {note}")
}

/// What one pane will be given to run, and the command line if any.
pub(super) fn pane_action(pane: &PaneShape, layout_only: bool) -> (PaneAction, Option<String>) {
    if layout_only {
        return (PaneAction::Shell, None);
    }
    let Some(command) = relaunch_command(pane) else {
        if resume_hint(pane).is_some() {
            return (PaneAction::ResumeByHand, None);
        }
        return (PaneAction::Shell, None);
    };
    if !pane.replayable {
        return (PaneAction::Manual, Some(command));
    }
    let resumes = pane.agent.as_ref().is_some_and(|agent| {
        !agent.session_id.is_empty()
            && !agent.session_id.starts_with("synthetic")
            && matches!(agent.kind.as_str(), "claude_code" | "codex")
    });
    let action = if resumes {
        PaneAction::Resume
    } else {
        PaneAction::Replay
    };
    (action, Some(command))
}

/// Sessions in the order the snapshot first lists them.
fn session_order(snapshot: &Snapshot) -> Vec<&str> {
    let mut seen = BTreeSet::new();
    snapshot
        .windows
        .iter()
        .map(|window| window.session.as_str())
        .filter(|session| seen.insert(*session))
        .collect()
}

/// The per-session decision against the sessions the server has now.
pub(super) fn plan(
    snapshot: &Snapshot,
    live: &BTreeSet<String>,
    only_missing: bool,
    layout_only: bool,
) -> Vec<SessionPlan> {
    session_order(snapshot)
        .into_iter()
        .map(|session| {
            let action = match (live.contains(session), only_missing) {
                (false, _) => SessionAction::Create,
                (true, true) => SessionAction::Skip,
                (true, false) => SessionAction::FillMissing,
            };
            let windows = snapshot
                .windows
                .iter()
                .filter(|window| window.session == session)
                .map(|window| WindowPlan {
                    index: window.index.clone(),
                    name: window.name.clone(),
                    layout: window.layout.clone(),
                    panes: snapshot
                        .panes
                        .iter()
                        .filter(|pane| pane.session == session && pane.window_index == window.index)
                        .map(|pane| {
                            let (action, command) = pane_action(pane, layout_only);
                            PanePlan {
                                index: pane.pane_index.clone(),
                                path: pane.path.clone(),
                                action,
                                command,
                                agent_kind: pane.agent.as_ref().map(|agent| agent.kind.clone()),
                                note: if action == PaneAction::ResumeByHand {
                                    resume_hint(pane)
                                } else {
                                    None
                                },
                                result: None,
                                error: None,
                                target: super::pane_target(pane),
                            }
                        })
                        .collect(),
                })
                .collect();
            SessionPlan {
                name: session.to_owned(),
                action,
                result: None,
                error: None,
                windows,
            }
        })
        .collect()
}

/// The snapshot minus every session the plan skips, so nothing below —
/// splits, `cd`, relaunches — can address a pane of a live session.
pub(super) fn without_skipped(snapshot: &Snapshot, sessions: &[SessionPlan]) -> Snapshot {
    let skipped: BTreeSet<&str> = sessions
        .iter()
        .filter(|session| session.action == SessionAction::Skip)
        .map(|session| session.name.as_str())
        .collect();
    let mut kept = snapshot.clone();
    kept.windows
        .retain(|window| !skipped.contains(window.session.as_str()));
    kept.panes
        .retain(|pane| !skipped.contains(pane.session.as_str()));
    kept
}

/// Fold what `apply` did back into the plan.
pub(super) fn attach_results(sessions: &mut [SessionPlan], report: &RestoreReport) {
    let aborted = report.failed.is_some();
    for session in sessions {
        if session.action == SessionAction::Skip {
            session.result = Some(SessionResult::Skipped);
            continue;
        }
        if let Some((failed, error)) = &report.failed {
            if *failed == session.name {
                session.result = Some(SessionResult::Failed);
                session.error = Some(error.clone());
                continue;
            }
        }
        if !report.reached.contains(&session.name) {
            session.result = Some(SessionResult::Failed);
            session.error = Some("not attempted".into());
            continue;
        }
        session.result = Some(match session.action {
            SessionAction::FillMissing => SessionResult::Filled,
            _ => SessionResult::Created,
        });
        if aborted {
            // The restore stopped before any pane was set up.
            continue;
        }
        for pane in session
            .windows
            .iter_mut()
            .flat_map(|window| &mut window.panes)
        {
            if let Some(error) = report.pane_failures.get(&pane.target) {
                pane.result = Some(PaneResult::Failed);
                pane.error = Some(error.clone());
                continue;
            }
            if let Some(note) = report.panes_left.get(&pane.target) {
                pane.result = Some(PaneResult::Manual);
                pane.error = Some(note.clone());
                continue;
            }
            pane.result = Some(match pane.action {
                PaneAction::Resume | PaneAction::Replay => PaneResult::Relaunched,
                PaneAction::Shell => PaneResult::Shell,
                PaneAction::Manual | PaneAction::ResumeByHand => PaneResult::Manual,
            });
        }
    }
}

pub(super) fn totals(sessions: &[SessionPlan]) -> Totals {
    let mut totals = Totals::default();
    for session in sessions {
        match session.result {
            Some(SessionResult::Created | SessionResult::Filled) => {
                totals.sessions_created += 1;
            }
            Some(SessionResult::Skipped) => totals.sessions_skipped += 1,
            Some(SessionResult::Failed) => totals.sessions_failed += 1,
            None => {}
        }
        for pane in session.windows.iter().flat_map(|window| &window.panes) {
            let Some(result) = pane.result else { continue };
            totals.panes += 1;
            match result {
                PaneResult::Relaunched => totals.relaunched += 1,
                PaneResult::Shell => totals.shell += 1,
                PaneResult::Manual => totals.manual += 1,
                PaneResult::Failed => totals.failed += 1,
            }
        }
    }
    totals
}

pub(super) fn summarize(snapshot: &Snapshot) -> Summary {
    Summary {
        taken_at: snapshot.taken_at.clone(),
        origin: snapshot.origin,
        host: snapshot.host.clone(),
        socket: snapshot.socket.clone(),
        sessions: session_order(snapshot).len(),
        windows: snapshot.windows.len(),
        panes: snapshot.panes.len(),
        agents: snapshot
            .panes
            .iter()
            .filter(|pane| pane.agent.is_some())
            .count(),
        resumable: snapshot
            .panes
            .iter()
            .filter(|pane| pane_action(pane, false).0 == PaneAction::Resume)
            .count(),
        not_replayable: snapshot
            .panes
            .iter()
            .filter(|pane| pane.command.is_some() && !pane.replayable)
            .count(),
    }
}

/// The dry-run text: a line per session saying what happens to it, its
/// windows, then the command each restored pane will be given.
pub(super) fn print_plan(sessions: &[SessionPlan], layout_only: bool) {
    for line in plan_lines(sessions, layout_only) {
        println!("{line}");
    }
}

pub(super) fn plan_lines(sessions: &[SessionPlan], layout_only: bool) -> Vec<String> {
    let mut lines = Vec::new();
    for session in sessions {
        let decision = match session.action {
            SessionAction::Create => "will create",
            SessionAction::Skip => "exists — skipped",
            SessionAction::FillMissing => "exists — only missing windows and panes will be added",
        };
        lines.push(format!("  {} — {decision}", session.name));
        for window in &session.windows {
            lines.push(format!(
                "  {}:{} {} — {} pane(s)",
                session.name,
                window.index,
                window.name,
                window.panes.len()
            ));
        }
    }
    if layout_only {
        return lines;
    }
    for session in sessions {
        if session.action == SessionAction::Skip {
            continue;
        }
        for pane in session.windows.iter().flat_map(|window| &window.panes) {
            if let Some(note) = pane.note.as_deref() {
                lines.push(format!("  {}\n    {note}", pane.target));
                continue;
            }
            let Some(command) = pane.command.as_deref() else {
                continue;
            };
            let note = if pane.action == PaneAction::Manual {
                "  [process title, not a command]"
            } else {
                ""
            };
            lines.push(format!("  {}{note}\n    {command}", pane.target));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::super::{AgentShape, WindowShape};
    use super::*;

    fn window(session: &str, index: &str) -> WindowShape {
        WindowShape {
            session: session.into(),
            index: index.into(),
            name: format!("w{index}"),
            active: false,
            layout: "c3a1,80x24,0,0,5".into(),
        }
    }

    fn pane(session: &str, window: &str, index: &str, command: Option<&str>) -> PaneShape {
        PaneShape {
            session: session.into(),
            window_index: window.into(),
            pane_index: index.into(),
            path: "/tmp".into(),
            replayable: command.is_some_and(super::super::is_replayable),
            command: command.map(str::to_owned),
            argv: None,
            agent: None,
        }
    }

    fn snapshot() -> Snapshot {
        let mut claude = pane("work", "0", "1", Some("claude"));
        claude.agent = Some(AgentShape {
            kind: "claude_code".into(),
            session_id: "abc".into(),
        });
        Snapshot {
            version: 1,
            taken_at: "2026-09-23T05:05:12Z".into(),
            origin: SnapshotOrigin::Manual,
            host: "tmux".into(),
            socket: "default".into(),
            windows: vec![
                window("work", "0"),
                window("work", "1"),
                window("side", "0"),
            ],
            panes: vec![
                pane("work", "0", "0", None),
                claude,
                pane(
                    "work",
                    "1",
                    "0",
                    Some("puma 5.6.8 (tcp://0.0.0.0:5072) [admin]"),
                ),
                pane("side", "0", "0", Some("npm run dev")),
            ],
        }
    }

    #[test]
    fn only_missing_skips_live_sessions_and_nothing_else() {
        let live = BTreeSet::from(["work".to_owned()]);
        let sessions = plan(&snapshot(), &live, true, false);
        let decisions: Vec<_> = sessions
            .iter()
            .map(|session| (session.name.as_str(), session.action))
            .collect();
        assert_eq!(
            decisions,
            [
                ("work", SessionAction::Skip),
                ("side", SessionAction::Create)
            ]
        );

        // Nothing of a skipped session survives into what `apply` sees, so
        // no split, `cd` or relaunch can land on one of its live panes.
        let kept = without_skipped(&snapshot(), &sessions);
        assert!(kept.windows.iter().all(|window| window.session == "side"));
        assert!(kept.panes.iter().all(|pane| pane.session == "side"));
        assert_eq!(kept.panes.len(), 1);
    }

    #[test]
    fn without_only_missing_a_live_session_gets_only_what_it_lacks() {
        let live = BTreeSet::from(["work".to_owned()]);
        let sessions = plan(&snapshot(), &live, false, false);
        assert_eq!(sessions[0].action, SessionAction::FillMissing);
        let lines = plan_lines(&sessions, false);
        assert!(lines.contains(
            &"  work — exists — only missing windows and panes will be added".to_owned()
        ));
        assert!(lines.contains(&"  side — will create".to_owned()));
        assert_eq!(without_skipped(&snapshot(), &sessions).panes.len(), 4);
    }

    #[test]
    fn each_pane_says_what_it_will_run() {
        let sessions = plan(&snapshot(), &BTreeSet::new(), true, false);
        let actions: Vec<_> = sessions
            .iter()
            .flat_map(|session| &session.windows)
            .flat_map(|window| &window.panes)
            .map(|pane| (pane.target.as_str(), pane.action, pane.command.as_deref()))
            .collect();
        assert_eq!(
            actions,
            [
                ("=work:0.0", PaneAction::Shell, None),
                ("=work:0.1", PaneAction::Resume, Some("claude --resume abc")),
                (
                    "=work:1.0",
                    PaneAction::Manual,
                    Some("puma 5.6.8 (tcp://0.0.0.0:5072) [admin]")
                ),
                ("=side:0.0", PaneAction::Replay, Some("npm run dev")),
            ]
        );
        // An agent whose command line was never captured is resumed by
        // hand, and the plan says with what.
        let mut uncaptured = snapshot();
        uncaptured.panes[0].agent = Some(AgentShape {
            kind: "codex".into(),
            session_id: "xyz".into(),
        });
        let sessions = plan(&uncaptured, &BTreeSet::new(), true, false);
        let pane = &sessions[0].windows[0].panes[0];
        assert_eq!(pane.action, PaneAction::ResumeByHand);
        assert!(pane.note.as_deref().unwrap().contains("codex resume xyz"));
        assert!(plan_lines(&sessions, false)
            .iter()
            .any(|line| line.starts_with("  =work:0.0\n    command line was not captured")));

        let layout_only = plan(&snapshot(), &BTreeSet::new(), true, true);
        assert!(layout_only
            .iter()
            .flat_map(|session| &session.windows)
            .flat_map(|window| &window.panes)
            .all(|pane| pane.action == PaneAction::Shell && pane.command.is_none()));
    }

    #[test]
    fn results_and_totals_follow_the_report() {
        let live = BTreeSet::from(["work".to_owned()]);
        let mut sessions = plan(&snapshot(), &live, true, false);
        let mut report = RestoreReport::new(false);
        report.reached.insert("side".into());
        report.pane_failed("=side:0.0".into(), "can't find pane".into());
        attach_results(&mut sessions, &report);
        assert_eq!(sessions[0].result, Some(SessionResult::Skipped));
        assert_eq!(sessions[1].result, Some(SessionResult::Created));
        let side = &sessions[1].windows[0].panes[0];
        assert_eq!(side.result, Some(PaneResult::Failed));
        assert_eq!(side.error.as_deref(), Some("can't find pane"));
        assert_eq!(
            totals(&sessions),
            Totals {
                sessions_created: 1,
                sessions_skipped: 1,
                panes: 1,
                failed: 1,
                ..Totals::default()
            }
        );
    }

    #[test]
    fn an_aborted_restore_marks_the_rest_not_attempted() {
        let mut sessions = plan(&snapshot(), &BTreeSet::new(), true, false);
        let mut report = RestoreReport::new(false);
        report.failed = Some(("work".into(), "creating session work: boom".into()));
        attach_results(&mut sessions, &report);
        assert_eq!(sessions[0].result, Some(SessionResult::Failed));
        assert_eq!(
            sessions[0].error.as_deref(),
            Some("creating session work: boom")
        );
        assert_eq!(sessions[1].error.as_deref(), Some("not attempted"));
    }

    #[test]
    fn summary_counts_what_a_listing_shows() {
        let summary = summarize(&snapshot());
        assert_eq!(
            (
                summary.sessions,
                summary.windows,
                summary.panes,
                summary.agents,
                summary.resumable,
                summary.not_replayable
            ),
            (2, 3, 4, 1, 1, 1)
        );
    }

    #[test]
    fn the_json_document_uses_the_documented_field_names() {
        let live = BTreeSet::from(["work".to_owned()]);
        let document = RestoreDocument {
            id: "1790000000".into(),
            dir: "/s/1790000000".into(),
            summary: summarize(&snapshot()),
            socket: "default".into(),
            only_missing: true,
            layout_only: false,
            server_reachable: true,
            run: false,
            sessions: plan(&snapshot(), &live, true, false),
            totals: None,
        };
        let value = serde_json::to_value(&document).unwrap();
        assert_eq!(value["summary"]["origin"], "manual");
        assert_eq!(value["summary"]["resumable"], 1);
        assert_eq!(value["sessions"][0]["action"], "skip");
        assert_eq!(value["sessions"][1]["action"], "create");
        let pane = &value["sessions"][0]["windows"][0]["panes"][1];
        assert_eq!(pane["action"], "resume");
        assert_eq!(pane["command"], "claude --resume abc");
        assert_eq!(pane["agent_kind"], "claude_code");
        assert!(pane.get("target").is_none(), "internal key stays internal");
        assert!(pane.get("result").is_none(), "no results in a dry run");
        assert!(value.get("totals").is_none());
    }

    #[test]
    fn restore_text_lines_are_unchanged() {
        // `muxa restore --run` printed these with `println!` before the
        // report existed; `--json` aside, the words must not move.
        assert_eq!(
            rebuilt_line(3, 12, 4),
            "rebuilt 3 session(s)/window(s), 12 pane(s) (4 already there)"
        );
        assert_eq!(relaunched_line(9), "relaunched 9 pane(s)");
        assert_eq!(
            manual_line(
                "=work:1.0",
                "argv was a process title, not a command — start it by hand"
            ),
            "  =work:1.0: argv was a process title, not a command — start it by hand"
        );
        assert_eq!(
            manual_line("=work:0.2", "already running something; left alone"),
            "  =work:0.2: already running something; left alone"
        );
        assert_eq!(
            split_failure_line("=work:0.5", "no space for new pane"),
            "  =work:0.5: could not split — no space for new pane"
        );
    }
}
