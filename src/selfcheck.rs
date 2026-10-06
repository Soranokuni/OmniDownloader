//! Nightly self-check (plan P6.7).
//!
//! What breaks a downloader is the outside world: a platform changes its
//! pages, a portal swaps its video player, a browser update leaves the
//! browser library behind. None of it shows until a journalist's job fails.
//! This checks a list of links known to hold a video, one per route a job
//! can take, the way a job would (yt-dlp, then the browser for a news page),
//! without downloading anything. It says which route stopped working and
//! since when, and it is what decides whether a new yt-dlp is kept.

use std::path::Path;
use std::time::Duration;

use chrono::{DateTime, Local, Utc};
use tracing::{info, warn};

use omni_broadcast::downloader::{is_video_platform, Downloader};
use omni_browser::sniffer::StreamSniffer;
use omni_core::health::{checks, Check, HealthState};
use omni_core::models::SelfcheckLink;
use omni_core::repository::Repository;

const PROBE_TIMEOUT: Duration = Duration::from_secs(90);
/// Between the two tries of a link: one dropped connection is not a broken
/// site (Instagram refused one connection while these defaults were chosen).
const RETRY_PAUSE: Duration = Duration::from_secs(10);

/// One link's result, in words an operator can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkOutcome {
    pub ok: bool,
    pub detail: String,
}

async fn probe_twice(dl: &Downloader, url: &str, give_up_early: impl Fn(&str) -> bool) -> Result<String, String> {
    let mut last = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(RETRY_PAUSE).await;
        }
        match dl.probe(url, PROBE_TIMEOUT).await {
            Ok(p) => return Ok(format!("{} video {}", p.extractor, p.id)),
            Err(e) => last = e,
        }
        if give_up_early(&last) {
            break;
        }
    }
    Err(last)
}

/// Check one link the way a job would take it.
pub async fn check_link(ytdl: &Path, url: &str) -> LinkOutcome {
    let dl = Downloader::new(ytdl);
    let platform = is_video_platform(url);
    // A news page yt-dlp has no extractor for is the browser's job, not a
    // failure worth a second try.
    let direct = probe_twice(&dl, url, |e| !platform && e.contains("Unsupported URL")).await;
    let reason = match direct {
        Ok(found) => return LinkOutcome { ok: true, detail: format!("found {found}") },
        Err(e) => e,
    };
    if platform {
        return LinkOutcome { ok: false, detail: reason };
    }

    let mut last = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(RETRY_PAUSE).await;
        }
        match StreamSniffer::extract_media_bundle(url, 25).await {
            Ok(media) => match probe_twice(&dl, &media.primary_stream, |_| false).await {
                Ok(found) => {
                    return LinkOutcome { ok: true, detail: format!("found {found} in the page's player") };
                }
                Err(e) => last = format!("the page's video ({}) could not be read: {e}", media.primary_stream),
            },
            Err(e) => last = format!("no video found on the page ({e})"),
        }
    }
    LinkOutcome { ok: false, detail: last }
}

fn when(t: Option<DateTime<Utc>>) -> String {
    t.map(|t| t.with_timezone(&Local).format("%d/%m %H:%M").to_string())
        .unwrap_or_else(|| "now".into())
}

/// The health line for the whole list: OK, or which links fail and since
/// when. A link never checked yet does not count either way.
pub fn summary(links: &[SelfcheckLink]) -> Check {
    let checked: Vec<&SelfcheckLink> = links.iter().filter(|l| l.last_ok.is_some()).collect();
    if checked.is_empty() {
        return Check::ok("no links checked yet");
    }
    let failing: Vec<String> = checked
        .iter()
        .filter(|l| l.last_ok == Some(false))
        .map(|l| format!("{} (since {})", l.label, when(l.failing_since)))
        .collect();
    if failing.is_empty() {
        Check::ok(format!("all {} links work", checked.len()))
    } else {
        Check::degraded(format!("{} of {} not working: {}", failing.len(), checked.len(), failing.join(", ")))
    }
}

