//! Timeline projection over the activity ledger.
//!
//! The activity ledger is append-only and transition-shaped. This module
//! turns those rows into clipped intervals that UIs can render as lanes.

use crate::activity::{ActivityEntry, HumanInteractionKind};
use crate::event::{AgentKind, AgentState};
use crate::history::HistoryEntry;
use crate::scope_filter::ScopeExclusions;
use crate::session_activity::SessionActivity;
use crate::state::{Agent, SYNTHETIC_SESSION_PREFIX};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime, UtcOffset, Weekday};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TimelineRange {
    pub label: String,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub since_at: Option<OffsetDateTime>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        with = "time::serde::rfc3339::option"
    )]
    pub until_at: Option<OffsetDateTime>,
}

impl TimelineRange {
    #[must_use]
    pub fn includes_end(&self, at: OffsetDateTime) -> bool {
        self.since_at.is_none_or(|since| at >= since)
            && self.until_at.is_none_or(|until| at < until)
    }

    #[must_use]
    pub fn effective_end(&self, now: OffsetDateTime) -> OffsetDateTime {
        self.until_at.map_or(now, |until| until.min(now))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TimelineDocument {
    #[serde(with = "time::serde::rfc3339")]
    pub generated_at: OffsetDateTime,
    pub range: TimelineRange,
    #[serde(with = "time::serde::rfc3339")]
    pub window_started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub window_ended_at: OffsetDateTime,
    pub lanes: Vec<TimelineLane>,
    pub totals: TimelineTotals,
    pub active_sessions: Vec<TimelineActiveSession>,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimelineLane {
    pub id: String,
    pub label: String,
    pub kind: TimelineLaneKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_kind: Option<AgentKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    pub totals: TimelineTotals,
    pub intervals: Vec<TimelineInterval>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TimelineLaneKind {
    Agent,
    Human,
    Tmux,
}

#[derive(Debug, Clone, Serialize)]
pub struct TimelineInterval {
    pub source: TimelineIntervalSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<AgentState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_kind: Option<HumanInteractionKind>,
    #[serde(with = "time::serde::rfc3339")]
    pub started_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    pub ended_at: OffsetDateTime,
    pub duration_secs: u64,
    pub open: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pane: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum TimelineIntervalSource {
    AgentState,
    HumanInteraction,
    SessionForeground,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct TimelineTotals {
    pub active_secs: u64,
    pub working_secs: u64,
    pub waiting_secs: u64,
    pub error_secs: u64,
    pub idle_secs: u64,
    pub starting_secs: u64,
    pub stopped_secs: u64,
    pub human_secs: u64,
    pub foreground_secs: u64,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct TimelineActiveSession {
    pub label: String,
    pub active_secs: u64,
}

#[derive(Debug, Clone, Default)]
pub struct TimelineFilters {
    pub session: Option<String>,
    pub agent_kind: Option<AgentKind>,
    pub exclusions: ScopeExclusions,
}

pub struct TimelineBuildInput<'a> {
    pub now: OffsetDateTime,
    pub range: TimelineRange,
    pub prompt_entries: &'a [HistoryEntry],
    pub activity_entries: &'a [ActivityEntry],
    pub agents: &'a [Agent],
    pub session_activities: &'a [SessionActivity],
    pub pane_sessions: &'a HashMap<String, String>,
    pub active_lookback_secs: u64,
    pub active_timeout_secs: u64,
    pub active_tick_timeout_secs: u64,
    /// Whether tmux input ticks (keypress / scroll) seed the engaged-ACTIVE lane.
    /// `false` drops them so ACTIVE anchors only on prompts and thinking — see
    /// `StatsConfig::count_tmux_input` for why (mouse motion is indistinguishable
    /// from keypresses behind tmux `#{client_activity}`).
    pub count_tmux_input: bool,
    pub filters: TimelineFilters,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TimelineSummaryProjection {
    pub generated_at: OffsetDateTime,
    pub range: TimelineRange,
    pub window_started_at: OffsetDateTime,
    pub window_ended_at: OffsetDateTime,
    pub totals: TimelineTotals,
    pub active_sessions: Vec<TimelineActiveSession>,
    pub notes: Vec<String>,
    pub sessions: Vec<TimelineSessionProjection>,
    pub days: Vec<TimelineDayProjection>,
    pub sources: Vec<TimelineSourceProjection>,
    pub human_presence_secs: u64,
}

#[derive(Debug, Clone)]
pub struct TimelineSessionProjection {
    pub label: String,
    pub lanes: usize,
    pub latest_at: Option<OffsetDateTime>,
    pub totals: TimelineTotals,
    pub human_presence_secs: u64,
}

#[derive(Debug, Clone)]
pub struct TimelineDayProjection {
    pub date: String,
    pub totals: TimelineTotals,
    pub top_sessions: Vec<TimelineActiveSession>,
}

#[derive(Debug, Clone)]
pub struct TimelineSourceProjection {
    pub kind: TimelineLaneKind,
    pub lanes: usize,
    pub sessions: usize,
    pub totals: TimelineTotals,
}

#[must_use]
#[allow(clippy::too_many_lines)] // central projection pass keeps ledger clipping and live spans together
pub fn build_document(input: TimelineBuildInput<'_>) -> TimelineDocument {
    let mut lanes: BTreeMap<String, LaneAccumulator> = BTreeMap::new();

    for entry in input.activity_entries {
        match entry {
            ActivityEntry::StateTransition(entry) => {
                let session_name = entry.session_name.clone().or_else(|| {
                    entry
                        .pane
                        .as_ref()
                        .and_then(|pane| input.pane_sessions.get(pane))
                        .cloned()
                });
                if !matches_session_filter(
                    &input.filters,
                    Some(&entry.session_id),
                    session_name.as_deref(),
                    entry.pane.as_deref(),
                ) || !matches_agent_filter(&input.filters, Some(entry.kind))
                {
                    continue;
                }
                let started_at = entry.state_entered_at.unwrap_or_else(|| {
                    entry.at
                        - time::Duration::seconds(
                            i64::try_from(entry.duration_secs).unwrap_or(i64::MAX),
                        )
                });
                let Some((started_at, ended_at)) =
                    clip_interval(&input.range, input.now, started_at, entry.at)
                else {
                    continue;
                };
                let lane_id = agent_lane_id(entry.kind, &entry.session_id);
                let label =
                    agent_lane_label(entry.kind, session_name.as_deref(), &entry.session_id);
                let interval = TimelineInterval {
                    source: TimelineIntervalSource::AgentState,
                    state: Some(entry.from),
                    human_kind: None,
                    duration_secs: duration_secs(started_at, ended_at),
                    started_at,
                    ended_at,
                    open: false,
                    pane: entry.pane.clone(),
                    session_id: Some(entry.session_id.clone()),
                    session_name: session_name.clone(),
                    cwd: entry.cwd.clone(),
                    detail: format!("{} {} -> {}", entry.kind, entry.from, entry.to),
                };
                lanes
                    .entry(lane_id.clone())
                    .or_insert_with(|| {
                        LaneAccumulator::agent(
                            lane_id,
                            label,
                            entry.kind,
                            entry.session_id.clone(),
                            session_name,
                        )
                    })
                    .push(interval);
            }
            ActivityEntry::SessionForeground(entry) => {
                if !matches_session_filter(
                    &input.filters,
                    Some(&entry.session_id),
                    Some(&entry.session_name),
                    None,
                ) {
                    continue;
                }
                let Some((started_at, ended_at)) =
                    clip_interval(&input.range, input.now, entry.started_at, entry.ended_at)
                else {
                    continue;
                };
                let lane_id = tmux_lane_id(&entry.session_id, &entry.session_name);
                let interval = TimelineInterval {
                    source: TimelineIntervalSource::SessionForeground,
                    state: None,
                    human_kind: None,
                    duration_secs: duration_secs(started_at, ended_at),
                    started_at,
                    ended_at,
                    open: false,
                    pane: None,
                    session_id: Some(entry.session_id.clone()),
                    session_name: Some(entry.session_name.clone()),
                    cwd: None,
                    detail: "tmux foreground".to_string(),
                };
                lanes
                    .entry(lane_id.clone())
                    .or_insert_with(|| {
                        LaneAccumulator::tmux(
                            lane_id,
                            format!("tmux/{}", entry.session_name),
                            entry.session_id.clone(),
                            entry.session_name.clone(),
                        )
                    })
                    .push(interval);
            }
            ActivityEntry::HumanInteraction(entry) => {
                if !matches_session_filter(
                    &input.filters,
                    entry.session_id.as_deref(),
                    entry.session_name.as_deref(),
                    entry.pane.as_deref(),
                ) {
                    continue;
                }
                let Some((started_at, ended_at)) =
                    clip_interval(&input.range, input.now, entry.started_at, entry.ended_at)
                else {
                    continue;
                };
                let lane_id = human_lane_id(
                    entry.session_id.as_deref(),
                    entry.session_name.as_deref(),
                    entry.pane.as_deref(),
                );
                let label = human_lane_label(
                    entry.session_id.as_deref(),
                    entry.session_name.as_deref(),
                    entry.pane.as_deref(),
                );
                let interval = TimelineInterval {
                    source: TimelineIntervalSource::HumanInteraction,
                    state: None,
                    human_kind: Some(entry.kind),
                    duration_secs: duration_secs(started_at, ended_at),
                    started_at,
                    ended_at,
                    open: false,
                    pane: entry.pane.clone(),
                    session_id: entry.session_id.clone(),
                    session_name: entry.session_name.clone(),
                    cwd: None,
                    detail: format!("human {}", human_kind_label(entry.kind)),
                };
                lanes
                    .entry(lane_id.clone())
                    .or_insert_with(|| {
                        LaneAccumulator::human(
                            lane_id,
                            label,
                            entry.session_id.clone(),
                            entry.session_name.clone(),
                        )
                    })
                    .push(interval);
            }
        }
    }

    for agent in input.agents {
        if agent.state == AgentState::Stopped {
            continue;
        }
        if !live_agent_has_range_activity(agent, &input.range) {
            continue;
        }
        let session_name = agent
            .pane
            .as_ref()
            .and_then(|pane| input.pane_sessions.get(pane))
            .cloned();
        if !matches_session_filter(
            &input.filters,
            Some(&agent.session_id),
            session_name.as_deref(),
            agent.pane.as_deref(),
        ) || !matches_agent_filter(&input.filters, Some(agent.kind))
        {
            continue;
        }
        let Some((started_at, ended_at)) =
            clip_interval(&input.range, input.now, agent.state_entered_at, input.now)
        else {
            continue;
        };
        let lane_id = agent_lane_id(agent.kind, &agent.session_id);
        let label = agent_lane_label(agent.kind, session_name.as_deref(), &agent.session_id);
        let interval = TimelineInterval {
            source: TimelineIntervalSource::AgentState,
            state: Some(agent.state),
            human_kind: None,
            duration_secs: duration_secs(started_at, ended_at),
            started_at,
            ended_at,
            open: true,
            pane: agent.pane.clone(),
            session_id: Some(agent.session_id.clone()),
            session_name: session_name.clone(),
            cwd: agent.cwd.clone(),
            detail: format!("{} {} (open)", agent.kind, agent.state),
        };
        lanes
            .entry(lane_id.clone())
            .or_insert_with(|| {
                LaneAccumulator::agent(
                    lane_id,
                    label,
                    agent.kind,
                    agent.session_id.clone(),
                    session_name,
                )
            })
            .push(interval);
    }

    for activity in input.session_activities {
        let Some(attached_since) = activity.attached_since else {
            continue;
        };
        if !matches_session_filter(
            &input.filters,
            Some(&activity.session_id),
            Some(&activity.name),
            None,
        ) {
            continue;
        }
        let Some((started_at, ended_at)) =
            clip_interval(&input.range, input.now, attached_since, input.now)
        else {
            continue;
        };
        let lane_id = tmux_lane_id(&activity.session_id, &activity.name);
        let interval = TimelineInterval {
            source: TimelineIntervalSource::SessionForeground,
            state: None,
            human_kind: None,
            duration_secs: duration_secs(started_at, ended_at),
            started_at,
            ended_at,
            open: true,
            pane: None,
            session_id: Some(activity.session_id.clone()),
            session_name: Some(activity.name.clone()),
            cwd: None,
            detail: "tmux foreground (open)".to_string(),
        };
        lanes
            .entry(lane_id.clone())
            .or_insert_with(|| {
                LaneAccumulator::tmux(
                    lane_id,
                    format!("tmux/{}", activity.name),
                    activity.session_id.clone(),
                    activity.name.clone(),
                )
            })
            .push(interval);
    }

    let mut lanes = lanes
        .into_values()
        .map(LaneAccumulator::finish)
        .collect::<Vec<_>>();
    lanes.sort_by(|a, b| {
        lane_rank(a.kind)
            .cmp(&lane_rank(b.kind))
            .then_with(|| a.label.cmp(&b.label))
    });

    let mut window_started_at = input.range.since_at.unwrap_or_else(|| {
        lanes
            .iter()
            .flat_map(|lane| lane.intervals.iter().map(|interval| interval.started_at))
            .min()
            .unwrap_or(input.now - time::Duration::hours(1))
    });
    let window_ended_at = input.range.effective_end(input.now);
    if window_ended_at <= window_started_at {
        window_started_at = window_ended_at - time::Duration::seconds(1);
    }
    if input.range.since_at.is_none() {
        window_started_at = lanes
            .iter()
            .flat_map(|lane| lane.intervals.iter().map(|interval| interval.started_at))
            .min()
            .unwrap_or(window_started_at);
    }

    let mut totals = lanes
        .iter()
        .fold(TimelineTotals::default(), |mut acc, lane| {
            acc.add_totals(&lane.totals);
            acc
        });
    let active_sessions = active_sessions_for_input(&input);
    totals.active_secs = active_sessions
        .iter()
        .map(|session| session.active_secs)
        .sum();
    let mut notes = input.notes;
    if lanes.is_empty() {
        notes.push("no timeline intervals in this view".to_string());
    }

    TimelineDocument {
        generated_at: input.now,
        range: input.range,
        window_started_at,
        window_ended_at,
        lanes,
        totals,
        active_sessions,
        notes,
    }
}

#[must_use]
#[allow(clippy::too_many_lines)]
pub fn build_summary(
    input: TimelineBuildInput<'_>,
    timezone_offset: UtcOffset,
) -> TimelineSummaryProjection {
    let mut lanes = BTreeMap::<String, SummaryLaneAccumulator<'_>>::new();

    for entry in input.activity_entries {
        match entry {
            ActivityEntry::StateTransition(entry) => {
                let session_name = entry.session_name.as_deref().or_else(|| {
                    entry
                        .pane
                        .as_ref()
                        .and_then(|pane| input.pane_sessions.get(pane))
                        .map(String::as_str)
                });
                if !matches_session_filter(
                    &input.filters,
                    Some(&entry.session_id),
                    session_name,
                    entry.pane.as_deref(),
                ) || !matches_agent_filter(&input.filters, Some(entry.kind))
                {
                    continue;
                }
                let started_at = entry.state_entered_at.unwrap_or_else(|| {
                    entry.at
                        - time::Duration::seconds(
                            i64::try_from(entry.duration_secs).unwrap_or(i64::MAX),
                        )
                });
                let Some((started_at, ended_at)) =
                    clip_interval(&input.range, input.now, started_at, entry.at)
                else {
                    continue;
                };
                push_summary_span(
                    &mut lanes,
                    agent_lane_id(entry.kind, &entry.session_id),
                    TimelineLaneKind::Agent,
                    session_name.unwrap_or(&entry.session_id),
                    session_name.unwrap_or(&entry.session_id),
                    SummarySpan {
                        source: TimelineIntervalSource::AgentState,
                        state: Some(entry.from),
                        human_kind: None,
                        started_at,
                        ended_at,
                        open: false,
                        pane: entry.pane.as_deref(),
                        session_id: Some(&entry.session_id),
                        session_name,
                        cwd: entry.cwd.as_deref(),
                    },
                );
            }
            ActivityEntry::SessionForeground(entry) => {
                if !matches_session_filter(
                    &input.filters,
                    Some(&entry.session_id),
                    Some(&entry.session_name),
                    None,
                ) {
                    continue;
                }
                let Some((started_at, ended_at)) =
                    clip_interval(&input.range, input.now, entry.started_at, entry.ended_at)
                else {
                    continue;
                };
                push_summary_span(
                    &mut lanes,
                    tmux_lane_id(&entry.session_id, &entry.session_name),
                    TimelineLaneKind::Tmux,
                    &entry.session_name,
                    &entry.session_name,
                    SummarySpan {
                        source: TimelineIntervalSource::SessionForeground,
                        state: None,
                        human_kind: None,
                        started_at,
                        ended_at,
                        open: false,
                        pane: None,
                        session_id: Some(&entry.session_id),
                        session_name: Some(&entry.session_name),
                        cwd: None,
                    },
                );
            }
            ActivityEntry::HumanInteraction(entry) => {
                if !matches_session_filter(
                    &input.filters,
                    entry.session_id.as_deref(),
                    entry.session_name.as_deref(),
                    entry.pane.as_deref(),
                ) {
                    continue;
                }
                let Some((started_at, ended_at)) =
                    clip_interval(&input.range, input.now, entry.started_at, entry.ended_at)
                else {
                    continue;
                };
                let lane_label = human_lane_label(
                    entry.session_id.as_deref(),
                    entry.session_name.as_deref(),
                    entry.pane.as_deref(),
                );
                push_summary_span(
                    &mut lanes,
                    human_lane_id(
                        entry.session_id.as_deref(),
                        entry.session_name.as_deref(),
                        entry.pane.as_deref(),
                    ),
                    TimelineLaneKind::Human,
                    entry
                        .session_name
                        .as_deref()
                        .or(entry.session_id.as_deref())
                        .unwrap_or("no session"),
                    entry
                        .session_name
                        .as_deref()
                        .or(entry.session_id.as_deref())
                        .unwrap_or(&lane_label),
                    SummarySpan {
                        source: TimelineIntervalSource::HumanInteraction,
                        state: None,
                        human_kind: Some(entry.kind),
                        started_at,
                        ended_at,
                        open: false,
                        pane: entry.pane.as_deref(),
                        session_id: entry.session_id.as_deref(),
                        session_name: entry.session_name.as_deref(),
                        cwd: None,
                    },
                );
            }
        }
    }

    for agent in input.agents {
        if agent.state == AgentState::Stopped || !live_agent_has_range_activity(agent, &input.range)
        {
            continue;
        }
        let session_name = agent
            .pane
            .as_ref()
            .and_then(|pane| input.pane_sessions.get(pane))
            .map(String::as_str);
        if !matches_session_filter(
            &input.filters,
            Some(&agent.session_id),
            session_name,
            agent.pane.as_deref(),
        ) || !matches_agent_filter(&input.filters, Some(agent.kind))
        {
            continue;
        }
        let Some((started_at, ended_at)) =
            clip_interval(&input.range, input.now, agent.state_entered_at, input.now)
        else {
            continue;
        };
        push_summary_span(
            &mut lanes,
            agent_lane_id(agent.kind, &agent.session_id),
            TimelineLaneKind::Agent,
            session_name.unwrap_or(&agent.session_id),
            session_name.unwrap_or(&agent.session_id),
            SummarySpan {
                source: TimelineIntervalSource::AgentState,
                state: Some(agent.state),
                human_kind: None,
                started_at,
                ended_at,
                open: true,
                pane: agent.pane.as_deref(),
                session_id: Some(&agent.session_id),
                session_name,
                cwd: agent.cwd.as_deref(),
            },
        );
    }

    for activity in input.session_activities {
        let Some(attached_since) = activity.attached_since else {
            continue;
        };
        if !matches_session_filter(
            &input.filters,
            Some(&activity.session_id),
            Some(&activity.name),
            None,
        ) {
            continue;
        }
        let Some((started_at, ended_at)) =
            clip_interval(&input.range, input.now, attached_since, input.now)
        else {
            continue;
        };
        push_summary_span(
            &mut lanes,
            tmux_lane_id(&activity.session_id, &activity.name),
            TimelineLaneKind::Tmux,
            &activity.name,
            &activity.name,
            SummarySpan {
                source: TimelineIntervalSource::SessionForeground,
                state: None,
                human_kind: None,
                started_at,
                ended_at,
                open: true,
                pane: None,
                session_id: Some(&activity.session_id),
                session_name: Some(&activity.name),
                cwd: None,
            },
        );
    }

    let lanes = lanes
        .into_values()
        .map(SummaryLaneAccumulator::finish)
        .collect::<Vec<_>>();
    finish_summary_projection(input, timezone_offset, lanes)
}

pub fn parse_since(
    raw: &str,
    now: OffsetDateTime,
    all_label: &str,
) -> Result<TimelineRange, String> {
    let trimmed = raw.trim();
    let normalized = normalize_since(trimmed);
    if normalized == "all" {
        return Ok(TimelineRange {
            label: all_label.to_string(),
            since_at: None,
            until_at: None,
        });
    }

    let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
    if let Some(date) =
        parse_iso_date(trimmed).map_err(|error| format!("{error}\n{}", since_help()))?
    {
        let next = date
            .next_day()
            .ok_or_else(|| "could not compute next date".to_string())?;
        return Ok(TimelineRange {
            label: date.to_string(),
            since_at: Some(local_day_start(date, offset)),
            until_at: Some(local_day_start(next, offset)),
        });
    }

    if let Some(range) = parse_keyword_since(&normalized, now, offset)? {
        return Ok(range);
    }

    if let Ok(at) = OffsetDateTime::parse(trimmed, &Rfc3339) {
        return Ok(TimelineRange {
            label: format!("since {trimmed}"),
            since_at: Some(at),
            until_at: None,
        });
    }
    if trimmed.is_empty() {
        return Err(invalid_since_value(trimmed));
    }

    parse_duration_since(trimmed, now)
}

fn normalize_since(raw: &str) -> String {
    raw.trim().to_ascii_lowercase().replace(['-', ' '], "_")
}

fn parse_keyword_since(
    normalized: &str,
    now: OffsetDateTime,
    offset: UtcOffset,
) -> Result<Option<TimelineRange>, String> {
    match normalized {
        "today" | "tod" => {
            let start = local_day_start(now.to_offset(offset).date(), offset);
            Ok(Some(TimelineRange {
                label: "today".to_string(),
                since_at: Some(start),
                until_at: None,
            }))
        }
        "yesterday" | "yday" => {
            let today = now.to_offset(offset).date();
            let yesterday = today
                .previous_day()
                .ok_or_else(|| "could not compute yesterday date".to_string())?;
            Ok(Some(TimelineRange {
                label: "yesterday".to_string(),
                since_at: Some(local_day_start(yesterday, offset)),
                until_at: Some(local_day_start(today, offset)),
            }))
        }
        "last_week" | "lastweek" | "previous_week" | "previousweek" | "prev_week" | "prevweek" => {
            let (since_at, until_at) = previous_calendar_week(now, offset)?;
            Ok(Some(TimelineRange {
                label: "last week".to_string(),
                since_at: Some(since_at),
                until_at: Some(until_at),
            }))
        }
        "last_month" | "lastmonth" | "previous_month" | "previousmonth" | "prev_month"
        | "prevmonth" => {
            let (since_at, until_at) = previous_calendar_month(now, offset)?;
            Ok(Some(TimelineRange {
                label: "last month".to_string(),
                since_at: Some(since_at),
                until_at: Some(until_at),
            }))
        }
        "week" | "last7d" | "last_7d" | "7days" => Ok(Some(TimelineRange {
            label: "last 7d".to_string(),
            since_at: Some(now - time::Duration::days(7)),
            until_at: None,
        })),
        "month" | "last30d" | "last_30d" | "30days" => Ok(Some(TimelineRange {
            label: "last 30d".to_string(),
            since_at: Some(now - time::Duration::days(30)),
            until_at: None,
        })),
        _ => Ok(None),
    }
}

fn parse_duration_since(trimmed: &str, now: OffsetDateTime) -> Result<TimelineRange, String> {
    let unit = trimmed
        .chars()
        .last()
        .ok_or_else(|| invalid_since_value(trimmed))?;
    let number = &trimmed[..trimmed.len() - unit.len_utf8()];
    let amount: i64 = number.parse().map_err(|_| invalid_since_value(trimmed))?;
    if amount <= 0 {
        return Err(format!(
            "invalid --since duration {trimmed:?}: amount must be greater than zero\n{}",
            since_help()
        ));
    }
    let duration = match unit {
        's' => time::Duration::seconds(amount),
        'm' => time::Duration::minutes(amount),
        'h' => time::Duration::hours(amount),
        'd' => time::Duration::days(amount),
        'w' => time::Duration::weeks(amount),
        _ => {
            return Err(format!(
                "invalid --since duration {trimmed:?}: unit must be one of s, m, h, d, w\n{}",
                since_help()
            ));
        }
    };

    Ok(TimelineRange {
        label: format!("last {trimmed}"),
        since_at: Some(now - duration),
        until_at: None,
    })
}

#[derive(Debug)]
struct LaneAccumulator {
    id: String,
    label: String,
    kind: TimelineLaneKind,
    agent_kind: Option<AgentKind>,
    session_id: Option<String>,
    session_name: Option<String>,
    intervals: Vec<TimelineInterval>,
}

impl LaneAccumulator {
    fn agent(
        id: String,
        label: String,
        agent_kind: AgentKind,
        session_id: String,
        session_name: Option<String>,
    ) -> Self {
        Self {
            id,
            label,
            kind: TimelineLaneKind::Agent,
            agent_kind: Some(agent_kind),
            session_id: Some(session_id),
            session_name,
            intervals: Vec::new(),
        }
    }

    fn human(
        id: String,
        label: String,
        session_id: Option<String>,
        session_name: Option<String>,
    ) -> Self {
        Self {
            id,
            label,
            kind: TimelineLaneKind::Human,
            agent_kind: None,
            session_id,
            session_name,
            intervals: Vec::new(),
        }
    }

    fn tmux(id: String, label: String, session_id: String, session_name: String) -> Self {
        Self {
            id,
            label,
            kind: TimelineLaneKind::Tmux,
            agent_kind: None,
            session_id: Some(session_id),
            session_name: Some(session_name),
            intervals: Vec::new(),
        }
    }

    fn push(&mut self, interval: TimelineInterval) {
        self.intervals.push(interval);
    }

    fn finish(mut self) -> TimelineLane {
        self.intervals
            .sort_by_key(|interval| (interval.started_at, interval.ended_at));
        let mut merged: Vec<TimelineInterval> = Vec::with_capacity(self.intervals.len());
        for interval in self.intervals {
            if let Some(prev) = merged.last_mut() {
                if mergeable(prev, &interval) {
                    prev.ended_at = prev.ended_at.max(interval.ended_at);
                    prev.duration_secs = duration_secs(prev.started_at, prev.ended_at);
                    prev.open |= interval.open;
                    continue;
                }
            }
            merged.push(interval);
        }
        let totals = totals_for_intervals(&merged);
        TimelineLane {
            id: self.id,
            label: self.label,
            kind: self.kind,
            agent_kind: self.agent_kind,
            session_id: self.session_id,
            session_name: self.session_name,
            totals,
            intervals: merged,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct SummarySpan<'a> {
    source: TimelineIntervalSource,
    state: Option<AgentState>,
    human_kind: Option<HumanInteractionKind>,
    started_at: OffsetDateTime,
    ended_at: OffsetDateTime,
    open: bool,
    pane: Option<&'a str>,
    session_id: Option<&'a str>,
    session_name: Option<&'a str>,
    cwd: Option<&'a str>,
}

#[derive(Debug)]
struct SummaryLaneAccumulator<'a> {
    kind: TimelineLaneKind,
    session_label: String,
    day_session_label: String,
    spans: Vec<SummarySpan<'a>>,
}

impl<'a> SummaryLaneAccumulator<'a> {
    fn finish(mut self) -> SummaryLane<'a> {
        self.spans
            .sort_unstable_by_key(|span| (span.started_at, span.ended_at));
        let mut merged = Vec::with_capacity(self.spans.len());
        for span in self.spans {
            if let Some(previous) = merged.last_mut() {
                if summary_spans_mergeable(previous, &span) {
                    previous.ended_at = previous.ended_at.max(span.ended_at);
                    continue;
                }
            }
            merged.push(span);
        }
        let mut totals = TimelineTotals::default();
        for span in &merged {
            add_summary_span_secs(
                &mut totals,
                span,
                duration_secs(span.started_at, span.ended_at),
            );
        }
        SummaryLane {
            kind: self.kind,
            session_label: self.session_label,
            day_session_label: self.day_session_label,
            totals,
            spans: merged,
        }
    }
}

#[derive(Debug)]
struct SummaryLane<'a> {
    kind: TimelineLaneKind,
    session_label: String,
    day_session_label: String,
    totals: TimelineTotals,
    spans: Vec<SummarySpan<'a>>,
}

#[derive(Debug)]
struct SummarySessionAccumulator {
    lanes: usize,
    latest_at: Option<OffsetDateTime>,
    totals: TimelineTotals,
    human_presence: Vec<(OffsetDateTime, OffsetDateTime)>,
}

impl SummarySessionAccumulator {
    fn new() -> Self {
        Self {
            lanes: 0,
            latest_at: None,
            totals: TimelineTotals::default(),
            human_presence: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct SummaryDayAccumulator {
    totals: TimelineTotals,
    sessions: BTreeMap<String, u64>,
}

impl SummaryDayAccumulator {
    fn new() -> Self {
        Self {
            totals: TimelineTotals::default(),
            sessions: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
struct SummarySourceAccumulator {
    lanes: usize,
    sessions: BTreeSet<String>,
    totals: TimelineTotals,
}

struct SummaryAggregation {
    totals: TimelineTotals,
    sessions: BTreeMap<String, SummarySessionAccumulator>,
    sources: [SummarySourceAccumulator; 3],
    days: BTreeMap<Date, SummaryDayAccumulator>,
}

impl SummarySourceAccumulator {
    fn new() -> Self {
        Self {
            lanes: 0,
            sessions: BTreeSet::new(),
            totals: TimelineTotals::default(),
        }
    }
}

fn push_summary_span<'a>(
    lanes: &mut BTreeMap<String, SummaryLaneAccumulator<'a>>,
    lane_id: String,
    kind: TimelineLaneKind,
    session_label: &str,
    day_session_label: &str,
    span: SummarySpan<'a>,
) {
    lanes
        .entry(lane_id)
        .or_insert_with(|| SummaryLaneAccumulator {
            kind,
            session_label: session_label.to_string(),
            day_session_label: day_session_label.to_string(),
            spans: Vec::new(),
        })
        .spans
        .push(span);
}

fn summary_spans_mergeable(previous: &SummarySpan<'_>, next: &SummarySpan<'_>) -> bool {
    previous.source == next.source
        && previous.state == next.state
        && previous.human_kind == next.human_kind
        && previous.open == next.open
        && previous.pane == next.pane
        && previous.session_id == next.session_id
        && previous.session_name == next.session_name
        && previous.cwd == next.cwd
        && previous.ended_at >= next.started_at
}

fn finish_summary_projection(
    input: TimelineBuildInput<'_>,
    timezone_offset: UtcOffset,
    lanes: Vec<SummaryLane<'_>>,
) -> TimelineSummaryProjection {
    let earliest_span = lanes
        .iter()
        .flat_map(|lane| lane.spans.iter().map(|span| span.started_at))
        .min();
    let window_ended_at = input.range.effective_end(input.now);
    let mut window_started_at = input
        .range
        .since_at
        .or(earliest_span)
        .unwrap_or(input.now - time::Duration::hours(1));
    if window_ended_at <= window_started_at {
        window_started_at = window_ended_at - time::Duration::seconds(1);
    }

    let active_sessions = active_sessions_for_input(&input);
    let active_by_session = active_sessions
        .iter()
        .map(|session| (session.label.as_str(), session.active_secs))
        .collect::<HashMap<_, _>>();
    let SummaryAggregation {
        mut totals,
        sessions,
        mut sources,
        days,
    } = aggregate_summary_lanes(&lanes, window_started_at, window_ended_at, timezone_offset);

    totals.active_secs = active_sessions
        .iter()
        .map(|session| session.active_secs)
        .sum();
    sources[0].totals.active_secs = totals.active_secs;
    let sessions = sessions
        .into_iter()
        .map(|(label, mut session)| {
            session.totals.active_secs =
                active_by_session.get(label.as_str()).copied().unwrap_or(0);
            TimelineSessionProjection {
                label,
                lanes: session.lanes,
                latest_at: session.latest_at,
                totals: session.totals,
                human_presence_secs: merged_summary_duration_secs(&mut session.human_presence),
            }
        })
        .collect::<Vec<_>>();
    let human_presence_secs = sessions
        .iter()
        .map(|session| session.human_presence_secs)
        .sum();
    let sources = [
        TimelineLaneKind::Agent,
        TimelineLaneKind::Human,
        TimelineLaneKind::Tmux,
    ]
    .into_iter()
    .zip(sources)
    .map(|(kind, source)| TimelineSourceProjection {
        kind,
        lanes: source.lanes,
        sessions: source.sessions.len(),
        totals: source.totals,
    })
    .collect();
    let days = finish_summary_days(days);
    let mut notes = input.notes;
    if lanes.is_empty() {
        notes.push("no timeline intervals in this view".to_string());
    }

    TimelineSummaryProjection {
        generated_at: input.now,
        range: input.range,
        window_started_at,
        window_ended_at,
        totals,
        active_sessions,
        notes,
        sessions,
        days,
        sources,
        human_presence_secs,
    }
}

fn aggregate_summary_lanes(
    lanes: &[SummaryLane<'_>],
    window_started_at: OffsetDateTime,
    window_ended_at: OffsetDateTime,
    timezone_offset: UtcOffset,
) -> SummaryAggregation {
    let mut aggregation = SummaryAggregation {
        totals: TimelineTotals::default(),
        sessions: BTreeMap::new(),
        sources: [
            SummarySourceAccumulator::new(),
            SummarySourceAccumulator::new(),
            SummarySourceAccumulator::new(),
        ],
        days: summary_days(window_started_at, window_ended_at, timezone_offset),
    };
    for lane in lanes {
        aggregation.totals.add_totals(&lane.totals);
        let source = &mut aggregation.sources[usize::from(lane_rank(lane.kind))];
        source.lanes += 1;
        source.sessions.insert(lane.session_label.clone());
        source.totals.add_totals(&lane.totals);

        let session = aggregation
            .sessions
            .entry(lane.session_label.clone())
            .or_insert_with(SummarySessionAccumulator::new);
        session.lanes += 1;
        session.totals.add_totals(&lane.totals);
        for span in &lane.spans {
            session.latest_at = Some(
                session
                    .latest_at
                    .map_or(span.ended_at, |latest| latest.max(span.ended_at)),
            );
            if matches!(
                span.source,
                TimelineIntervalSource::HumanInteraction
                    | TimelineIntervalSource::SessionForeground
            ) {
                session
                    .human_presence
                    .push((span.started_at, span.ended_at));
            }
            let day_session_label = span
                .session_name
                .or(span.session_id)
                .unwrap_or(&lane.day_session_label);
            add_summary_span_to_days(
                &mut aggregation.days,
                day_session_label,
                span,
                window_started_at,
                window_ended_at,
                timezone_offset,
            );
        }
    }
    aggregation
}

fn summary_days(
    window_started_at: OffsetDateTime,
    window_ended_at: OffsetDateTime,
    timezone_offset: UtcOffset,
) -> BTreeMap<Date, SummaryDayAccumulator> {
    let mut days = BTreeMap::new();
    let mut date = window_started_at.to_offset(timezone_offset).date();
    let final_date = (window_ended_at - time::Duration::nanoseconds(1))
        .to_offset(timezone_offset)
        .date();
    while date <= final_date {
        days.insert(date, SummaryDayAccumulator::new());
        let Some(next) = date.next_day() else {
            break;
        };
        date = next;
    }
    days
}

fn add_summary_span_to_days(
    days: &mut BTreeMap<Date, SummaryDayAccumulator>,
    session_label: &str,
    span: &SummarySpan<'_>,
    window_started_at: OffsetDateTime,
    window_ended_at: OffsetDateTime,
    timezone_offset: UtcOffset,
) {
    let mut cursor = span.started_at.max(window_started_at);
    let ended_at = span.ended_at.min(window_ended_at);
    while cursor < ended_at {
        let date = cursor.to_offset(timezone_offset).date();
        let Some(next_date) = date.next_day() else {
            break;
        };
        let segment_end = ended_at.min(next_date.midnight().assume_offset(timezone_offset));
        let secs = duration_secs(cursor, segment_end);
        if secs == 0 {
            break;
        }
        if let Some(day) = days.get_mut(&date) {
            add_summary_span_secs(&mut day.totals, span, secs);
            if summary_span_is_active(span) {
                let total = day.sessions.entry(session_label.to_string()).or_default();
                *total = total.saturating_add(secs);
            }
        }
        cursor = segment_end;
    }
}

fn finish_summary_days(days: BTreeMap<Date, SummaryDayAccumulator>) -> Vec<TimelineDayProjection> {
    days.into_iter()
        .map(|(date, day)| {
            let mut top_sessions = day.sessions.into_iter().collect::<Vec<_>>();
            top_sessions
                .sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
            top_sessions.truncate(3);
            TimelineDayProjection {
                date: date.to_string(),
                totals: day.totals,
                top_sessions: top_sessions
                    .into_iter()
                    .map(|(label, active_secs)| TimelineActiveSession { label, active_secs })
                    .collect(),
            }
        })
        .collect()
}

fn add_summary_span_secs(totals: &mut TimelineTotals, span: &SummarySpan<'_>, secs: u64) {
    match span.source {
        TimelineIntervalSource::HumanInteraction => {
            totals.human_secs = totals.human_secs.saturating_add(secs);
        }
        TimelineIntervalSource::SessionForeground => {
            totals.foreground_secs = totals.foreground_secs.saturating_add(secs);
        }
        TimelineIntervalSource::AgentState => match span.state {
            Some(AgentState::Working) => {
                totals.working_secs = totals.working_secs.saturating_add(secs);
            }
            Some(AgentState::WaitingInput | AgentState::WaitingChoice) => {
                totals.waiting_secs = totals.waiting_secs.saturating_add(secs);
            }
            Some(AgentState::Error) => {
                totals.error_secs = totals.error_secs.saturating_add(secs);
            }
            Some(AgentState::Idle) => {
                totals.idle_secs = totals.idle_secs.saturating_add(secs);
            }
            Some(AgentState::Starting) => {
                totals.starting_secs = totals.starting_secs.saturating_add(secs);
            }
            Some(AgentState::Stopped) => {
                totals.stopped_secs = totals.stopped_secs.saturating_add(secs);
            }
            None => {}
        },
    }
}

fn summary_span_is_active(span: &SummarySpan<'_>) -> bool {
    match span.source {
        TimelineIntervalSource::HumanInteraction | TimelineIntervalSource::SessionForeground => {
            true
        }
        TimelineIntervalSource::AgentState => matches!(
            span.state,
            Some(
                AgentState::Working
                    | AgentState::WaitingInput
                    | AgentState::WaitingChoice
                    | AgentState::Error
            )
        ),
    }
}

fn merged_summary_duration_secs(intervals: &mut [(OffsetDateTime, OffsetDateTime)]) -> u64 {
    intervals.sort_unstable_by_key(|(started_at, ended_at)| (*started_at, *ended_at));
    let Some(&(mut started_at, mut ended_at)) = intervals.first() else {
        return 0;
    };
    let mut total = 0u64;
    for &(next_start, next_end) in &intervals[1..] {
        if next_start <= ended_at {
            ended_at = ended_at.max(next_end);
        } else {
            total = total.saturating_add(duration_secs(started_at, ended_at));
            started_at = next_start;
            ended_at = next_end;
        }
    }
    total.saturating_add(duration_secs(started_at, ended_at))
}

impl TimelineTotals {
    fn add_interval(&mut self, interval: &TimelineInterval) {
        match interval.source {
            TimelineIntervalSource::AgentState => match interval.state {
                Some(AgentState::Working) => {
                    self.working_secs = self.working_secs.saturating_add(interval.duration_secs);
                }
                Some(AgentState::WaitingInput | AgentState::WaitingChoice) => {
                    self.waiting_secs = self.waiting_secs.saturating_add(interval.duration_secs);
                }
                Some(AgentState::Error) => {
                    self.error_secs = self.error_secs.saturating_add(interval.duration_secs);
                }
                Some(AgentState::Idle) => {
                    self.idle_secs = self.idle_secs.saturating_add(interval.duration_secs);
                }
                Some(AgentState::Starting) => {
                    self.starting_secs = self.starting_secs.saturating_add(interval.duration_secs);
                }
                Some(AgentState::Stopped) => {
                    self.stopped_secs = self.stopped_secs.saturating_add(interval.duration_secs);
                }
                None => {}
            },
            TimelineIntervalSource::HumanInteraction => {
                self.human_secs = self.human_secs.saturating_add(interval.duration_secs);
            }
            TimelineIntervalSource::SessionForeground => {
                self.foreground_secs = self.foreground_secs.saturating_add(interval.duration_secs);
            }
        }
    }

    fn add_totals(&mut self, other: &Self) {
        self.active_secs = self.active_secs.saturating_add(other.active_secs);
        self.working_secs = self.working_secs.saturating_add(other.working_secs);
        self.waiting_secs = self.waiting_secs.saturating_add(other.waiting_secs);
        self.error_secs = self.error_secs.saturating_add(other.error_secs);
        self.idle_secs = self.idle_secs.saturating_add(other.idle_secs);
        self.starting_secs = self.starting_secs.saturating_add(other.starting_secs);
        self.stopped_secs = self.stopped_secs.saturating_add(other.stopped_secs);
        self.human_secs = self.human_secs.saturating_add(other.human_secs);
        self.foreground_secs = self.foreground_secs.saturating_add(other.foreground_secs);
    }
}

#[derive(Debug, Clone)]
struct ActiveScopedInterval {
    started_at: OffsetDateTime,
    ended_at: OffsetDateTime,
    pane: Option<String>,
    session_name: Option<String>,
    scope_key: String,
    group_key: String,
}

#[derive(Debug, Clone)]
struct ActiveAnchor {
    interval: ActiveScopedInterval,
    group_key: String,
    anchor: OffsetDateTime,
}

struct ActiveWindow {
    start: i64,
    end: i64,
    anchor: i128,
    group_key: String,
}

fn active_sessions_for_input(input: &TimelineBuildInput<'_>) -> Vec<TimelineActiveSession> {
    let mut sessions = last_touch_attribution(&active_windows(input))
        .into_iter()
        .filter(|(_, secs)| *secs > 0)
        .map(|(label, active_secs)| TimelineActiveSession { label, active_secs })
        .collect::<Vec<_>>();
    sessions.sort_by(|a, b| a.label.cmp(&b.label));
    sessions
}

fn active_windows(input: &TimelineBuildInput<'_>) -> Vec<ActiveWindow> {
    active_anchor_intervals(input)
        .into_iter()
        .map(|anchor| ActiveWindow {
            start: anchor.interval.started_at.unix_timestamp(),
            end: anchor.interval.ended_at.unix_timestamp(),
            anchor: anchor.anchor.unix_timestamp_nanos(),
            group_key: anchor.group_key,
        })
        .collect()
}

fn active_anchor_intervals(input: &TimelineBuildInput<'_>) -> Vec<ActiveAnchor> {
    let mut intervals = Vec::new();
    let active_lookback = secs_to_duration(input.active_lookback_secs);
    let active_timeout = secs_to_duration(input.active_timeout_secs);
    let active_tick_timeout = secs_to_duration(input.active_tick_timeout_secs);
    let active_presences = active_human_presence_intervals(input, ActivePresenceMode::Active);
    let active_presence_index = ActivePresenceIndex::new(&active_presences);

    for prompt in input.prompt_entries {
        if !input.range.includes_end(prompt.at) {
            continue;
        }
        let session_name = prompt
            .tmux_session
            .clone()
            .or_else(|| input.pane_sessions.get(&prompt.pane).cloned());
        let group_key = session_name
            .clone()
            .unwrap_or_else(|| prompt.session_id.clone());
        if !matches_session_filter(
            &input.filters,
            Some(&prompt.session_id),
            session_name.as_deref(),
            Some(&prompt.pane),
        ) || !matches_agent_filter(&input.filters, Some(prompt.kind))
        {
            continue;
        }
        if let Some(interval) = active_scoped_interval(
            input,
            prompt.at - active_lookback,
            prompt.at + active_timeout,
            Some(prompt.pane.clone()),
            session_name,
            &prompt.session_id,
        ) {
            for segment in overlapping_active_presence_segments(&interval, &active_presence_index) {
                intervals.push(ActiveAnchor {
                    interval: segment,
                    group_key: group_key.clone(),
                    anchor: prompt.at,
                });
            }
        }
    }

    for entry in input
        .count_tmux_input
        .then_some(input.activity_entries)
        .into_iter()
        .flatten()
    {
        let ActivityEntry::HumanInteraction(entry) = entry else {
            continue;
        };
        // Both keypress (TmuxInput) and scrollback (TmuxScroll) ticks count toward
        // the timeline's engaged-ACTIVE lane — unless `count_tmux_input` is off, in
        // which case the loop above never yields (mouse-vs-keypress is ambiguous
        // behind `#{client_activity}`; see `StatsConfig::count_tmux_input`).
        if !entry.kind.is_input_tick() || !input.range.includes_end(entry.ended_at) {
            continue;
        }
        if !matches_session_filter(
            &input.filters,
            entry.session_id.as_deref(),
            entry.session_name.as_deref(),
            entry.pane.as_deref(),
        ) {
            continue;
        }
        if let Some(interval) = active_scoped_interval(
            input,
            entry.started_at - active_lookback,
            entry.ended_at + active_tick_timeout,
            entry.pane.clone(),
            entry.session_name.clone(),
            entry.session_id.as_deref().unwrap_or("human_interaction"),
        ) {
            let group_key = active_human_group_key(&interval);
            for segment in overlapping_active_presence_segments(&interval, &active_presence_index) {
                intervals.push(ActiveAnchor {
                    interval: segment,
                    group_key: group_key.clone(),
                    anchor: entry.ended_at,
                });
            }
        }
    }

    let presences = active_human_presence_intervals(input, ActivePresenceMode::Thinking);
    let presence_index = ActivePresenceIndex::new(&presences);
    for attention in active_attention_intervals(input) {
        for segment in overlapping_active_presence_segments(&attention.interval, &presence_index) {
            let anchor = segment.started_at;
            intervals.push(ActiveAnchor {
                interval: segment,
                group_key: attention.group_key.clone(),
                anchor,
            });
        }
    }

    intervals
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActivePresenceMode {
    Active,
    Thinking,
}

fn active_human_presence_intervals(
    input: &TimelineBuildInput<'_>,
    mode: ActivePresenceMode,
) -> Vec<ActiveScopedInterval> {
    let mut intervals = Vec::new();
    for entry in input.activity_entries {
        match entry {
            ActivityEntry::SessionForeground(entry) => {
                if !matches_session_filter(
                    &input.filters,
                    Some(&entry.session_id),
                    Some(&entry.session_name),
                    None,
                ) {
                    continue;
                }
                if let Some(interval) = active_scoped_interval(
                    input,
                    entry.started_at,
                    entry.ended_at,
                    None,
                    Some(entry.session_name.clone()),
                    &entry.session_id,
                ) {
                    intervals.push(interval);
                }
            }
            ActivityEntry::HumanInteraction(entry) => {
                if entry.kind.is_input_tick() {
                    continue;
                }
                if mode == ActivePresenceMode::Active
                    && !human_interaction_counts_for_active(entry.kind)
                {
                    continue;
                }
                if mode == ActivePresenceMode::Thinking
                    && !human_interaction_counts_for_thinking(entry.kind)
                {
                    continue;
                }
                if !matches_session_filter(
                    &input.filters,
                    entry.session_id.as_deref(),
                    entry.session_name.as_deref(),
                    entry.pane.as_deref(),
                ) {
                    continue;
                }
                if let Some(interval) = active_scoped_interval(
                    input,
                    entry.started_at,
                    entry.ended_at,
                    entry.pane.clone(),
                    entry.session_name.clone(),
                    entry.session_id.as_deref().unwrap_or("human_interaction"),
                ) {
                    intervals.push(interval);
                }
            }
            ActivityEntry::StateTransition(_) => {}
        }
    }

    for activity in input.session_activities {
        let Some(since) = activity.attached_since else {
            continue;
        };
        if !matches_session_filter(
            &input.filters,
            Some(&activity.session_id),
            Some(&activity.name),
            None,
        ) {
            continue;
        }
        if let Some(interval) = active_scoped_interval(
            input,
            since,
            input.now,
            None,
            Some(activity.name.clone()),
            &activity.session_id,
        ) {
            intervals.push(interval);
        }
    }

    intervals
}

fn active_attention_intervals(input: &TimelineBuildInput<'_>) -> Vec<ActiveAnchor> {
    let mut intervals = Vec::new();

    for entry in input.activity_entries {
        let ActivityEntry::StateTransition(entry) = entry else {
            continue;
        };
        if !is_attention_state(entry.from)
            || !matches_agent_filter(&input.filters, Some(entry.kind))
        {
            continue;
        }
        let session_name = entry.session_name.clone().or_else(|| {
            entry
                .pane
                .as_ref()
                .and_then(|pane| input.pane_sessions.get(pane))
                .cloned()
        });
        let group_key = session_name
            .clone()
            .unwrap_or_else(|| entry.session_id.clone());
        if !matches_session_filter(
            &input.filters,
            Some(&entry.session_id),
            session_name.as_deref(),
            entry.pane.as_deref(),
        ) {
            continue;
        }
        let started_at = entry.state_entered_at.unwrap_or_else(|| {
            entry.at
                - time::Duration::seconds(i64::try_from(entry.duration_secs).unwrap_or(i64::MAX))
        });
        if let Some(interval) = active_scoped_interval(
            input,
            started_at,
            entry.at,
            entry.pane.clone(),
            session_name,
            &entry.session_id,
        ) {
            let anchor = interval.started_at;
            intervals.push(ActiveAnchor {
                interval,
                group_key,
                anchor,
            });
        }
    }

    for agent in input.agents {
        if !is_attention_state(agent.state)
            || !matches_agent_filter(&input.filters, Some(agent.kind))
        {
            continue;
        }
        let session_name = agent
            .pane
            .as_ref()
            .and_then(|pane| input.pane_sessions.get(pane))
            .cloned();
        let group_key = session_name
            .clone()
            .unwrap_or_else(|| agent.session_id.clone());
        if !matches_session_filter(
            &input.filters,
            Some(&agent.session_id),
            session_name.as_deref(),
            agent.pane.as_deref(),
        ) {
            continue;
        }
        if let Some(interval) = active_scoped_interval(
            input,
            agent.state_entered_at,
            input.now,
            agent.pane.clone(),
            session_name,
            &agent.session_id,
        ) {
            let anchor = interval.started_at;
            intervals.push(ActiveAnchor {
                interval,
                group_key,
                anchor,
            });
        }
    }

    intervals
}

fn active_scoped_interval(
    input: &TimelineBuildInput<'_>,
    started_at: OffsetDateTime,
    ended_at: OffsetDateTime,
    pane: Option<String>,
    session_name: Option<String>,
    fallback_scope: &str,
) -> Option<ActiveScopedInterval> {
    let (started_at, ended_at) = clip_interval(&input.range, input.now, started_at, ended_at)?;
    let scope_key = active_scope_key(pane.as_deref(), session_name.as_deref(), fallback_scope);
    let group_key = session_name
        .clone()
        .or_else(|| pane.clone())
        .unwrap_or_else(|| fallback_scope.to_string());
    Some(ActiveScopedInterval {
        started_at,
        ended_at,
        pane,
        session_name,
        scope_key,
        group_key,
    })
}

fn active_scope_key(pane: Option<&str>, session_name: Option<&str>, fallback: &str) -> String {
    if let Some(session_name) = session_name {
        return format!("session:{session_name}");
    }
    if let Some(pane) = pane {
        return format!("pane:{pane}");
    }
    format!("unknown:{fallback}")
}

fn active_human_group_key(interval: &ActiveScopedInterval) -> String {
    interval
        .session_name
        .clone()
        .or_else(|| interval.pane.clone())
        .unwrap_or_else(|| "unknown".to_string())
}

fn human_interaction_counts_for_thinking(kind: HumanInteractionKind) -> bool {
    matches!(
        kind,
        HumanInteractionKind::MuxaPromptInput | HumanInteractionKind::TmuxAttach
    )
}

fn human_interaction_counts_for_active(kind: HumanInteractionKind) -> bool {
    matches!(
        kind,
        HumanInteractionKind::MuxaPromptInput | HumanInteractionKind::TmuxAttach
    )
}

fn is_attention_state(state: AgentState) -> bool {
    matches!(
        state,
        AgentState::WaitingInput | AgentState::WaitingChoice | AgentState::Error
    )
}

struct ActivePresenceIndex<'a> {
    intervals: &'a [ActiveScopedInterval],
    by_pane: HashMap<&'a str, Vec<usize>>,
    by_session: HashMap<&'a str, Vec<usize>>,
}

impl<'a> ActivePresenceIndex<'a> {
    fn new(intervals: &'a [ActiveScopedInterval]) -> Self {
        let mut index = Self {
            intervals,
            by_pane: HashMap::new(),
            by_session: HashMap::new(),
        };
        for (position, interval) in intervals.iter().enumerate() {
            if let Some(pane) = interval.pane.as_deref() {
                index.by_pane.entry(pane).or_default().push(position);
            }
            if let Some(session) = interval.session_name.as_deref() {
                index.by_session.entry(session).or_default().push(position);
            }
        }
        index
    }

    fn candidates(&self, attention: &ActiveScopedInterval) -> Vec<usize> {
        let mut candidates = Vec::new();
        if let Some(pane) = attention.pane.as_deref() {
            if let Some(positions) = self.by_pane.get(pane) {
                candidates.extend_from_slice(positions);
            }
        }
        if let Some(session) = attention.session_name.as_deref() {
            if let Some(positions) = self.by_session.get(session) {
                candidates.extend_from_slice(positions);
            }
        }
        candidates.sort_unstable();
        candidates.dedup();
        candidates
    }
}

fn overlapping_active_presence_segments(
    attention: &ActiveScopedInterval,
    presences: &ActivePresenceIndex<'_>,
) -> Vec<ActiveScopedInterval> {
    let mut segments = Vec::new();
    for position in presences.candidates(attention) {
        let presence = &presences.intervals[position];
        let started_at = attention.started_at.max(presence.started_at);
        let ended_at = attention.ended_at.min(presence.ended_at);
        if ended_at <= started_at {
            continue;
        }
        segments.push(ActiveScopedInterval {
            started_at,
            ended_at,
            pane: attention.pane.clone(),
            session_name: attention.session_name.clone(),
            scope_key: attention.scope_key.clone(),
            group_key: attention.group_key.clone(),
        });
    }
    segments
}

fn last_touch_attribution(windows: &[ActiveWindow]) -> BTreeMap<String, u64> {
    let mut events: Vec<(i64, bool, usize)> = Vec::new();
    for (i, window) in windows.iter().enumerate() {
        if window.end > window.start {
            events.push((window.start, false, i));
            events.push((window.end, true, i));
        }
    }
    events.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));

    let mut active: BTreeSet<(i128, usize)> = BTreeSet::new();
    let mut out: BTreeMap<String, u64> = BTreeMap::new();
    let mut i = 0;
    while i < events.len() {
        let t = events[i].0;
        while i < events.len() && events[i].0 == t {
            let (_, is_end, idx) = events[i];
            let key = (windows[idx].anchor, idx);
            if is_end {
                active.remove(&key);
            } else {
                active.insert(key);
            }
            i += 1;
        }
        let next_t = events.get(i).map_or(t, |event| event.0);
        if next_t <= t {
            continue;
        }
        let Some((_, idx)) = active.iter().next_back().copied() else {
            continue;
        };
        let secs = u64::try_from(next_t - t).unwrap_or(0);
        *out.entry(windows[idx].group_key.clone()).or_default() += secs;
    }

    out
}

fn secs_to_duration(secs: u64) -> time::Duration {
    time::Duration::seconds(i64::try_from(secs).unwrap_or(i64::MAX))
}

fn totals_for_intervals(intervals: &[TimelineInterval]) -> TimelineTotals {
    let mut totals = TimelineTotals::default();
    for interval in intervals {
        totals.add_interval(interval);
    }
    totals
}

fn mergeable(prev: &TimelineInterval, next: &TimelineInterval) -> bool {
    prev.source == next.source
        && prev.state == next.state
        && prev.human_kind == next.human_kind
        && prev.open == next.open
        && prev.pane == next.pane
        && prev.session_id == next.session_id
        && prev.session_name == next.session_name
        && prev.cwd == next.cwd
        && prev.ended_at >= next.started_at
}

fn clip_interval(
    range: &TimelineRange,
    now: OffsetDateTime,
    started_at: OffsetDateTime,
    ended_at: OffsetDateTime,
) -> Option<(OffsetDateTime, OffsetDateTime)> {
    let start = range
        .since_at
        .map_or(started_at, |since| started_at.max(since));
    let end = ended_at.min(range.effective_end(now));
    (end > start).then_some((start, end))
}

fn duration_secs(started_at: OffsetDateTime, ended_at: OffsetDateTime) -> u64 {
    u64::try_from((ended_at - started_at).whole_seconds().max(0)).unwrap_or(u64::MAX)
}

fn matches_session_filter(
    filters: &TimelineFilters,
    session_id: Option<&str>,
    session_name: Option<&str>,
    pane: Option<&str>,
) -> bool {
    if filters.exclusions.excludes(pane, session_id, session_name) {
        return false;
    }
    let Some(filter) = filters.session.as_deref().filter(|s| !s.is_empty()) else {
        return true;
    };
    session_id == Some(filter) || session_name == Some(filter) || pane == Some(filter)
}

fn matches_agent_filter(filters: &TimelineFilters, kind: Option<AgentKind>) -> bool {
    filters.agent_kind.is_none_or(|wanted| kind == Some(wanted))
}

fn live_agent_has_range_activity(agent: &Agent, range: &TimelineRange) -> bool {
    if agent.session_id.starts_with(SYNTHETIC_SESSION_PREFIX) {
        return false;
    }
    range.includes_end(agent.last_activity_at)
}

fn agent_lane_id(kind: AgentKind, session_id: &str) -> String {
    format!("agent:{kind}:{session_id}")
}

fn agent_lane_label(kind: AgentKind, session_name: Option<&str>, session_id: &str) -> String {
    let suffix = session_name.unwrap_or(session_id);
    format!("{kind}/{suffix}")
}

fn tmux_lane_id(session_id: &str, session_name: &str) -> String {
    if session_id.is_empty() {
        format!("tmux:{session_name}")
    } else {
        format!("tmux:{session_id}")
    }
}

fn human_lane_id(
    session_id: Option<&str>,
    session_name: Option<&str>,
    pane: Option<&str>,
) -> String {
    if let Some(session_id) = session_id.filter(|value| !value.is_empty()) {
        format!("human:{session_id}")
    } else if let Some(session_name) = session_name.filter(|value| !value.is_empty()) {
        format!("human:{session_name}")
    } else if let Some(pane) = pane.filter(|value| !value.is_empty()) {
        format!("human:{pane}")
    } else {
        "human".to_string()
    }
}

fn human_lane_label(
    session_id: Option<&str>,
    session_name: Option<&str>,
    pane: Option<&str>,
) -> String {
    if let Some(session_name) = session_name.filter(|value| !value.is_empty()) {
        format!("human/{session_name}")
    } else if let Some(session_id) = session_id.filter(|value| !value.is_empty()) {
        format!("human/{session_id}")
    } else if let Some(pane) = pane.filter(|value| !value.is_empty()) {
        format!("human/{pane}")
    } else {
        "human".to_string()
    }
}

fn lane_rank(kind: TimelineLaneKind) -> u8 {
    match kind {
        TimelineLaneKind::Agent => 0,
        TimelineLaneKind::Human => 1,
        TimelineLaneKind::Tmux => 2,
    }
}

fn human_kind_label(kind: HumanInteractionKind) -> &'static str {
    match kind {
        HumanInteractionKind::MuxaWatch => "muxa_watch",
        HumanInteractionKind::MuxaPromptInput => "muxa_prompt_input",
        HumanInteractionKind::TmuxAttach => "tmux_attach",
        HumanInteractionKind::TmuxInput => "tmux_input",
        HumanInteractionKind::TmuxScroll => "tmux_scroll",
    }
}

fn local_day_start(date: Date, offset: UtcOffset) -> OffsetDateTime {
    date.midnight().assume_offset(offset)
}

fn invalid_since_value(raw: &str) -> String {
    if raw.is_empty() {
        format!("missing --since value\n{}", since_help())
    } else {
        format!("unsupported --since value {raw:?}\n{}", since_help())
    }
}

fn since_help() -> &'static str {
    "supported --since values:\n  keywords: today, yesterday, week (rolling 7d), month (rolling 30d), last-week / \"last week\", last-month / \"last month\", all\n  durations: 24h, 7d, 4w, 30d (units: s, m, h, d, w)\n  dates: 2026-06-06\n  timestamps: 2026-06-06T09:00:00+09:00"
}

fn previous_calendar_week(
    now: OffsetDateTime,
    offset: UtcOffset,
) -> Result<(OffsetDateTime, OffsetDateTime), String> {
    let current_week_start = week_start_monday(now.to_offset(offset).date())?;
    let previous_week_start = previous_days(current_week_start, 7)?;
    Ok((
        local_day_start(previous_week_start, offset),
        local_day_start(current_week_start, offset),
    ))
}

fn previous_calendar_month(
    now: OffsetDateTime,
    offset: UtcOffset,
) -> Result<(OffsetDateTime, OffsetDateTime), String> {
    let current_month_start = month_start(now.to_offset(offset).date())?;
    let previous_month_anchor = current_month_start
        .previous_day()
        .ok_or_else(|| "could not compute previous month date".to_string())?;
    let previous_month_start = month_start(previous_month_anchor)?;
    Ok((
        local_day_start(previous_month_start, offset),
        local_day_start(current_month_start, offset),
    ))
}

fn month_start(date: Date) -> Result<Date, String> {
    Date::from_calendar_date(date.year(), date.month(), 1)
        .map_err(|_| "could not compute month start date".to_string())
}

fn week_start_monday(mut date: Date) -> Result<Date, String> {
    for _ in 0..weekday_days_from_monday(date.weekday()) {
        date = date
            .previous_day()
            .ok_or_else(|| "could not compute week start date".to_string())?;
    }
    Ok(date)
}

fn previous_days(mut date: Date, days: u8) -> Result<Date, String> {
    for _ in 0..days {
        date = date
            .previous_day()
            .ok_or_else(|| "could not compute previous date".to_string())?;
    }
    Ok(date)
}

fn weekday_days_from_monday(weekday: Weekday) -> u8 {
    match weekday {
        Weekday::Monday => 0,
        Weekday::Tuesday => 1,
        Weekday::Wednesday => 2,
        Weekday::Thursday => 3,
        Weekday::Friday => 4,
        Weekday::Saturday => 5,
        Weekday::Sunday => 6,
    }
}

fn parse_iso_date(raw: &str) -> Result<Option<Date>, String> {
    if raw.len() != 10 {
        return Ok(None);
    }
    let mut parts = raw.split('-');
    let (Some(year), Some(month), Some(day), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Ok(None);
    };
    let year = year
        .parse::<i32>()
        .map_err(|_| format!("invalid date {raw:?}: year must be four digits"))?;
    let month = month
        .parse::<u8>()
        .map_err(|_| format!("invalid date {raw:?}: month must be 01-12"))?;
    let day = day
        .parse::<u8>()
        .map_err(|_| format!("invalid date {raw:?}: day must be 01-31"))?;
    let month =
        Month::try_from(month).map_err(|_| format!("invalid date {raw:?}: month must be 01-12"))?;
    Date::from_calendar_date(year, month, day)
        .map(Some)
        .map_err(|_| format!("invalid date {raw:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::{
        HumanInteractionEntry, HumanInteractionInput, SessionForegroundEntry, StateTransitionEntry,
        StateTransitionInput,
    };
    use crate::event::{AgentKind, AgentState};
    use time::macros::{datetime, offset};

    #[test]
    fn state_transition_interval_uses_from_state() {
        let now = datetime!(2026-06-05 12:00:00 UTC);
        let entry =
            ActivityEntry::StateTransition(StateTransitionEntry::new(StateTransitionInput {
                at: datetime!(2026-06-05 11:10:00 UTC),
                kind: AgentKind::Codex,
                session_id: "s1".into(),
                pane: Some("%1".into()),
                session_name: Some("main".into()),
                cwd: None,
                from: AgentState::Working,
                to: AgentState::WaitingInput,
                state_entered_at: Some(datetime!(2026-06-05 11:00:00 UTC)),
            }));
        let range = TimelineRange {
            label: "today".into(),
            since_at: Some(datetime!(2026-06-05 10:30:00 UTC)),
            until_at: None,
        };
        let doc = build_document(TimelineBuildInput {
            now,
            range,
            prompt_entries: &[],
            activity_entries: &[entry],
            agents: &[],
            session_activities: &[],
            pane_sessions: &HashMap::new(),
            active_lookback_secs: 60,
            active_timeout_secs: 300,
            active_tick_timeout_secs: 300,
            count_tmux_input: true,
            filters: TimelineFilters::default(),
            notes: Vec::new(),
        });

        assert_eq!(doc.lanes.len(), 1);
        assert_eq!(doc.lanes[0].intervals[0].state, Some(AgentState::Working));
        assert_eq!(doc.lanes[0].totals.working_secs, 600);
    }

    #[test]
    fn range_clips_interval_start() {
        let now = datetime!(2026-06-05 12:00:00 UTC);
        let entry =
            ActivityEntry::StateTransition(StateTransitionEntry::new(StateTransitionInput {
                at: datetime!(2026-06-05 11:10:00 UTC),
                kind: AgentKind::Codex,
                session_id: "s1".into(),
                pane: None,
                session_name: None,
                cwd: None,
                from: AgentState::Working,
                to: AgentState::WaitingInput,
                state_entered_at: Some(datetime!(2026-06-05 11:00:00 UTC)),
            }));
        let range = TimelineRange {
            label: "recent".into(),
            since_at: Some(datetime!(2026-06-05 11:05:00 UTC)),
            until_at: None,
        };
        let doc = build_document(TimelineBuildInput {
            now,
            range,
            prompt_entries: &[],
            activity_entries: &[entry],
            agents: &[],
            session_activities: &[],
            pane_sessions: &HashMap::new(),
            active_lookback_secs: 60,
            active_timeout_secs: 300,
            active_tick_timeout_secs: 300,
            count_tmux_input: true,
            filters: TimelineFilters::default(),
            notes: Vec::new(),
        });

        assert_eq!(doc.lanes[0].intervals[0].duration_secs, 300);
        assert_eq!(doc.totals.working_secs, 300);
    }

    #[test]
    fn human_interactions_are_laned_by_session() {
        let now = datetime!(2026-06-05 12:00:00 UTC);
        let entries = [
            ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
                kind: HumanInteractionKind::MuxaPromptInput,
                pane: Some("%1".into()),
                session_id: Some("s1".into()),
                session_name: Some("main".into()),
                started_at: datetime!(2026-06-05 11:00:00 UTC),
                ended_at: datetime!(2026-06-05 11:02:00 UTC),
            })),
            ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
                kind: HumanInteractionKind::MuxaPromptInput,
                pane: Some("%2".into()),
                session_id: Some("s2".into()),
                session_name: Some("side".into()),
                started_at: datetime!(2026-06-05 11:05:00 UTC),
                ended_at: datetime!(2026-06-05 11:07:00 UTC),
            })),
        ];
        let range = TimelineRange {
            label: "today".into(),
            since_at: Some(datetime!(2026-06-05 10:00:00 UTC)),
            until_at: None,
        };
        let doc = build_document(TimelineBuildInput {
            now,
            range,
            prompt_entries: &[],
            activity_entries: &entries,
            agents: &[],
            session_activities: &[],
            pane_sessions: &HashMap::new(),
            active_lookback_secs: 60,
            active_timeout_secs: 300,
            active_tick_timeout_secs: 300,
            count_tmux_input: true,
            filters: TimelineFilters::default(),
            notes: Vec::new(),
        });

        let labels = doc
            .lanes
            .iter()
            .map(|lane| lane.label.as_str())
            .collect::<Vec<_>>();
        assert_eq!(labels, vec!["human/main", "human/side"]);
        assert!(doc
            .lanes
            .iter()
            .all(|lane| lane.kind == TimelineLaneKind::Human));
        assert_eq!(doc.totals.human_secs, 240);
    }

    struct SummaryFixture {
        now: OffsetDateTime,
        range: TimelineRange,
        entries: [ActivityEntry; 4],
        prompts: [HistoryEntry; 1],
        pane_sessions: HashMap<String, String>,
    }

    fn summary_fixture() -> SummaryFixture {
        let now = datetime!(2026-06-05 01:00:00 UTC);
        let range = TimelineRange {
            label: "cross-midnight".into(),
            since_at: Some(datetime!(2026-06-04 23:55:00 UTC)),
            until_at: None,
        };
        let entries = [
            ActivityEntry::StateTransition(StateTransitionEntry::new(StateTransitionInput {
                at: datetime!(2026-06-05 00:10:00 UTC),
                kind: AgentKind::Codex,
                session_id: "agent-main".into(),
                pane: Some("%1".into()),
                session_name: Some("main".into()),
                cwd: Some("/repo".into()),
                from: AgentState::Working,
                to: AgentState::Idle,
                state_entered_at: Some(datetime!(2026-06-04 23:50:00 UTC)),
            })),
            ActivityEntry::StateTransition(StateTransitionEntry::new(StateTransitionInput {
                at: datetime!(2026-06-05 00:20:00 UTC),
                kind: AgentKind::Codex,
                session_id: "agent-main".into(),
                pane: Some("%1".into()),
                session_name: Some("main".into()),
                cwd: Some("/repo".into()),
                from: AgentState::Working,
                to: AgentState::WaitingInput,
                state_entered_at: Some(datetime!(2026-06-05 00:05:00 UTC)),
            })),
            ActivityEntry::SessionForeground(SessionForegroundEntry::new(
                "$1",
                "main",
                datetime!(2026-06-04 23:58:00 UTC),
                datetime!(2026-06-05 00:08:00 UTC),
            )),
            ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
                kind: HumanInteractionKind::MuxaPromptInput,
                pane: Some("%1".into()),
                session_id: Some("agent-main".into()),
                session_name: Some("main".into()),
                started_at: datetime!(2026-06-05 00:00:00 UTC),
                ended_at: datetime!(2026-06-05 00:05:00 UTC),
            })),
        ];
        let prompts = [HistoryEntry::new(
            AgentKind::Codex,
            "agent-main",
            "%1",
            "prompt",
            datetime!(2026-06-05 00:01:00 UTC),
            None,
        )];
        let pane_sessions = HashMap::from([("%1".to_string(), "main".to_string())]);
        SummaryFixture {
            now,
            range,
            entries,
            prompts,
            pane_sessions,
        }
    }

