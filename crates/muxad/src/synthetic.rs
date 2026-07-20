//! Shared synthetic-row machinery for muxa's screen-inference producers.
//!
//! Two daemon tasks mint SYNTHETIC registry rows from non-hook sources: the
//! [`herdr_bridge`](crate::herdr_bridge) (herdr's own agent-state detection) and
//! [`screen_detect`](crate::screen_detect) (TOML manifest rules matched against
//! a pane capture). Both obey the SAME precedence and row-identity rules, so the
//! mechanics live here once:
//!
//! * **Hook-authoritative precedence** ([`occupant_is_authoritative`],
//!   [`apply_if_unowned`]): a live, non-synthetic (real hook) row owns its pane;
//!   a synthetic producer must drop its update wholesale rather than clobber it.
//!   And because these rows are synthetic, `Store::apply`'s synthetic-eviction
//!   pass drops them the instant a real hook `Started` claims the pane — so the
//!   hook-authoritative rule falls out of the existing machinery for free.
//! * **Row liveness** ([`stop_synthetic_row`]): when a producer stops seeing an
//!   agent on a pane whose shell is still open, it drives the synthetic row to
//!   `Stopped` so it doesn't freeze at its last state forever.
//! * **State → events** ([`state_events`]): the working/blocked/idle → muxa
//!   event mapping both producers share, plus the trailing `Heartbeat` that
//!   stamps the agent name into the row's `model` field (so an `Unknown`-kind
//!   row still names its agent).

use muxa::event::{AgentEvent, AgentId, NotificationLevel};
use muxa::state::{Agent, SYNTHETIC_SESSION_PREFIX};
use muxa::{AgentState, SharedStore};
use time::OffsetDateTime;

/// The three states screen inference can distinguish. Both synthetic producers
/// map their host-specific signal onto this before building events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntheticState {
    /// Actively generating → `AgentEvent::ToolStarted` → muxa `Working`.
    Working,
    /// Waiting on operator input → `NotificationFired(NeedsInput)` →
    /// `WaitingInput`.
    Blocked,
    /// Idle prompt → `TurnStopped` → `Idle`.
    Idle,
}

/// True when `agent` owns its pane *authoritatively* — i.e. it's a real
/// (non-synthetic) hook-driven row that is not `Stopped`.
///
/// A `Stopped` occupant does NOT own the pane: GC keeps a `Stopped` row around
/// for up to an hour, and a fresh (hook-less) agent launched in that same pane
/// during that window would otherwise be invisible to a synthetic producer the
/// whole time. So only NON-`Stopped` non-synthetic occupants block an update.
#[must_use]
pub fn occupant_is_authoritative(agent: &Agent) -> bool {
    !agent.session_id.starts_with(SYNTHETIC_SESSION_PREFIX) && agent.state != AgentState::Stopped
}

/// Apply `events` to the pane, enforcing hook-authoritative precedence: if a
/// *live* non-synthetic (real hook) row already owns `pane_id`, drop the whole
/// update. A `Stopped` real row is a stale tombstone, not an owner — see
/// [`occupant_is_authoritative`].
pub async fn apply_if_unowned(store: &SharedStore, pane_id: &str, events: &[AgentEvent]) {
    let occupants = store.by_pane(pane_id).await;
    if occupants.iter().any(occupant_is_authoritative) {
        tracing::debug!(
            pane = %pane_id,
            "synthetic: pane owned by a live hooked agent, dropping update",
        );
        return;
    }
    for ev in events {
        store.apply(ev).await;
    }
}

/// The producer no longer sees an agent on `pane_id`. If a *synthetic* row is
/// still mirroring one there, stop it — otherwise its last state
/// (`Working` / `WaitingInput`) freezes forever: the pane's shell is alive (so
/// the reconciler won't reap it) and the row isn't `Stopped` (so GC won't evict
/// it). Emitting `SessionEnded` drives the synthetic row to `Stopped`, after
/// which GC can reclaim it. A pane with no synthetic row is left untouched (no
/// row is invented), and a real hook row is never disturbed by this path.
pub async fn stop_synthetic_row(store: &SharedStore, pane_id: &str) {
    let occupants = store.by_pane(pane_id).await;
    for occ in occupants {
        if !occ.session_id.starts_with(SYNTHETIC_SESSION_PREFIX) || occ.state == AgentState::Stopped
        {
            // Real hook rows are not ours to stop; already-`Stopped` synthetic
            // rows need no further event.
            continue;
        }
        let id = AgentId {
            kind: occ.kind,
            session_id: occ.session_id.clone(),
            surface: occ.surface.clone(),
            pane: occ.pane.clone(),
            tmux_socket: occ.tmux_socket.clone(),
            cwd: occ.cwd.clone(),
        };
        store
            .apply(&AgentEvent::SessionEnded {
                id,
                at: OffsetDateTime::now_utc(),
            })
            .await;
        tracing::debug!(pane = %pane_id, "synthetic: agent gone, stopped synthetic row");
    }
}

