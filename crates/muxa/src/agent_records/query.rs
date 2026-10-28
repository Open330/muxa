//! Transparent, local natural-language query planning; no model or network calls.
use regex::Regex;
use serde::Serialize;
use time::{format_description::well_known::Rfc3339, Date, Month, OffsetDateTime, UtcOffset};

#[derive(Debug, Clone, Serialize)]
pub struct QueryPlan {
    pub query: String,
    pub terms: Vec<String>,
    pub expanded_terms: Vec<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub timezone: String,
    pub method: &'static str,
    pub notes: Vec<String>,
}

pub fn offset(raw: &str) -> Result<UtcOffset, String> {
    match raw {
        "UTC" | "Z" => Ok(UtcOffset::UTC),
        "KST" | "Asia/Seoul" => UtcOffset::from_hms(9, 0, 0).map_err(|e| e.to_string()),
        _ => {
            let re = Regex::new(r"^([+-])(\d{2}):(\d{2})$").unwrap();
            let c = re
                .captures(raw)
                .ok_or("timezone must be UTC, KST, Asia/Seoul or ±HH:MM")?;
            let sign = if &c[1] == "-" { -1 } else { 1 };
            let h: i8 = c[2].parse().map_err(|_| "invalid timezone hour")?;
            let m: i8 = c[3].parse().map_err(|_| "invalid timezone minute")?;
            UtcOffset::from_hms(sign * h, sign * m, 0).map_err(|e| e.to_string())
        }
    }
}

pub fn timestamp(raw: &str) -> Result<i64, String> {
    let at = OffsetDateTime::parse(raw, &Rfc3339).map_err(|_| {
        format!("invalid RFC3339 timestamp: {raw}; include Z or an explicit offset")
    })?;
    i64::try_from(at.unix_timestamp_nanos() / 1_000_000)
        .map_err(|_| "timestamp out of range".into())
}

fn fmt(t: OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_default()
}

