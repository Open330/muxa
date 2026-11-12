//! Where an agent row's current state came from.
//!
//! Several producers write agent rows: provider hooks, the Codex rollout
//! poll, screen detection, the herdr bridge, discovery, and the
//! reconciler's sweeps. Each state change now records which kind of signal
//! caused it, so a guess can be told from a fact — by an operator (`tend
//! status --json`, `tend doctor`) and by the registry itself when two
//! producers disagree. See `docs/proposals/DETERMINISTIC_FIRST.md` §1.1.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// The kind of signal behind a state change.
///
/// Deterministic sources report a fact the provider, the OS, the
/// multiplexer or tend itself established. Heuristic sources infer one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateSource {
    /// A provider hook event (Claude/Codex/agy/opencode hooks, statusline).
    Hook,
    /// A provider-written session or transcript record (Codex rollout).
    ProviderRecord,
    /// A provider API answer or provider-issued id.
    ProviderApi,
    /// A mark tend set at launch, or a tend-owned PTY's own lifecycle.
    LaunchMark,
    /// The exact process: a tracked pid's liveness, or its argv.
    ProcessExact,
    /// The multiplexer's own pane inventory (a pane is gone).
    PaneInventory,
    /// An action tend itself carried out (a drain it finished, an Ask run
    /// it ended).
    Control,
    /// A process matched by program name (discovery, "no agent process").
    ProcessName,
    /// Pane capture text (screen manifests, dialog recovery).
    Screen,
    /// The pane host's own agent detection (herdr).
    HostReport,
    /// Correlation by elimination (the only candidate left, matching cwd).
    Correlation,
    /// A timer: staleness, inactivity, row caps.
    Timing,
}

impl StateSource {
    /// Whether this source reports a fact rather than an inference.
    #[must_use]
    pub const fn is_deterministic(self) -> bool {
        matches!(
            self,
            Self::Hook
                | Self::ProviderRecord
                | Self::ProviderApi
                | Self::LaunchMark
                | Self::ProcessExact
                | Self::PaneInventory
                | Self::Control
        )
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Hook => "hook",
            Self::ProviderRecord => "provider_record",
            Self::ProviderApi => "provider_api",
            Self::LaunchMark => "launch_mark",
            Self::ProcessExact => "process_exact",
            Self::PaneInventory => "pane_inventory",
            Self::Control => "control",
            Self::ProcessName => "process_name",
            Self::Screen => "screen",
            Self::HostReport => "host_report",
            Self::Correlation => "correlation",
            Self::Timing => "timing",
        }
    }
}

impl std::fmt::Display for StateSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The source of a row's current state and when that source observed it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateProvenance {
    pub source: StateSource,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    /// The newest deterministic observation this row's state has had,
    /// kept across later heuristic refinements. Its presence means a
    /// deterministic source has described the row; it is the watermark
    /// [`admit`] holds observations to. `None` while only heuristics have.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub deterministic_at: Option<OffsetDateTime>,
}

impl StateProvenance {
    /// A first observation, with no history to carry.
    #[must_use]
    pub const fn new(source: StateSource, at: OffsetDateTime) -> Self {
        Self {
            source,
            at,
            deterministic_at: if source.is_deterministic() {
                Some(at)
            } else {
                None
            },
        }
    }

    /// The provenance after `source` sets the state at `at`, keeping the
    /// deterministic watermark of `previous`.
    #[must_use]
    pub fn advance(previous: Option<Self>, source: StateSource, at: OffsetDateTime) -> Self {
        let carried = previous.and_then(|previous| previous.deterministic_at);
        let deterministic_at = if source.is_deterministic() {
            Some(carried.map_or(at, |carried| carried.max(at)))
        } else {
            carried
        };
        Self {
            source,
            at,
            deterministic_at,
        }
    }
}

/// Why an observation was not allowed to change a row's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The row holds a deterministic state observed at or after this one.
    Superseded,
    /// A heuristic would stop a row whose state a deterministic source set.
    HeuristicStop,
}