/// Build the ordered events for one synthetic state change: the status-bearing
/// event first (so a fresh row transitions straight into its real state),
/// followed by a `Heartbeat` that stamps the agent `name` into the row's
/// `model` metadata.
///
/// `message` is the human text for a `Blocked` row (the approval prompt / what
/// it's waiting on); `None` falls back to `"<name> is waiting"`. It's ignored
/// for `Working`/`Idle`.
#[must_use]
pub fn state_events(
    id: AgentId,
    state: SyntheticState,
    name: &str,
    message: Option<String>,
    at: OffsetDateTime,
) -> Vec<AgentEvent> {
    let status_event = match state {
        SyntheticState::Working => AgentEvent::ToolStarted {
            id: id.clone(),
            tool: name.to_owned(),
            subagent: None,
            at,
        },
        SyntheticState::Blocked => AgentEvent::NotificationFired {
            id: id.clone(),
            level: NotificationLevel::NeedsInput,
            message: message.unwrap_or_else(|| format!("{name} is waiting")),
            at,
        },
        SyntheticState::Idle => AgentEvent::TurnStopped {
            id: id.clone(),
            response: None,
            recap: None,
            ai_title: None,
            // Positive idle evidence: the producer OBSERVED the pane go idle
            // (the approval prompt / spinner is gone). This marker lets a
            // synthetic `WaitingInput` row clear to `Idle` on this response-less
            // stop, without reopening the Codex response-less-stop quirk (which
            // is markerless). See `state::mutate_for_event`'s `TurnStopped` arm.
            idle_confirmed: true,
            at,
        },
    };
    let heartbeat = AgentEvent::Heartbeat {
        id,
        model: Some(name.to_owned()),
        context_used_pct: None,
        cost_usd: None,
        rate_limit_5h_pct: None,
        rate_limit_5h_resets_at: None,
        rate_limit_7d_pct: None,
        rate_limit_7d_resets_at: None,
        at,
    };
    vec![status_event, heartbeat]
}

#[cfg(test)]
mod tests {
    use muxa::event::AgentKind;
    use muxa::state::Store;
    use time::macros::datetime;

    use super::*;

    const AT: OffsetDateTime = datetime!(2026-07-20 12:00:00 UTC);

    fn id(pane: &str) -> AgentId {
        AgentId {
            kind: AgentKind::Unknown,
            session_id: format!("{SYNTHETIC_SESSION_PREFIX}{pane}"),
            surface: None,
            pane: Some(pane.to_owned()),
            tmux_socket: None,
            cwd: None,
        }
    }

    #[test]
    fn working_builds_tool_started_plus_named_heartbeat() {
        let events = state_events(id("%1"), SyntheticState::Working, "cursor", None, AT);
        assert_eq!(events.len(), 2);
        match &events[0] {
            AgentEvent::ToolStarted { tool, .. } => assert_eq!(tool, "cursor"),
            other => panic!("expected ToolStarted, got {other:?}"),
        }
        match &events[1] {
            AgentEvent::Heartbeat { model, .. } => assert_eq!(model.as_deref(), Some("cursor")),
            other => panic!("expected Heartbeat, got {other:?}"),
        }
    }

