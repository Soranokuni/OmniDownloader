//! Storing and reading real timestamps (plan P0.3, defect D-11).
//!
//! The repository used to substitute `Utc::now()` for every stored timestamp on
//! read, so the MCR archive showed "Completed At: now" for every job ever
//! delivered — useless for the one question operators actually ask it ("when
//! did this go to Dalet?").
//!
//! Timestamps are written as RFC3339 UTC with milliseconds
//! (`strftime('%Y-%m-%dT%H:%M:%fZ','now')`), which sorts and compares correctly
//! as text — important because session expiry is a string comparison
//! (`expires_at > ?`). Rows written by the old code use SQLite's
//! `CURRENT_TIMESTAMP` format (`YYYY-MM-DD HH:MM:SS`, UTC, no zone marker), so
//! [`parse`] accepts both.

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};

/// SQL expression for "now", for use inside statements.
pub const SQL_NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ','now')";

/// Format a timestamp the way the schema stores it.
pub fn format(dt: DateTime<Utc>) -> String {
    dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

/// Current time in storage format. Use for values bound as parameters.
pub fn now_string() -> String {
    format(Utc::now())
}

/// Parse a stored timestamp, tolerating the legacy `CURRENT_TIMESTAMP` shape.
///
/// Returns `None` rather than `Utc::now()` for an unparseable value: a caller
/// that shows "unknown" is honest, whereas a caller that shows the current time
/// is actively misleading (that was defect D-11).
pub fn parse(raw: &str) -> Option<DateTime<Utc>> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    // Legacy: "2026-09-19 17:27:05" — SQLite's CURRENT_TIMESTAMP, always UTC.
    for fmt in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(s, fmt) {
            return Some(Utc.from_utc_datetime(&naive));
        }
    }
    None
}

/// Parse an optional column, mapping both SQL NULL and an unparseable value to
/// `None`.
pub fn parse_opt(raw: Option<String>) -> Option<DateTime<Utc>> {
    raw.as_deref().and_then(parse)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_storage_format() {
        let now = Utc::now();
        let text = format(now);
        let back = parse(&text).expect("formatted value must parse");
        // Storage keeps milliseconds, so allow sub-millisecond drift.
        assert!((back - now).num_milliseconds().abs() <= 1, "{back} vs {now}");
    }

    #[test]
    fn accepts_legacy_current_timestamp_rows() {
        let dt = parse("2026-09-19 17:27:05").expect("legacy format must parse");
        assert_eq!(dt.to_rfc3339(), "2026-09-19T17:27:05+00:00");
        // ...and the fractional variant SQLite emits for some builds.
        assert!(parse("2026-09-19 17:27:05.123").is_some());
    }

    #[test]
    fn unparseable_values_are_none_not_now() {
        // The whole point of D-11: never invent a timestamp.
        assert!(parse("").is_none());
        assert!(parse("not a date").is_none());
        assert!(parse_opt(None).is_none());
        assert!(parse_opt(Some("garbage".into())).is_none());
    }

    #[test]
    fn storage_format_sorts_chronologically_as_text() {
        // Session expiry compares as a string in SQL; lexical order must match
        // chronological order or sessions expire at the wrong time.
        let early = format(Utc.with_ymd_and_hms(2026, 9, 19, 9, 5, 0).unwrap());
        let late = format(Utc.with_ymd_and_hms(2026, 9, 19, 17, 30, 0).unwrap());
        let next_year = format(Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap());
        assert!(early < late);
        assert!(late < next_year);
    }
}