    #[test]
    fn direct_summary_matches_detail_projection_and_day_buckets() {
        let SummaryFixture {
            now,
            range,
            entries,
            prompts,
            pane_sessions,
        } = summary_fixture();
        let detail = build_document(TimelineBuildInput {
            now,
            range: range.clone(),
            prompt_entries: &prompts,
            activity_entries: &entries,
            agents: &[],
            session_activities: &[],
            pane_sessions: &pane_sessions,
            active_lookback_secs: 60,
            active_timeout_secs: 300,
            active_tick_timeout_secs: 300,
            count_tmux_input: true,
            filters: TimelineFilters::default(),
            notes: Vec::new(),
        });
        let summary = build_summary(
            TimelineBuildInput {
                now,
                range,
                prompt_entries: &prompts,
                activity_entries: &entries,
                agents: &[],
                session_activities: &[],
                pane_sessions: &pane_sessions,
                active_lookback_secs: 60,
                active_timeout_secs: 300,
                active_tick_timeout_secs: 300,
                count_tmux_input: true,
                filters: TimelineFilters::default(),
                notes: Vec::new(),
            },
            UtcOffset::UTC,
        );

        assert_eq!(summary.totals, detail.totals);
        assert_eq!(summary.active_sessions, detail.active_sessions);
        assert_eq!(summary.window_started_at, detail.window_started_at);
        assert_eq!(summary.window_ended_at, detail.window_ended_at);
        assert_eq!(summary.sessions.len(), 1);
        assert_eq!(summary.sessions[0].label, "main");
        assert_eq!(summary.sessions[0].lanes, 3);
        assert_eq!(summary.sessions[0].totals, detail.totals);
        assert_eq!(summary.sessions[0].human_presence_secs, 600);
        assert_eq!(
            summary.sessions[0].latest_at,
            Some(datetime!(2026-06-05 00:20:00 UTC))
        );
        assert_eq!(summary.days.len(), 2);
        assert_eq!(summary.days[0].date, "2026-06-04");
        assert_eq!(summary.days[0].totals.working_secs, 300);
        assert_eq!(summary.days[0].totals.foreground_secs, 120);
        assert_eq!(summary.days[1].date, "2026-06-05");
        assert_eq!(summary.days[1].totals.working_secs, 1_200);
        assert_eq!(summary.days[1].totals.human_secs, 300);
        assert_eq!(summary.days[1].totals.foreground_secs, 480);
        assert_eq!(summary.sources[0].kind, TimelineLaneKind::Agent);
        assert_eq!(summary.sources[0].lanes, 1);
        assert_eq!(summary.sources[0].totals.working_secs, 1_500);
        assert_eq!(summary.sources[1].kind, TimelineLaneKind::Human);
        assert_eq!(summary.sources[2].kind, TimelineLaneKind::Tmux);
    }

