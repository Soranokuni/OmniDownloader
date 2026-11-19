//! A platform whose extractor just broke (plan P11.4d).
//!
//! The verified yt-dlp update runs at 03:00. When a platform changes in the
//! morning, every job on it fails until the next night, although yt-dlp
//! usually ships a fix within hours. [`ExtractionTrouble`] counts the
//! extraction failures per platform; [`early_run_due`] decides whether that
//! is reason enough to run the same update now. Pure: the caller gives the
//! clock and the last run of `ytdl_update`.

use chrono::{DateTime, Duration, Utc};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

/// Failures on one platform within [`WINDOW`] that ask for an early update.
pub const THRESHOLD: usize = 3;

/// How far back a failure still counts.
pub const WINDOW: Duration = Duration::minutes(30);

/// No early update while `ytdl_update` ran (nightly or early) less than this
/// long ago, and at most one early request per this much time.
pub const GAP: Duration = Duration::hours(6);

/// Whether a job's final `code` says the extractor may be broken. The codes
/// a second chance after an update re-queues; a platform limiting our
/// requests (`RATE_LIMITED`) or wanting a login (`LOGIN_REQUIRED`) is not
/// extractor breakage and a new yt-dlp would not help.
pub fn counts(code: &str) -> bool {
    crate::second_chance::SECOND_CHANCE_SITE_CODES.contains(&code)
}

/// The recent extraction failures of each platform. Shared by every worker.
#[derive(Debug, Default)]
pub struct ExtractionTrouble {
    failures: Mutex<HashMap<String, VecDeque<DateTime<Utc>>>>,
}

impl ExtractionTrouble {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one extraction failure on `platform` at `now`. True when it
    /// makes [`THRESHOLD`] or more on that platform within [`WINDOW`]; it
    /// stays true for each further failure while they are recent (the
    /// [`GAP`] rule is what keeps that from asking again). A success resets
    /// nothing: old failures simply expire.
    pub fn note_failure(&self, platform: &str, now: DateTime<Utc>) -> bool {
        let mut all = self.failures.lock().unwrap_or_else(|p| p.into_inner());
        // Other platforms' expired entries go too, so the map stays small.
        all.retain(|_, times| {
            times.retain(|t| now - *t < WINDOW);
            !times.is_empty()
        });
        let times = all.entry(platform.to_string()).or_default();
        times.push_back(now);
        times.len() >= THRESHOLD
    }
}

/// After an early run was postponed because jobs were still running (it
/// swapped nothing and is not recorded), the next early request may come
/// this much later instead of [`GAP`].
pub const BUSY_RETRY: Duration = Duration::minutes(30);

/// Whether to run the yt-dlp update early now: only when `ytdl_update` last
/// ran (nightly or early; `None`: never) more than [`GAP`] ago and `now` is
/// not before `not_before` (`None`: no early request yet), the time
/// [`not_before_after_request`] or [`not_before_after_busy`] set.
pub fn early_run_due(
    last_run: Option<DateTime<Utc>>,
    not_before: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> bool {
    last_run.map_or(true, |t| now - t > GAP) && not_before.map_or(true, |t| now >= t)
}

/// The next early request after one sent at `now`: [`GAP`] later.
pub fn not_before_after_request(now: DateTime<Utc>) -> DateTime<Utc> {
    now + GAP
}

/// The next early request after an early run at `now` found the pool busy:
/// [`BUSY_RETRY`] later.
pub fn not_before_after_busy(now: DateTime<Utc>) -> DateTime<Utc> {
    now + BUSY_RETRY
}

/// The audit line of an early run.
pub fn audit_line(platform: &str) -> String {
    format!("Πρόωρος έλεγχος ενημέρωσης yt-dlp: {THRESHOLD} αποτυχίες εξαγωγής στο {platform} σε 30 λεπτά")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn t0() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 19, 9, 0, 0).unwrap()
    }

    fn at(min: i64) -> DateTime<Utc> {
        t0() + Duration::minutes(min)
    }

    #[test]
    fn three_failures_in_thirty_minutes_on_one_platform_trip_on_the_third() {
        let t = ExtractionTrouble::new();
        assert!(!t.note_failure("youtube", at(0)));
        assert!(!t.note_failure("youtube", at(5)));
        assert!(t.note_failure("youtube", at(29)));
    }

    #[test]
    fn other_platforms_do_not_add_up() {
        let t = ExtractionTrouble::new();
        assert!(!t.note_failure("youtube", at(0)));
        assert!(!t.note_failure("youtube", at(1)));
        assert!(!t.note_failure("x", at(2)));
    }

    #[test]
    fn a_failure_older_than_thirty_minutes_has_expired() {
        let t = ExtractionTrouble::new();
        assert!(!t.note_failure("youtube", at(0)));
        assert!(!t.note_failure("youtube", at(10)));
        assert!(!t.note_failure("youtube", at(31)), "the one at 0 min still counted");
        // The one at 10 min is still inside the window: a fourth trips it.
        assert!(t.note_failure("youtube", at(32)));
    }

    #[test]
    fn the_tracker_stays_true_after_tripping_the_gap_rule_is_what_limits() {
        let t = ExtractionTrouble::new();
        for m in 0..2 {
            t.note_failure("youtube", at(m));
        }
        assert!(t.note_failure("youtube", at(2)));
        assert!(t.note_failure("youtube", at(2)), "pinned: a fourth recent failure is still true");
    }

    #[test]
    fn only_extraction_codes_count() {
        for c in ["NO_STREAM_FOUND", "VIDEO_UNRESOLVED", "HTTP_403", "UNSUPPORTED_URL"] {
            assert!(counts(c), "{c}");
        }
        for c in ["RATE_LIMITED", "LOGIN_REQUIRED", "NETWORK_ERROR", "TIMEOUT", "DRM_PROTECTED", ""] {
            assert!(!counts(c), "{c}");
        }
    }

    #[test]
    fn the_early_run_waits_six_hours_after_any_run() {
        let now = t0();
        let ago = |h: i64| Some(now - Duration::hours(h));
        let requested = |h: i64| Some(not_before_after_request(now - Duration::hours(h)));
        assert!(!early_run_due(ago(2), None, now), "nightly 2 h ago");
        assert!(early_run_due(ago(7), None, now), "nightly 7 h ago");
        assert!(!early_run_due(ago(7), requested(3), now), "an early run 3 h ago");
        assert!(early_run_due(ago(8), requested(7), now), "both older than 6 h");
        assert!(early_run_due(None, None, now), "never ran");
    }

    #[test]
    fn after_a_busy_pool_the_next_request_may_come_in_thirty_minutes() {
        let busy = t0();
        let nb = Some(not_before_after_busy(busy));
        assert!(!early_run_due(None, nb, busy + Duration::minutes(29)));
        assert!(early_run_due(None, nb, busy + Duration::minutes(30)));
        // The nightly run's 6 hours still hold.
        assert!(!early_run_due(Some(busy), nb, busy + Duration::minutes(31)));
    }

    #[test]
    fn the_audit_line_names_the_platform_and_nothing_else() {
        assert_eq!(
            audit_line("youtube"),
            "Πρόωρος έλεγχος ενημέρωσης yt-dlp: 3 αποτυχίες εξαγωγής στο youtube σε 30 λεπτά"
        );
    }
}