#[allow(clippy::too_many_lines)] // Bounded multilingual grammar and expansion vocabulary, with no opaque model step.
pub fn plan(query: &str, timezone: &str, now: OffsetDateTime) -> Result<QueryPlan, String> {
    let zone = if Regex::new(r"(?i)(?:^|[^A-Za-z])KST(?:$|[^A-Za-z])")
        .unwrap()
        .is_match(query)
    {
        "KST"
    } else if Regex::new(r"(?i)(?:^|[^A-Za-z])UTC(?:$|[^A-Za-z])")
        .unwrap()
        .is_match(query)
    {
        "UTC"
    } else {
        timezone
    };
    let tz = offset(zone)?;
    let mut out = QueryPlan {
        query: query.to_owned(),
        terms: Vec::new(),
        expanded_terms: Vec::new(),
        from: None,
        to: None,
        timezone: zone.to_owned(),
        method: "local_keyword_expansion",
        notes: Vec::new(),
    };
    let dates = Regex::new(r"(?P<y>20\d{2})\s*(?:[-./]|년\s*)\s*(?P<m>\d{1,2})\s*(?:[-./]|월\s*)\s*(?P<d>\d{1,2})\s*일?").unwrap();
    // A date embedded in an identifier or version (v2026.10.1, 2026.10.1.2) is a term.
    let found: Vec<_> = dates
        .captures_iter(query)
        .filter(|c| {
            let day_end = c.name("d").map_or(0, |d| d.end());
            let mut after = query[day_end..].chars();
            !before(query, c.get(0).map_or(0, |m| m.start()))
                .is_some_and(|p| p.is_ascii_alphanumeric() || "._/-".contains(p))
                && !match (after.next(), after.next()) {
                    (Some(n), _) if n.is_ascii_digit() => true,
                    (Some('.' | '-' | '/'), Some(n)) => n.is_ascii_digit(),
                    _ => false,
                }
        })
        .collect();
    let date = if found.len() > 1 {
        out.notes.push("Multiple dates found; no automatic time filter applied. Specify from/to to investigate that range.".into());
        None
    } else if let Some(c) = found.first() {
        Some(
            Date::from_calendar_date(
                c["y"].parse().map_err(|_| "invalid year")?,
                Month::try_from(c["m"].parse::<u8>().map_err(|_| "invalid month")?)
                    .map_err(|e| e.to_string())?,
                c["d"].parse().map_err(|_| "invalid day")?,
            )
            .map_err(|e| e.to_string())?,
        )
    } else if query.contains("어제") || query.to_lowercase().contains("yesterday") {
        now.to_offset(tz).date().previous_day()
    } else if query.contains("오늘") || query.to_lowercase().contains("today") {
        Some(now.to_offset(tz).date())
    } else {
        None
    };
    let stripped = cut(
        query,
        found.iter().filter_map(|c| c.get(0)).map(|m| m.range()),
    );
    let clocks = clocks(&stripped);
    if let Some(day) = date {
        let start = day.midnight().assume_offset(tz);
        if let [clock] = clocks.as_slice() {
            let mut h = clock.hour;
            if let Some(pm) = clock.pm {
                if !(1..=12).contains(&h) {
                    return Err("AM/PM hours must be 1..12".into());
                }
                h = h % 12 + if pm { 12 } else { 0 };
            }
            let at = day
                .with_hms(h, clock.minute, 0)
                .map_err(|e| e.to_string())?
                .assume_offset(tz);
            out.from = Some(fmt(at - time::Duration::minutes(15)));
            out.to = Some(fmt(at + time::Duration::minutes(15)));
            out.notes.push(
                "A date and clock in the question means ±15 minutes; use from/to to override."
                    .into(),
            );
        } else {
            if clocks.len() > 1 {
                out.notes.push("Multiple clocks found; using the whole day. Specify from/to for an exact range.".into());
            }
            out.from = Some(fmt(start));
            out.to = Some(fmt(start + time::Duration::days(1)));
        }
    }
    let stripped = cut(&stripped, clocks.into_iter().map(|c| c.range));
    let words = Regex::new(r"[\p{L}\p{N}]+(?:[-_./][\p{L}\p{N}]+)*").unwrap();
    let stop = [
        "the",
        "a",
        "an",
        "and",
        "or",
        "in",
        "on",
        "at",
        "of",
        "to",
        "for",
        "was",
        "were",
        "is",
        "did",
        "we",
        "who",
        "when",
        "how",
        "our",
        "please",
        "find",
        "search",
        "about",
        "around",
        "near",
        "records",
        "record",
        "session",
        "sessions",
        "history",
        "kst",
        "utc",
        "today",
        "yesterday",
        "claude",
        "codex",
        "agent",
        "우리",
        "우리가",
        "우리의",
        "해당",
        "관련",
        "기록",
        "기록을",
        "세션",
        "근처",
        "전후",
        "검색",
        "검색해",
        "검색해주세요",
        "찾아",
        "찾아주세요",
        "확인",
        "확인해주세요",
        "살펴봐",
        "살펴보세요",
        "했는지",
        "있는지",
        "내용",
        "대한",
        "관해서",
        "어제",
        "오늘",
        "오전",
        "오후",
        "경",
    ];
    for m in words.find_iter(&stripped) {
        let mut word = m.as_str().to_lowercase();
        if stop.contains(&word.as_str()) || word.chars().all(char::is_numeric) {
            continue;
        }
        if word.chars().any(|c| ('가'..='힣').contains(&c)) {
            for suffix in [
                "이라는",
                "에관한",
                "관련한",
                "했는지",
                "했나요",
                "했습니까",
                "했는가",
                "해주세요",
                "되었는지",
                "됐는지",
                "했을까",
                "만들었는지",
                "에서는",
                "에서",
                "으로",
                "관련",
                "관한",
                "에",
                "을",
                "를",
                "이",
                "가",
                "은",
                "는",
                "도",
            ] {
                if word.ends_with(suffix) && word.chars().count() > suffix.chars().count() + 1 {
                    word.truncate(word.len() - suffix.len());
                    break;
                }
            }
        }
        if word.chars().count() >= 2 && !out.terms.contains(&word) {
            out.terms.push(word);
        }
    }
    let lower = query.to_lowercase();
    for (triggers, expansion) in [
        (
            &["minio", "미니오", "버킷", "bucket"][..],
            &["minio", "bucket", "버킷", "s3api"][..],
        ),
        (
            &["생성", "만들", "만든", "create", "created", "creation"][..],
            &[
                "create_bucket",
                "CreateBucket",
                "make_bucket",
                "mc mb",
                "생성",
                "created",
            ][..],
        ),
        (
            &["삭제", "지웠", "delete", "remove"][..],
            &["delete_bucket", "DeleteBucket", "mc rb", "삭제", "removed"][..],
        ),
        (
            &["업로드", "올렸", "올린", "upload", "publish"][..],
            &["put_object", "PutObject", "upload", "publish", "업로드"][..],
        ),
        (
            &["배포", "deploy", "release"][..],
            &["deploy", "release", "배포"][..],
        ),
        (
            &["머지", "병합", "merge"][..],
            &[
                "gh pr merge",
                "merge_pull_request",
                "merged",
                "머지",
                "병합",
            ][..],
        ),
        (
            &["재시작", "restart"][..],
            &["restart", "재시작", "systemctl", "reload"][..],
        ),
    ] {
        if triggers.iter().any(|t| lower.contains(t)) {
            for term in expansion {
                let term = term.to_lowercase();
                if !out.terms.contains(&term) && !out.expanded_terms.contains(&term) {
                    out.expanded_terms.push(term);
                }
            }
        }
    }
    out.terms.truncate(24);
    out.expanded_terms.truncate(24);
    if !query.trim().is_empty()
        && out.terms.is_empty()
        && out.expanded_terms.is_empty()
        && out.from.is_none()
    {
        out.notes
            .push("No distinctive keywords found. Supply terms or a time/session filter.".into());
    }
    Ok(out)
}

