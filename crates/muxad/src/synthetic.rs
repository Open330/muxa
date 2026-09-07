//! Shared synthetic-row machinery for muxa's screen-inference producers.
//!
//! Two daemon tasks mint SYNTHETIC registry rows from non-hook sources: the
//! [`herdr_bridge`](crate::herdr_bridge) (herdr's own agent-state detection) and
//! [`screen_detect`](crate::screen_detect) (TOML manifest rules matched against
//! a pane capture). Both obey the SAME precedence and row-identity rules, so the
//! mechanics live here once:
//!
//! * **Hook-authoritative precedence** ([`occupant_is_authoritative`],
//!   [`pane_ownership`], [`apply_if_unowned`]): a live, non-synthetic (real
//!   hook) row owns its pane; a synthetic producer must drop its update
//!   wholesale rather than clobber it. And because these rows are synthetic,
//!   `Store::apply`'s synthetic-eviction pass drops them the instant a real
//!   hook `Started` claims the pane — so the hook-authoritative rule falls out
//!   of the existing machinery for free.
//! * **Attention refinement** ([`attention_refinement_events`]): the one
//!   documented exception. When the owning row's agent CLI has no hook that can
//!   report "waiting on the operator"
//!   (`AgentKind::hooks_report_attention`), dropping the update wholesale
//!   would mean that agent can *never* show as `WaitingInput`. Such a pane
//!   instead gets an attention-only update applied to the REAL row — never a
//!   second synthetic one, which `Store::apply` would evict on the next hook
//!   event and re-mint on the next tick, flapping a duplicate row forever.
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
use time::{Duration, OffsetDateTime};

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

/// What a pane's current occupants permit a synthetic producer to do.
///
/// Not `PartialEq` — it carries an [`AgentId`], which isn't comparable; call
/// sites and tests match on the variant instead.
#[derive(Debug, Clone)]
pub enum PaneOwnership {
    /// No live real row. Mint and drive the synthetic row as usual.
    Free,
    /// A live real row owns the pane and its hooks can report every state muxa
    /// tracks, attention included. Stay out entirely.
    Hooked,
    /// A live real row owns the pane, but its agent CLI exposes no hook that
    /// can report "waiting on the operator". Screen inference is the only
    /// source for that one signal, so the producer may refine THIS row's
    /// attention state — and nothing else. See [`attention_refinement_events`].
    AttentionBlind { id: AgentId, state: AgentState },
}

/// How long after launch a hook-reporting row still accepts screen-sourced
/// attention refinement.
///
/// A hooked agent's hooks are authoritative — but only once they exist. Codex
/// asks its trust/policy question *before* creating the session that owns the
/// hooks, so during that gate no hook can fire and the row sits at whatever
/// state muxa launched it in. Treating the pane as fully `Hooked` from second
/// zero means nothing ever looks at the screen and the gate reads as `idle`.
///
/// Inside this window the row is merely [`PaneOwnership::AttentionBlind`], so
/// [`attention_refinement_events`] may promote it to waiting (and only that —
/// `Working` still contributes nothing, so a healthy agent's hooks are never
/// raced). The window is generous relative to what it covers: the gate paints
/// within a second of launch, so anything that has not shown one by then never
/// will.
pub const STARTUP_ATTENTION_WINDOW: Duration = Duration::minutes(5);

/// Is this row still inside the post-launch window where its hooks may not
/// exist yet? Only `Idle` qualifies: any other state is itself proof that
/// something already reported, and a row muxa launched starts out `Idle`.
fn in_startup_window(owner: &Agent, now: OffsetDateTime) -> bool {
    owner.state == AgentState::Idle && now - owner.started_at < STARTUP_ATTENTION_WINDOW
}

/// Classify `occupants` (as returned by `Store::by_pane`) into the update the
/// producer is allowed to make.
///
/// The first authoritative occupant wins; in practice there is at most one,
/// because `Store::apply` evicts synthetic rows from a pane a real row claims.
///
/// `now` exists for the [`STARTUP_ATTENTION_WINDOW`] carve-out described on
/// that constant.
#[must_use]
pub fn pane_ownership(occupants: &[Agent], now: OffsetDateTime) -> PaneOwnership {
    let Some(owner) = occupants.iter().find(|a| occupant_is_authoritative(a)) else {
        return PaneOwnership::Free;
    };
    if owner.kind.hooks_report_attention() && !in_startup_window(owner, now) {
        return PaneOwnership::Hooked;
    }
    PaneOwnership::AttentionBlind {
        id: AgentId {
            kind: owner.kind,
            session_id: owner.session_id.clone(),
            surface: owner.surface.clone(),
            pane: owner.pane.clone(),
            tmux_socket: owner.tmux_socket.clone(),
            cwd: owner.cwd.clone(),
        },
        state: owner.state,
    }
}

