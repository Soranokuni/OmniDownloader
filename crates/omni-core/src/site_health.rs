//! Per-site health bookkeeping (plan P10.3).
//!
//! A single page that fails is "this page has a problem". A news site that
//! used to give us video and now fails three times in a row is "this site
//! changed". This module is the pure state machine for telling them apart;
//! the repository stores the rows and `run_job` feeds in outcomes. It never
//! changes a job's outcome.

use chrono::{DateTime, Duration, Utc};

use crate::health::{checks, Check, HealthState};
use crate::repository::Repository;

/// How many outcomes `recent` remembers.
pub const RECENT_LEN: usize = 20;
/// Consecutive site failures before a known-good site is called degraded.
pub const DEGRADE_AFTER: i64 = 3;
/// Successes in `recent` (before the failure) that make a site "known good".
pub const KNOWN_GOOD_SUCCESSES: usize = 5;
/// A site whose last success is older than this is not "known good".
pub const KNOWN_GOOD_DAYS: i64 = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SiteState {
    Healthy,
    Degraded,
    Broken,
}

impl SiteState {
    pub fn as_str(&self) -> &'static str {
        match self {
            SiteState::Healthy => "healthy",
            SiteState::Degraded => "degraded",
            SiteState::Broken => "broken",
        }
    }

    /// Unknown strings read as healthy: a stray value must not raise an alarm.
    pub fn parse(s: &str) -> SiteState {
        match s {
            "degraded" => SiteState::Degraded,
            "broken" => SiteState::Broken,
            _ => SiteState::Healthy,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SiteOutcome {
    Success,
    /// A failure that says something about the site; carries the error code.
    SiteFailure(String),
}

/// Mirrors the `site_stats` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SiteRow {
    pub domain: String,
    /// Last [`RECENT_LEN`] outcomes, '1' success / '0' failure, newest last.
    pub recent: String,
    pub consecutive_fail: i64,
    pub last_ok_at: Option<DateTime<Utc>>,
    pub last_fail_at: Option<DateTime<Utc>>,
    pub last_code: Option<String>,
    pub state: SiteState,
    pub state_since: Option<DateTime<Utc>>,
    pub llm_breaker_until: Option<DateTime<Utc>>,
}

impl SiteRow {
    pub fn new(domain: &str) -> Self {
        Self {
            domain: domain.to_string(),
            recent: String::new(),
            consecutive_fail: 0,
            last_ok_at: None,
            last_fail_at: None,
            last_code: None,
            state: SiteState::Healthy,
            state_since: None,
            llm_breaker_until: None,
        }
    }
}

/// Error codes that say something about the site rather than the one page or
/// the machine. Everything else (no video on the page, 404, DRM, login,
/// transcode, disk, ...) never counts.
pub fn counts_against_site(code: &str) -> bool {
    matches!(
        code,
        "NO_STREAM_FOUND" | "VIDEO_UNRESOLVED" | "HTTP_403" | "UNSUPPORTED_URL"
    )
}

/// Apply one outcome. `Broken` is only ever set from outside (`set_site_state`).
pub fn apply(mut row: SiteRow, outcome: SiteOutcome, now: DateTime<Utc>) -> SiteRow {
    let before = row.state;
    let ones_before = row.recent.chars().filter(|c| *c == '1').count();
    let ok_is_recent = row
        .last_ok_at
        .map(|t| now - t <= Duration::days(KNOWN_GOOD_DAYS))
        .unwrap_or(false);

    let mark = match &outcome {
        SiteOutcome::Success => '1',
        SiteOutcome::SiteFailure(_) => '0',
    };
    row.recent.push(mark);
    let len = row.recent.chars().count();
    if len > RECENT_LEN {
        row.recent = row.recent.chars().skip(len - RECENT_LEN).collect();
    }

    match outcome {
        SiteOutcome::Success => {
            row.consecutive_fail = 0;
            row.last_ok_at = Some(now);
            row.state = SiteState::Healthy;
        }
        SiteOutcome::SiteFailure(code) => {
            row.consecutive_fail += 1;
            row.last_fail_at = Some(now);
            row.last_code = Some(code);
            if row.state == SiteState::Healthy
                && row.consecutive_fail >= DEGRADE_AFTER
                && ones_before >= KNOWN_GOOD_SUCCESSES
                && ok_is_recent
            {
                row.state = SiteState::Degraded;
            }
        }
    }
    if row.state != before {
        row.state_since = Some(now);
    }
    row
}

/// The domain whose health a job's link counts toward, if any: web pages on a
/// news site only. Video platforms and attachments have no site health.
pub fn site_for_job(url: &str, is_video_platform: bool) -> Option<String> {
    if is_video_platform || !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    crate::urlnorm::registrable_domain(url)
}

/// One entry for the health check: `domain (degraded since HH:MM, N failures)`.
pub fn describe(row: &SiteRow) -> String {
    let since = row
        .state_since
        // Local time: MCR reads this on the desk banner.
        .map(|t| format!(" since {}", t.with_timezone(&chrono::Local).format("%H:%M")))
        .unwrap_or_default();
    format!(
        "{} ({}{}, {} failures)",
        row.domain,
        row.state.as_str(),
        since,
        row.consecutive_fail
    )
}

/// The detail text of the `sites` health check: up to five sites.
pub fn health_detail(rows: &[SiteRow]) -> String {
    let mut parts: Vec<String> = rows.iter().take(5).map(describe).collect();
    if rows.len() > 5 {
        parts.push(format!("+{} more", rows.len() - 5));
    }
    parts.join("; ")
}

/// The check for [`crate::health::checks::SITES`] from the rows.
pub fn health_check(rows: &[SiteRow]) -> Check {
    if rows.is_empty() {
        Check::ok("all sites healthy")
    } else {
        Check::degraded(health_detail(rows))
    }
}

/// Refresh the `sites` check from the database. A query error leaves the
/// previous value alone.
pub fn refresh_health(repo: &Repository, health: &HealthState) {
    match repo.list_sites_not_healthy() {
        Ok(rows) => health.set_if_changed(checks::SITES, health_check(&rows)),
        Err(e) => tracing::warn!("Site health check not refreshed: {e:#}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::Health;

    fn t(days_ago: i64) -> DateTime<Utc> {
        Utc::now() - Duration::days(days_ago)
    }
    fn fail() -> SiteOutcome {
        SiteOutcome::SiteFailure("NO_STREAM_FOUND".into())
    }
    fn good_site(successes: usize) -> SiteRow {
        let mut r = SiteRow::new("example.gr");
        for _ in 0..successes {
            r = apply(r, SiteOutcome::Success, t(1));
        }
        r
    }

    #[test]
    fn three_failures_on_a_known_good_site_degrade_it() {
        let mut r = good_site(6);
        r = apply(r, fail(), Utc::now());
        r = apply(r, fail(), Utc::now());
        assert_eq!(r.state, SiteState::Healthy);
        r = apply(r, fail(), Utc::now());
        assert_eq!(r.state, SiteState::Degraded);
        assert_eq!(r.consecutive_fail, 3);
        assert!(r.state_since.is_some());
        assert_eq!(r.last_code.as_deref(), Some("NO_STREAM_FOUND"));
    }

    #[test]
    fn a_site_with_two_successes_is_not_known_good() {
        let mut r = good_site(2);
        for _ in 0..6 {
            r = apply(r, fail(), Utc::now());
        }
        assert_eq!(r.state, SiteState::Healthy);
        assert_eq!(r.consecutive_fail, 6);
    }

    #[test]
    fn a_stale_last_success_is_not_known_good() {
        let mut r = SiteRow::new("example.gr");
        r.recent = "1".repeat(10);
        r.last_ok_at = Some(t(91));
        for _ in 0..4 {
            r = apply(r, fail(), Utc::now());
        }
        assert_eq!(r.state, SiteState::Healthy);
    }

    #[test]
    fn one_success_heals_a_degraded_site() {
        let mut r = good_site(6);
        for _ in 0..3 {
            r = apply(r, fail(), Utc::now());
        }
        assert_eq!(r.state, SiteState::Degraded);
        r = apply(r, SiteOutcome::Success, Utc::now());
        assert_eq!(r.state, SiteState::Healthy);
        assert_eq!(r.consecutive_fail, 0);
    }

    #[test]
    fn broken_stays_broken_on_failure_and_heals_on_success() {
        let mut r = good_site(6);
        r.state = SiteState::Broken;
        r = apply(r, fail(), Utc::now());
        assert_eq!(r.state, SiteState::Broken);
        r = apply(r, SiteOutcome::Success, Utc::now());
        assert_eq!(r.state, SiteState::Healthy);
    }

    #[test]
    fn recent_is_capped_at_twenty_newest_last() {
        let mut r = good_site(25);
        assert_eq!(r.recent.len(), RECENT_LEN);
        r = apply(r, fail(), Utc::now());
        assert_eq!(r.recent.len(), RECENT_LEN);
        assert!(r.recent.ends_with("10"));
    }

    #[test]
    fn only_site_codes_count_against_a_site() {
        for c in ["NO_STREAM_FOUND", "VIDEO_UNRESOLVED", "HTTP_403", "UNSUPPORTED_URL"] {
            assert!(counts_against_site(c), "{c}");
        }
        for c in [
            "NO_VIDEO_ON_PAGE",
            "PAGE_NOT_FOUND",
            "DRM_PROTECTED",
            "LOGIN_REQUIRED",
            "TRANSCODE_FAILED",
            "DISK_FULL",
            "TIMEOUT",
            "NETWORK_ERROR",
            "",
        ] {
            assert!(!counts_against_site(c), "{c}");
        }
    }

    #[test]
    fn site_for_job_decides_which_domain_counts() {
        assert_eq!(site_for_job("https://www.youtube.com/watch?v=x", true), None);
        assert_eq!(site_for_job("attachment://12/clip.mp4", false), None);
        assert_eq!(
            site_for_job("https://www.news.example.gr/a/b", false).as_deref(),
            Some("example.gr")
        );
    }

    #[test]
    fn the_sites_check_is_ok_when_clean_and_degraded_otherwise() {
        assert_eq!(health_check(&[]).state, Health::Ok);
        let mut r = SiteRow::new("example.gr");
        r.state = SiteState::Degraded;
        assert_eq!(health_check(&[r]).state, Health::Degraded);
    }

    #[test]
    fn health_detail_lists_five_and_counts_the_rest() {
        let rows: Vec<SiteRow> = (0..7)
            .map(|i| {
                let mut r = SiteRow::new(&format!("site{i}.gr"));
                r.state = SiteState::Degraded;
                r.consecutive_fail = 3;
                r.state_since = Some(Utc::now());
                r
            })
            .collect();
        let d = health_detail(&rows);
        assert!(d.contains("site0.gr (degraded since "));
        assert!(d.contains(", 3 failures)"));
        assert!(!d.contains("site5.gr"));
        assert!(d.ends_with("+2 more"));
    }
}