    #[test]
    fn direct_summary_preserves_pane_only_day_attribution_label() {
        let now = datetime!(2026-06-05 01:00:00 UTC);
        let entries = [ActivityEntry::HumanInteraction(HumanInteractionEntry::new(
            HumanInteractionInput {
                kind: HumanInteractionKind::MuxaWatch,
                pane: Some("%9".into()),
                session_id: None,
                session_name: None,
                started_at: datetime!(2026-06-05 00:00:00 UTC),
                ended_at: datetime!(2026-06-05 00:01:00 UTC),
            },
        ))];
        let summary = build_summary(
            TimelineBuildInput {
                now,
                range: TimelineRange {
                    label: "hour".into(),
                    since_at: Some(datetime!(2026-06-05 00:00:00 UTC)),
                    until_at: None,
                },
                prompt_entries: &[],
                activity_entries: &entries,
                agents: &[],
                session_activities: &[],
                pane_sessions: &HashMap::new(),
                active_lookback_secs: 60,
                active_timeout_secs: 300,
                active_tick_timeout_secs: 300,
                count_tmux_input: true,
                filters: TimelineFilters::default(),
                notes: Vec::new(),
            },
            UtcOffset::UTC,
        );

        assert_eq!(summary.sessions[0].label, "no session");
        assert_eq!(summary.days[0].top_sessions[0].label, "human/%9");
    }

