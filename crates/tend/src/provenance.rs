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
}

impl StateProvenance {
    #[must_use]
    pub const fn new(source: StateSource, at: OffsetDateTime) -> Self {
        Self { source, at }
    }
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
