//! Time windows: "past 6 hours", "today", "Oct 28 through Oct 30".
//!
//! One grammar, shared by the CLI and the HTTP API, so `wx rain 3d` and
//! `/api/weather/rain?window=3d` mean exactly the same thing:
//!
//! | Spec | Meaning |
//! |---|---|
//! | `15m`, `45m` | the last N minutes of radar-only data, rounded up to 15-minute steps |
//! | `6h`, `3d`, `2w` | the last N hours / days / weeks of available data |
//! | `today` | local midnight → latest data |
//! | `yesterday` | the previous local calendar day |
//! | `2026-10-28..2026-10-30` | Oct 28 00:00 → Oct 31 00:00 local (an end *date* is inclusive) |
//! | `2026-11-01T08:00..now` | a local time → latest data; `..` with no end also means now |
//! | `2026-11-01T12:00Z..2026-11-02T00:00Z` | explicit UTC (or any `±HH:MM` offset) |
//! | `a/b` | same as `a..b` |
//!
//! **Local** means the configured time zone (the system's by default), not
//! the machine's: "today" in Denver is the same day wherever the CLI runs.
//!
//! Gauge-corrected MRMS is hourly, so every window but `Nm` snaps **outward** to
//! whole hours — a start of 08:30 begins at 08:00 — and the resolved window is
//! always reported back, so nobody mistakes a snapped total for an exact one. The
//! end is clipped to the newest hour that has data.

use jiff::{civil, tz::TimeZone, Timestamp, ToSpan};
use serde::Serialize;
use thiserror::Error;

pub const HOUR_SECS: i64 = 3600;

/// Length of the shortest MRMS accumulation.
pub const QUARTER_SECS: i64 = 900;

#[derive(Debug, Error, PartialEq)]
pub enum WindowError {
    #[error("unrecognized window {0:?} — try 24h, 3d, today, yesterday, or 2026-11-01..now")]
    Syntax(String),
    #[error("unrecognized date/time {0:?} — use YYYY-MM-DD, YYYY-MM-DDTHH:MM, or RFC 3339")]
    Time(String),
    #[error("window is empty: no hours of data fall inside it yet (data through {through})")]
    Empty { through: String },
    #[error("window is {hours} h; the limit here is {max} h")]
    TooLong { hours: i64, max: i64 },
    #[error("{0}")]
    BadRequest(String),
}

/// One end of an explicit range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    /// The newest hour with data.
    Now,
    /// An instant.
    At(Timestamp),
    /// A calendar date in the region's zone: its midnight as a start, the
    /// following midnight as an end.
    Date(civil::Date),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowSpec {
    Last { hours: i64 },
    LastMinutes { minutes: i64 },
    Today,
    Yesterday,
    Range { start: Endpoint, end: Endpoint },
}

/// A resolved window: accumulation hours ending in `(start, end]`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Window {
    pub start: Timestamp,
    pub end: Timestamp,
}

impl Window {
    pub fn new(start: Timestamp, end: Timestamp) -> Self {
        Self { start, end }
    }

    pub fn hours(&self) -> i64 {
        (self.end.as_second() - self.start.as_second()) / HOUR_SECS
    }

    pub fn minutes(&self) -> i64 {
        (self.end.as_second() - self.start.as_second()) / 60
    }

    /// Whether whole-hour products can cover this window exactly.
    pub fn is_hour_aligned(&self) -> bool {
        self.start.as_second().rem_euclid(HOUR_SECS) == 0
            && self.end.as_second().rem_euclid(HOUR_SECS) == 0
    }

    /// Valid times of every 15-minute product in the window.
    pub fn quarter_ends(&self) -> impl Iterator<Item = Timestamp> {
        let (s, e) = (self.start.as_second(), self.end.as_second());
        (1..=(e - s) / QUARTER_SECS).map(move |k| ts(s + k * QUARTER_SECS))
    }