/// The events a screen observation may contribute to an
/// [`PaneOwnership::AttentionBlind`] row. Empty means "the observation adds
/// nothing" — the overwhelmingly common case.
///
/// Deliberately narrower than [`state_events`] in three ways:
///
/// 1. **No `Heartbeat`.** A real row's `model` is the model name its hooks
///    reported (`gemini-3.7-flash-high`); stamping the manifest name over it
///    would lose real information.
/// 2. **`Working` contributes nothing.** Hooks own that transition and fire it
///    within milliseconds of the tool starting, whereas a screen tick is
///    seconds late. Racing them buys nothing and can only fight.
/// 3. **`Idle` only ever *clears*.** It is the backstop for the one way a
///    refined row could get stuck: hooks stop arriving (the CLI died mid-turn,
///    or an event was lost) while the row sits at `WaitingInput`. An idle
///    screen on a row that is NOT waiting is left to the hooks.
#[must_use]
pub fn attention_refinement_events(
    id: AgentId,
    owner_state: AgentState,
    observed: SyntheticState,
    name: &str,
    message: Option<String>,
    at: OffsetDateTime,
) -> Vec<AgentEvent> {
    let waiting = matches!(
        owner_state,
        AgentState::WaitingInput | AgentState::WaitingChoice
    );
    match observed {
        // An approval prompt is on screen and the row doesn't know it yet.
        // `Error` is left alone: it is a fact the hooks reported, and masking
        // it with a wait would hide the failure the operator needs to see.
        SyntheticState::Blocked if !waiting && owner_state != AgentState::Error => {
            vec![AgentEvent::NotificationFired {
                id,
                level: NotificationLevel::NeedsInput,
                message: message.unwrap_or_else(|| format!("{name} is waiting")),
                at,
            }]
        }
        // The prompt is gone and nothing is generating, yet the row is still
        // waiting — release it. `idle_confirmed` is exactly this: positive
        // evidence from a producer that OBSERVED the pane go idle.
        SyntheticState::Idle if waiting => {
            vec![AgentEvent::TurnStopped {
                id,
                response: None,
                recap: None,
                ai_title: None,
                idle_confirmed: true,
                at,
            }]
        }
        SyntheticState::Blocked | SyntheticState::Idle | SyntheticState::Working => Vec::new(),
    }
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
    /// Past [`STARTUP_ATTENTION_WINDOW`] from `AT` — steady state, where a
    /// hook-reporting row is fully hook-authoritative.
    const LATER: OffsetDateTime = datetime!(2026-07-20 13:00:00 UTC);

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

    /// Build a bare `Agent` row for ownership tests.
    fn row(kind: AgentKind, session_id: &str, state: AgentState) -> Agent {
        Agent {
            kind,
            session_id: session_id.to_owned(),
            surface: None,
            pane: Some("%1".into()),
            tmux_socket: None,
            tmux_session: None,
            cwd: None,
            pid: None,
            workload: muxa::WorkloadSummary::default(),
            subagents: Vec::new(),
            state,
            last_prompt: None,
            last_prompt_at: None,
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
        }
    }

    #[test]
    fn pane_ownership_free_when_no_live_real_row() {
        assert!(matches!(pane_ownership(&[], AT), PaneOwnership::Free));
        // A synthetic row does not own the pane...
        let synth = row(
            AgentKind::Unknown,
            &format!("{SYNTHETIC_SESSION_PREFIX}%1"),
            AgentState::Working,
        );
        assert!(matches!(pane_ownership(&[synth], AT), PaneOwnership::Free));
        // ...and neither does a stopped real one.
        let dead = row(AgentKind::ClaudeCode, "real", AgentState::Stopped);
        assert!(matches!(pane_ownership(&[dead], AT), PaneOwnership::Free));
    }

    /// Every agent whose hooks can report a permission prompt keeps screen
    /// inference fully out of the way — the pre-existing behavior.
    #[test]
    fn pane_ownership_hooked_for_agents_that_report_attention() {
        for kind in [
            AgentKind::ClaudeCode,
            AgentKind::Codex,
            AgentKind::GeminiCli,
            AgentKind::Opencode,
        ] {
            let owner = row(kind, "real", AgentState::Working);
            assert!(
                matches!(pane_ownership(&[owner], LATER), PaneOwnership::Hooked),
                "{kind} hooks report attention, so nothing may refine it",
            );
        }
    }

    /// agy is the one kind with no permission hook, so its row is refinable —
    /// and the refinement must target the REAL row's identity, never a
    /// synthetic one (a second row would flap: `Store::apply` evicts it on the
    /// next hook event and the next tick re-mints it).
    #[test]
    fn pane_ownership_attention_blind_for_antigravity() {
        let owner = row(AgentKind::Antigravity, "conv-1", AgentState::Working);
        match pane_ownership(&[owner], LATER) {
            PaneOwnership::AttentionBlind { id, state } => {
                assert_eq!(id.session_id, "conv-1");
                assert_eq!(id.kind, AgentKind::Antigravity);
                assert!(!id.session_id.starts_with(SYNTHETIC_SESSION_PREFIX));
                assert_eq!(state, AgentState::Working);
            }
            other => panic!("expected AttentionBlind, got {other:?}"),
        }
    }

    /// The gap this carve-out exists for: codex's trust gate paints before the
    /// session (and therefore the hooks) exist, so a freshly launched, still
    /// `Idle` row must stay refinable even though codex hooks normally report
    /// attention.
    #[test]
    fn pane_ownership_refinable_inside_the_startup_window() {
        let owner = row(AgentKind::Codex, "real", AgentState::Idle);
        let just_after = AT + Duration::seconds(3);
        match pane_ownership(&[owner], just_after) {
            PaneOwnership::AttentionBlind { id, state } => {
                assert_eq!(id.kind, AgentKind::Codex);
                assert_eq!(state, AgentState::Idle);
            }
            other => panic!("a just-launched codex row must be refinable, got {other:?}"),
        }
    }

    /// ...and the carve-out is bounded on both axes: it closes once the window
    /// elapses, and it never applies to a row that already reported a state,
    /// since that state is itself proof the hooks are alive.
    #[test]
    fn startup_carve_out_is_bounded_by_time_and_by_state() {
        let stale = row(AgentKind::Codex, "real", AgentState::Idle);
        assert!(
            matches!(
                pane_ownership(&[stale], AT + STARTUP_ATTENTION_WINDOW),
                PaneOwnership::Hooked
            ),
            "the window must close, or screen inference races hooks forever",
        );

        let reported = row(AgentKind::Codex, "real", AgentState::Working);
        assert!(
            matches!(
                pane_ownership(&[reported], AT + Duration::seconds(3)),
                PaneOwnership::Hooked
            ),
            "a row that already left Idle has live hooks; stay out of its way",
        );
    }

    /// The registry names the agent on a pane; `pane_current_command` only
    /// names the process. For an npm-installed codex those disagree (`node`),
    /// which is why manifest selection consults the kind first.
    #[test]
    fn codex_kind_names_its_manifest() {
        assert_eq!(AgentKind::Codex.screen_manifest_name(), Some("codex"));
        assert_eq!(AgentKind::Antigravity.screen_manifest_name(), Some("agy"));
        // Kinds whose hooks fully cover attention ship no manifest.
        assert_eq!(AgentKind::ClaudeCode.screen_manifest_name(), None);

        let set = muxa::screen::load_manifests();
        assert!(
            set.manifest_for_name("codex").is_some(),
            "every name screen_manifest_name returns must resolve",
        );
        assert!(set.manifest_for_name("agy").is_some());
    }

    fn refine(owner_state: AgentState, observed: SyntheticState) -> Vec<AgentEvent> {
        attention_refinement_events(
            AgentId {
                kind: AgentKind::Antigravity,
                session_id: "conv-1".into(),
                surface: None,
                pane: Some("%1".into()),
                tmux_socket: None,
                cwd: None,
            },
            owner_state,
            observed,
            "agy",
            None,
            AT,
        )
    }

    #[test]
    fn refinement_raises_attention_on_a_blocked_screen() {
        for owner_state in [AgentState::Working, AgentState::Idle, AgentState::Starting] {
            match refine(owner_state, SyntheticState::Blocked).as_slice() {
                [AgentEvent::NotificationFired {
                    level, message, id, ..
                }] => {
                    assert_eq!(*level, NotificationLevel::NeedsInput);
                    assert_eq!(message, "agy is waiting");
                    assert_eq!(id.session_id, "conv-1", "must target the real row");
                }
                other => panic!("expected one NotificationFired for {owner_state}, got {other:?}"),
            }
        }
    }

    /// No `Heartbeat` — the real row's `model` came from its hooks
    /// (`gemini-3.7-flash-high`), and stamping the manifest name over it would
    /// destroy real information. This is the difference from `state_events`.
    #[test]
    fn refinement_never_emits_a_heartbeat() {
        for owner_state in [
            AgentState::Working,
            AgentState::Idle,
            AgentState::WaitingInput,
            AgentState::Error,
        ] {
            for observed in [
                SyntheticState::Working,
                SyntheticState::Blocked,
                SyntheticState::Idle,
            ] {
                assert!(
                    !refine(owner_state, observed)
                        .iter()
                        .any(|e| matches!(e, AgentEvent::Heartbeat { .. })),
                    "{owner_state} + {observed:?} must not stamp the model field",
                );
            }
        }
    }

    /// Hooks own `Working`: they report it in milliseconds, a screen tick is
    /// seconds late, so racing them buys nothing and can only fight.
    #[test]
    fn refinement_ignores_a_working_screen() {
        for owner_state in [
            AgentState::Working,
            AgentState::Idle,
            AgentState::WaitingInput,
        ] {
            assert!(refine(owner_state, SyntheticState::Working).is_empty());
        }
    }

    /// Idle only ever *clears* a stuck wait; on a row that isn't waiting it is
    /// the hooks' business.
    #[test]
    fn refinement_idle_clears_only_a_waiting_row() {
        for owner_state in [AgentState::WaitingInput, AgentState::WaitingChoice] {
            match refine(owner_state, SyntheticState::Idle).as_slice() {
                [AgentEvent::TurnStopped {
                    idle_confirmed,
                    response,
                    id,
                    ..
                }] => {
                    assert!(*idle_confirmed, "must be positive idle evidence");
                    assert_eq!(*response, None);
                    assert_eq!(id.session_id, "conv-1");
                }
                other => panic!("expected TurnStopped for {owner_state}, got {other:?}"),
            }
        }
        for owner_state in [AgentState::Working, AgentState::Idle, AgentState::Error] {
            assert!(refine(owner_state, SyntheticState::Idle).is_empty());
        }
    }

    /// Re-observing the same prompt must not re-fire, and an `Error` the hooks
    /// reported must not be masked by a wait.
    #[test]
    fn refinement_is_idempotent_and_preserves_error() {
        assert!(refine(AgentState::WaitingInput, SyntheticState::Blocked).is_empty());
        assert!(refine(AgentState::WaitingChoice, SyntheticState::Blocked).is_empty());
        assert!(refine(AgentState::Error, SyntheticState::Blocked).is_empty());
    }

    /// The whole point, end to end against a real store: an agy row that hooks
    /// drove to `Working` reaches `WaitingInput` from a blocked screen, and its
    /// hook-supplied `model` survives.
    #[tokio::test]
    async fn refinement_moves_a_real_agy_row_into_waiting_input() {
        let store = Store::shared();
        let real = AgentId {
            kind: AgentKind::Antigravity,
            session_id: "conv-1".into(),
            surface: None,
            pane: Some("%1".into()),
            tmux_socket: None,
            cwd: None,
        };
        store
            .apply(&AgentEvent::Started {
                id: real.clone(),
                at: AT,
            })
            .await;
        store
            .apply(&AgentEvent::Heartbeat {
                id: real.clone(),
                model: Some("gemini-3.7-flash-high".into()),
                context_used_pct: None,
                cost_usd: None,
                rate_limit_5h_pct: None,
                rate_limit_5h_resets_at: None,
                rate_limit_7d_pct: None,
                rate_limit_7d_resets_at: None,
                at: AT,
            })
            .await;
        store
            .apply(&AgentEvent::ToolStarted {
                id: real.clone(),
                tool: "run_command".into(),
                subagent: None,
                at: AT,
            })
            .await;
        assert_eq!(store.by_pane("%1").await[0].state, AgentState::Working);

        let occupants = store.by_pane("%1").await;
        let PaneOwnership::AttentionBlind { id, state } =
            pane_ownership(&occupants, OffsetDateTime::now_utc())
        else {
            panic!("an agy row must be attention-blind");
        };
        for ev in attention_refinement_events(id, state, SyntheticState::Blocked, "agy", None, AT) {
            store.apply(&ev).await;
        }

        let rows = store.by_pane("%1").await;
        assert_eq!(rows.len(), 1, "refinement must not mint a second row");
        assert_eq!(rows[0].session_id, "conv-1");
        assert_eq!(rows[0].state, AgentState::WaitingInput);
        assert_eq!(
            rows[0].model.as_deref(),
            Some("gemini-3.7-flash-high"),
            "the hook-supplied model must survive refinement",
        );

        // And the agy hook stream releases it again, unaided.
        store
            .apply(&AgentEvent::ToolCompleted {
                id: real,
                tool: "run_command".into(),
                success: true,
                at: AT,
            })
            .await;
        assert_eq!(store.by_pane("%1").await[0].state, AgentState::Working);
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
            last_prompt_at: None,
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