    #[test]
    fn blocked_uses_message_then_falls_back() {
        let with_msg = state_events(
            id("%1"),
            SyntheticState::Blocked,
            "cursor",
            Some("Approve rm?".into()),
            AT,
        );
        match &with_msg[0] {
            AgentEvent::NotificationFired { level, message, .. } => {
                assert_eq!(*level, NotificationLevel::NeedsInput);
                assert_eq!(message, "Approve rm?");
            }
            other => panic!("expected NotificationFired, got {other:?}"),
        }
        let no_msg = state_events(id("%1"), SyntheticState::Blocked, "cursor", None, AT);
        match &no_msg[0] {
            AgentEvent::NotificationFired { message, .. } => {
                assert_eq!(message, "cursor is waiting");
            }
            other => panic!("expected NotificationFired, got {other:?}"),
        }
    }

    #[test]
    fn idle_builds_idle_confirmed_turn_stopped() {
        // The synthetic idle event MUST carry `idle_confirmed = true` — that
        // marker is what lets it clear a WaitingInput row without reopening the
        // Codex markerless-stop quirk (see state::mutate_for_event).
        let events = state_events(id("%1"), SyntheticState::Idle, "cursor", None, AT);
        match &events[0] {
            AgentEvent::TurnStopped {
                idle_confirmed,
                response,
                ..
            } => {
                assert!(
                    *idle_confirmed,
                    "synthetic idle is an explicit idle observation"
                );
                assert_eq!(*response, None);
            }
            other => panic!("expected TurnStopped, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn apply_if_unowned_drops_when_a_live_hook_row_owns_the_pane() {
        let store = Store::shared();
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "real".into(),
                    surface: None,
                    pane: Some("%1".into()),
                    tmux_socket: None,
                    cwd: None,
                },
                at: AT,
            })
            .await;
        let events = state_events(id("%1"), SyntheticState::Working, "cursor", None, AT);
        apply_if_unowned(&store, "%1", &events).await;

        let rows = store.by_pane("%1").await;
        assert_eq!(rows.len(), 1, "only the real row remains");
        assert_eq!(rows[0].session_id, "real");
    }

    #[tokio::test]
    async fn apply_if_unowned_applies_when_pane_is_free() {
        let store = Store::shared();
        let events = state_events(id("%1"), SyntheticState::Working, "cursor", None, AT);
        apply_if_unowned(&store, "%1", &events).await;
        let rows = store.by_pane("%1").await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state, AgentState::Working);
        assert_eq!(rows[0].model.as_deref(), Some("cursor"));
    }

    #[tokio::test]
    async fn stop_synthetic_row_stops_a_synthetic_but_not_a_real_row() {
        let store = Store::shared();
        // Synthetic working row.
        let events = state_events(id("%1"), SyntheticState::Working, "cursor", None, AT);
        apply_if_unowned(&store, "%1", &events).await;
        stop_synthetic_row(&store, "%1").await;
        assert_eq!(store.by_pane("%1").await[0].state, AgentState::Stopped);

        // A real row on another pane must be left alone.
        store
            .apply(&AgentEvent::Started {
                id: AgentId {
                    kind: AgentKind::ClaudeCode,
                    session_id: "real".into(),
                    surface: None,
                    pane: Some("%2".into()),
                    tmux_socket: None,
                    cwd: None,
                },
                at: AT,
            })
            .await;
        stop_synthetic_row(&store, "%2").await;
        assert_eq!(store.by_pane("%2").await[0].state, AgentState::Idle);
    }

    #[test]
    fn stopped_real_row_is_not_authoritative() {
        let mut agent = Agent {
            kind: AgentKind::ClaudeCode,
            session_id: "real".into(),
            surface: None,
            pane: Some("%1".into()),
            tmux_socket: None,
            tmux_session: None,
            cwd: None,
            pid: None,
            workload: muxa::WorkloadSummary::default(),
            subagents: Vec::new(),
            state: AgentState::Working,
            last_prompt: None,
            last_response: None,
            recap: None,
            ai_title: None,
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
            started_at: AT,
            last_activity_at: AT,
            state_entered_at: AT,
        };
        assert!(occupant_is_authoritative(&agent), "live real row owns pane");
        agent.state = AgentState::Stopped;
        assert!(
            !occupant_is_authoritative(&agent),
            "stopped real row is a tombstone, not an owner",
        );
        agent.session_id = format!("{SYNTHETIC_SESSION_PREFIX}%1");
        agent.state = AgentState::Working;
        assert!(
            !occupant_is_authoritative(&agent),
            "synthetic rows never own authoritatively",
        );
    }
}
