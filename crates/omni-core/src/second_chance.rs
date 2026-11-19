//! Second chance (P11.5): a job waiting in review goes back to the queue on its
//! own, once per cause, when the reason it failed has just gone away.
//!
//! The causes: a new yt-dlp (or Deno) that was kept after its self-check, a
//! cookie jar saved or tested good, and a site that works again. Never a job a
//! person acted on, never one older than the window, never twice for one cause.
//! [`crate::repository::Repository::second_chance`] does the moving; this module
//! names the causes and is the one place the triggers call. A trigger never
//! fails its caller: an error is a warning in the log.

use chrono::{DateTime, Duration, Utc};
use tracing::{info, warn};

use crate::repository::Repository;

/// Between two causes in `queue.second_chances`; never part of a cause.
pub const SEPARATOR: char = '\n';

/// `retry.second_chance_window_mins` when the config sets none.
pub const DEFAULT_WINDOW_MINS: i64 = 120;

/// The codes a cookie jar can resolve: a login wall, and a rate limit that
/// outlasted its retries on a platform (the station's cookies lift it).
pub const COOKIE_CODES: &[&str] = &["LOGIN_REQUIRED", "RATE_LIMITED"];

/// The codes a new yt-dlp (or Deno) or a site that works again can resolve:
/// the extraction failures. Today the same as
/// [`crate::site_health::SITE_CODES`], but kept apart on purpose: a code added
/// there to count against a site must not start re-queuing jobs (a delivery
/// failure, say) without a decision here.
pub const SECOND_CHANCE_SITE_CODES: &[&str] = &["NO_STREAM_FOUND", "VIDEO_UNRESOLVED", "HTTP_403", "UNSUPPORTED_URL"];

/// Second chances one job may get in all, whatever the causes.
pub const MAX_PER_JOB: usize = 3;

/// The prefix of a site cause; a job gets at most one (a site that flaps
/// between degraded and healthy must not move the same job again and again).
pub const SITE_PREFIX: &str = "site:";

/// Whether a job whose `second_chances` are `stored` may get one for `cause`:
/// not given for this cause yet, no other `site:` cause when this is one, and
/// fewer than [`MAX_PER_JOB`] in all.
pub fn allows(stored: Option<&str>, cause: &str) -> bool {
    let used: Vec<&str> = causes(stored).collect();
    if used.contains(&cause) {
        return false;
    }
    if cause.starts_with(SITE_PREFIX) && used.iter().any(|c| c.starts_with(SITE_PREFIX)) {
        return false;
    }
    used.len() < MAX_PER_JOB
}

/// The window as the daemon reads `retry.second_chance_window_mins`: 30..=360
/// minutes, so a typo can neither switch it off nor reach yesterday's jobs.
pub fn window(mins: i64) -> Duration {
    Duration::minutes(mins.clamp(30, 360))
}

/// The window the daemon uses: [`window`] of `second_chance_window_mins`, but
/// never longer than `max_age_mins`, the owner's limit on automatic retries
/// (a `max_age_mins` below 30 is taken as it is).
pub fn window_for(retry: &crate::config::RetryConfig) -> Duration {
    window(retry.second_chance_window_mins).min(Duration::minutes(retry.max_age_mins))
}

/// The causes stored in `second_chances` (`None`: none).
pub fn causes(stored: Option<&str>) -> impl Iterator<Item = &str> {
    stored.unwrap_or("").split(SEPARATOR).map(str::trim).filter(|c| !c.is_empty())
}

/// Whether a job's link `url` is on `site` (a jar or site domain): a web link
/// whose registrable domain, with the jar aliases applied (`twitter.com` is
/// `x.com`, `youtu.be` is `youtube.com`), is `site`'s.
pub fn url_is_on(url: &str, site: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://"))
        && crate::cookies::jar_domain_for_url(url).as_deref() == Some(crate::cookies::canonical_jar_domain(site).as_str())
}

/// The cause of a new downloader build: `ytdlp:<version or hash>`.
pub fn ytdlp_cause(build: &str) -> String {
    format!("ytdlp:{}", one_line(build))
}

/// The cause of a new Deno: `deno:<version>`.
pub fn deno_cause(version: &str) -> String {
    format!("deno:{}", one_line(version))
}

/// The cause of a cookie jar saved or tested on `at`'s local day:
/// `cookies:<domain>:<yyyymmdd>`. A jar uploaded again on another day can
/// help a new job; one upload never moves the same job twice.
pub fn cookies_cause(domain: &str, at: DateTime<Utc>) -> String {
    format!("cookies:{}:{}", one_line(domain), at.with_timezone(&chrono::Local).format("%Y%m%d"))
}

/// The cause of a site that works again since `at`: `site:<domain>:<yyyymmdd HHMM>` (UTC).
pub fn site_cause(domain: &str, at: DateTime<Utc>) -> String {
    format!("{SITE_PREFIX}{}:{}", one_line(domain), at.format("%Y%m%d %H%M"))
}

fn one_line(s: &str) -> String {
    s.trim().replace(SEPARATOR, " ")
}

/// Give the second chance and log how many jobs moved (no link, no cookie).
/// Never fails: an error is a warning. Returns the number moved.
fn give(repo: &Repository, codes: &[&str], site: Option<&str>, cause: &str, note_el: &str) -> usize {
    match repo.second_chance(codes, site, repo.second_chance_window(), cause, note_el) {
        Ok(moved) => {
            info!("Second chance ({cause}): {} job(s) in review re-queued", moved.len());
            if !moved.is_empty() {
                let ids = moved.iter().map(|id| format!("#{id}")).collect::<Vec<_>>().join(", ");
                let _ = repo.log_audit("INFO", "QUEUE", &format!("Second chance ({cause}): re-queued {ids}"));
            }
            moved.len()
        }
        Err(e) => {
            warn!("Second chance ({cause}) not given: {e:#}");
            0
        }
    }
}