    #[test]
    fn direct_summary_attributes_renamed_sessions_per_span() {
        let now = datetime!(2026-06-05 01:00:00 UTC);
        let entries = [
            ActivityEntry::StateTransition(StateTransitionEntry::new(StateTransitionInput {
                at: datetime!(2026-06-05 00:01:00 UTC),
                kind: AgentKind::Codex,
                session_id: "agent-main".into(),
                pane: Some("%1".into()),
                session_name: Some("before-rename".into()),
                cwd: Some("/repo".into()),
                from: AgentState::Working,
                to: AgentState::Idle,
                state_entered_at: Some(datetime!(2026-06-05 00:00:00 UTC)),
            })),
            ActivityEntry::StateTransition(StateTransitionEntry::new(StateTransitionInput {
                at: datetime!(2026-06-05 00:03:00 UTC),
                kind: AgentKind::Codex,
                session_id: "agent-main".into(),
                pane: Some("%1".into()),
                session_name: Some("after-rename".into()),
                cwd: Some("/repo".into()),
                from: AgentState::Working,
                to: AgentState::Idle,
                state_entered_at: Some(datetime!(2026-06-05 00:01:00 UTC)),
            })),
        ];
        let summary = build_summary(
            TimelineBuildInput {
                now,
                range: TimelineRange {
                    label: "hour".into(),
                    since_at: Some(datetime!(2026-06-05 00:00:00 UTC)),
                    until_at: None,
                },
                prompt_entries: &[],
                activity_entries: &entries,
                agents: &[],
                session_activities: &[],
                pane_sessions: &HashMap::new(),
                active_lookback_secs: 60,
                active_timeout_secs: 300,
                active_tick_timeout_secs: 300,
                count_tmux_input: true,
                filters: TimelineFilters::default(),
                notes: Vec::new(),
            },
            UtcOffset::UTC,
        );

        assert_eq!(summary.days[0].top_sessions.len(), 2);
        assert_eq!(summary.days[0].top_sessions[0].label, "after-rename");
        assert_eq!(summary.days[0].top_sessions[0].active_secs, 120);
        assert_eq!(summary.days[0].top_sessions[1].label, "before-rename");
        assert_eq!(summary.days[0].top_sessions[1].active_secs, 60);
    }

