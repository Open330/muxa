//! Timeline projection over the activity ledger.
//!
//! The activity ledger is append-only and transition-shaped. This module
//! turns those rows into clipped intervals that UIs can render as lanes.

use crate::activity::{ActivityEntry, HumanInteractionKind};
use crate::event::{AgentKind, AgentState};
use crate::scope_filter::ScopeExclusions;
use crate::session_activity::SessionActivity;
use crate::state::{Agent, SYNTHETIC_SESSION_PREFIX};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
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

#[derive(Debug, Clone, Default, Serialize)]
pub struct TimelineTotals {
    pub working_secs: u64,
    pub waiting_secs: u64,
    pub error_secs: u64,
    pub idle_secs: u64,
    pub starting_secs: u64,
    pub stopped_secs: u64,
    pub human_secs: u64,
    pub foreground_secs: u64,
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
    pub activity_entries: &'a [ActivityEntry],
    pub agents: &'a [Agent],
    pub session_activities: &'a [SessionActivity],
    pub pane_sessions: &'a HashMap<String, String>,
    pub filters: TimelineFilters,
    pub notes: Vec<String>,
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

    let totals = lanes
        .iter()
        .fold(TimelineTotals::default(), |mut acc, lane| {
            acc.add_totals(&lane.totals);
            acc
        });
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
        notes,
    }
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
        HumanInteractionEntry, HumanInteractionInput, StateTransitionEntry, StateTransitionInput,
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
            activity_entries: &[entry],
            agents: &[],
            session_activities: &[],
            pane_sessions: &HashMap::new(),
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
            activity_entries: &[entry],
            agents: &[],
            session_activities: &[],
            pane_sessions: &HashMap::new(),
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
            activity_entries: &entries,
            agents: &[],
            session_activities: &[],
            pane_sessions: &HashMap::new(),
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
            activity_entries: &entries,
            agents: &[],
            session_activities: &[],
            pane_sessions: &HashMap::new(),
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
            activity_entries: &[],
            agents: &agents,
            session_activities: &[],
            pane_sessions: &HashMap::new(),
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
            activity_entries: &[],
            agents: &agents,
            session_activities: &[],
            pane_sessions: &HashMap::new(),
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
            activity_entries: &[],
            agents: &agents,
            session_activities: &[],
            pane_sessions: &HashMap::new(),
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
            kind: AgentKind::Codex,
            session_id: session_id.into(),
            surface: None,
            pane: Some("%1".into()),
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
            started_at: state_entered_at,
            last_activity_at,
            state_entered_at,
        }
    }
}