/// The single precedence rule for daemon observations of a row's state.
///
/// `watermark` is the newest deterministic observation of the row's state
/// (see [`StateProvenance::deterministic_at`]); `None` when only heuristics
/// have described it. `incoming` is the observation's source, `incoming_at`
/// when it was observed, and `stops` whether it would take the row to
/// `Stopped`.
///
/// 1. Nothing changes a deterministically described row unless it was
///    observed strictly *after* the newest deterministic observation: an
///    older rollout record or screen capture cannot undo a newer hook, and
///    re-reading an already applied record is not news — also after a
///    heuristic has since refined the row.
/// 2. A heuristic alone never stops a row a deterministic source has
///    described. It may stop a row only heuristics ever described (a
///    screen-detected or discovered placeholder).
///
/// A heuristic may still refine a deterministic row with a *newer*
/// observation that does not stop it (a resolved dialog clearing
/// `WaitingInput`); that only ever makes tend do less.
///
/// Hook events are not filtered here: they are the authority this rule
/// protects, applied in the order they arrive.
pub fn admit(
    watermark: Option<OffsetDateTime>,
    incoming: StateSource,
    incoming_at: OffsetDateTime,
    stops: bool,
) -> Result<(), Refusal> {
    let Some(watermark) = watermark else {
        return Ok(());
    };
    if incoming_at <= watermark {
        return Err(Refusal::Superseded);
    }
    if stops && !incoming.is_deterministic() {
        return Err(Refusal::HeuristicStop);
    }
    Ok(())
}

/// IPC capability advertising `Agent::state_source` on agent rows.
pub const STATE_SOURCE_CAPABILITY: &str = "agent_state_source_v1";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_and_heuristic_sources_are_split_as_documented() {
        for source in [
            StateSource::Hook,
            StateSource::ProviderRecord,
            StateSource::ProviderApi,
            StateSource::LaunchMark,
            StateSource::ProcessExact,
            StateSource::PaneInventory,
            StateSource::Control,
        ] {
            assert!(source.is_deterministic(), "{source}");
        }
        for source in [
            StateSource::ProcessName,
            StateSource::Screen,
            StateSource::HostReport,
            StateSource::Correlation,
            StateSource::Timing,
        ] {
            assert!(!source.is_deterministic(), "{source}");
        }
    }

    #[test]
    fn the_precedence_rule() {
        let t = OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1);
        let earlier = t - time::Duration::seconds(1);
        let later = t + time::Duration::seconds(1);
        let hooked = Some(t);
        // Older or equal evidence never changes a deterministic row.
        assert_eq!(
            admit(hooked, StateSource::ProviderRecord, earlier, false),
            Err(Refusal::Superseded)
        );
        assert_eq!(
            admit(hooked, StateSource::ProviderRecord, t, false),
            Err(Refusal::Superseded),
            "the same instant re-read is not news"
        );
        assert_eq!(
            admit(hooked, StateSource::Screen, earlier, false),
            Err(Refusal::Superseded)
        );
        // Newer deterministic evidence wins, including a stop.
        assert_eq!(
            admit(hooked, StateSource::ProviderRecord, later, true),
            Ok(())
        );
        // A newer heuristic may refine but never stop a deterministic row.
        assert_eq!(admit(hooked, StateSource::Screen, later, false), Ok(()));
        assert_eq!(
            admit(hooked, StateSource::Screen, later, true),
            Err(Refusal::HeuristicStop)
        );
        // A row only heuristics described is theirs to change, and to stop.
        assert_eq!(admit(None, StateSource::Screen, earlier, true), Ok(()));
    }

    #[test]
    fn a_heuristic_refinement_keeps_the_deterministic_watermark() {
        let t = OffsetDateTime::UNIX_EPOCH + time::Duration::hours(1);
        let hook = StateProvenance::new(StateSource::Hook, t);
        assert_eq!(hook.deterministic_at, Some(t));
        let refined = StateProvenance::advance(
            Some(hook),
            StateSource::Timing,
            t + time::Duration::minutes(10),
        );
        assert_eq!(refined.source, StateSource::Timing);
        assert_eq!(refined.deterministic_at, Some(t));
        let rechecked = StateProvenance::advance(
            Some(refined),
            StateSource::ProviderRecord,
            t + time::Duration::minutes(11),
        );
        assert_eq!(
            rechecked.deterministic_at,
            Some(t + time::Duration::minutes(11))
        );
        assert_eq!(
            StateProvenance::new(StateSource::Screen, t).deterministic_at,
            None
        );
    }

    #[test]
    fn serializes_as_snake_case() {
        let provenance =
            StateProvenance::new(StateSource::ProviderRecord, OffsetDateTime::UNIX_EPOCH);
        let json = serde_json::to_value(provenance).unwrap();
        assert_eq!(json["source"], "provider_record");
        assert_eq!(
            serde_json::to_value(StateSource::PaneInventory).unwrap(),
            StateSource::PaneInventory.as_str()
        );
    }
}
