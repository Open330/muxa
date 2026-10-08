//! `muxa stats --timeline`: a Gantt view of when each project (or session /
//! agent kind) had an agent working, laid out on a real time axis instead of
//! summed into one WORK number.
//!
//! Each row is the wall-clock union of WORK spans for one group key, so two
//! agents working in the same project at once paint one bar rather than
//! double-counting. Shade encodes how much of each cell was covered.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use comfy_table::presets::NOTHING;
use comfy_table::{ColumnConstraint, ContentArrangement, Table, Width};
use muxa::event::{AgentKind, AgentState};
use muxa::ActivityEntry;
use time::{Date, OffsetDateTime, UtcOffset};
use unicode_width::UnicodeWidthStr;

use super::{
    agent_group_key, format_duration, format_local_seconds, local_offset,
    state_transition_group_key, GroupBy, StatsData,
};
use crate::theme::{CliTheme, TableTone};
use crate::truncate_cell;

type Spans = Vec<(i64, i64)>;
type Lanes = BTreeMap<String, Spans>;
type DayLanes<'a> = BTreeMap<i64, BTreeMap<&'a str, Spans>>;

const DAY_SECS: i64 = 86_400;
/// Cell widths (minutes) the day layout may pick from; each divides an hour.
const DAY_CELLS: [i64; 6] = [5, 10, 15, 20, 30, 60];
/// Cell widths (minutes) the range layout may pick from; each divides a day.
const RANGE_CELLS: [i64; 13] = [5, 10, 15, 20, 30, 60, 120, 180, 240, 360, 480, 720, 1440];
const MAX_LABEL_WIDTH: usize = 24;
const DURATION_WIDTH: usize = 6;
/// Rows working less than this in a day (or the range) are noise, not work.
const MIN_ROW_SECS: i64 = 60;
const SHADES: [char; 4] = ['░', '▒', '▓', '█'];

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(super) enum TimelineLayout {
    /// One block per local day with an hour-of-day axis.
    Day,
    /// One row per group across the whole range.
    Range,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct TimelineOptions {
    pub group_by: GroupBy,
    pub layout: TimelineLayout,
    /// Local hour window `[start, end)` for the day layout; `None` fits the data.
    pub hours: Option<(u8, u8)>,
    /// Cell width in minutes; `None` fits the terminal.
    pub cell_minutes: Option<i64>,
    /// Max rows per day (day layout) or overall (range layout); 0 = all.
    pub limit: usize,
}

/// Parse `--hours START-END` (local, END exclusive, `24` allowed).
pub(super) fn parse_hours(value: &str) -> Result<(u8, u8), String> {
    let (start, end) = value
        .split_once('-')
        .ok_or_else(|| format!("expected START-END such as 9-18, got `{value}`"))?;
    let parse = |part: &str| {
        part.trim()
            .parse::<u8>()
            .map_err(|_| format!("`{part}` is not an hour"))
    };
    let (start, end) = (parse(start)?, parse(end)?);
    if start >= end || end > 24 {
        return Err(format!(
            "hours must satisfy 0 <= START < END <= 24, got {start}-{end}"
        ));
    }
    Ok((start, end))
}

/// Merged WORK spans `[start, end)` in unix seconds, keyed by group.
fn work_lanes(data: &StatsData, group_by: GroupBy) -> Lanes {
    let range_start = data.range.since_at.map(OffsetDateTime::unix_timestamp);
    let range_end = data.range.effective_end(data.now).unix_timestamp();
    let mut lanes = Lanes::new();
    let mut push = |key: String, started_at: OffsetDateTime, ended_at: OffsetDateTime| {
        let start = range_start.map_or(started_at.unix_timestamp(), |since| {
            started_at.unix_timestamp().max(since)
        });
        let end = ended_at.unix_timestamp().min(range_end);
        if end > start {
            lanes.entry(key).or_default().push((start, end));
        }
    };

    for entry in &data.activity_entries {
        let ActivityEntry::StateTransition(entry) = entry else {
            continue;
        };
        if entry.from != AgentState::Working || entry.kind == AgentKind::Task {
            continue;
        }
        let started_at = entry.state_entered_at.unwrap_or_else(|| {
            entry.at
                - time::Duration::seconds(i64::try_from(entry.duration_secs).unwrap_or(i64::MAX))
        });
        push(
            state_transition_group_key(data, entry, group_by),
            started_at,
            entry.at,
        );
    }
    // The ledger only records a span when the agent leaves Working, so the
    // in-flight span of a live agent comes from the registry. Background tasks
    // (dev servers, watchers) sit in Working for days and are not hands-on work.
    for agent in &data.agents {
        if agent.state == AgentState::Working && agent.kind != AgentKind::Task {
            push(
                agent_group_key(data, agent, group_by),
                agent.state_entered_at,
                data.now,
            );
        }
    }

    for spans in lanes.values_mut() {
        *spans = merge_spans(std::mem::take(spans));
    }
    lanes
}

fn merge_spans(mut spans: Vec<(i64, i64)>) -> Vec<(i64, i64)> {
    spans.sort_unstable();
    let mut merged: Vec<(i64, i64)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

fn span_secs(spans: &[(i64, i64)]) -> i64 {
    spans.iter().map(|(start, end)| end - start).sum()
}

/// Seconds of `spans` falling in each of `cells` cells of `cell_secs` from `origin`.
fn bin_spans(spans: &[(i64, i64)], origin: i64, cell_secs: i64, cells: usize) -> Vec<i64> {
    let mut busy = vec![0_i64; cells];
    let limit = origin + cell_secs * i64::try_from(cells).unwrap_or(i64::MAX);
    for &(start, end) in spans {
        let (mut start, end) = (start.max(origin), end.min(limit));
        while start < end {
            let index = (start - origin) / cell_secs;
            let cell_end = (origin + (index + 1) * cell_secs).min(end);
            if let Some(slot) = usize::try_from(index).ok().and_then(|i| busy.get_mut(i)) {
                *slot += cell_end - start;
            }
            start = cell_end;
        }
    }
    busy
}

/// Paint one row: shaded cells where work happened, a light grid elsewhere.
fn render_bar(busy: &[i64], cell_secs: i64, marks: &[bool]) -> String {
    busy.iter()
        .zip(marks)
        .map(|(&secs, &mark)| {
            if secs > 0 {
                // Round up so any work in a cell shows at least the lightest shade.
                let level = ceil_div(secs * 4, cell_secs).clamp(1, 4);
                SHADES[usize::try_from(level - 1).unwrap_or(0)]
            } else if mark {
                '┊'
            } else {
                '·'
            }
        })
        .collect()
}

/// Lay `labels` (cell index, text) onto an axis of `cells` columns, skipping
/// any label that would collide with the previous one.
fn render_axis(cells: usize, labels: &[(usize, String)]) -> String {
    let mut axis = vec![' '; cells];
    let mut next_free = 0;
    for (index, label) in labels {
        let width = label.chars().count();
        if *index < next_free || index + width > cells {
            continue;
        }
        for (offset, ch) in label.chars().enumerate() {
            axis[index + offset] = ch;
        }
        next_free = index + width + 1;
    }
    axis.into_iter().collect()
}

fn pick_cell(options: &[i64], span_minutes: i64, width: usize) -> i64 {
    let width = i64::try_from(width.max(1)).unwrap_or(i64::MAX);
    options
        .iter()
        .copied()
        .find(|cell| ceil_div(span_minutes, *cell) <= width)
        .unwrap_or(options[options.len() - 1])
}

/// Ceiling division for the non-negative spans and counts used here.
fn ceil_div(value: i64, divisor: i64) -> i64 {
    (value + divisor - 1) / divisor
}

fn local_day(secs: i64, offset: i64) -> i64 {
    (secs + offset).div_euclid(DAY_SECS)
}

fn day_date(day: i64) -> Option<Date> {
    OffsetDateTime::from_unix_timestamp(day * DAY_SECS)
        .ok()
        .map(OffsetDateTime::date)
}

struct Row {
    label: String,
    duration: String,
    bar: String,
    tone: TableTone,
}

struct Grid {
    label_width: usize,
    bar_width: usize,
}

impl Grid {
    fn new(lanes: &Lanes, terminal_width: usize) -> Self {
        let label_width = lanes
            .keys()
            .map(|key| UnicodeWidthStr::width(key.as_str()))
            .max()
            .unwrap_or(0)
            .clamp("Wed 10-07".len(), MAX_LABEL_WIDTH);
        // Two spaces between each of the three columns.
        let bar_width = terminal_width
            .saturating_sub(label_width + DURATION_WIDTH + 4)
            .max(12);
        Self {
            label_width,
            bar_width,
        }
    }

    fn table(&self, rows: Vec<Row>, theme: CliTheme) -> String {
        let mut table = Table::new();
        table
            .load_preset(NOTHING)
            .set_content_arrangement(ContentArrangement::Disabled);
        for row in rows {
            let header = matches!(row.tone, TableTone::Header);
            table.add_row([
                theme.cell(
                    truncate_cell(&row.label, self.label_width),
                    if header {
                        TableTone::Header
                    } else {
                        TableTone::Accent
                    },
                ),
                theme.right_cell(row.duration, row.tone),
                theme.cell(
                    row.bar,
                    if header {
                        TableTone::Dim
                    } else {
                        TableTone::Good
                    },
                ),
            ]);
        }
        // Columns exist only once rows are added; the last one gets no gutter.
        let widths: [(usize, u16); 3] = [
            (self.label_width, 2),
            (DURATION_WIDTH, 2),
            (self.bar_width, 0),
        ];
        for (column, (width, gutter)) in table.column_iter_mut().zip(widths) {
            column.set_padding((0, gutter));
            column.set_constraint(ColumnConstraint::Absolute(Width::Fixed(
                u16::try_from(width + usize::from(gutter)).unwrap_or(u16::MAX),
            )));
        }
        let mut out = String::new();
        for line in table.lines() {
            let _ = writeln!(out, "{}", line.trim_end());
        }
        out
    }
}

pub(super) fn render(
    data: &StatsData,
    options: TimelineOptions,
    terminal_width: usize,
    theme: CliTheme,
) -> String {
    render_with_offset(data, options, terminal_width, theme, local_offset())
}

fn render_with_offset(
    data: &StatsData,
    options: TimelineOptions,
    terminal_width: usize,
    theme: CliTheme,
    offset: UtcOffset,
) -> String {
    let group_by = match options.group_by {
        // Days are the timeline's axis, not a row key.
        GroupBy::Day => GroupBy::Project,
        other => other,
    };
    let lanes: BTreeMap<_, _> = work_lanes(data, group_by)
        .into_iter()
        .filter(|(_, spans)| span_secs(spans) >= MIN_ROW_SECS)
        .collect();

    let mut out = String::new();
    let _ = writeln!(out, "muxa stats · WORK timeline by {}", group_by.as_str());
    let _ = write!(out, "Range: {}", data.range.label);
    if let Some(since_at) = data.range.since_at {
        let _ = write!(out, " · since {}", format_local_seconds(since_at));
    }
    out.push_str("\n\n");
    if lanes.is_empty() {
        out.push_str("no agent WORK recorded in this range\n");
        return out;
    }

    let grid = Grid::new(&lanes, terminal_width);
    let offset = i64::from(offset.whole_seconds());
    let body = match options.layout {
        TimelineLayout::Day => render_days(&lanes, &grid, options, offset, theme),
        TimelineLayout::Range => render_range(data, &lanes, &grid, options, offset, theme),
    };
    out.push_str(&body);
    out
}

/// Every lane split at local midnight: day -> key -> spans, minus noise rows.
fn split_by_day(lanes: &Lanes, offset: i64) -> DayLanes<'_> {
    let mut days = DayLanes::new();
    for (key, spans) in lanes {
        for &(start, end) in spans {
            let mut cursor = start;
            while cursor < end {
                let day = local_day(cursor, offset);
                let piece_end = end.min((day + 1) * DAY_SECS - offset);
                days.entry(day)
                    .or_default()
                    .entry(key.as_str())
                    .or_default()
                    .push((cursor, piece_end));
                cursor = piece_end;
            }
        }
    }
    for keys in days.values_mut() {
        keys.retain(|_, spans| span_secs(spans) >= MIN_ROW_SECS);
    }
    days.retain(|_, keys| !keys.is_empty());
    days
}

/// Smallest whole-hour window `[lo, hi)` covering every day's work, so all
/// days share one aligned axis.
fn fit_hours(days: &DayLanes<'_>, offset: i64) -> (i64, i64) {
    let minute_of_day = |secs: i64| (secs + offset).rem_euclid(DAY_SECS) / 60;
    let mut lo = 24 * 60;
    let mut hi = 0;
    for (day, keys) in days {
        let day_start = day * DAY_SECS - offset;
        for &(start, end) in keys.values().flatten() {
            lo = lo.min(minute_of_day(start));
            hi = hi.max(if end - day_start >= DAY_SECS {
                24 * 60
            } else {
                minute_of_day(end - 1) + 1
            });
        }
    }
    (lo / 60, ceil_div(hi, 60).max(lo / 60 + 1))
}

fn render_days(
    lanes: &Lanes,
    grid: &Grid,
    options: TimelineOptions,
    offset: i64,
    theme: CliTheme,
) -> String {
    let days = split_by_day(lanes, offset);
    let (lo_hour, hi_hour) = options.hours.map_or_else(
        || fit_hours(&days, offset),
        |(start, end)| (i64::from(start), i64::from(end)),
    );
    let span_minutes = (hi_hour - lo_hour) * 60;
    let cell = options
        .cell_minutes
        .unwrap_or_else(|| pick_cell(&DAY_CELLS, span_minutes, grid.bar_width));
    let cells = usize::try_from(ceil_div(span_minutes, cell)).unwrap_or(0);
    let cells_per_hour = usize::try_from((60 / cell).max(1)).unwrap_or(1);
    let marks: Vec<bool> = (0..cells).map(|i| i % cells_per_hour == 0).collect();
    let axis = render_axis(
        cells,
        &(0..cells)
            .step_by(cells_per_hour)
            .map(|i| {
                let hour = lo_hour + i64::try_from(i / cells_per_hour).unwrap_or(0);
                (i, format!("{:02}", hour % 24))
            })
            .collect::<Vec<_>>(),
    );

    let mut rows = Vec::new();
    for (day, keys) in &days {
        let origin = day * DAY_SECS - offset + lo_hour * 3_600;
        let day_union = merge_spans(keys.values().flatten().copied().collect());
        if !rows.is_empty() {
            rows.push(Row {
                label: String::new(),
                duration: String::new(),
                bar: String::new(),
                tone: TableTone::Dim,
            });
        }
        rows.push(Row {
            label: day_date(*day).map_or_else(String::new, |date| {
                format!(
                    "{} {:02}-{:02}",
                    &date.weekday().to_string()[..3],
                    u8::from(date.month()),
                    date.day()
                )
            }),
            duration: format_duration(u64::try_from(span_secs(&day_union)).unwrap_or(0)),
            bar: axis.clone(),
            tone: TableTone::Header,
        });
        let mut ordered: Vec<_> = keys.iter().collect();
        ordered.sort_by_key(|(key, spans)| (spans.first().map_or(i64::MAX, |s| s.0), *key));
        let hidden = limit_rows(&mut ordered, options.limit);
        for (key, spans) in ordered {
            rows.push(Row {
                label: (*key).to_string(),
                duration: format_duration(u64::try_from(span_secs(spans)).unwrap_or(0)),
                bar: render_bar(
                    &bin_spans(spans, origin, cell * 60, cells),
                    cell * 60,
                    &marks,
                ),
                tone: TableTone::Good,
            });
        }
        push_hidden(&mut rows, hidden);
    }
    let mut out = grid.table(rows, theme);
    let _ = writeln!(out, "\none cell = {cell} min");
    out
}

fn render_range(
    data: &StatsData,
    lanes: &Lanes,
    grid: &Grid,
    options: TimelineOptions,
    offset: i64,
    theme: CliTheme,
) -> String {
    let first = lanes
        .values()
        .filter_map(|spans| spans.first().map(|span| span.0))
        .min()
        .unwrap_or(0);
    let last = data.range.effective_end(data.now).unix_timestamp();
    // Start on the first active local day so empty leading days don't eat width.
    let start = local_day(first, offset) * DAY_SECS - offset;
    let span_minutes = ceil_div((last - start).max(60), 60);
    let cell = options
        .cell_minutes
        .unwrap_or_else(|| pick_cell(&RANGE_CELLS, span_minutes, grid.bar_width));
    let cells = usize::try_from(ceil_div(span_minutes, cell)).unwrap_or(0);
    let cell_secs = cell * 60;
    let cell_start = |i: usize| start + i64::try_from(i).unwrap_or(0) * cell_secs;
    let day_marks: Vec<bool> = (0..cells)
        .map(|i| (cell_start(i) + offset).rem_euclid(DAY_SECS) == 0)
        .collect();
    let axis = render_axis(
        cells,
        &(0..cells)
            .filter(|&i| day_marks[i])
            .filter_map(|i| {
                let day = day_date(local_day(cell_start(i), offset))?;
                Some((i, format!("{:02}-{:02}", u8::from(day.month()), day.day())))
            })
            .collect::<Vec<_>>(),
    );

    let union = merge_spans(lanes.values().flatten().copied().collect());
    let mut rows = vec![Row {
        label: "all".to_string(),
        duration: format_duration(u64::try_from(span_secs(&union)).unwrap_or(0)),
        bar: axis,
        tone: TableTone::Header,
    }];
    let mut ordered: Vec<_> = lanes.iter().collect();
    ordered.sort_by_key(|(key, spans)| (std::cmp::Reverse(span_secs(spans)), *key));
    let hidden = limit_rows(&mut ordered, options.limit);
    for (key, spans) in ordered {
        rows.push(Row {
            label: key.clone(),
            duration: format_duration(u64::try_from(span_secs(spans)).unwrap_or(0)),
            bar: render_bar(
                &bin_spans(spans, start, cell_secs, cells),
                cell_secs,
                &day_marks,
            ),
            tone: TableTone::Good,
        });
    }
    push_hidden(&mut rows, hidden);
    let mut out = grid.table(rows, theme);
    let cell_label = if cell % 60 == 0 {
        format!("{}h", cell / 60)
    } else {
        format!("{cell} min")
    };
    let _ = writeln!(out, "\none cell = {cell_label}");
    out
}

fn limit_rows<T>(rows: &mut Vec<T>, limit: usize) -> usize {
    if limit == 0 || rows.len() <= limit {
        return 0;
    }
    let hidden = rows.len() - limit;
    rows.truncate(limit);
    hidden
}

fn push_hidden(rows: &mut Vec<Row>, hidden: usize) {
    if hidden > 0 {
        rows.push(Row {
            label: format!("+{hidden} more"),
            duration: String::new(),
            bar: "--limit 0 shows all".to_string(),
            tone: TableTone::Dim,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stats::tests::data;
    use muxa::{StateTransitionEntry, StateTransitionInput};
    use time::macros::datetime;

    fn working(cwd: &str, from: OffsetDateTime, to: OffsetDateTime) -> ActivityEntry {
        ActivityEntry::StateTransition(StateTransitionEntry::new(StateTransitionInput {
            at: to,
            kind: AgentKind::Codex,
            session_id: format!("agent-{cwd}-{}", from.unix_timestamp()),
            pane: None,
            session_name: None,
            cwd: Some(format!("/work/{cwd}")),
            from: AgentState::Working,
            to: AgentState::Idle,
            state_entered_at: Some(from),
        }))
    }

    fn options(layout: TimelineLayout) -> TimelineOptions {
        TimelineOptions {
            group_by: GroupBy::Day,
            layout,
            hours: None,
            cell_minutes: None,
            limit: 0,
        }
    }

    fn render_utc(data: &StatsData, options: TimelineOptions, width: usize) -> String {
        render_with_offset(data, options, width, CliTheme::plain(), UtcOffset::UTC)
    }

    #[test]
    fn parallel_agents_in_one_project_merge_into_one_span() {
        let mut d = data(Vec::new());
        d.activity_entries = vec![
            working(
                "muxa",
                datetime!(2026-05-30 09:00 UTC),
                datetime!(2026-05-30 10:00 UTC),
            ),
            working(
                "muxa",
                datetime!(2026-05-30 09:30 UTC),
                datetime!(2026-05-30 10:30 UTC),
            ),
            working(
                "other",
                datetime!(2026-05-30 09:00 UTC),
                datetime!(2026-05-30 09:10 UTC),
            ),
        ];
        let lanes = work_lanes(&d, GroupBy::Project);
        assert_eq!(span_secs(&lanes["muxa"]), 90 * 60);
        assert_eq!(span_secs(&lanes["other"]), 10 * 60);
    }

    #[test]
    fn live_working_agent_contributes_its_open_span() {
        let mut d = data(Vec::new());
        d.agents = vec![crate::stats::tests::live_agent(
            AgentState::Working,
            datetime!(2026-05-30 11:00 UTC),
            Some("/work/muxa"),
        )];
        let lanes = work_lanes(&d, GroupBy::Project);
        assert_eq!(span_secs(&lanes["muxa"]), 3_600);

        d.agents[0].kind = AgentKind::Task;
        assert!(work_lanes(&d, GroupBy::Project).is_empty());
    }

    #[test]
    fn bin_spans_splits_across_cells() {
        let busy = bin_spans(&[(50, 250)], 0, 100, 3);
        assert_eq!(busy, vec![50, 100, 50]);
        let marks = [true, false, false];
        assert_eq!(render_bar(&busy, 100, &marks), "▒█▒");
        assert_eq!(render_bar(&[0, 0, 1], 100, &marks), "┊·░");
    }

    #[test]
    fn axis_skips_colliding_labels() {
        let labels = [(0, "09".into()), (1, "10".into()), (3, "11".into())];
        assert_eq!(render_axis(6, &labels), "09 11 ");
    }

    #[test]
    fn parse_hours_accepts_window_and_rejects_bad_input() {
        assert_eq!(parse_hours("9-24"), Ok((9, 24)));
        assert!(parse_hours("18-9").is_err());
        assert!(parse_hours("9").is_err());
        assert!(parse_hours("0-25").is_err());
    }

    #[test]
    fn day_layout_places_work_at_its_clock_position() {
        let mut d = data(Vec::new());
        d.activity_entries = vec![
            working(
                "muxa",
                datetime!(2026-05-30 09:00 UTC),
                datetime!(2026-05-30 10:00 UTC),
            ),
            working(
                "barshelf",
                datetime!(2026-05-30 10:00 UTC),
                datetime!(2026-05-30 11:00 UTC),
            ),
        ];
        let mut opts = options(TimelineLayout::Day);
        opts.cell_minutes = Some(15);
        let out = render_utc(&d, opts, 80);
        let muxa = out.lines().find(|l| l.starts_with("muxa  ")).unwrap();
        let barshelf = out.lines().find(|l| l.starts_with("barshelf")).unwrap();
        assert!(muxa.ends_with("1h00m  ████┊···"), "{out}");
        assert!(barshelf.ends_with("1h00m  ┊···████"), "{out}");
        assert!(out.contains("Sat 05-30   2h00m  09  10"), "{out}");
    }

    #[test]
    fn day_layout_limits_rows_per_day() {
        let mut d = data(Vec::new());
        d.activity_entries = ["a", "b", "c"]
            .into_iter()
            .map(|p| {
                working(
                    p,
                    datetime!(2026-05-30 09:00 UTC),
                    datetime!(2026-05-30 10:00 UTC),
                )
            })
            .collect();
        let mut opts = options(TimelineLayout::Day);
        opts.limit = 2;
        let out = render_utc(&d, opts, 80);
        assert!(out.contains("+1 more"), "{out}");
        assert!(!out.lines().any(|l| l.starts_with("c ")), "{out}");
    }

    #[test]
    fn range_layout_keeps_one_row_per_project_ranked_by_work() {
        let mut d = data(Vec::new());
        d.activity_entries = vec![
            working(
                "small",
                datetime!(2026-05-28 09:00 UTC),
                datetime!(2026-05-28 09:30 UTC),
            ),
            working(
                "big",
                datetime!(2026-05-29 09:00 UTC),
                datetime!(2026-05-29 12:00 UTC),
            ),
            working(
                "big",
                datetime!(2026-05-30 09:00 UTC),
                datetime!(2026-05-30 10:00 UTC),
            ),
        ];
        let out = render_utc(&d, options(TimelineLayout::Range), 80);
        let rows: Vec<_> = out
            .lines()
            .filter(|l| l.starts_with("big") || l.starts_with("small"))
            .collect();
        assert_eq!(rows.len(), 2, "{out}");
        assert!(
            rows[0].starts_with("big") && rows[0].contains("4h00m"),
            "{out}"
        );
        assert!(out.contains("05-28"), "{out}");
    }
}