/// A new yt-dlp build was kept after its self-check.
pub fn after_ytdlp_update(repo: &Repository, build: &str) -> usize {
    give(repo, SECOND_CHANCE_SITE_CODES, None, &ytdlp_cause(build), "νέα έκδοση yt-dlp")
}

/// A new Deno (which yt-dlp needs for YouTube) was kept after its self-check.
pub fn after_deno_update(repo: &Repository, version: &str) -> usize {
    give(repo, SECOND_CHANCE_SITE_CODES, None, &deno_cause(version), "νέα έκδοση Deno")
}

/// A cookie jar for `domain` was saved, or its test passed.
pub fn after_cookies(repo: &Repository, domain: &str) -> usize {
    give(
        repo,
        COOKIE_CODES,
        Some(domain),
        &cookies_cause(domain, Utc::now()),
        &format!("νέα cookies για {domain}"),
    )
}

/// `domain` went from degraded or broken to healthy at `at`. Called by the
/// repository where the site's state is stored.
pub fn after_site_healthy(repo: &Repository, domain: &str, at: DateTime<Utc>) -> usize {
    give(
        repo,
        SECOND_CHANCE_SITE_CODES,
        Some(domain),
        &site_cause(domain, at),
        &format!("ο ιστότοπος {domain} λειτουργεί ξανά"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_is_read_as_30_to_360_minutes() {
        assert_eq!(window(10).num_minutes(), 30);
        assert_eq!(window(120).num_minutes(), 120);
        assert_eq!(window(999).num_minutes(), 360);
        assert_eq!(window(-5).num_minutes(), 30);
        assert_eq!(window(DEFAULT_WINDOW_MINS).num_minutes(), 120);
    }

    #[test]
    fn the_window_never_exceeds_the_retry_age_limit() {
        let retry = |window: i64, max_age: i64| crate::config::RetryConfig {
            second_chance_window_mins: window,
            max_age_mins: max_age,
            ..Default::default()
        };
        assert_eq!(window_for(&retry(360, 120)).num_minutes(), 120);
        assert_eq!(window_for(&retry(120, 30)).num_minutes(), 30);
        assert_eq!(window_for(&retry(120, 10)).num_minutes(), 10, "a max age below 30 is taken as it is");
        assert_eq!(window_for(&retry(90, 120)).num_minutes(), 90);
        assert_eq!(window_for(&retry(10, 120)).num_minutes(), 30);
        assert_eq!(window_for(&crate::config::RetryConfig::default()).num_minutes(), 120);
    }

    #[test]
    fn a_job_gets_one_chance_per_cause_one_site_cause_and_three_in_all() {
        assert!(allows(None, "ytdlp:a"));
        assert!(!allows(Some("ytdlp:a"), "ytdlp:a"));
        assert!(allows(Some("ytdlp:a"), "site:news.example:20261119 0800"));
        assert!(!allows(Some("site:news.example:20261119 0800"), "site:news.example:20261119 0900"), "a flapping site");
        assert!(!allows(Some("site:a.example:20261119 0800"), "site:b.example:20261119 0900"));
        assert!(allows(Some("ytdlp:a\ncookies:x.com:20261119"), "ytdlp:b"));
        assert!(!allows(Some("ytdlp:a\ncookies:x.com:20261119\nytdlp:b"), "ytdlp:c"), "a fourth chance");
    }

    /// The trigger lists hold only codes a fixed cause can resolve: never one
    /// of a delivery, a held output or an operator's decision.
    #[test]
    fn no_trigger_list_holds_a_delivery_or_decision_code() {
        let never = [
            "DELIVERY_UNCONFIRMED",
            "DELIVERY_FAILED",
            "DELIVER_TIMEOUT",
            "HELD_FILE_CHANGED",
            "PIPELINE_FAILED",
            "LEASE_EXPIRED",
            "APPROVAL_EXPIRED",
            "REJECTED_BY_MCR",
        ];
        for list in [SECOND_CHANCE_SITE_CODES, COOKIE_CODES] {
            for code in never {
                assert!(!list.contains(&code), "{code} in {list:?}");
            }
        }
    }

    #[test]
    fn causes_are_one_line_each() {
        assert_eq!(causes(None).count(), 0);
        assert_eq!(causes(Some("a\nb\n")).collect::<Vec<_>>(), vec!["a", "b"]);
        assert!(!ytdlp_cause("2026.11.01\nx").contains(SEPARATOR));
        let at = DateTime::parse_from_rfc3339("2026-11-19T08:05:00Z").unwrap().with_timezone(&Utc);
        assert_eq!(site_cause("news.example", at), "site:news.example:20261119 0805");
        assert!(cookies_cause("x.com", at).starts_with("cookies:x.com:2026111"));
    }

    #[test]
    fn a_link_is_on_its_jar_site_with_the_aliases() {
        assert!(url_is_on("https://twitter.com/a/status/1", "x.com"));
        assert!(url_is_on("https://www.x.com/a/status/1", "x.com"));
        assert!(url_is_on("https://youtu.be/abc", "youtube.com"));
        assert!(url_is_on("https://www.news.example.gr/a/b", "example.gr"));
        assert!(!url_is_on("https://www.instagram.com/p/1", "x.com"));
        assert!(!url_is_on("attachment://12/clip.mp4", "x.com"));
    }
}