    /// Valid times of every hourly product in the window: `start+1h ..= end`.
    pub fn hour_ends(&self) -> impl Iterator<Item = Timestamp> {
        let (s, e) = (self.start.as_second(), self.end.as_second());
        (1..=(e - s) / HOUR_SECS).map(move |k| ts(s + k * HOUR_SECS))
    }
}

/// Floor a timestamp to its quarter hour.
pub fn floor_quarter(t: Timestamp) -> Timestamp {
    ts(t.as_second().div_euclid(QUARTER_SECS) * QUARTER_SECS)
}

/// Floor a timestamp to the top of its UTC hour. Every US zone offset is a
/// whole number of hours, so this is also a local hour boundary.
pub fn floor_hour(t: Timestamp) -> Timestamp {
    ts(t.as_second().div_euclid(HOUR_SECS) * HOUR_SECS)
}

pub fn ceil_hour(t: Timestamp) -> Timestamp {
    let s = t.as_second();
    ts(-(-s).div_euclid(HOUR_SECS) * HOUR_SECS)
}

fn ts(secs: i64) -> Timestamp {
    Timestamp::from_second(secs).expect("timestamp in range")
}

impl WindowSpec {
    pub fn parse(s: &str, tz: &TimeZone) -> Result<Self, WindowError> {
        let s = s.trim();
        let lower = s.to_ascii_lowercase();
        match lower.as_str() {
            "today" => return Ok(Self::Today),
            "yesterday" => return Ok(Self::Yesterday),
            _ => {}
        }

        if let Some(hours) = parse_duration(&lower) {
            return Ok(Self::Last { hours });
        }
        if let Some(minutes) = parse_minutes(&lower) {
            return Ok(Self::LastMinutes { minutes });
        }

        let (a, b) = s
            .split_once("..")
            .or_else(|| s.split_once('/'))
            .ok_or_else(|| WindowError::Syntax(s.into()))?;
        let start = parse_endpoint(a, tz)?;
        let end = if b.trim().is_empty() {
            Endpoint::Now
        } else {
            parse_endpoint(b, tz)?
        };
        if start == Endpoint::Now {
            return Err(WindowError::Syntax(s.into()));
        }
        Ok(Self::Range { start, end })
    }

    /// Short human label, e.g. "Past 24 hours".
    pub fn label(&self) -> String {
        match *self {
            Self::Last { hours } if hours % 24 == 0 && hours >= 48 => {
                format!("Past {} days", hours / 24)
            }
            Self::LastMinutes { minutes } => format!("Past {minutes} minutes"),
            Self::Last { hours: 1 } => "Past hour".into(),
            Self::Last { hours } => format!("Past {hours} hours"),
            Self::Today => "Today".into(),
            Self::Yesterday => "Yesterday".into(),
            Self::Range { .. } => "Custom range".into(),
        }
    }

    /// Pin `Nm` to concrete quarter hours ending at `latest_quarter`.
    pub fn resolve_minutes(
        minutes: i64,
        latest_quarter: Timestamp,
        max_hours: i64,
    ) -> Result<Resolved, WindowError> {
        let secs = (minutes * 60 + QUARTER_SECS - 1).div_euclid(QUARTER_SECS) * QUARTER_SECS;
        if secs > max_hours * HOUR_SECS {
            return Err(WindowError::TooLong {
                hours: secs / HOUR_SECS,
                max: max_hours,
            });
        }
        Ok(Resolved {
            window: Window::new(ts(latest_quarter.as_second() - secs), latest_quarter),
            clipped: false,
            label: Self::LastMinutes { minutes: secs / 60 }.label(),
        })
    }