fn before(text: &str, at: usize) -> Option<char> {
    text[..at].chars().next_back()
}

/// Replace non-overlapping, ascending byte ranges with a space.
fn cut(text: &str, ranges: impl Iterator<Item = std::ops::Range<usize>>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for range in ranges {
        out.push_str(&text[at..range.start]);
        out.push(' ');
        at = range.end;
    }
    out.push_str(&text[at..]);
    out
}

struct Clock {
    hour: u8,
    minute: u8,
    /// `Some(true)` for PM/오후, `Some(false)` for AM/오전, `None` for 24-hour.
    pm: Option<bool>,
    range: std::ops::Range<usize>,
}

/// HH:MM, Korean 오전/오후 H시 [M분|반] and English H[:MM] am/pm, in source order.
/// Bare numbers count only with am/pm; 3시간 (a duration) and digits inside
/// identifiers or versions are not clocks.
fn clocks(text: &str) -> Vec<Clock> {
    let re = Regex::new(r"(?:(?P<k>오전|오후)\s*)?(?:(?P<h>\d{1,2})\s*:\s*(?P<m>\d{1,2})(?::\d{2})?|(?P<kh>\d{1,2})\s*시(?:\s*(?P<km>\d{1,2})\s*분|(?P<kmt>\d{1,2})|\s*(?P<half>반))?|(?P<eh>\d{1,2}))").unwrap();
    let meridiem = Regex::new(r"(?i)^\s*(?P<ap>[ap])\.?m\.?").unwrap();
    let mut out = Vec::new();
    for c in re.captures_iter(text) {
        let whole = c.get(0).unwrap();
        if c.name("k").is_none()
            && before(text, whole.start()).is_some_and(|p| {
                p.is_ascii_digit() || (p.is_ascii_alphabetic() && p != 'T') || ".:/_".contains(p)
            })
        {
            continue;
        }
        let mut end = whole.end();
        if text[end..].starts_with(|n: char| n.is_ascii_digit()) {
            continue;
        }
        let english = meridiem
            .captures(&text[end..])
            .filter(|m| !text[end + m[0].len()..].starts_with(|n: char| n.is_ascii_alphabetic()));
        let korean = c.name("k").map(|k| k.as_str() == "오후");
        let number = |name: &str| c.name(name).and_then(|v| v.as_str().parse::<u8>().ok());
        let (hour, minute, pm) = if let Some(hour) = number("kh") {
            if c.name("km").is_none()
                && c.name("kmt").is_none()
                && c.name("half").is_none()
                && text[end..].starts_with('간')
            {
                continue;
            }
            let minute = number("km")
                .or_else(|| number("kmt"))
                .unwrap_or(if c.name("half").is_some() { 30 } else { 0 });
            (hour, minute, korean)
        } else {
            let pm = english
                .as_ref()
                .map(|m| m["ap"].eq_ignore_ascii_case("p"))
                .or(korean);
            if let Some(m) = &english {
                end += m[0].len();
            }
            match (number("h"), number("m"), number("eh")) {
                (Some(hour), Some(minute), _) => (hour, minute, pm),
                (_, _, Some(hour)) if english.is_some() => (hour, 0, pm),
                _ => continue,
            }
        };
        out.push(Clock {
            hour,
            minute,
            pm,
            range: whole.start()..end,
        });
    }
    out
}

