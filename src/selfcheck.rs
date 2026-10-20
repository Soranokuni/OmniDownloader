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

use omni_broadcast::downloader::{first_attempt_resolved, is_video_platform, Downloader, FirstAttempt, PageScan, Probe};
use omni_browser::sniffer::{Sniffed, StreamSniffer};
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
    /// Nothing can be said about the site: the page's only videos are the site's furniture (P3.16).
    /// Never a failure.
    pub inconclusive: bool,
}

/// The outside questions a link check asks, so a test can answer them.
pub(crate) trait LinkTools {
    /// The corrected link when the page is a confirmed 404 with a known repair.
    async fn repair(&self, url: &str) -> Option<String>;
    /// The video the site's own API names for the page.
    async fn site_api(&self, url: &str) -> Option<String>;
    /// yt-dlp's page scan (`--flat-playlist -J`).
    async fn page_scan(&self, url: &str) -> PageScan;
    /// Where the page's address ends up after redirects.
    async fn final_url(&self, url: &str) -> Option<String>;
    /// One yt-dlp probe: "extractor video id", or the reason.
    async fn probe(&self, url: &str) -> Result<String, String>;
    /// One look of the browser at the page.
    async fn sniff(&self, url: &str) -> Sniffed;
    /// What yt-dlp says about one of the browser's streams, asked with the browser's own referer,
    /// user agent and cookies and the site's cookie jar, as a job asks (P3.15).
    async fn probe_candidate(&self, url: &str, referer: &str, user_agent: &str, cookies: Option<&str>) -> Probe;
}

struct LiveTools {
    ytdl: std::path::PathBuf,
    /// The site's cookie jars, and where a run's copy of one is written; none for the self-check's own links.
    jars: Option<(Repository, std::path::PathBuf)>,
}

impl LiveTools {
    fn dl(&self) -> Downloader {
        Downloader::new(&self.ytdl)
    }
}

impl LinkTools for LiveTools {
    async fn repair(&self, url: &str) -> Option<String> {
        omni_browser::pages::repair_dead_link(url).await
    }
    async fn site_api(&self, url: &str) -> Option<String> {
        omni_browser::pages::resolve(url).await.ok().flatten()
    }
    async fn page_scan(&self, url: &str) -> PageScan {
        self.dl().page_videos(url, PROBE_TIMEOUT).await
    }
    async fn final_url(&self, url: &str) -> Option<String> {
        crate::repair::final_page_url(url).await
    }
    async fn probe(&self, url: &str) -> Result<String, String> {
        self.dl().probe(url, PROBE_TIMEOUT).await.map(|p| format!("{} video {}", p.extractor, p.id))
    }
    async fn sniff(&self, url: &str) -> Sniffed {
        StreamSniffer::extract_with_evidence(url, 25).await
    }
    async fn probe_candidate(&self, url: &str, referer: &str, user_agent: &str, cookies: Option<&str>) -> Probe {
        let jar = self.jars.as_ref().and_then(|(repo, scratch)| omni_broadcast::cookie_file::for_url(repo, url, scratch, true));
        let probed = self
            .dl()
            .with_cookie_jar(jar.as_ref())
            .probe_candidate(url, Some(referer), Some(user_agent), cookies, Duration::from_secs(15))
            .await;
        drop(jar);
        probed
    }
}

async fn probe_twice(tools: &impl LinkTools, url: &str, give_up_early: impl Fn(&str) -> bool) -> Result<String, String> {
    let mut last = String::new();
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(RETRY_PAUSE).await;
        }
        match tools.probe(url).await {
            Ok(found) => return Ok(found),
            Err(e) => last = e,
        }
        if give_up_early(&last) {
            break;
        }
    }
    Err(last)
}

/// Check one link the way a job would take it (see [`check_link_with`]), remembering
/// nothing about the site.
pub async fn check_link(ytdl: &Path, url: &str) -> LinkOutcome {
    check_link_with(&LiveTools { ytdl: ytdl.to_path_buf(), jars: None }, None, url).await
}

/// [`check_link`] for the nightly canary pass: what the browser sees on the page is
/// remembered for the site (P3.16), as a job's sniff does, and a video the site shows
/// on its other articles does not count as the page's video.
pub async fn check_link_recording(ytdl: &Path, repo: &Repository, scratch: &Path, url: &str) -> LinkOutcome {
    let tools = LiveTools { ytdl: ytdl.to_path_buf(), jars: Some((repo.clone(), scratch.to_path_buf())) };
    check_link_with(&tools, Some(repo), url).await
}