    /// Pin the spec to concrete hours.
    ///
    /// `now` decides which calendar day is "today"; `latest` is the end of the
    /// newest hour that has data, and caps every window.
    pub fn resolve(
        &self,
        now: Timestamp,
        latest: Timestamp,
        tz: &TimeZone,
        max_hours: i64,
    ) -> Result<Resolved, WindowError> {
        let latest = floor_hour(latest);
        let today = now.to_zoned(tz.clone()).date();
        let midnight = |d: civil::Date| -> Timestamp {
            d.to_zoned(tz.clone())
                .expect("local midnight exists in US zones")
                .timestamp()
        };

        let (start, end) = match *self {
            Self::Last { hours } => (ts(latest.as_second() - hours * HOUR_SECS), latest),
            Self::LastMinutes { minutes } => {
                let secs = (minutes * 60 + HOUR_SECS - 1).div_euclid(HOUR_SECS) * HOUR_SECS;
                (ts(latest.as_second() - secs), latest)
            }
            Self::Today => (midnight(today), latest),
            Self::Yesterday => {
                let y = today.checked_sub(1.day()).expect("date in range");
                (midnight(y), midnight(today))
            }
            Self::Range { start, end } => {
                let s = match start {
                    Endpoint::At(t) => floor_hour(t),
                    Endpoint::Date(d) => midnight(d),
                    Endpoint::Now => latest,
                };
                let e = match end {
                    Endpoint::Now => latest,
                    Endpoint::At(t) => ceil_hour(t),
                    Endpoint::Date(d) => midnight(d.checked_add(1.day()).expect("date in range")),
                };
                (s, e)
            }
        };

        let clipped = end > latest;
        let end = end.min(latest);
        if end <= start {
            return Err(WindowError::Empty {
                through: local_label(latest, tz),
            });
        }
        let w = Window::new(start, end);
        if w.hours() > max_hours {
            return Err(WindowError::TooLong {
                hours: w.hours(),
                max: max_hours,
            });
        }
        Ok(Resolved {
            window: w,
            clipped,
            label: self.label(),
        })
    }
}

/// A window plus how it was arrived at.
#[derive(Debug, Clone, Serialize)]
pub struct Resolved {
    pub window: Window,
    /// The requested end was later than the newest data, so it was cut short.
    pub clipped: bool,
    pub label: String,
}

/// `15m`, `45min`, `90 minutes`.
fn parse_minutes(s: &str) -> Option<i64> {
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = num.parse().ok()?;
    let is_minutes = matches!(unit.trim(), "m" | "min" | "mins" | "minute" | "minutes");
    (n > 0 && is_minutes).then_some(n)
}

/// `24h`, `3d`, `2w`.
fn parse_duration(s: &str) -> Option<i64> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: i64 = num.parse().ok()?;
    if n <= 0 {
        return None;
    }
    let per = match unit.trim() {
        "h" | "hr" | "hrs" | "hour" | "hours" => 1,
        "d" | "day" | "days" => 24,
        "w" | "wk" | "week" | "weeks" => 24 * 7,
        _ => return None,
    };
    n.checked_mul(per)
}

fn parse_endpoint(s: &str, tz: &TimeZone) -> Result<Endpoint, WindowError> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("now") {
        return Ok(Endpoint::Now);
    }
    // An explicit instant (Z or ±HH:MM).
    if let Ok(t) = s.parse::<Timestamp>() {
        return Ok(Endpoint::At(t));
    }
    // Only a bare date is a date. jiff's `Date` parser also accepts
    // "2026-11-02 08:30" and silently drops the time.
    if s.len() == "YYYY-MM-DD".len() {
        if let Ok(d) = s.parse::<civil::Date>() {
            return Ok(Endpoint::Date(d));
        }
    }
    // A local wall-clock time. Accept "T" or a space, with or without minutes.
    let norm = s.replacen(' ', "T", 1);
    let with_minutes = if norm.len() == "YYYY-MM-DDTHH".len() {
        format!("{norm}:00")
    } else {
        norm
    };
    if let Ok(dt) = with_minutes.parse::<civil::DateTime>() {
        let z = dt
            .to_zoned(tz.clone())
            .map_err(|_| WindowError::Time(s.into()))?;
        return Ok(Endpoint::At(z.timestamp()));
    }
    Err(WindowError::Time(s.into()))
}

/// "Nov 2, 6:00 PM EST" in the region's zone.
pub fn local_label(t: Timestamp, tz: &TimeZone) -> String {
    t.to_zoned(tz.clone())
        .strftime("%b %-d, %-I:%M %p %Z")
        .to_string()
}