    #[test]
    fn active_sessions_use_prompt_anchors_with_last_touch_dedup() {
        let now = datetime!(2026-06-05 12:00:00 UTC);
        let range = TimelineRange {
            label: "today".into(),
            since_at: Some(datetime!(2026-06-05 10:00:00 UTC)),
            until_at: None,
        };
        let prompts = [
            HistoryEntry::new(
                AgentKind::Codex,
                "agent-main",
                "%1",
                "prompt main",
                datetime!(2026-06-05 11:00:00 UTC),
                None,
            ),
            HistoryEntry::new(
                AgentKind::ClaudeCode,
                "agent-side",
                "%2",
                "prompt side",
                datetime!(2026-06-05 11:03:00 UTC),
                None,
            ),
        ];
        let pane_sessions = HashMap::from([
            ("%1".to_string(), "main".to_string()),
            ("%2".to_string(), "side".to_string()),
        ]);

        let entries = [
            ActivityEntry::SessionForeground(SessionForegroundEntry::new(
                "$1",
                "main",
                datetime!(2026-06-05 10:00:00 UTC),
                datetime!(2026-06-05 12:00:00 UTC),
            )),
            ActivityEntry::SessionForeground(SessionForegroundEntry::new(
                "$2",
                "side",
                datetime!(2026-06-05 10:00:00 UTC),
                datetime!(2026-06-05 12:00:00 UTC),
            )),
        ];

        let doc = build_document(TimelineBuildInput {
            now,
            range,
            prompt_entries: &prompts,
            activity_entries: &entries,
            agents: &[],
            session_activities: &[],
            pane_sessions: &pane_sessions,
            active_lookback_secs: 60,
            active_timeout_secs: 300,
            active_tick_timeout_secs: 300,
            count_tmux_input: true,
            filters: TimelineFilters::default(),
            notes: Vec::new(),
        });

        assert_eq!(doc.totals.active_secs, 540);
        assert_eq!(
            doc.active_sessions,
            vec![
                TimelineActiveSession {
                    label: "main".into(),
                    active_secs: 180,
                },
                TimelineActiveSession {
                    label: "side".into(),
                    active_secs: 360,
                },
            ]
        );
    }

