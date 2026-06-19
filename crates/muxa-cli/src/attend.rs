//! `muxa attend` — jump straight to the agent that needs you.
//!
//! `muxa` already knows, at every instant, which agents are blocked on a
//! human (`WaitingInput` / `WaitingChoice` / `Error`). This command turns
//! that knowledge into a single action: focus the pane of whichever agent
//! has been waiting longest, or — with `--cycle` — rotate through the
//! blocked agents one keypress at a time.
//!
//! It deliberately reuses `main::jump_to_pane`, the same focus machinery
//! `muxa watch`'s Enter action already relies on: [`run`] only *chooses* a
//! pane and hands the id back to the caller, which performs the
//! tmux/zellij + inside/outside-multiplexer jump. Keeping the side effect
//! in one place means `attend` and `watch` can never drift on how a jump
//! actually lands.

use anyhow::Result;
use owo_colors::OwoColorize;
use time::OffsetDateTime;

use muxa::ipc::Client;
use muxa::state::Agent;
use muxa::tmux::PaneInfo;
use muxa::{AgentState, PaneBackend};

use crate::{pane_display, state_icon, state_style, terminal_width, truncate_cell, use_colors};

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Rotate to the next waiting agent *after* the current pane, wrapping
    /// around, instead of jumping to the one that's been waiting longest.
    /// Bind it to a tmux key to tab through everything that needs you:
    /// `bind-key a run-shell "muxa attend --cycle"`.
    #[arg(long)]
    pub cycle: bool,

    /// Print the ranked queue of agents that need you and exit without
    /// jumping anywhere. Longest-waiting first — the same order a bare
    /// `muxa attend` would jump in.
    #[arg(long)]
    pub list: bool,
}

/// States that block on a human. An agent in any of these is "waiting for
/// you" and is a jump target; `Working` / `Idle` / `Starting` / `Stopped`
/// agents are skipped. `WaitingChoice` is folded in alongside
/// `WaitingInput` here exactly as the notifier and sink filters do.
fn needs_attention(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::WaitingInput | AgentState::WaitingChoice | AgentState::Error
    )
}

/// Spatial sort key for a pane — `(session, window_index, pane_index)` when
/// the pane resolves against the host inventory, else the raw pane id with
/// max indices so unresolved panes sort last but still deterministically.
/// Parsing the indices to `u32` (rather than comparing the raw strings)
/// keeps `window 2` before `window 10`.
type SpatialKey = (String, u32, u32);

fn spatial_key(pane: &str, panes: &[PaneInfo]) -> SpatialKey {
    panes.iter().find(|p| p.pane_id == pane).map_or_else(
        || (pane.to_string(), u32::MAX, u32::MAX),
        |p| {
            (
                p.session.clone(),
                p.window_index.parse().unwrap_or(u32::MAX),
                p.pane_index.parse().unwrap_or(u32::MAX),
            )
        },
    )
}

/// An attention-needing agent paired with the pane we'd jump to and its
/// spatial key. Borrows the agent out of the snapshot — lives only for the
/// duration of one `attend` invocation.
struct Candidate<'a> {
    agent: &'a Agent,
    pane: &'a str,
    key: SpatialKey,
}

/// Filter the snapshot down to attention-needing agents that have a pane,
/// sorted into spatial order. Agents with no pane are dropped: there's
/// nothing to focus, and `muxa watch` already surfaces paneless agents.
fn candidates<'a>(agents: &'a [Agent], panes: &[PaneInfo]) -> Vec<Candidate<'a>> {
    let mut cands: Vec<Candidate<'a>> = agents
        .iter()
        .filter(|a| needs_attention(a.state))
        .filter_map(|a| {
            a.pane.as_deref().map(|pane| Candidate {
                agent: a,
                pane,
                key: spatial_key(pane, panes),
            })
        })
        .collect();
    cands.sort_by(|a, b| a.key.cmp(&b.key));
    cands
}