/// Check `links` with the yt-dlp at `ytdl` and store each result. Returns
/// the outcomes in order. A link that changes state is written to the audit
/// log; one that stays broken is not repeated there every night.
pub async fn check_and_record(repo: &Repository, ytdl: &Path, links: &[SelfcheckLink]) -> Vec<(SelfcheckLink, LinkOutcome)> {
    let mut out = Vec::new();
    for link in links {
        let outcome = check_link(ytdl, &link.url).await;
        match repo.record_selfcheck_result(link.id, outcome.ok, &outcome.detail) {
            Ok(previous) => {
                if previous != Some(false) && !outcome.ok {
                    warn!("Self-check: {} stopped working: {}", link.label, outcome.detail);
                    let _ = repo.log_audit(
                        "WARN",
                        "SYSTEM",
                        &format!("Self-check: {} stopped working ({}): {}", link.label, link.url, outcome.detail),
                    );
                } else if previous == Some(false) && outcome.ok {
                    info!("Self-check: {} works again", link.label);
                    let _ = repo.log_audit("INFO", "SYSTEM", &format!("Self-check: {} works again", link.label));
                }
            }
            Err(e) => warn!("Self-check: could not store the result for {}: {e:#}", link.label),
        }
        out.push((link.clone(), outcome));
    }
    out
}

/// Refresh the `selfcheck` health line from what is stored.
pub fn refresh_health(repo: &Repository, health: &HealthState) {
    if let Ok(links) = repo.list_selfcheck_links() {
        health.set(checks::SELFCHECK, summary(&links));
    }
}

/// Launch the browser on its test page and record the outcome.
pub async fn check_browser(health: &HealthState) -> bool {
    match StreamSniffer::self_test().await {
        Ok(version) => {
            info!("Browser self-test passed ({version})");
            health.set(checks::BROWSER, Check::ok(format!("{version} works")));
            true
        }
        Err(e) => {
            warn!("Browser self-test failed: {e:#}");
            health.set(
                checks::BROWSER,
                Check::degraded(format!(
                    "the browser failed its test ({e:#}); video in news articles cannot be found until this is fixed"
                )),
            );
            false
        }
    }
}

/// The whole nightly run: browser, then every link.
pub async fn run(repo: &Repository, ytdl: &Path, health: &HealthState) -> anyhow::Result<usize> {
    check_browser(health).await;
    let links = repo.list_selfcheck_links()?;
    let results = check_and_record(repo, ytdl, &links).await;
    refresh_health(repo, health);
    let failing = results.iter().filter(|(_, o)| !o.ok).count();
    info!("Self-check: {} of {} links work", results.len() - failing, results.len());
    Ok(failing)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(label: &str, ok: Option<bool>) -> SelfcheckLink {
        SelfcheckLink {
            id: 1,
            label: label.into(),
            url: "https://example.org/v".into(),
            last_checked_at: ok.map(|_| Utc::now()),
            last_ok: ok,
            last_detail: None,
            last_ok_at: None,
            failing_since: (ok == Some(false)).then(Utc::now),
        }
    }

    #[test]
    fn the_health_line_names_what_is_failing() {
        let ok = summary(&[link("YouTube", Some(true)), link("TikTok", Some(true))]);
        assert_eq!(ok.state, omni_core::health::Health::Ok);
        assert_eq!(ok.detail.as_deref(), Some("all 2 links work"));

        let bad = summary(&[link("YouTube", Some(true)), link("Instagram reel", Some(false)), link("New", None)]);
        assert_eq!(bad.state, omni_core::health::Health::Degraded);
        let d = bad.detail.unwrap();
        assert!(d.starts_with("1 of 2 not working: Instagram reel (since "), "{d}");

        assert_eq!(summary(&[link("New", None)]).state, omni_core::health::Health::Ok);
    }
}