    #[test]
    fn active_sessions_count_tmux_input_ticks() {
        let now = datetime!(2026-06-05 12:00:00 UTC);
        let range = TimelineRange {
            label: "today".into(),
            since_at: Some(datetime!(2026-06-05 10:00:00 UTC)),
            until_at: None,
        };
        let entries = [
            ActivityEntry::SessionForeground(SessionForegroundEntry::new(
                "$1",
                "main",
                datetime!(2026-06-05 10:59:00 UTC),
                datetime!(2026-06-05 11:05:01 UTC),
            )),
            ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
                kind: HumanInteractionKind::TmuxInput,
                pane: Some("%1".into()),
                session_id: Some("agent-main".into()),
                session_name: Some("main".into()),
                started_at: datetime!(2026-06-05 11:00:00 UTC),
                ended_at: datetime!(2026-06-05 11:00:01 UTC),
            })),
        ];

        let doc = build_document(TimelineBuildInput {
            now,
            range,
            prompt_entries: &[],
            activity_entries: &entries,
            agents: &[],
            session_activities: &[],
            pane_sessions: &HashMap::new(),
            active_lookback_secs: 60,
            active_timeout_secs: 300,
            active_tick_timeout_secs: 300,
            count_tmux_input: true,
            filters: TimelineFilters::default(),
            notes: Vec::new(),
        });

        assert_eq!(doc.totals.active_secs, 361);
        assert_eq!(
            doc.active_sessions,
            vec![TimelineActiveSession {
                label: "main".into(),
                active_secs: 361,
            }]
        );
    }

    #[test]
    fn active_sessions_use_tick_timeout_for_tmux_input_ticks() {
        let now = datetime!(2026-06-05 12:00:00 UTC);
        let range = TimelineRange {
            label: "today".into(),
            since_at: Some(datetime!(2026-06-05 10:00:00 UTC)),
            until_at: None,
        };
        let entries = [
            ActivityEntry::SessionForeground(SessionForegroundEntry::new(
                "$1",
                "main",
                datetime!(2026-06-05 10:59:00 UTC),
                datetime!(2026-06-05 11:01:31 UTC),
            )),
            ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
                kind: HumanInteractionKind::TmuxInput,
                pane: Some("%1".into()),
                session_id: Some("agent-main".into()),
                session_name: Some("main".into()),
                started_at: datetime!(2026-06-05 11:00:00 UTC),
                ended_at: datetime!(2026-06-05 11:00:01 UTC),
            })),
        ];

        let doc = build_document(TimelineBuildInput {
            now,
            range,
            prompt_entries: &[],
            activity_entries: &entries,
            agents: &[],
            session_activities: &[],
            pane_sessions: &HashMap::new(),
            active_lookback_secs: 60,
            active_timeout_secs: 300,
            active_tick_timeout_secs: 90,
            count_tmux_input: true,
            filters: TimelineFilters::default(),
            notes: Vec::new(),
        });

        assert_eq!(doc.totals.active_secs, 151);
        assert_eq!(
            doc.active_sessions,
            vec![TimelineActiveSession {
                label: "main".into(),
                active_secs: 151,
            }]
        );
    }

    #[test]
    fn active_sessions_clip_prompt_padding_to_presence() {
        let now = datetime!(2026-06-05 12:00:00 UTC);
        let range = TimelineRange {
            label: "today".into(),
            since_at: Some(datetime!(2026-06-05 10:00:00 UTC)),
            until_at: None,
        };
        let prompts = [HistoryEntry::new(
            AgentKind::Codex,
            "agent-main",
            "%1",
            "prompt main",
            datetime!(2026-06-05 11:00:00 UTC),
            None,
        )];
        let pane_sessions = HashMap::from([("%1".to_string(), "main".to_string())]);
        let entries = [ActivityEntry::SessionForeground(
            SessionForegroundEntry::new(
                "$1",
                "main",
                datetime!(2026-06-05 10:59:30 UTC),
                datetime!(2026-06-05 11:01:00 UTC),
            ),
        )];

        let doc = build_document(TimelineBuildInput {
            now,
            range,
            prompt_entries: &prompts,
            activity_entries: &entries,
            agents: &[],
            session_activities: &[],
            pane_sessions: &pane_sessions,
            active_lookback_secs: 60,
            active_timeout_secs: 300,
            active_tick_timeout_secs: 300,
            count_tmux_input: true,
            filters: TimelineFilters::default(),
            notes: Vec::new(),
        });

        assert_eq!(doc.totals.foreground_secs, 90);
        assert_eq!(doc.totals.human_secs, 0);
        assert_eq!(doc.totals.active_secs, 90);
        assert_eq!(
            doc.active_sessions,
            vec![TimelineActiveSession {
                label: "main".into(),
                active_secs: 90,
            }]
        );
    }

    #[test]
    fn active_sessions_ignore_watch_only_presence() {
        let now = datetime!(2026-06-05 12:00:00 UTC);
        let range = TimelineRange {
            label: "today".into(),
            since_at: Some(datetime!(2026-06-05 10:00:00 UTC)),
            until_at: None,
        };
        let prompts = [HistoryEntry::new(
            AgentKind::Codex,
            "agent-main",
            "%1",
            "prompt main",
            datetime!(2026-06-05 11:00:00 UTC),
            None,
        )];
        let entries = [ActivityEntry::HumanInteraction(HumanInteractionEntry::new(
            HumanInteractionInput {
                kind: HumanInteractionKind::MuxaWatch,
                pane: Some("%1".into()),
                session_id: Some("agent-main".into()),
                session_name: Some("main".into()),
                started_at: datetime!(2026-06-05 10:00:00 UTC),
                ended_at: datetime!(2026-06-05 12:00:00 UTC),
            },
        ))];

        let doc = build_document(TimelineBuildInput {
            now,
            range,
            prompt_entries: &prompts,
            activity_entries: &entries,
            agents: &[],
            session_activities: &[],
            pane_sessions: &HashMap::new(),
            active_lookback_secs: 60,
            active_timeout_secs: 300,
            active_tick_timeout_secs: 300,
            count_tmux_input: true,
            filters: TimelineFilters::default(),
            notes: Vec::new(),
        });

        assert_eq!(doc.totals.human_secs, 7_200);
        assert_eq!(doc.totals.active_secs, 0);
        assert!(doc.active_sessions.is_empty());
    }

    #[test]
    fn active_sessions_count_presence_while_agent_needs_attention() {
        let now = datetime!(2026-06-05 12:00:00 UTC);
        let range = TimelineRange {
            label: "today".into(),
            since_at: Some(datetime!(2026-06-05 10:00:00 UTC)),
            until_at: None,
        };
        let entries = [
            ActivityEntry::StateTransition(StateTransitionEntry::new(StateTransitionInput {
                at: datetime!(2026-06-05 11:10:00 UTC),
                kind: AgentKind::Codex,
                session_id: "agent-main".into(),
                pane: Some("%1".into()),
                session_name: Some("main".into()),
                cwd: None,
                from: AgentState::WaitingInput,
                to: AgentState::Working,
                state_entered_at: Some(datetime!(2026-06-05 11:00:00 UTC)),
            })),
            ActivityEntry::HumanInteraction(HumanInteractionEntry::new(HumanInteractionInput {
                kind: HumanInteractionKind::TmuxAttach,
                pane: Some("%1".into()),
                session_id: Some("agent-main".into()),
                session_name: Some("main".into()),
                started_at: datetime!(2026-06-05 10:50:00 UTC),
                ended_at: datetime!(2026-06-05 11:05:00 UTC),
            })),
        ];

        let doc = build_document(TimelineBuildInput {
            now,
            range,
            prompt_entries: &[],
            activity_entries: &entries,
            agents: &[],
            session_activities: &[],
            pane_sessions: &HashMap::new(),
            active_lookback_secs: 60,
            active_timeout_secs: 300,
            active_tick_timeout_secs: 300,
            count_tmux_input: true,
            filters: TimelineFilters::default(),
            notes: Vec::new(),
        });

        assert_eq!(doc.totals.active_secs, 300);
        assert_eq!(
            doc.active_sessions,
            vec![TimelineActiveSession {
                label: "main".into(),
                active_secs: 300,
            }]
        );
    }

    #[test]
    fn scope_exclusions_drop_matching_lanes() {
        let now = datetime!(2026-06-05 12:00:00 UTC);
        let entries = [
            ActivityEntry::StateTransition(StateTransitionEntry::new(StateTransitionInput {
                at: datetime!(2026-06-05 11:10:00 UTC),
                kind: AgentKind::Codex,
                session_id: "agent-main".into(),
                pane: Some("%1".into()),
                session_name: Some("main".into()),
                cwd: None,
                from: AgentState::Working,
                to: AgentState::WaitingInput,
                state_entered_at: Some(datetime!(2026-06-05 11:00:00 UTC)),
            })),
            ActivityEntry::StateTransition(StateTransitionEntry::new(StateTransitionInput {
                at: datetime!(2026-06-05 11:10:00 UTC),
                kind: AgentKind::Codex,
                session_id: "agent-monitor".into(),
                pane: Some("%2".into()),
                session_name: Some("monitoring".into()),
                cwd: None,
                from: AgentState::Working,
                to: AgentState::WaitingInput,
                state_entered_at: Some(datetime!(2026-06-05 11:00:00 UTC)),
            })),
        ];
        let range = TimelineRange {
            label: "today".into(),
            since_at: Some(datetime!(2026-06-05 10:00:00 UTC)),
            until_at: None,
        };
        let doc = build_document(TimelineBuildInput {
            now,
            range,
            prompt_entries: &[],
            activity_entries: &entries,
            agents: &[],
            session_activities: &[],
            pane_sessions: &HashMap::new(),
            active_lookback_secs: 60,
            active_timeout_secs: 300,
            active_tick_timeout_secs: 300,
            count_tmux_input: true,
            filters: TimelineFilters {
                session: None,
                agent_kind: None,
                exclusions: ScopeExclusions::new(Vec::new(), vec!["monitor*".into()]),
            },
            notes: Vec::new(),
        });

        assert_eq!(doc.lanes.len(), 1);
        assert_eq!(doc.lanes[0].session_name.as_deref(), Some("main"));
        assert_eq!(doc.totals.working_secs, 600);
    }

    #[test]
    fn parse_since_accepts_iso_date_as_single_day() {
        let range = parse_since(
            "2026-06-05",
            datetime!(2026-06-05 12:00:00 UTC),
            "all retained activity",
        )
        .unwrap();

        assert_eq!(range.label, "2026-06-05");
        assert_eq!(
            range.until_at.unwrap() - range.since_at.unwrap(),
            time::Duration::days(1)
        );
    }

    #[test]
    fn parse_since_accepts_previous_calendar_week() {
        let range = parse_since(
            "last-week",
            datetime!(2026-06-08 12:00:00 UTC),
            "all retained activity",
        )
        .unwrap();

        assert_eq!(range.label, "last week");
        assert_eq!(
            range.until_at.unwrap() - range.since_at.unwrap(),
            time::Duration::days(7)
        );
    }

    #[test]
    fn parse_since_accepts_month_as_rolling_thirty_days() {
        let range = parse_since(
            "month",
            datetime!(2026-06-08 12:00:00 UTC),
            "all retained activity",
        )
        .unwrap();

        assert_eq!(range.label, "last 30d");
        assert_eq!(range.since_at, Some(datetime!(2026-05-09 12:00:00 UTC)));
        assert_eq!(range.until_at, None);
    }

    #[test]
    fn parse_since_accepts_previous_calendar_month() {
        let now = datetime!(2026-06-08 09:30:00 +9);
        let range = parse_since("last-month", now, "all retained activity").unwrap();
        let offset = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);
        let (since_at, until_at) = previous_calendar_month(now, offset).unwrap();

        assert_eq!(range.label, "last month");
        assert_eq!(range.since_at, Some(since_at));
        assert_eq!(range.until_at, Some(until_at));
    }

    #[test]
    fn previous_calendar_week_uses_monday_boundaries() {
        let (since_at, until_at) =
            previous_calendar_week(datetime!(2026-06-08 09:30:00 +9), offset!(+9)).unwrap();

        assert_eq!(since_at, datetime!(2026-06-01 00:00:00 +9));
        assert_eq!(until_at, datetime!(2026-06-08 00:00:00 +9));
    }

    #[test]
    fn invalid_since_value_lists_supported_values() {
        let error = parse_since(
            "quarter",
            datetime!(2026-06-08 12:00:00 UTC),
            "all retained activity",
        )
        .unwrap_err();

        assert!(error.contains("unsupported --since value \"quarter\""));
        assert!(error.contains("supported --since values:"));
        assert!(error.contains("last-week"));
        assert!(error.contains("month (rolling 30d)"));
        assert!(error.contains("24h, 7d, 4w, 30d"));
    }

    #[test]
    fn invalid_since_duration_unit_lists_supported_values() {
        let error = parse_since(
            "1y",
            datetime!(2026-06-08 12:00:00 UTC),
            "all retained activity",
        )
        .unwrap_err();

        assert!(error.contains("invalid --since duration \"1y\""));
        assert!(error.contains("unit must be one of s, m, h, d, w"));
        assert!(error.contains("supported --since values:"));
    }

    #[test]
    fn live_snapshot_agent_without_range_activity_is_not_synthesized() {
        let now = datetime!(2026-06-05 12:00:00 UTC);
        let range = TimelineRange {
            label: "today".into(),
            since_at: Some(datetime!(2026-06-05 10:00:00 UTC)),
            until_at: None,
        };
        let agents = [agent(
            "s1",
            AgentState::Idle,
            datetime!(2026-06-05 09:00:00 UTC),
            datetime!(2026-06-05 09:30:00 UTC),
        )];

        let doc = build_document(TimelineBuildInput {
            now,
            range,
            prompt_entries: &[],
            activity_entries: &[],
            agents: &agents,
            session_activities: &[],
            pane_sessions: &HashMap::new(),
            active_lookback_secs: 60,
            active_timeout_secs: 300,
            active_tick_timeout_secs: 300,
            count_tmux_input: true,
            filters: TimelineFilters::default(),
            notes: Vec::new(),
        });

        assert!(doc.lanes.is_empty());
        assert!(doc
            .notes
            .iter()
            .any(|note| note == "no timeline intervals in this view"));
    }

    #[test]
    fn live_snapshot_agent_with_range_activity_keeps_open_interval() {
        let now = datetime!(2026-06-05 12:00:00 UTC);
        let range = TimelineRange {
            label: "today".into(),
            since_at: Some(datetime!(2026-06-05 10:00:00 UTC)),
            until_at: None,
        };
        let agents = [agent(
            "s1",
            AgentState::Working,
            datetime!(2026-06-05 09:00:00 UTC),
            datetime!(2026-06-05 11:00:00 UTC),
        )];

        let doc = build_document(TimelineBuildInput {
            now,
            range,
            prompt_entries: &[],
            activity_entries: &[],
            agents: &agents,
            session_activities: &[],
            pane_sessions: &HashMap::new(),
            active_lookback_secs: 60,
            active_timeout_secs: 300,
            active_tick_timeout_secs: 300,
            count_tmux_input: true,
            filters: TimelineFilters::default(),
            notes: Vec::new(),
        });

        assert_eq!(doc.lanes.len(), 1);
        assert_eq!(doc.lanes[0].intervals.len(), 1);
        assert_eq!(doc.lanes[0].intervals[0].state, Some(AgentState::Working));
        assert_eq!(
            doc.lanes[0].intervals[0].started_at,
            datetime!(2026-06-05 10:00:00 UTC)
        );
        assert!(doc.lanes[0].intervals[0].open);
        assert_eq!(doc.lanes[0].totals.working_secs, 7200);
    }

    #[test]
    fn synthetic_snapshot_agent_is_not_timeline_activity() {
        let now = datetime!(2026-06-05 12:00:00 UTC);
        let range = TimelineRange {
            label: "today".into(),
            since_at: Some(datetime!(2026-06-05 10:00:00 UTC)),
            until_at: None,
        };
        let agents = [agent(
            "synthetic-%1",
            AgentState::Idle,
            datetime!(2026-06-05 11:00:00 UTC),
            datetime!(2026-06-05 11:00:00 UTC),
        )];

        let doc = build_document(TimelineBuildInput {
            now,
            range,
            prompt_entries: &[],
            activity_entries: &[],
            agents: &agents,
            session_activities: &[],
            pane_sessions: &HashMap::new(),
            active_lookback_secs: 60,
            active_timeout_secs: 300,
            active_tick_timeout_secs: 300,
            count_tmux_input: true,
            filters: TimelineFilters::default(),
            notes: Vec::new(),
        });

        assert!(doc.lanes.is_empty());
    }

    fn agent(
        session_id: &str,
        state: AgentState,
        state_entered_at: OffsetDateTime,
        last_activity_at: OffsetDateTime,
    ) -> Agent {
        Agent {
            tmux_socket: None,
            tmux_session: None,
            kind: AgentKind::Codex,
            session_id: session_id.into(),
            surface: None,
            pane: Some("%1".into()),
            pid: None,
            workload: crate::WorkloadSummary::default(),
            subagents: Vec::new(),
            cwd: None,
            state,
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
            started_at: state_entered_at,
            last_activity_at,
            state_entered_at,
        }
    }
}