/// The agent blocked longest: smallest `state_entered_at`. Ties (same
/// instant — e.g. synthetic rows replayed at rehydrate) break by spatial
/// key so the pick is stable across back-to-back invocations.
fn longest_waiting<'c>(cands: &'c [Candidate<'_>]) -> Option<&'c Candidate<'c>> {
    cands.iter().min_by(|a, b| {
        a.agent
            .state_entered_at
            .cmp(&b.agent.state_entered_at)
            .then_with(|| a.key.cmp(&b.key))
    })
}

/// Cycle target: the first candidate positioned strictly after `current` in
/// spatial order, wrapping to the first. `cands` must already be sorted by
/// key. When `current` is unknown (no `$TMUX_PANE`) we start from the
/// first, so the binding still rotates from a sensible anchor.
fn next_after<'c>(
    cands: &'c [Candidate<'_>],
    current: Option<&SpatialKey>,
) -> Option<&'c Candidate<'c>> {
    match current {
        Some(cur) => cands
            .iter()
            .find(|c| &c.key > cur)
            .or_else(|| cands.first()),
        None => cands.first(),
    }
}

/// Choose a pane to attend to, or print the queue / a "nothing to do"
/// line. Returns the pane id the caller should jump to, or `None` when
/// there's nothing to jump to (`--list`, or no agent needs attention).
pub async fn run(client: &Client, backend: &dyn PaneBackend, args: Args) -> Result<Option<String>> {
    let agents = client.snapshot().await?;
    // Resolve panes once: spatial ordering + `pane_display` both read it,
    // and it's empty (harmless) on backends without pane metadata.
    let panes = backend.list_panes();
    let cands = candidates(&agents, &panes);

    if cands.is_empty() {
        // Separate "nothing tracked" from "everything's busy" so the line
        // tells the user which it is.
        if agents.is_empty() {
            println!("no agents tracked");
        } else {
            let n = agents.len();
            println!(
                "nothing needs you — {n} agent{} working or idle",
                if n == 1 { "" } else { "s" }
            );
        }
        return Ok(None);
    }

    if args.list {
        print_queue(&cands, &panes);
        return Ok(None);
    }

    let chosen = if args.cycle {
        let current = backend.current_pane().map(|p| spatial_key(&p, &panes));
        next_after(&cands, current.as_ref())
    } else {
        longest_waiting(&cands)
    };

    Ok(chosen.map(|c| c.pane.to_string()))
}