/// Check one link the way a job would take it: the same corrections first
/// (a glued "-ΒΙΝΤΕΟ", a confirmed 404 repair, a site's own API), then the
/// route a job takes for a news page (P3.15): yt-dlp's page scan says who
/// chooses, yt-dlp for a page it has an extractor for, the browser for a page
/// it reads only generically (yt-dlp's own pick is the fallback) or not at
/// all. The browser's finds are tried in turn as a job skips an embedded post
/// without a video. With `repo`, what the browser saw is recorded.
pub(crate) async fn check_link_with(tools: &impl LinkTools, repo: Option<&Repository>, url: &str) -> LinkOutcome {
    let mut url = omni_email::parser::without_annotation(url).to_string();
    let mut route = String::new();
    if url.starts_with("http") {
        if let Some(fixed) = tools.repair(&url).await {
            route = " (link repaired)".into();
            url = fixed;
        }
        if let Some(video) = tools.site_api(&url).await {
            return match probe_twice(tools, &video, |_| false).await {
                Ok(found) => LinkOutcome { inconclusive: false, ok: true, detail: format!("found {found} named by the site's API{route}") },
                Err(e) => LinkOutcome { inconclusive: false, ok: false, detail: format!("the site's API named {video}, which could not be read: {e}") },
            };
        }
    }
    let url = url.as_str();
    let platform = is_video_platform(url);
    let web = url.starts_with("http") && !platform;
    let scan = if web { tools.page_scan(url).await } else { PageScan::default() };
    let first = first_attempt_resolved(false, platform, false, &scan, url, |u| async move { tools.final_url(&u).await }).await;
    // yt-dlp's own pick for the page is a video the site shows on its other articles: not the page's.
    let pick_is_furniture = match repo {
        Some(repo) if web && !scan.media_urls.is_empty() => !crate::site_memory::recall(repo, url, None, &scan.media_urls).is_empty(),
        _ => false,
    };

    if first == FirstAttempt::YtDlp && !pick_is_furniture {
        // A news page yt-dlp has no extractor for is the browser's job, not a
        // failure worth a second try.
        match probe_twice(tools, url, |e| !platform && e.contains("Unsupported URL")).await {
            Ok(found) => return LinkOutcome { inconclusive: false, ok: true, detail: format!("found {found}{route}") },
            Err(reason) if platform => return LinkOutcome { inconclusive: false, ok: false, detail: reason },
            Err(_) => {}
        }
    }

    let mut last = String::new();
    // The browser found a stream at least once (so yt-dlp's own pick is not the fallback).
    let mut browser_found = false;
    // Some stream the browser found could not be read (neither gone nor live).
    let mut any_unknown = false;
    let mut furniture_only = false;
    for attempt in 0..2 {
        if attempt > 0 {
            tokio::time::sleep(RETRY_PAUSE).await;
        }
        let sniffed = tools.sniff(url).await;
        match sniffed.media {
            Ok(media) => {
                browser_found = true;
                let (pool, furniture) = match repo {
                    Some(repo) => {
                        let wide = crate::site_memory::remember(repo, url, Some(&sniffed.snapshot), &media.all_streams);
                        omni_core::site_videos::split_site_wide(&media.all_streams, &wide)
                    }
                    None => (media.all_streams.clone(), Vec::new()),
                };
                // The primary first, then the page's other finds: a job
                // skips an embedded post that has no video (P3.13).
                let mut candidates: Vec<String> = Vec::new();
                if !furniture.iter().any(|f| f.url == media.primary_stream) {
                    candidates.push(media.primary_stream.clone());
                }
                candidates.extend(pool.iter().filter(|s| **s != media.primary_stream).take(5).cloned());
                if candidates.is_empty() {
                    // The page's videos are the site's furniture: the same on the next look.
                    last = "the page's only videos are ones the site shows on its other articles".to_string();
                    furniture_only = true;
                    break;
                }
                // Each asked as a job asks (P3.15): with the browser's session and the site's jar, so a
                // stream locked to its page's referer is not read as broken.
                let mut unknown = 0usize;
                for c in &candidates {
                    match tools.probe_candidate(c, &media.referer, &media.user_agent, media.cookies.as_deref()).await {
                        Probe::Ok => {
                            return LinkOutcome { inconclusive: false, ok: true, detail: format!("found a video in the page's player{route}") };
                        }
                        Probe::Live => last = format!("the page's video ({c}) is a live stream"),
                        Probe::Dead(code) => last = format!("the page's video ({c}) is gone ({})", code.as_str()),
                        Probe::Unknown => {
                            unknown += 1;
                            last = format!("the page's video ({c}) could not be read");
                        }
                    }
                }
                any_unknown |= unknown > 0;
            }
            Err(e) => last = format!("no video found on the page ({e})"),
        }
    }
    // The browser found no video on a page yt-dlp reads only generically: yt-dlp's own pick, as a job takes it.
    // The browser found no video and yt-dlp's own pick is the site's furniture, by whichever route: the
    // job takes no video here and records no failure of the site, and neither does the check.
    if !browser_found && pick_is_furniture {
        return LinkOutcome {
            inconclusive: true,
            ok: false,
            detail: "the browser found no video and yt-dlp's pick is a video the site shows on its other articles".to_string(),
        };
    }
    // Also when some of the browser's streams could not be read and none was Ok: never worse than
    // asking yt-dlp about the page.
    if first == FirstAttempt::SnifferFirst && (!browser_found || any_unknown) && !pick_is_furniture {
        if let Ok(found) = probe_twice(tools, url, |e| e.contains("Unsupported URL")).await {
            return LinkOutcome { inconclusive: false, ok: true, detail: format!("found {found}{route}") };
        }
    }
    LinkOutcome { inconclusive: furniture_only, ok: false, detail: last }
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

#[cfg(test)]
mod route_tests {
    use super::*;
    use crate::site_memory::tests::repo as test_repo;
    use omni_browser::sniffer::DiscoveredMedia;
    use omni_browser::PageSnapshot;
    use std::sync::Mutex;

    const PAGE: &str = "https://www.news.example/a/story-1";
    const OTHER: &str = "https://www.news.example/a/story-2";
    const PROMO: &str = "https://media.news.example/promo-hor.mp4";
    const CLIP: &str = "https://media.news.example/clips/real-1.mp4";

    /// Every outside question answered from a script, remembering what was asked.
    struct Fake {
        scan: PageScan,
        /// What the browser sees: its primary and every stream.
        sees: Option<(&'static str, Vec<&'static str>)>,
        asked: Mutex<Vec<String>>,
        /// The referers a candidate was asked with.
        referers: Mutex<Vec<String>>,
        /// A stream that only plays with this referer (else a 403: not gone, not readable).
        locked_to: Option<&'static str>,
        /// No stream can be read, whatever it is asked with.
        unreadable: bool,
        /// Streams that are gone (a dead-candidate answer).
        dead: Vec<&'static str>,
        /// Streams that cannot be read, whatever it is asked with.
        unknown: Vec<&'static str>,
    }

    impl Fake {
        fn new(scan: PageScan, sees: Option<(&'static str, Vec<&'static str>)>) -> Self {
            Self { scan, sees, asked: Mutex::new(Vec::new()), referers: Mutex::new(Vec::new()), locked_to: None, unreadable: false, dead: Vec::new(), unknown: Vec::new() }
        }
        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl LinkTools for Fake {
        async fn repair(&self, _url: &str) -> Option<String> {
            None
        }
        async fn site_api(&self, _url: &str) -> Option<String> {
            None
        }
        async fn page_scan(&self, _url: &str) -> PageScan {
            self.asked.lock().unwrap().push("scan".into());
            self.scan.clone()
        }
        async fn final_url(&self, _url: &str) -> Option<String> {
            None
        }
        async fn probe(&self, url: &str) -> Result<String, String> {
            self.asked.lock().unwrap().push(format!("probe {url}"));
            Ok("generic video 1".into())
        }
        async fn probe_candidate(&self, url: &str, referer: &str, _user_agent: &str, _cookies: Option<&str>) -> Probe {
            self.asked.lock().unwrap().push(format!("probe {url}"));
            self.referers.lock().unwrap().push(referer.to_string());
            match self.locked_to {
                _ if self.dead.contains(&url) => Probe::Dead(omni_broadcast::errors::ErrorCode::PageNotFound),
                _ if self.unknown.contains(&url) => Probe::Unknown,
                _ if self.unreadable => Probe::Unknown,
                Some(needed) if needed != referer => Probe::Unknown,
                _ => Probe::Ok,
            }
        }
        async fn sniff(&self, url: &str) -> Sniffed {
            self.asked.lock().unwrap().push("sniff".into());
            let snapshot = PageSnapshot::new(url);
            let media = match &self.sees {
                Some((primary, all)) => Ok(DiscoveredMedia {
                    primary_stream: primary.to_string(),
                    all_streams: all.iter().map(|s| s.to_string()).collect(),
                    meta: Default::default(),
                    user_agent: "UA".into(),
                    referer: url.to_string(),
                    cookies: None,
                }),
                None => Err(anyhow::anyhow!("nothing")),
            };
            Sniffed { media, snapshot }
        }
    }

    #[tokio::test]
    async fn a_referer_locked_stream_is_probed_with_the_browsers_referer_and_passes() {
        let mut tools = Fake::new(generic(), Some((CLIP, vec![CLIP])));
        tools.locked_to = Some(PAGE);
        let out = check_link_with(&tools, None, PAGE).await;
        assert!(out.ok, "{}", out.detail);
        assert_eq!(*tools.referers.lock().unwrap(), [PAGE], "asked with the session the browser had");
    }

    #[tokio::test]
    async fn streams_that_cannot_be_read_at_all_fall_back_to_yt_dlp_asking_about_the_page() {
        // Neither gone nor live: the check is never worse than asking yt-dlp about the page.
        let mut tools = Fake::new(generic(), Some((CLIP, vec![CLIP])));
        tools.unreadable = true;
        let out = check_link_with(&tools, None, PAGE).await;
        assert!(out.ok, "{}", out.detail);
        let asked = tools.asked();
        assert_eq!(asked.last().unwrap(), &format!("probe {PAGE}"));
    }

    #[tokio::test]
    async fn a_browser_that_finds_nothing_and_a_furniture_pick_is_undecided() {
        let (_db, repo) = test_repo();
        crate::site_memory::remember(&repo, OTHER, None, &[PROMO.to_string()]);
        crate::site_memory::remember(&repo, "https://www.news.example/a/story-3", None, &[PROMO.to_string()]);
        let scan = PageScan { generic_only: true, media_urls: vec![PROMO.to_string()], ..Default::default() };
        let tools = Fake::new(scan.clone(), None);
        let out = check_link_with(&tools, Some(&repo), PAGE).await;
        assert!(!out.ok && out.inconclusive, "{}", out.detail);
        assert!(!tools.asked().contains(&format!("probe {PAGE}")), "yt-dlp's pick is not the page's video");
        // Control: without the memory the same page falls back to yt-dlp's pick and passes.
        let (_db2, fresh) = test_repo();
        let tools = Fake::new(scan, None);
        assert!(check_link_with(&tools, Some(&fresh), PAGE).await.ok);
    }

    #[tokio::test]
    async fn one_unreadable_stream_among_gone_ones_still_falls_back_to_yt_dlp_asking_about_the_page() {
        let mut tools = Fake::new(generic(), Some((CLIP, vec![CLIP, PROMO])));
        tools.dead = vec![CLIP];
        tools.unknown = vec![PROMO];
        let out = check_link_with(&tools, None, PAGE).await;
        assert!(out.ok, "{}", out.detail);
        assert_eq!(tools.asked().last().unwrap(), &format!("probe {PAGE}"));
        // Control: every stream gone is a real failure; the page is not asked.
        let mut gone = Fake::new(generic(), Some((CLIP, vec![CLIP])));
        gone.dead = vec![CLIP];
        let out = check_link_with(&gone, None, PAGE).await;
        assert!(!out.ok && !out.inconclusive, "{}", out.detail);
        assert!(!gone.asked().contains(&format!("probe {PAGE}")));
    }

    #[tokio::test]
    async fn a_refused_furniture_pick_and_a_browser_that_finds_nothing_is_undecided_on_the_yt_dlp_first_route_too() {
        let (_db, repo) = test_repo();
        crate::site_memory::remember(&repo, OTHER, None, &[PROMO.to_string()]);
        crate::site_memory::remember(&repo, "https://www.news.example/a/story-3", None, &[PROMO.to_string()]);
        // No generic_only flag: yt-dlp has an extractor for the page, so it goes first.
        let scan = PageScan { media_urls: vec![PROMO.to_string()], ..Default::default() };
        let tools = Fake::new(scan, None);
        let out = check_link_with(&tools, Some(&repo), PAGE).await;
        assert!(!out.ok && out.inconclusive, "{}", out.detail);
    }

    fn rows(db: &crate::site_memory::tests::TestDb) -> i64 {
        let conn = rusqlite::Connection::open(db.0.join("omni.db")).unwrap();
        conn.query_row("SELECT COUNT(*) FROM site_videos", [], |r| r.get(0)).unwrap()
    }

    fn generic() -> PageScan {
        PageScan { generic_only: true, ..Default::default() }
    }

    #[tokio::test]
    async fn a_generic_only_canary_page_is_sniffed_first_and_what_the_browser_saw_is_recorded() {
        let (db, repo) = test_repo();
        let tools = Fake::new(generic(), Some((CLIP, vec![CLIP, PROMO])));
        let out = check_link_with(&tools, Some(&repo), PAGE).await;
        assert!(out.ok, "{}", out.detail);
        assert!(out.detail.contains("page's player"), "{}", out.detail);
        // The browser chose: yt-dlp was not asked about the page itself.
        assert_eq!(tools.asked(), ["scan", "sniff", &format!("probe {CLIP}")]);
        assert_eq!(rows(&db), 2, "every stream the browser saw is remembered, before any filtering");
    }

    #[tokio::test]
    async fn a_page_yt_dlp_has_an_extractor_for_is_read_by_yt_dlp_and_nothing_is_recorded() {
        let (db, repo) = test_repo();
        let tools = Fake::new(PageScan::default(), Some((CLIP, vec![CLIP])));
        let out = check_link_with(&tools, Some(&repo), PAGE).await;
        assert!(out.ok);
        assert_eq!(tools.asked(), ["scan", &format!("probe {PAGE}")]);
        assert_eq!(rows(&db), 0, "only the browser's sightings are remembered");
    }

    #[tokio::test]
    async fn a_video_the_site_shows_on_its_other_articles_is_not_the_pages_video() {
        let (_db, repo) = test_repo();
        crate::site_memory::remember(&repo, OTHER, None, &[PROMO.to_string()]);
        // The promo is the browser's own first choice; the article's clip is next.
        let tools = Fake::new(generic(), Some((PROMO, vec![PROMO, CLIP])));
        let out = check_link_with(&tools, Some(&repo), PAGE).await;
        assert!(out.ok, "{}", out.detail);
        assert_eq!(tools.asked(), ["scan", "sniff", &format!("probe {CLIP}")], "the promo is never probed as the page's video");
    }

    #[tokio::test]
    async fn a_page_whose_only_video_is_site_furniture_does_not_pass() {
        let (_db, repo) = test_repo();
        crate::site_memory::remember(&repo, OTHER, None, &[PROMO.to_string()]);
        let tools = Fake::new(generic(), Some((PROMO, vec![PROMO])));
        let out = check_link_with(&tools, Some(&repo), PAGE).await;
        assert!(!out.ok);
        assert!(out.detail.contains("other articles"), "{}", out.detail);
        assert!(out.inconclusive, "undecided, never a failure");
        assert_eq!(tools.asked(), ["scan", "sniff"], "nothing is probed, and yt-dlp's pick is not the fallback");
    }

    #[tokio::test]
    async fn a_first_sighting_still_passes_and_without_a_repo_nothing_is_remembered() {
        let (db, repo) = test_repo();
        let tools = Fake::new(generic(), Some((PROMO, vec![PROMO, CLIP])));
        let out = check_link_with(&tools, Some(&repo), PAGE).await;
        assert!(out.ok);
        assert_eq!(tools.asked()[2], format!("probe {PROMO}"), "one article is not furniture: the browser's first choice stands");
        let before = rows(&db);
        let tools = Fake::new(generic(), Some((PROMO, vec![PROMO, CLIP])));
        assert!(check_link_with(&tools, None, OTHER).await.ok);
        assert_eq!(rows(&db), before, "the self-check's own links record nothing");
    }
}
