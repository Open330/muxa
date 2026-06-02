use anyhow::{bail, Context, Result};
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime, UtcOffset};

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
        bail!("--since must be today, yesterday, week, a duration like 7d, an RFC3339 timestamp, or all");
    }

    let unit = trimmed.chars().last().context(
        "--since must be today, yesterday, week, a duration like 7d, an RFC3339 timestamp, or all",
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
