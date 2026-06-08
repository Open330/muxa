use anyhow::{bail, Context, Result};
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime, UtcOffset, Weekday};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TimeRange {
    pub label: String,
    pub since_at: Option<OffsetDateTime>,
    pub until_at: Option<OffsetDateTime>,
}

impl TimeRange {
    pub(crate) fn includes(&self, at: OffsetDateTime) -> bool {
        self.since_at.is_none_or(|since| at >= since)
            && self.until_at.is_none_or(|until| at < until)
    }

    pub(crate) fn effective_end(&self, now: OffsetDateTime) -> OffsetDateTime {
        self.until_at.map_or(now, |until| until.min(now))
    }
}

pub(crate) fn parse_since(raw: &str, now: OffsetDateTime, all_label: &str) -> Result<TimeRange> {
    let trimmed = raw.trim();
    let normalized = normalize(trimmed);
    if normalized == "all" {
        return Ok(TimeRange {
            label: all_label.to_string(),
            since_at: None,
            until_at: None,
        });
    }

    let offset = local_offset();
    if let Some(date) = parse_iso_date(trimmed)? {
        let next = date.next_day().context("could not compute next date")?;
        return Ok(TimeRange {
            label: date.to_string(),
            since_at: Some(local_day_start(date, offset)),
            until_at: Some(local_day_start(next, offset)),
        });
    }

    match normalized.as_str() {
        "today" | "tod" => {
            let start = local_day_start(now.to_offset(offset).date(), offset);
            return Ok(TimeRange {
                label: "today".to_string(),
                since_at: Some(start),
                until_at: None,
            });
        }
        "yesterday" | "yday" => {
            let today = now.to_offset(offset).date();
            let yesterday = today
                .previous_day()
                .context("could not compute yesterday date")?;
            return Ok(TimeRange {
                label: "yesterday".to_string(),
                since_at: Some(local_day_start(yesterday, offset)),
                until_at: Some(local_day_start(today, offset)),
            });
        }
        "last_week" | "lastweek" | "previous_week" | "previousweek" | "prev_week" | "prevweek" => {
            let (since_at, until_at) = previous_calendar_week(now, offset)?;
            return Ok(TimeRange {
                label: "last week".to_string(),
                since_at: Some(since_at),
                until_at: Some(until_at),
            });
        }
        "week" | "last7d" | "last_7d" | "7days" => {
            return Ok(TimeRange {
                label: "last 7d".to_string(),
                since_at: Some(now - time::Duration::days(7)),
                until_at: None,
            });
        }
        _ => {}
    }

    if let Ok(at) = OffsetDateTime::parse(trimmed, &Rfc3339) {
        return Ok(TimeRange {
            label: format!("since {trimmed}"),
            since_at: Some(at),
            until_at: None,
        });
    }
    if trimmed.is_empty() {
        bail!("--since must be today, yesterday, week, last-week, a date like 2026-06-06, a duration like 7d, an RFC3339 timestamp, or all");
    }

    let unit = trimmed.chars().last().context(
        "--since must be today, yesterday, week, last-week, a date like 2026-06-06, a duration like 7d, an RFC3339 timestamp, or all",
    )?;
    let number = &trimmed[..trimmed.len() - unit.len_utf8()];
    let amount: i64 = number
        .parse()
        .with_context(|| format!("invalid --since duration {trimmed:?}"))?;
    if amount <= 0 {
        bail!("--since duration must be greater than zero");
    }
    let duration = match unit {
        's' => time::Duration::seconds(amount),
        'm' => time::Duration::minutes(amount),
        'h' => time::Duration::hours(amount),
        'd' => time::Duration::days(amount),
        'w' => time::Duration::weeks(amount),
        _ => bail!("--since duration unit must be one of s, m, h, d, w"),
    };

    Ok(TimeRange {
        label: format!("last {trimmed}"),
        since_at: Some(now - duration),
        until_at: None,
    })
}

fn normalize(raw: &str) -> String {
    raw.trim().to_ascii_lowercase().replace(['-', ' '], "_")
}

fn local_offset() -> UtcOffset {
    UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC)
}

fn local_day_start(date: Date, offset: UtcOffset) -> OffsetDateTime {
    date.midnight().assume_offset(offset)
}