/// Render the attention queue, longest-waiting first. Each row is the
/// state glyph, `session:window.pane` location, agent kind, how long it's
/// been blocked, and the first line of its last prompt for context.
fn print_queue(cands: &[Candidate<'_>], panes: &[PaneInfo]) {
    let now = OffsetDateTime::now_utc();
    let color = use_colors();
    let terminal_width = terminal_width();
    let (loc_width, kind_width) = attend_queue_widths(terminal_width);

    let mut ranked: Vec<&Candidate<'_>> = cands.iter().collect();
    ranked.sort_by(|a, b| {
        a.agent
            .state_entered_at
            .cmp(&b.agent.state_entered_at)
            .then_with(|| a.key.cmp(&b.key))
    });

    let n = ranked.len();
    println!("{n} agent{} need you:", if n == 1 { "" } else { "s" });
    for c in ranked {
        let icon = state_icon(c.agent.state);
        let loc = truncate_cell(&pane_display(c.agent, panes), loc_width);
        let kind = truncate_cell(&c.agent.kind.to_string(), kind_width);
        let waited = humanize_since(c.agent.state_entered_at, now);
        let visible_head =
            format!("  {icon} {loc:<loc_width$} {kind:<kind_width$} waiting {waited:>4}");
        let styled_head = if color {
            visible_head.style(state_style(c.agent.state)).to_string()
        } else {
            visible_head.clone()
        };
        let snippet = c
            .agent
            .last_prompt
            .as_deref()
            .and_then(|p| p.lines().next())
            .map(|line| {
                let snippet_width = terminal_width
                    .saturating_sub(visible_head.chars().count())
                    .saturating_sub(2);
                truncate_cell(line, snippet_width)
            })
            .unwrap_or_default();
        if snippet.is_empty() {
            println!("{styled_head}");
        } else {
            println!("{styled_head}  {snippet}");
        }
    }
}

fn attend_queue_widths(terminal_width: usize) -> (usize, usize) {
    if terminal_width < 70 {
        (10, 8)
    } else if terminal_width < 100 {
        (16, 12)
    } else {
        (24, 12)
    }
}

/// Compact elapsed-time label: `42s`, `7m`, `3h`, `2d`. Coarse on purpose —
/// this is a "how stale is this" hint, not a stopwatch.
fn humanize_since(then: OffsetDateTime, now: OffsetDateTime) -> String {
    let secs = (now - then).whole_seconds().max(0);
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

#[cfg(test)]
/// Truncate to at most `max` characters (counting by `char`, so multi-byte
/// prompts don't panic on a byte boundary), appending `…` when clipped.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use muxa::AgentKind;
    use time::Duration;

    fn pane(id: &str, session: &str, window: u32, idx: u32) -> PaneInfo {
        PaneInfo {
            pane_id: id.into(),
            session: session.into(),
            window_index: window.to_string(),
            pane_index: idx.to_string(),
            tty: "/dev/pts/0".into(),
            current_command: "claude".into(),
            title: String::new(),
            pane_pid: 0,
        }
    }

    /// Build an agent in `state`, pinned to `pane`, that entered its state
    /// `entered` (used to order by how long it's been blocked).
    fn agent(
        session: &str,
        pane: Option<&str>,
        state: AgentState,
        entered: OffsetDateTime,
    ) -> Agent {
        Agent {
            kind: AgentKind::ClaudeCode,
            session_id: session.into(),
            surface: None,
            pane: pane.map(Into::into),
            pid: None,
            workload: muxa::WorkloadSummary::default(),
            cwd: None,
            state,
            last_prompt: None,
            last_response: None,
            last_notification: None,
            model: None,
            context_used_pct: None,
            cost_usd: None,
            rate_limit_5h_pct: None,
            rate_limit_5h_resets_at: None,
            rate_limit_7d_pct: None,
            rate_limit_7d_resets_at: None,
            rate_limited_until: None,
            rate_limit_scope: None,
            rate_limit_source: None,
            started_at: entered,
            last_activity_at: entered,
            state_entered_at: entered,
        }
    }

    fn t(secs: i64) -> OffsetDateTime {
        // Fixed anchor + offset — Date::now is forbidden in this codebase's
        // spirit anyway, and tests want determinism.
        time::macros::datetime!(2026-05-31 12:00:00 UTC) + Duration::seconds(secs)
    }

    /// Only the three blocking states are jump targets; everything else is
    /// skipped even when it has a pane.
    #[test]
    fn needs_attention_only_blocking_states() {
        assert!(needs_attention(AgentState::WaitingInput));
        assert!(needs_attention(AgentState::WaitingChoice));
        assert!(needs_attention(AgentState::Error));
        assert!(!needs_attention(AgentState::Working));
        assert!(!needs_attention(AgentState::Idle));
        assert!(!needs_attention(AgentState::Starting));
        assert!(!needs_attention(AgentState::Stopped));
    }

    /// Working/idle agents and paneless waiters are dropped; the survivors
    /// come back in spatial (session, window, pane) order regardless of
    /// snapshot order.
    #[test]
    fn candidates_filters_and_sorts() {
        let panes = vec![pane("%1", "main", 2, 0), pane("%2", "main", 1, 0)];
        let agents = vec![
            agent("a", Some("%1"), AgentState::WaitingInput, t(0)), // main:2.0
            agent("b", Some("%2"), AgentState::Error, t(0)),        // main:1.0 (sorts first)
            agent("c", Some("%9"), AgentState::Working, t(0)),      // busy → dropped
            agent("d", None, AgentState::WaitingChoice, t(0)),      // paneless → dropped
        ];
        let cands = candidates(&agents, &panes);
        let panes_out: Vec<&str> = cands.iter().map(|c| c.pane).collect();
        assert_eq!(panes_out, vec!["%2", "%1"]);
    }

    /// `longest_waiting` returns whoever entered its blocked state earliest.
    #[test]
    fn longest_waiting_picks_oldest() {
        let panes = vec![pane("%1", "main", 1, 0), pane("%2", "main", 2, 0)];
        let agents = vec![
            agent("a", Some("%1"), AgentState::WaitingInput, t(100)),
            agent("b", Some("%2"), AgentState::Error, t(10)), // blocked longest
        ];
        let cands = candidates(&agents, &panes);
        assert_eq!(longest_waiting(&cands).unwrap().pane, "%2");
    }

    /// Window indices sort numerically, so `window 2` beats `window 10` —
    /// a plain string sort would invert these.
    #[test]
    fn spatial_key_orders_windows_numerically() {
        let panes = vec![pane("%1", "s", 10, 0), pane("%2", "s", 2, 0)];
        let agents = vec![
            agent("a", Some("%1"), AgentState::WaitingInput, t(0)),
            agent("b", Some("%2"), AgentState::WaitingInput, t(0)),
        ];
        let cands = candidates(&agents, &panes);
        assert_eq!(
            cands.iter().map(|c| c.pane).collect::<Vec<_>>(),
            vec!["%2", "%1"]
        );
    }

    /// `--cycle` from a pane in the middle of the queue lands on the next
    /// one; from the last it wraps back to the first.
    #[test]
    fn next_after_cycles_and_wraps() {
        let panes = vec![
            pane("%1", "s", 0, 0),
            pane("%2", "s", 1, 0),
            pane("%3", "s", 2, 0),
        ];
        let agents = vec![
            agent("a", Some("%1"), AgentState::WaitingInput, t(0)),
            agent("b", Some("%2"), AgentState::WaitingInput, t(0)),
            agent("c", Some("%3"), AgentState::WaitingInput, t(0)),
        ];
        let cands = candidates(&agents, &panes);

        // On %1 → next is %2.
        let cur = spatial_key("%1", &panes);
        assert_eq!(next_after(&cands, Some(&cur)).unwrap().pane, "%2");

        // On the last (%3) → wrap to %1.
        let cur = spatial_key("%3", &panes);
        assert_eq!(next_after(&cands, Some(&cur)).unwrap().pane, "%1");

        // Unknown current pane → start from the first.
        assert_eq!(next_after(&cands, None).unwrap().pane, "%1");
    }

    /// Cycling from a *non-waiting* pane (e.g. you're focused on an editor)
    /// still advances to the next blocked pane after that position rather
    /// than getting stuck.
    #[test]
    fn next_after_from_non_candidate_pane() {
        let panes = vec![
            pane("%1", "s", 0, 0), // editor, not waiting
            pane("%2", "s", 1, 0),
            pane("%3", "s", 2, 0),
        ];
        let agents = vec![
            agent("b", Some("%2"), AgentState::WaitingInput, t(0)),
            agent("c", Some("%3"), AgentState::WaitingInput, t(0)),
        ];
        let cands = candidates(&agents, &panes);
        let cur = spatial_key("%1", &panes);
        assert_eq!(next_after(&cands, Some(&cur)).unwrap().pane, "%2");
    }

    #[test]
    fn humanize_since_buckets() {
        let now = t(0);
        assert_eq!(humanize_since(t(-5), now), "5s");
        assert_eq!(humanize_since(t(-90), now), "1m");
        assert_eq!(humanize_since(t(-3 * 3600), now), "3h");
        assert_eq!(humanize_since(t(-2 * 86_400), now), "2d");
        // Clock skew (future timestamp) clamps to 0s rather than going
        // negative.
        assert_eq!(humanize_since(t(10), now), "0s");
    }

    #[test]
    fn truncate_clips_with_ellipsis() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("0123456789abc", 5), "0123…");
        // Multi-byte chars are counted, not bytes — no panic on the
        // boundary.
        assert_eq!(truncate("한글테스트입니다", 4), "한글테…");
    }
}