/// Quote user input as FTS literals, never as FTS operators.
pub fn fts(terms: impl Iterator<Item = String>) -> String {
    terms
        .filter(|s| !s.trim().is_empty())
        .map(|s| format!("\"{}\"*", s.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(query: &str, timezone: &str) -> (Option<String>, Option<String>) {
        let p = plan(query, timezone, OffsetDateTime::UNIX_EPOCH).unwrap();
        (p.from, p.to)
    }

    fn window(from: &str, to: &str) -> (Option<String>, Option<String>) {
        (Some(from.into()), Some(to.into()))
    }

    #[test]
    fn english_meridiem_including_midnight_and_noon() {
        let p = plan(
            "2026-10-22 11:54 PM KST bucket",
            "UTC",
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(
            (p.from, p.to),
            window("2026-10-22T23:39:00+09:00", "2026-10-23T00:09:00+09:00")
        );
        assert_eq!(p.terms, ["bucket"]);
        assert_eq!(
            at("2026-10-22 12:05 am bucket", "UTC"),
            window("2026-10-21T23:50:00Z", "2026-10-22T00:20:00Z")
        );
        assert_eq!(
            at("2026-10-22 12PM bucket", "UTC"),
            window("2026-10-22T11:45:00Z", "2026-10-22T12:15:00Z")
        );
        assert_eq!(
            at("2026-10-22 3:30 p.m. bucket", "+05:00"),
            window("2026-10-22T15:15:00+05:00", "2026-10-22T15:45:00+05:00")
        );
        assert_eq!(
            at("2026-10-22 3pm에 버킷", "KST"),
            window("2026-10-22T14:45:00+09:00", "2026-10-22T15:15:00+09:00")
        );
        // "am" must be a whole word, and AM/PM hours stay 1..12.
        assert_eq!(
            at("2026-10-22 11:54 amazon", "UTC"),
            window("2026-10-22T11:39:00Z", "2026-10-22T12:09:00Z")
        );
        assert!(plan("2026-10-22 13pm bucket", "UTC", OffsetDateTime::UNIX_EPOCH).is_err());
        assert!(plan(
            "2026-10-22 0:10 am bucket",
            "UTC",
            OffsetDateTime::UNIX_EPOCH
        )
        .is_err());
    }

    #[test]
    fn korean_hour_without_minutes_half_hours_and_durations() {
        let p = plan(
            "2026년 10월 22일 오후 3시 버킷",
            "KST",
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(
            (p.from, p.to),
            window("2026-10-22T14:45:00+09:00", "2026-10-22T15:15:00+09:00")
        );
        assert_eq!(p.terms, ["버킷"]);
        assert_eq!(
            at("2026-10-22 오후 3시 반 배포", "KST"),
            window("2026-10-22T15:15:00+09:00", "2026-10-22T15:45:00+09:00")
        );
        assert_eq!(
            at("2026-10-22 오전 12시 배포", "KST"),
            window("2026-10-21T23:45:00+09:00", "2026-10-22T00:15:00+09:00")
        );
        assert_eq!(
            at("2026-10-22 11시54 배포", "KST"),
            window("2026-10-22T11:39:00+09:00", "2026-10-22T12:09:00+09:00")
        );
        assert_eq!(
            at("2026-10-22 15시 배포", "KST"),
            window("2026-10-22T14:45:00+09:00", "2026-10-22T15:15:00+09:00")
        );
        // A duration is not a clock: the whole day stays selected.
        assert_eq!(
            at("2026-10-22 3시간 동안 배포", "KST"),
            window("2026-10-22T00:00:00+09:00", "2026-10-23T00:00:00+09:00")
        );
        assert!(plan("2026-10-22 오후 13시", "KST", OffsetDateTime::UNIX_EPOCH).is_err());
    }

    #[test]
    fn versions_and_identifiers_are_terms_not_dates() {
        for query in [
            "v2026.10.1 릴리즈 태그",
            "2026.10.1.2 태그",
            "release-2026-10-22 태그",
            "build_2026.10.22 artifact",
            "2026.10.221 태그",
        ] {
            let p = plan(query, "UTC", OffsetDateTime::UNIX_EPOCH).unwrap();
            assert_eq!((p.from.clone(), p.to.clone()), (None, None), "{query}");
            assert!(p.notes.iter().all(|n| !n.contains("Multiple")), "{query}");
        }
        let p = plan("v2026.10.1 릴리즈 태그", "UTC", OffsetDateTime::UNIX_EPOCH).unwrap();
        assert!(p.terms.contains(&"v2026.10.1".into()));
        // A version beside a real date neither counts as a second date nor
        // contributes clock digits.
        let p = plan(
            "v2026.10.1 배포 2026-10-22",
            "UTC",
            OffsetDateTime::UNIX_EPOCH,
        )
        .unwrap();
        assert_eq!(
            (p.from, p.to),
            window("2026-10-22T00:00:00Z", "2026-10-23T00:00:00Z")
        );
        assert!(p.terms.contains(&"v2026.10.1".into()));
        assert_eq!(
            at("2026-10-22T11:54:00 KST bucket", "UTC"),
            window("2026-10-22T11:39:00+09:00", "2026-10-22T12:09:00+09:00")
        );
        assert_eq!(
            at("2026.10.22. 버킷", "UTC"),
            window("2026-10-22T00:00:00Z", "2026-10-23T00:00:00Z")
        );
    }

    #[test]
    fn clock_without_a_date_is_not_a_filter_or_a_term() {
        let p = plan("3pm 오후 3시 bucket", "UTC", OffsetDateTime::UNIX_EPOCH).unwrap();
        assert_eq!((p.from, p.to), (None, None));
        assert_eq!(p.terms, ["bucket"]);
    }
}