fn previous_calendar_week(
    now: OffsetDateTime,
    offset: UtcOffset,
) -> Result<(OffsetDateTime, OffsetDateTime)> {
    let current_week_start = week_start_monday(now.to_offset(offset).date())?;
    let previous_week_start = previous_days(current_week_start, 7)?;
    Ok((
        local_day_start(previous_week_start, offset),
        local_day_start(current_week_start, offset),
    ))
}

fn week_start_monday(mut date: Date) -> Result<Date> {
    for _ in 0..weekday_days_from_monday(date.weekday()) {
        date = date
            .previous_day()
            .context("could not compute week start date")?;
    }
    Ok(date)
}

fn previous_days(mut date: Date, days: u8) -> Result<Date> {
    for _ in 0..days {
        date = date
            .previous_day()
            .context("could not compute previous date")?;
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

fn parse_iso_date(raw: &str) -> Result<Option<Date>> {
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
        .with_context(|| format!("invalid date {raw:?}: year must be four digits"))?;
    let month = month
        .parse::<u8>()
        .with_context(|| format!("invalid date {raw:?}: month must be 01-12"))?;
    let day = day
        .parse::<u8>()
        .with_context(|| format!("invalid date {raw:?}: day must be 01-31"))?;
    let month = Month::try_from(month)
        .with_context(|| format!("invalid date {raw:?}: month must be 01-12"))?;
    Date::from_calendar_date(year, month, day)
        .map(Some)
        .with_context(|| format!("invalid date {raw:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::{datetime, offset};

    #[test]
    fn duration_range_keeps_prior_shape() {
        let now = datetime!(2026-06-02 12:00:00 UTC);
        let range = parse_since("2h", now, "all retained").unwrap();

        assert_eq!(range.label, "last 2h");
        assert_eq!(range.since_at, Some(datetime!(2026-06-02 10:00:00 UTC)));
        assert_eq!(range.until_at, None);
    }

    #[test]
    fn week_alias_means_last_seven_days() {
        let now = datetime!(2026-06-02 12:00:00 UTC);
        let range = parse_since("week", now, "all retained").unwrap();

        assert_eq!(range.label, "last 7d");
        assert_eq!(range.since_at, Some(datetime!(2026-05-26 12:00:00 UTC)));
        assert_eq!(range.until_at, None);
    }

    #[test]
    fn last_week_alias_means_previous_calendar_week() {
        let now = datetime!(2026-06-08 12:00:00 UTC);
        let range = parse_since("last-week", now, "all retained").unwrap();

        assert_eq!(range.label, "last week");
        assert_eq!(
            range.until_at.unwrap() - range.since_at.unwrap(),
            time::Duration::days(7)
        );
    }

    #[test]
    fn previous_calendar_week_uses_monday_boundaries() {
        let (since_at, until_at) =
            previous_calendar_week(datetime!(2026-06-08 09:30:00 +9), offset!(+9)).unwrap();

        assert_eq!(since_at, datetime!(2026-06-01 00:00:00 +9));
        assert_eq!(until_at, datetime!(2026-06-08 00:00:00 +9));
    }

    #[test]
    fn iso_date_means_one_local_calendar_day() {
        let now = datetime!(2026-06-06 12:00:00 UTC);
        let range = parse_since("2026-06-06", now, "all retained").unwrap();

        assert_eq!(range.label, "2026-06-06");
        assert_eq!(
            range.until_at.unwrap() - range.since_at.unwrap(),
            time::Duration::days(1)
        );
    }

    #[test]
    fn yesterday_range_has_exclusive_until() {
        let offset = offset!(+9);
        let today = Date::from_calendar_date(2026, time::Month::June, 2).unwrap();
        let yesterday = today.previous_day().unwrap();
        let range = TimeRange {
            label: "yesterday".into(),
            since_at: Some(local_day_start(yesterday, offset)),
            until_at: Some(local_day_start(today, offset)),
        };

        assert!(range.includes(datetime!(2026-06-01 00:00:00 +9)));
        assert!(range.includes(datetime!(2026-06-01 23:59:59 +9)));
        assert!(!range.includes(datetime!(2026-06-02 00:00:00 +9)));
    }
}
