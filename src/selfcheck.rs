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
use omni_broadcast::pacing::HostPacer;
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
    /// The daemon's pacer (P11.4): a check's yt-dlp waits for its turn like a job's.
    pacer: HostPacer,
    /// The site's cookie jars, and where a run's copy of one is written; none for the self-check's own links.
    jars: Option<(Repository, std::path::PathBuf)>,
}

impl LiveTools {
    fn dl(&self) -> Downloader {
        Downloader::new(&self.ytdl, &self.pacer)
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

/// Whether a probe's failure says the platform is refusing us for now, or
/// cooling down after it did (P11.4): nothing is learnt about the link, so the
/// check is inconclusive, never a failure, and is not asked again.
fn refused(reason: &str) -> bool {
    omni_broadcast::errors::classify_download_error(reason) == omni_broadcast::errors::ErrorCode::RateLimited
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
        // A second request to a platform that is refusing us only prolongs it.
        if give_up_early(&last) || refused(&last) {
            break;
        }
    }
    Err(last)
}

/// [`check_link_with`] for the nightly canary pass: what the browser sees on the page is
/// remembered for the site (P3.16), as a job's sniff does, and a video the site shows
/// on its other articles does not count as the page's video.
pub async fn check_link_recording(ytdl: &Path, pacer: &HostPacer, repo: &Repository, scratch: &Path, url: &str) -> LinkOutcome {
    let tools = LiveTools { ytdl: ytdl.to_path_buf(), pacer: pacer.clone(), jars: Some((repo.clone(), scratch.to_path_buf())) };
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
                Err(e) => LinkOutcome { inconclusive: refused(&e), ok: false, detail: format!("the site's API named {video}, which could not be read: {e}") },
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

    // yt-dlp was refused on this page as a rate limit: if the browser does not
    // find the video either, nothing was learnt (P11.4).
    let mut limited: Option<String> = None;
    if first == FirstAttempt::YtDlp && !pick_is_furniture {
        // A news page yt-dlp has no extractor for is the browser's job, not a
        // failure worth a second try.
        match probe_twice(tools, url, |e| !platform && e.contains("Unsupported URL")).await {
            Ok(found) => return LinkOutcome { inconclusive: false, ok: true, detail: format!("found {found}{route}") },
            Err(reason) if platform => return LinkOutcome { inconclusive: refused(&reason), ok: false, detail: reason },
            Err(reason) => limited = refused(&reason).then_some(reason),
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
        match probe_twice(tools, url, |e| e.contains("Unsupported URL")).await {
            Ok(found) => return LinkOutcome { inconclusive: false, ok: true, detail: format!("found {found}{route}") },
            Err(reason) if refused(&reason) => return LinkOutcome { inconclusive: true, ok: false, detail: reason },
            Err(_) => {}
        }
    }
    match limited {
        Some(reason) if !furniture_only => LinkOutcome { inconclusive: true, ok: false, detail: reason },
        _ => LinkOutcome { inconclusive: furniture_only, ok: false, detail: last },
    }
}

fn when(t: Option<DateTime<Utc>>) -> String {
    t.map(|t| t.with_timezone(&Local).format("%d/%m %H:%M").to_string())
        .unwrap_or_else(|| "now".into())
}

/// A link checked before and not since this long is stale (P11.4: a platform
/// refusing the checks every night leaves its last result standing).
pub const STALE_AFTER: chrono::Duration = chrono::Duration::days(2);

/// The health line for the whole list at `now`: OK, or which links fail and
/// since when. A link never checked yet does not count either way. Also amber:
/// links not checked for [`STALE_AFTER`] (their result is old news), and a
/// yt-dlp update postponed `postponed_nights` nights in a row, from
/// [`POSTPONED_NIGHTS_AMBER`] on.
pub fn summary(links: &[SelfcheckLink], now: DateTime<Utc>, postponed_nights: u32) -> Check {
    let checked: Vec<&SelfcheckLink> = links.iter().filter(|l| l.last_ok.is_some()).collect();
    let mut amber: Vec<String> = Vec::new();
    let failing: Vec<String> = checked
        .iter()
        .filter(|l| l.last_ok == Some(false))
        .map(|l| format!("{} (since {})", l.label, when(l.failing_since)))
        .collect();
    if !failing.is_empty() {
        amber.push(format!("{} of {} not working: {}", failing.len(), checked.len(), failing.join(", ")));
    }
    let stale: Vec<Option<DateTime<Utc>>> =
        checked.iter().map(|l| l.last_checked_at).filter(|at| at.map_or(true, |t| now - t > STALE_AFTER)).collect();
    if !stale.is_empty() {
        let since = stale.iter().flatten().min().copied();
        // Neutral: a platform refusing the checks, the self-check switched
        // off and the daemon down all look the same from here.
        amber.push(format!(
            "{} link(s) not checked since {}",
            stale.len(),
            since.map(|t| t.with_timezone(&Local).format("%d/%m").to_string()).unwrap_or_else(|| "an unknown date".into())
        ));
    }
    if postponed_nights >= POSTPONED_NIGHTS_AMBER {
        amber.push(format!("yt-dlp update waiting {postponed_nights} nights: the check links are being rate-limited"));
    }
    if !amber.is_empty() {
        Check::degraded(amber.join("; "))
    } else if checked.is_empty() {
        Check::ok("no links checked yet")
    } else {
        Check::ok(format!("all {} links work", checked.len()))
    }
}

/// Check `links` with the yt-dlp at `ytdl` and store each result. Returns
/// the outcomes in order. A link that changes state is written to the audit
/// log; one that stays broken is not repeated there every night.
pub async fn check_and_record(repo: &Repository, ytdl: &Path, pacer: &HostPacer, links: &[SelfcheckLink]) -> Vec<(SelfcheckLink, LinkOutcome)> {
    let tools = LiveTools { ytdl: ytdl.to_path_buf(), pacer: pacer.clone(), jars: None };
    check_and_record_with(repo, &tools, links).await
}

/// [`check_and_record`] with `tools` answering the outside questions.
///
/// One call is one round. An inconclusive link (P11.4: the platform refused us
/// as a rate limit, or cools down after it did) is "not checked this time":
/// nothing is stored for it, so neither the health line nor the list of links
/// that last worked changes. Once a link on a platform was refused, the round's
/// other links on that platform are inconclusive without being asked.
pub(crate) async fn check_and_record_with(
    repo: &Repository,
    tools: &impl LinkTools,
    links: &[SelfcheckLink],
) -> Vec<(SelfcheckLink, LinkOutcome)> {
    let mut out = Vec::new();
    let mut limiting: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
    for link in links {
        let key = omni_broadcast::pacing::platform_key(&link.url);
        let outcome = match key.filter(|k| limiting.contains(k)) {
            Some(k) => LinkOutcome {
                ok: false,
                inconclusive: true,
                detail: format!("not checked: {k} is limiting requests right now"),
            },
            None => check_link_with(tools, None, &link.url).await,
        };
        if outcome.inconclusive {
            if let Some(k) = key.filter(|_| refused(&outcome.detail)) {
                limiting.insert(k);
            }
            info!("Self-check: {} not checked this time: {}", link.label, outcome.detail);
            out.push((link.clone(), outcome));
            continue;
        }
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
pub fn refresh_health(repo: &Repository, health: &HealthState, bin_dir: &Path) {
    if let Ok(links) = repo.list_selfcheck_links() {
        health.set(checks::SELFCHECK, summary(&links, Utc::now(), postponed_nights(bin_dir)));
    }
}

/// Launch the browser on its test page and record the outcome.
pub async fn check_browser(health: &HealthState) -> bool {
    let result = StreamSniffer::self_test().await;
    match &result {
        Ok(version) => info!("Browser self-test passed ({version})"),
        Err(e) => warn!("Browser self-test failed: {e:#}"),
    }
    let ok = result.is_ok();
    health.set(checks::BROWSER, browser_check(result));
    ok
}

/// The `browser` line for a self-test result. A browser that does not start with its sandbox
/// (P2.13) is red: every article job needs it, and the fix is the account or the install, never
/// turning the sandbox off. One that starts but fails its page is amber, as before.
fn browser_check(result: anyhow::Result<String>) -> Check {
    match result {
        Ok(version) => Check::ok(format!("{version} works")),
        Err(e) if e.chain().any(|c| c.to_string() == omni_browser::sniffer::SANDBOXED_LAUNCH_FAILED) => Check::down(format!(
            "{e:#}; video in news articles cannot be found until the browser starts with its sandbox"
        )),
        Err(e) => Check::degraded(format!(
            "the browser failed its test ({e:#}); video in news articles cannot be found until this is fixed"
        )),
    }
}

/// The whole nightly run: browser, then every link.
pub async fn run(repo: &Repository, ytdl: &Path, pacer: &HostPacer, health: &HealthState, bin_dir: &Path) -> anyhow::Result<usize> {
    check_browser(health).await;
    let links = repo.list_selfcheck_links()?;
    let results = check_and_record(repo, ytdl, pacer, &links).await;
    refresh_health(repo, health, bin_dir);
    let failing = results.iter().filter(|(_, o)| !o.ok && !o.inconclusive).count();
    let unchecked = results.iter().filter(|(_, o)| o.inconclusive).count();
    info!("Self-check: {} of {} links work, {unchecked} not checked this time", results.len() - failing - unchecked, results.len());
    Ok(failing)
}

/// How the self-check judged a new build of a tool against the one before it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateVerdict {
    /// The new build stays; `checked` links were compared.
    Kept { checked: usize },
    /// The new build broke these links (they pass with the old one): the old one is back.
    RolledBack { broken: Vec<String> },
    /// Not judged (P11.4): some platform's links could not be compared, a
    /// platform limiting requests. The old build is live, the new one is not
    /// marked rejected, and the update is tried again the next night.
    Postponed { why: String },
    /// The new build is live but could not be verified, and the old one could
    /// not be put back (a swap failed). Not a success: the administrator looks.
    Unverified { why: String },
}

/// The group a self-check link is judged in: its video platform, or "other".
fn group_of(link: &SelfcheckLink) -> &'static str {
    omni_broadcast::pacing::platform_key(&link.url).unwrap_or("other")
}

/// The groups among `results` none of whose links could be compared (every
/// one inconclusive), in the order they first appear.
fn uncompared_groups(results: &[(SelfcheckLink, LinkOutcome)]) -> Vec<&'static str> {
    let mut groups: Vec<(&'static str, bool)> = Vec::new();
    for (link, outcome) in results {
        let g = group_of(link);
        match groups.iter_mut().find(|(k, _)| *k == g) {
            Some(entry) => entry.1 |= !outcome.inconclusive,
            None => groups.push((g, !outcome.inconclusive)),
        }
    }
    groups.into_iter().filter(|(_, compared)| !compared).map(|(g, _)| g).collect()
}

fn labels(links: &[SelfcheckLink]) -> String {
    links.iter().map(|l| l.label.as_str()).collect::<Vec<_>>().join(", ")
}

/// Judge a new yt-dlp that is already live against the links that `worked`
/// before it. `check` runs one round over some links with whatever build is
/// live; `swap` puts the other build live (`true` when it did).
///
/// Links are judged per platform (P11.4): a release that only breaks YouTube
/// must not pass on the strength of an X link. In order:
/// 1. A link the new build really fails and the old one passes: the release
///    broke it, rolled back (whatever else could not be compared).
/// 2. A platform none of whose links could be compared with the new build (a
///    rate limit), or a failed link the old build could not be asked about:
///    postponed, the old build live.
/// 3. Links both builds really fail are the sites, not the update: kept, the
///    new build put back.
///
/// A swap that fails leaves the new build live and unverified
/// ([`UpdateVerdict::Unverified`]), or the old one live when it was the swap
/// back to the new build (postponed).
///
/// Whenever the old build is left live (postponed, rolled back), what the new
/// build stored for a link the old build did not answer for is withdrawn:
/// `withdraw` puts those links back as `worked` holds them (their stored state
/// before the update), with the reason. Otherwise a link only the new build was
/// seen failing would read as failing, and drop out of the next night's check.
pub(crate) async fn verify_update<C, F>(
    worked: &[SelfcheckLink],
    mut check: C,
    mut swap: impl FnMut() -> bool,
    mut withdraw: impl FnMut(&[SelfcheckLink], &str),
) -> UpdateVerdict
where
    C: FnMut(Vec<SelfcheckLink>) -> F,
    F: std::future::Future<Output = Vec<(SelfcheckLink, LinkOutcome)>>,
{
    if worked.is_empty() {
        return UpdateVerdict::Kept { checked: 0 };
    }
    let results = check(worked.to_vec()).await;
    let uncompared = uncompared_groups(&results);
    let compared = results.iter().filter(|(_, o)| !o.inconclusive).count();
    // The links the new build stored a result for.
    let stored_by_new: Vec<i64> = results.iter().filter(|(_, o)| !o.inconclusive).map(|(l, _)| l.id).collect();
    // Those of them the old build did not answer for, as they were before the update.
    let unanswered_by_old = |answered: &[i64]| -> Vec<SelfcheckLink> {
        worked.iter().filter(|l| stored_by_new.contains(&l.id) && !answered.contains(&l.id)).cloned().collect()
    };
    let not_compared = || {
        format!("no {} link could be compared with the new build: the site was limiting requests", uncompared.join(", "))
    };
    let failed: Vec<SelfcheckLink> = results.into_iter().filter(|(_, o)| !o.ok && !o.inconclusive).map(|(l, _)| l).collect();
    if failed.is_empty() {
        if uncompared.is_empty() {
            return UpdateVerdict::Kept { checked: compared };
        }
        let why = not_compared();
        if !swap() {
            return UpdateVerdict::Unverified { why: format!("{why}, and the previous build could not be put back") };
        }
        withdraw(&unanswered_by_old(&[]), "the update was postponed");
        return UpdateVerdict::Postponed { why };
    }
    if !swap() {
        return UpdateVerdict::Unverified {
            why: format!("the new build fails {}, and the previous build could not be put back to compare", labels(&failed)),
        };
    }
    let with_old = check(failed.clone()).await;
    let answered: Vec<i64> = with_old.iter().filter(|(_, o)| !o.inconclusive).map(|(l, _)| l.id).collect();
    let broken: Vec<String> = with_old.iter().filter(|(_, o)| o.ok).map(|(l, _)| l.label.clone()).collect();
    if !broken.is_empty() {
        withdraw(&unanswered_by_old(&answered), "the update was rolled back");
        return UpdateVerdict::RolledBack { broken };
    }
    // The previous build is live from here on unless the new one is put back.
    if !uncompared.is_empty() {
        withdraw(&unanswered_by_old(&answered), "the update was postponed");
        return UpdateVerdict::Postponed { why: not_compared() };
    }
    let unasked: Vec<SelfcheckLink> = with_old.iter().filter(|(_, o)| o.inconclusive).map(|(l, _)| l.clone()).collect();
    if !unasked.is_empty() {
        withdraw(&unanswered_by_old(&answered), "the update was postponed");
        return UpdateVerdict::Postponed {
            why: format!("{} could not be compared with the previous build: the site was limiting requests", labels(&unasked)),
        };
    }
    // Both fail those links: the sites changed or the network is down. The
    // newer build is the better bet for a fix; put it back.
    if !swap() {
        withdraw(&unanswered_by_old(&answered), "the update was postponed");
        return UpdateVerdict::Postponed {
            why: format!("both builds fail {}, and the new build could not be put back", labels(&failed)),
        };
    }
    // Record what the build that stays in place sees.
    check(failed).await;
    UpdateVerdict::Kept { checked: compared }
}

/// [`verify_update`]'s `withdraw` against the database: each link's stored
/// result is put back as it was before the update, and one audit line says why.
pub(crate) fn withdraw_results(repo: &Repository, links: &[SelfcheckLink], why: &str) {
    if links.is_empty() {
        return;
    }
    for link in links {
        if let Err(e) = repo.restore_selfcheck_result(link) {
            warn!("Self-check: the result for {} could not be put back: {e:#}", link.label);
        }
    }
    info!("Self-check: the new yt-dlp build's results for {} withdrawn: {why}", labels(links));
    let _ = repo.log_audit(
        "INFO",
        "SYSTEM",
        &format!("Self-check: the new yt-dlp build's results for {} were withdrawn because {why}", labels(links)),
    );
}

/// Judge a new Deno that is already live: any link that worked and really
/// fails now rolls it back (there is no comparison with the old one).
/// Inconclusive links do not count. Deno only matters for YouTube, so when the
/// list has YouTube links and none of them could be compared, the update is
/// postponed (the caller puts the old Deno back and tries again the next night).
pub(crate) fn judge_deno(results: &[(SelfcheckLink, LinkOutcome)]) -> UpdateVerdict {
    let broken: Vec<String> = results.iter().filter(|(_, o)| !o.ok && !o.inconclusive).map(|(l, _)| l.label.clone()).collect();
    if !broken.is_empty() {
        return UpdateVerdict::RolledBack { broken };
    }
    if uncompared_groups(results).contains(&"youtube") {
        return UpdateVerdict::Postponed { why: "no youtube link could be compared with the new Deno: the site was limiting requests".into() };
    }
    UpdateVerdict::Kept { checked: results.iter().filter(|(_, o)| !o.inconclusive).count() }
}

/// After this many nights in a row with the yt-dlp update postponed, the
/// self-check line turns amber (P11.4).
pub const POSTPONED_NIGHTS_AMBER: u32 = 3;

/// The marker next to `yt-dlp.exe.rejected` that counts the nights in a row the
/// yt-dlp update was postponed: "3 2026-11-17", the count and the local date
/// of the last one.
pub fn postponed_marker(bin_dir: &Path) -> std::path::PathBuf {
    bin_dir.join(omni_core::dependencies::YTDL_POSTPONED_MARKER)
}

/// The marker's count and date. An older marker holding only a number reads
/// with no date; anything unreadable reads as none.
fn read_postponed(bin_dir: &Path) -> (u32, Option<chrono::NaiveDate>) {
    let text = std::fs::read_to_string(postponed_marker(bin_dir)).unwrap_or_default();
    let mut parts = text.split_whitespace();
    let count = parts.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    let date = parts.next().and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
    (count, date)
}

/// The nights in a row the yt-dlp update was postponed (0 without a marker).
pub fn postponed_nights(bin_dir: &Path) -> u32 {
    read_postponed(bin_dir).0
}

/// One more postponed night (`true`), or none in a row any more (`false`: the
/// update was judged, kept or rolled back, or there is nothing to update).
/// Counted at most once per local day: the admin's «Run now» on the same day
/// is not another night.
pub fn note_postponed(bin_dir: &Path, postponed: bool) {
    note_postponed_on(bin_dir, postponed, Local::now().date_naive());
}

/// [`note_postponed`] with `today` given.
pub(crate) fn note_postponed_on(bin_dir: &Path, postponed: bool, today: chrono::NaiveDate) {
    let marker = postponed_marker(bin_dir);
    let result = if postponed {
        let (count, last) = read_postponed(bin_dir);
        if last == Some(today) {
            return;
        }
        std::fs::write(&marker, format!("{} {}", count + 1, today.format("%Y-%m-%d")))
    } else {
        match std::fs::remove_file(&marker) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            other => other,
        }
    };
    if let Err(e) = result {
        warn!("Could not update {marker:?}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A browser that will not start sandboxed is red, with the reason; a page failure stays amber (P2.13).
    #[test]
    fn a_failed_sandboxed_launch_turns_the_browser_line_red() {
        use anyhow::Context;
        use omni_core::health::Health;
        let launch: anyhow::Result<String> = Err(anyhow::anyhow!("Failed launching Chromium via CDP"))
            .context(omni_browser::sniffer::SANDBOXED_LAUNCH_FAILED);
        let c = browser_check(launch);
        assert_eq!(c.state, Health::Down);
        assert!(c.detail.as_deref().unwrap_or("").contains("sandboxed launch failed"), "{c:?}");
        let page: anyhow::Result<String> = Err(anyhow::anyhow!("the browser opened its test page but did not find the video on it"));
        assert_eq!(browser_check(page).state, Health::Degraded);
        assert_eq!(browser_check(Ok("Chrome/141.0.7390.55".into())).state, Health::Ok);
    }

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
        let now = Utc::now();
        let ok = summary(&[link("YouTube", Some(true)), link("TikTok", Some(true))], now, 0);
        assert_eq!(ok.state, omni_core::health::Health::Ok);
        assert_eq!(ok.detail.as_deref(), Some("all 2 links work"));

        let bad = summary(&[link("YouTube", Some(true)), link("Instagram reel", Some(false)), link("New", None)], now, 0);
        assert_eq!(bad.state, omni_core::health::Health::Degraded);
        let d = bad.detail.unwrap();
        assert!(d.starts_with("1 of 2 not working: Instagram reel (since "), "{d}");

        assert_eq!(summary(&[link("New", None)], now, 0).state, omni_core::health::Health::Ok);
    }

    /// P11.4 review round 2: a platform that refuses the checks every night
    /// leaves its links' last result standing; after two days unchecked the
    /// line turns amber and says so, instead of "all N links work" for ever.
    /// Round 3: the text names no cause (a rate limit, the check switched off
    /// and the daemon down look the same).
    #[test]
    fn links_not_checked_for_two_days_turn_the_line_amber() {
        let now = Utc::now();
        let mut old = link("YouTube", Some(true));
        old.last_checked_at = Some(now - chrono::Duration::days(3));
        let mut recent = link("TikTok", Some(true));
        recent.last_checked_at = Some(now - chrono::Duration::days(1));
        let line = summary(&[old.clone(), recent.clone()], now, 0);
        assert_eq!(line.state, omni_core::health::Health::Degraded);
        let d = line.detail.unwrap();
        let date = old.last_checked_at.unwrap().with_timezone(&Local).format("%d/%m").to_string();
        assert_eq!(d, format!("1 link(s) not checked since {date}"));
        assert_eq!(summary(&[recent.clone()], now, 0).detail.as_deref(), Some("all 1 links work"));
        // A result with no check time at all is stale too.
        let mut never = link("X", Some(true));
        never.last_checked_at = None;
        assert!(summary(&[never], now, 0).detail.unwrap().contains("not checked since an unknown date"));
        // With a failing link too, both are said.
        let d = summary(&[old, link("Instagram reel", Some(false))], now, 0).detail.unwrap();
        assert!(d.starts_with("1 of 2 not working: Instagram reel") && d.contains("; 1 link(s) not checked since"), "{d}");
    }

    /// P11.4 review round 2: three postponed nights in a row turn the line amber.
    #[test]
    fn a_yt_dlp_update_postponed_three_nights_turns_the_line_amber() {
        let now = Utc::now();
        let links = [link("YouTube", Some(true))];
        assert_eq!(summary(&links, now, 2).state, omni_core::health::Health::Ok);
        let line = summary(&links, now, 3);
        assert_eq!(line.state, omni_core::health::Health::Degraded);
        assert_eq!(line.detail.as_deref(), Some("yt-dlp update waiting 3 nights: the check links are being rate-limited"));
        // Counted in a marker that survives a restart, ended by a judged update.
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(postponed_nights(dir.path()), 0);
        let day = |d: u32| chrono::NaiveDate::from_ymd_opt(2026, 11, d).unwrap();
        for d in 15..=17 {
            note_postponed_on(dir.path(), true, day(d));
        }
        assert_eq!(postponed_nights(dir.path()), 3);
        note_postponed(dir.path(), false);
        assert_eq!(postponed_nights(dir.path()), 0);
        assert!(!postponed_marker(dir.path()).exists());
        note_postponed(dir.path(), false); // nothing there: no error
    }

    /// Review round 3: «Run now» twice and the nightly run on one day are one
    /// postponed night; the next day counts again; an older marker that holds
    /// only a number still reads.
    #[test]
    fn postponed_nights_count_once_per_local_day() {
        let dir = tempfile::tempdir().unwrap();
        let day = |d: u32| chrono::NaiveDate::from_ymd_opt(2026, 11, d).unwrap();
        for _ in 0..3 {
            note_postponed_on(dir.path(), true, day(17));
        }
        assert_eq!(postponed_nights(dir.path()), 1);
        assert_eq!(std::fs::read_to_string(postponed_marker(dir.path())).unwrap(), "1 2026-11-17");
        note_postponed_on(dir.path(), true, day(18));
        assert_eq!(postponed_nights(dir.path()), 2);
        // The format before the date was kept: a number alone.
        std::fs::write(postponed_marker(dir.path()), "2\n").unwrap();
        assert_eq!(postponed_nights(dir.path()), 2);
        note_postponed_on(dir.path(), true, day(19));
        assert_eq!(postponed_nights(dir.path()), 3);
        note_postponed_on(dir.path(), true, day(19));
        assert_eq!(postponed_nights(dir.path()), 3);
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

/// P11.4 review round 1: the self-check and the nightly verification of a new
/// yt-dlp with a platform refusing us as a rate limit. Two builds answer from a
/// script; nothing leaves the process.
#[cfg(test)]
mod rate_limit_tests {
    use super::*;
    use crate::site_memory::tests::repo as test_repo;
    use std::collections::HashMap;
    use std::sync::Mutex;

    const YT_A: &str = "https://www.youtube.com/watch?v=aaaaaaaaaaa";
    const YT_B: &str = "https://www.youtube.com/watch?v=bbbbbbbbbbb";
    const X_C: &str = "https://x.com/someone/status/3";
    const BOT: &str = "ERROR: [youtube] aaaaaaaaaaa: Sign in to confirm you’re not a bot. Use --cookies-from-browser or --cookies";
    const BROKEN: &str = "ERROR: [twitter] 3: Unable to extract video";

    /// What each build's yt-dlp says about each link (anything not listed reads),
    /// which build is live, and what was asked of which build.
    struct Builds {
        live: Mutex<&'static str>,
        says: HashMap<(&'static str, &'static str), &'static str>,
        asked: Mutex<Vec<(String, &'static str)>>,
        /// This swap fails (1 for the first, 2 for the second, …): the build
        /// that is live stays live.
        swap_fails: Option<usize>,
        swaps: Mutex<usize>,
    }

    impl Builds {
        fn new(says: &[(&'static str, &'static str, &'static str)]) -> Self {
            Self {
                live: Mutex::new("new"),
                says: says.iter().map(|(b, u, e)| ((*b, *u), *e)).collect(),
                asked: Mutex::new(Vec::new()),
                swap_fails: None,
                swaps: Mutex::new(0),
            }
        }
        fn swap(&self) -> bool {
            let mut n = self.swaps.lock().unwrap();
            *n += 1;
            if self.swap_fails == Some(*n) {
                return false;
            }
            let mut live = self.live.lock().unwrap();
            *live = if *live == "new" { "old" } else { "new" };
            true
        }
        fn asked_of(&self, url: &str) -> usize {
            self.asked.lock().unwrap().iter().filter(|(u, _)| u == url).count()
        }
    }

    impl LinkTools for Builds {
        async fn repair(&self, _url: &str) -> Option<String> {
            None
        }
        async fn site_api(&self, _url: &str) -> Option<String> {
            None
        }
        async fn page_scan(&self, _url: &str) -> PageScan {
            PageScan::default()
        }
        async fn final_url(&self, _url: &str) -> Option<String> {
            None
        }
        async fn probe(&self, url: &str) -> Result<String, String> {
            let build = *self.live.lock().unwrap();
            self.asked.lock().unwrap().push((url.to_string(), build));
            match self.says.get(&(build, url)) {
                Some(e) => Err(e.to_string()),
                None => Ok("youtube video 1".into()),
            }
        }
        async fn sniff(&self, url: &str) -> Sniffed {
            Sniffed { media: Err(anyhow::anyhow!("nothing")), snapshot: omni_browser::PageSnapshot::new(url) }
        }
        async fn probe_candidate(&self, _url: &str, _referer: &str, _user_agent: &str, _cookies: Option<&str>) -> Probe {
            Probe::Unknown
        }
    }

    /// The self-check links as stored, each having worked last time.
    fn links(repo: &Repository, urls: &[&str]) -> Vec<SelfcheckLink> {
        for (i, u) in urls.iter().enumerate() {
            let id = repo.add_selfcheck_link(&format!("link {i}"), u).unwrap();
            repo.record_selfcheck_result(id, true, "ok").unwrap();
        }
        let wanted: Vec<String> = urls.iter().map(|u| u.to_string()).collect();
        repo.list_selfcheck_links().unwrap().into_iter().filter(|l| wanted.contains(&l.url)).collect()
    }

    async fn verify(repo: &Repository, b: &Builds, worked: &[SelfcheckLink]) -> UpdateVerdict {
        verify_update(worked, |l| async move { check_and_record_with(repo, b, &l).await }, || b.swap(), |links, why| withdraw_results(repo, links, why)).await
    }

    /// A rate-limited link is inconclusive, asked once, not stored as failed;
    /// the round's other YouTube links are not asked at all, and X still is.
    #[tokio::test(start_paused = true)]
    async fn a_rate_limited_link_is_not_checked_and_its_platform_is_left_alone_for_the_round() {
        let (_db, repo) = test_repo();
        let all = links(&repo, &[YT_A, YT_B, X_C]);
        let b = Builds::new(&[("new", YT_A, BOT)]);
        let out = check_and_record_with(&repo, &b, &all).await;
        assert!(out.iter().take(2).all(|(_, o)| o.inconclusive && !o.ok), "{out:?}");
        assert!(out[2].1.ok);
        assert_eq!((b.asked_of(YT_A), b.asked_of(YT_B), b.asked_of(X_C)), (1, 0, 1), "no second request to a platform refusing us");
        // Stored as it was: still working, the health line unchanged.
        let stored: Vec<SelfcheckLink> = repo.list_selfcheck_links().unwrap().into_iter().filter(|l| all.iter().any(|a| a.id == l.id)).collect();
        assert_eq!(stored.len(), 3);
        assert!(stored.iter().all(|l| l.last_ok == Some(true)), "{stored:?}");
        assert_eq!(stored[0].last_detail.as_deref(), Some("ok"), "a result was stored for the refused link");
        // A new round asks YouTube again.
        let b = Builds::new(&[]);
        check_and_record_with(&repo, &b, &all).await;
        assert_eq!(b.asked_of(YT_B), 1);
    }

    /// The case review round 1 found: the new build's YouTube link is refused
    /// as a rate limit, and the old one, asked later, would pass it. That is
    /// not the release breaking it: the build is not rejected (round 2: and,
    /// with YouTube not compared at all, not kept either: postponed).
    #[tokio::test(start_paused = true)]
    async fn a_good_release_is_not_rejected_for_a_rate_limit() {
        let (_db, repo) = test_repo();
        let worked = links(&repo, &[YT_A, X_C]);
        let b = Builds::new(&[("new", YT_A, BOT)]);
        let verdict = verify(&repo, &b, &worked).await;
        assert!(matches!(verdict, UpdateVerdict::Postponed { .. }), "{verdict:?}");
        assert_eq!(b.asked.lock().unwrap().iter().filter(|(_, build)| *build == "old").count(), 0, "the old build was asked");
    }

    /// Review round 2, finding 1: a release that makes every YouTube request a
    /// bot check must not pass on the strength of an X link. YouTube had
    /// worked links and none could be compared: postponed, the old build back.
    #[tokio::test(start_paused = true)]
    async fn a_platform_with_no_link_compared_postpones_even_when_another_platform_passes() {
        let (_db, repo) = test_repo();
        let worked = links(&repo, &[YT_A, YT_B, X_C]);
        let b = Builds::new(&[("new", YT_A, BOT)]);
        let verdict = verify(&repo, &b, &worked).await;
        assert!(matches!(&verdict, UpdateVerdict::Postponed { why } if why.contains("youtube")), "{verdict:?}");
        assert_eq!(*b.live.lock().unwrap(), "old", "the unverified build stayed live");
        // One YouTube link compared is enough for YouTube: the other one refused is not a failure.
        let b = Builds::new(&[("new", YT_B, BOT)]);
        assert_eq!(verify(&repo, &b, &worked).await, UpdateVerdict::Kept { checked: 2 });
        assert_eq!(*b.live.lock().unwrap(), "new");
    }

    /// Review round 2, test (a): a real break on one platform is a rollback
    /// even while another platform is rate-limited.
    #[tokio::test(start_paused = true)]
    async fn a_real_break_rolls_back_while_another_platform_is_rate_limited() {
        let (_db, repo) = test_repo();
        let worked = links(&repo, &[YT_A, X_C]);
        let b = Builds::new(&[("new", X_C, BROKEN), ("new", YT_A, BOT)]);
        assert_eq!(verify(&repo, &b, &worked).await, UpdateVerdict::RolledBack { broken: vec!["link 1".into()] });
        assert_eq!(*b.live.lock().unwrap(), "old");
    }

    /// Review round 2, finding 2: the new build really fails A and B; the old
    /// build fails B too but cannot be asked about A (rate-limited). A was never
    /// compared: postponed, the old build stays, the new one is not put back.
    #[tokio::test(start_paused = true)]
    async fn a_failed_link_the_old_build_could_not_be_asked_about_postpones() {
        let (_db, repo) = test_repo();
        let worked = links(&repo, &[X_C, YT_B]);
        let yt_broken = "ERROR: [youtube] bbbbbbbbbbb: Unable to extract video data";
        let b = Builds::new(&[
            ("new", X_C, BROKEN),
            ("new", YT_B, yt_broken),
            ("old", X_C, "ERROR: [twitter] 3: HTTP Error 429: Too Many Requests"),
            ("old", YT_B, yt_broken),
        ]);
        let verdict = verify(&repo, &b, &worked).await;
        assert!(matches!(&verdict, UpdateVerdict::Postponed { why } if why.contains("link 0")), "{verdict:?}");
        assert_eq!(*b.live.lock().unwrap(), "old");
    }

    /// Review round 2, finding 3: a swap that fails is never reported as a kept update.
    #[tokio::test(start_paused = true)]
    async fn a_failed_swap_leaves_the_new_build_unverified_not_kept() {
        let (_db, repo) = test_repo();
        // Nothing comparable on YouTube, and the old build cannot be put back.
        let worked = links(&repo, &[YT_A, X_C]);
        let mut b = Builds::new(&[("new", YT_A, BOT)]);
        b.swap_fails = Some(1);
        assert!(matches!(verify(&repo, &b, &worked).await, UpdateVerdict::Unverified { .. }));
        assert_eq!(*b.live.lock().unwrap(), "new");
        // A real failure the old build cannot be put back to compare.
        let mut b = Builds::new(&[("new", X_C, BROKEN)]);
        b.swap_fails = Some(1);
        let verdict = verify(&repo, &b, &worked).await;
        assert!(matches!(&verdict, UpdateVerdict::Unverified { why } if why.contains("link 1")), "{verdict:?}");
    }

    /// Review round 3, row 5 of the table: the new build really fails X and
    /// cannot be compared on YouTube; the old build fails X too. YouTube was
    /// never compared: postponed, the old build stays live.
    #[tokio::test(start_paused = true)]
    async fn a_platform_never_compared_postpones_even_when_both_builds_fail_another() {
        let (_db, repo) = test_repo();
        let worked = links(&repo, &[YT_A, X_C]);
        let b = Builds::new(&[("new", X_C, BROKEN), ("new", YT_A, BOT), ("old", X_C, BROKEN)]);
        let verdict = verify(&repo, &b, &worked).await;
        assert!(matches!(&verdict, UpdateVerdict::Postponed { why } if why.contains("youtube")), "{verdict:?}");
        assert_eq!(*b.live.lock().unwrap(), "old");
    }

    /// Review round 3, row 7: both builds really fail a link and the swap back
    /// to the new build fails: postponed, the old build live (not kept).
    #[tokio::test(start_paused = true)]
    async fn both_builds_failing_and_a_failed_swap_back_is_postponed_with_the_old_build_live() {
        let (_db, repo) = test_repo();
        let worked = links(&repo, &[YT_A, X_C]);
        let mut b = Builds::new(&[("new", X_C, BROKEN), ("old", X_C, BROKEN)]);
        b.swap_fails = Some(2);
        let verdict = verify(&repo, &b, &worked).await;
        assert!(matches!(&verdict, UpdateVerdict::Postponed { why } if why.contains("both builds fail link 1")), "{verdict:?}");
        assert_eq!(*b.live.lock().unwrap(), "old");
    }

    /// Review round 3, finding 1: a link only the new build was seen failing
    /// (the old build could not be asked) is put back as it was before the
    /// update when the update is postponed: still working, no failing date,
    /// and in the next night's list of links that worked.
    #[tokio::test(start_paused = true)]
    async fn a_result_of_a_build_that_is_not_left_live_is_withdrawn() {
        let (_db, repo) = test_repo();
        let worked = links(&repo, &[X_C, YT_B]);
        let before: Vec<SelfcheckLink> = worked.clone();
        let b = Builds::new(&[("new", X_C, BROKEN), ("old", X_C, "ERROR: [twitter] 3: HTTP Error 429: Too Many Requests")]);
        let verdict = verify(&repo, &b, &worked).await;
        assert!(matches!(verdict, UpdateVerdict::Postponed { .. }), "{verdict:?}");
        let now: Vec<SelfcheckLink> =
            repo.list_selfcheck_links().unwrap().into_iter().filter(|l| before.iter().any(|w| w.id == l.id)).collect();
        for (was, is) in before.iter().zip(&now) {
            assert_eq!(
                (is.last_ok, is.failing_since, is.last_checked_at, is.last_detail.clone(), is.last_ok_at),
                (was.last_ok, was.failing_since, was.last_checked_at, was.last_detail.clone(), was.last_ok_at),
                "{} was not put back",
                was.label
            );
        }
        let next_night: Vec<i64> = repo.list_selfcheck_links().unwrap().into_iter().filter(|l| l.last_ok == Some(true)).map(|l| l.id).collect();
        assert!(before.iter().all(|w| next_night.contains(&w.id)), "a link dropped out of the next night's check");

        // Rolled back: the link the old build answered keeps the old build's
        // result, and one it did not answer is put back too.
        let (_db2, repo) = test_repo();
        let worked = links(&repo, &[X_C, YT_B]);
        let b = Builds::new(&[("new", X_C, BROKEN)]);
        assert!(matches!(verify(&repo, &b, &worked).await, UpdateVerdict::RolledBack { .. }));
        let now: Vec<SelfcheckLink> =
            repo.list_selfcheck_links().unwrap().into_iter().filter(|l| worked.iter().any(|w| w.id == l.id)).collect();
        assert_eq!(now[0].last_ok, Some(true), "the old build's own result for X");
        assert_eq!(now[0].last_detail.as_deref(), Some("found youtube video 1"), "the old build's result is its own, not withdrawn");
        assert_eq!((now[1].last_checked_at, now[1].last_detail.clone()), (worked[1].last_checked_at, worked[1].last_detail.clone()));
    }

    /// Every link inconclusive: postponed, the old build back, nothing else asked.
    #[tokio::test(start_paused = true)]
    async fn with_nothing_compared_the_update_is_postponed_and_the_old_build_is_back() {
        let (_db, repo) = test_repo();
        let worked = links(&repo, &[YT_A, YT_B]);
        let b = Builds::new(&[("new", YT_A, BOT)]);
        assert!(matches!(verify(&repo, &b, &worked).await, UpdateVerdict::Postponed { .. }));
        assert_eq!(*b.live.lock().unwrap(), "old", "the previous build is live again");
        assert_eq!(b.asked.lock().unwrap().len(), 1);
        // The old build refused too, in its round: postponed, the old build stays.
        let worked = links(&repo, &[X_C]);
        let b = Builds::new(&[("new", X_C, BROKEN), ("old", X_C, "ERROR: [twitter] 3: HTTP Error 429: Too Many Requests")]);
        assert!(matches!(verify(&repo, &b, &worked).await, UpdateVerdict::Postponed { .. }));
        assert_eq!(*b.live.lock().unwrap(), "old");
    }

    /// Unchanged: a link the new build really breaks and the old one reads is a
    /// rollback, and one both fail keeps the new build.
    #[tokio::test(start_paused = true)]
    async fn a_link_the_release_really_breaks_still_rolls_it_back() {
        let (_db, repo) = test_repo();
        let worked = links(&repo, &[YT_A, X_C]);
        let b = Builds::new(&[("new", X_C, BROKEN)]);
        assert_eq!(verify(&repo, &b, &worked).await, UpdateVerdict::RolledBack { broken: vec!["link 1".into()] });
        assert_eq!(*b.live.lock().unwrap(), "old");
        let b = Builds::new(&[("new", X_C, BROKEN), ("old", X_C, BROKEN)]);
        assert_eq!(verify(&repo, &b, &worked).await, UpdateVerdict::Kept { checked: 2 });
        assert_eq!(*b.live.lock().unwrap(), "new");
    }

    /// Deno: a rate limit does not roll it back; it only matters for YouTube,
    /// so with YouTube links and none of them compared it is postponed, even
    /// when an X link passed (review round 2).
    #[test]
    fn deno_is_judged_on_the_youtube_links_that_could_be_compared() {
        let l = |label: &str, url: &str| SelfcheckLink {
            id: 1,
            label: label.into(),
            url: url.into(),
            last_checked_at: None,
            last_ok: Some(true),
            last_detail: None,
            last_ok_at: None,
            failing_since: None,
        };
        let o = |ok: bool, inconclusive: bool| LinkOutcome { ok, inconclusive, detail: String::new() };
        assert_eq!(judge_deno(&[(l("a", YT_A), o(false, true)), (l("b", YT_B), o(true, false))]), UpdateVerdict::Kept { checked: 1 });
        assert!(matches!(judge_deno(&[(l("a", YT_A), o(false, true))]), UpdateVerdict::Postponed { .. }));
        assert!(matches!(judge_deno(&[(l("a", YT_A), o(false, true)), (l("c", X_C), o(true, false))]), UpdateVerdict::Postponed { .. }));
        assert_eq!(judge_deno(&[(l("a", YT_A), o(false, true)), (l("b", YT_B), o(false, false))]), UpdateVerdict::RolledBack { broken: vec!["b".into()] });
        assert_eq!(judge_deno(&[(l("c", X_C), o(true, false))]), UpdateVerdict::Kept { checked: 1 });
        assert_eq!(judge_deno(&[]), UpdateVerdict::Kept { checked: 0 });
    }
}