pub fn region_tz(name: &str) -> anyhow::Result<TimeZone> {
    TimeZone::get(name).map_err(|e| anyhow::anyhow!("time zone {name:?}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tz() -> TimeZone {
        TimeZone::get("America/New_York").unwrap()
    }

    fn t(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    const MAX: i64 = 24 * 92;

    #[test]
    fn durations() {
        let z = tz();
        assert_eq!(
            WindowSpec::parse("6h", &z),
            Ok(WindowSpec::Last { hours: 6 })
        );
        assert_eq!(
            WindowSpec::parse("3d", &z),
            Ok(WindowSpec::Last { hours: 72 })
        );
        assert_eq!(
            WindowSpec::parse("2 weeks", &z),
            Ok(WindowSpec::Last { hours: 336 })
        );
        assert_eq!(WindowSpec::parse("TODAY", &z), Ok(WindowSpec::Today));
        assert!(WindowSpec::parse("0h", &z).is_err());
        assert_eq!(
            WindowSpec::parse("90m", &z),
            Ok(WindowSpec::LastMinutes { minutes: 90 })
        );
        assert!(WindowSpec::parse("soon", &z).is_err());
    }

    #[test]
    fn last_n_ends_at_latest_data() {
        let r = WindowSpec::Last { hours: 24 }
            .resolve(
                t("2026-11-02T23:22:00Z"),
                t("2026-11-02T23:00:00Z"),
                &tz(),
                MAX,
            )
            .unwrap();
        assert_eq!(r.window.start, t("2026-11-01T23:00:00Z"));
        assert_eq!(r.window.end, t("2026-11-02T23:00:00Z"));
        assert_eq!(r.window.hours(), 24);
        assert!(!r.clipped);
    }

    #[test]
    fn today_is_local_midnight_after_dst_ends() {
        // DST ended 2026-11-01 02:00 EDT; on Nov 2 local midnight is 05:00Z.
        let r = WindowSpec::Today
            .resolve(
                t("2026-11-02T23:22:00Z"),
                t("2026-11-02T23:00:00Z"),
                &tz(),
                MAX,
            )
            .unwrap();
        assert_eq!(r.window.start, t("2026-11-02T05:00:00Z"));
        assert_eq!(r.window.hours(), 18);
    }

    #[test]
    fn today_is_local_not_utc() {
        // 01:30Z on Nov 3 is still 8:30 PM Nov 2 in New York.
        let r = WindowSpec::Today
            .resolve(
                t("2026-11-03T01:30:00Z"),
                t("2026-11-03T01:00:00Z"),
                &tz(),
                MAX,
            )
            .unwrap();
        assert_eq!(r.window.start, t("2026-11-02T05:00:00Z"));
    }

    #[test]
    fn yesterday_spans_the_25_hour_fallback_day() {
        // Nov 1 2026 is the DST change: 25 hours long.
        let r = WindowSpec::Yesterday
            .resolve(
                t("2026-11-02T23:00:00Z"),
                t("2026-11-02T23:00:00Z"),
                &tz(),
                MAX,
            )
            .unwrap();
        assert_eq!(r.window.start, t("2026-11-01T04:00:00Z"));
        assert_eq!(r.window.end, t("2026-11-02T05:00:00Z"));
        assert_eq!(r.window.hours(), 25);
    }

    #[test]
    fn yesterday_is_clipped_when_last_hour_is_not_in_yet() {
        let r = WindowSpec::Yesterday
            .resolve(
                t("2026-11-02T05:10:00Z"),
                t("2026-11-02T04:00:00Z"),
                &tz(),
                MAX,
            )
            .unwrap();
        assert!(r.clipped);
        assert_eq!(r.window.end, t("2026-11-02T04:00:00Z"));
    }

    #[test]
    fn today_just_after_midnight_is_empty() {
        let err = WindowSpec::Today
            .resolve(
                t("2026-11-02T05:20:00Z"),
                t("2026-11-02T05:00:00Z"),
                &tz(),
                MAX,
            )
            .unwrap_err();
        assert!(matches!(err, WindowError::Empty { .. }));
    }

    #[test]
    fn date_range_end_date_is_inclusive() {
        let spec = WindowSpec::parse("2026-10-28..2026-10-30", &tz()).unwrap();
        let r = spec
            .resolve(
                t("2026-11-02T23:00:00Z"),
                t("2026-11-02T23:00:00Z"),
                &tz(),
                MAX,
            )
            .unwrap();
        // EDT (UTC-4) in late October.
        assert_eq!(r.window.start, t("2026-10-28T04:00:00Z"));
        assert_eq!(r.window.end, t("2026-10-31T04:00:00Z"));
        assert_eq!(r.window.hours(), 72);
    }

    #[test]
    fn local_times_snap_outward_and_open_end_means_now() {
        let spec = WindowSpec::parse("2026-11-02 08:30..", &tz()).unwrap();
        let r = spec
            .resolve(
                t("2026-11-02T23:22:00Z"),
                t("2026-11-02T23:00:00Z"),
                &tz(),
                MAX,
            )
            .unwrap();
        // 08:30 EST = 13:30Z → floors to 13:00Z.
        assert_eq!(r.window.start, t("2026-11-02T13:00:00Z"));
        assert_eq!(r.window.end, t("2026-11-02T23:00:00Z"));

        let spec = WindowSpec::parse("2026-11-02T08/2026-11-02T10:15", &tz()).unwrap();
        let r = spec
            .resolve(
                t("2026-11-02T23:22:00Z"),
                t("2026-11-02T23:00:00Z"),
                &tz(),
                MAX,
            )
            .unwrap();
        assert_eq!(r.window.start, t("2026-11-02T13:00:00Z"));
        // 10:15 EST = 15:15Z → ceils to 16:00Z.
        assert_eq!(r.window.end, t("2026-11-02T16:00:00Z"));
    }

    #[test]
    fn explicit_utc() {
        let spec = WindowSpec::parse("2026-11-01T12:00Z..2026-11-02T00:00Z", &tz()).unwrap();
        let r = spec
            .resolve(
                t("2026-11-02T23:22:00Z"),
                t("2026-11-02T23:00:00Z"),
                &tz(),
                MAX,
            )
            .unwrap();
        assert_eq!(r.window.hours(), 12);
    }

    #[test]
    fn too_long_is_rejected() {
        let err = WindowSpec::Last { hours: 24 * 400 }
            .resolve(
                t("2026-11-02T23:22:00Z"),
                t("2026-11-02T23:00:00Z"),
                &tz(),
                MAX,
            )
            .unwrap_err();
        assert!(matches!(err, WindowError::TooLong { .. }));
    }

    #[test]
    fn hour_ends_enumerates_valid_times() {
        let w = Window::new(t("2026-11-02T10:00:00Z"), t("2026-11-02T13:00:00Z"));
        let v: Vec<_> = w.hour_ends().collect();
        assert_eq!(
            v,
            vec![
                t("2026-11-02T11:00:00Z"),
                t("2026-11-02T12:00:00Z"),
                t("2026-11-02T13:00:00Z")
            ]
        );
    }

    #[test]
    fn rounding() {
        assert_eq!(
            floor_hour(t("2026-11-02T10:59:59Z")),
            t("2026-11-02T10:00:00Z")
        );
        assert_eq!(
            ceil_hour(t("2026-11-02T10:00:01Z")),
            t("2026-11-02T11:00:00Z")
        );
        assert_eq!(
            ceil_hour(t("2026-11-02T10:00:00Z")),
            t("2026-11-02T10:00:00Z")
        );
    }

    #[test]
    fn labels() {
        assert_eq!(WindowSpec::Last { hours: 24 }.label(), "Past 24 hours");
        assert_eq!(WindowSpec::Last { hours: 72 }.label(), "Past 3 days");
        assert_eq!(WindowSpec::Last { hours: 1 }.label(), "Past hour");
        assert_eq!(
            local_label(t("2026-11-02T23:00:00Z"), &tz()),
            "Nov 2, 6:00 PM EST"
        );
    }
}
