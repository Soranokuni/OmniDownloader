//! The admin's «Test URL» (plan P10.15b): a dry run of the normal path and the
//! repair ladder on one address, showing every step with its timing and every
//! candidate with the gate's checks.
//!
//! Probe only. It never downloads (the `Env` it hands the ladder refuses),
//! queues, holds or delivers; creates no repair attempt, no job event, no recipe
//! or recipe event, no bar and no skip. The one thing it writes is the model's
//! daily call count (`llm_usage`) when the admin ticked «use the model»: a
//! ticked run counts toward the daily cap like any other.

use serde::Serialize;
use std::sync::atomic::AtomicBool;

use omni_browser::evidence;
use omni_web::state::{TestUrlError, TestUrlFuture, UrlTester};

use super::*;

/// The whole test may take this long.
const TOTAL_BUDGET: Duration = Duration::from_secs(180);
/// yt-dlp's share of it.
const YTDL_MAX: Duration = Duration::from_secs(60);
/// The sniffer's share of it.
const SNIFF_SECS: u64 = 25;
/// Past the budget, the backstop around the whole run.
const BACKSTOP: Duration = Duration::from_secs(15);
/// Longest piece of text copied from a tool into the report.
const TEXT_MAX: usize = 200;

#[derive(Debug, Serialize)]
pub struct DryRunReport {
    pub host: String,
    pub use_llm: bool,
    pub repair_mode: String,
    pub total_ms: u64,
    pub verdict: String,
    /// Every step in order, with its time.
    pub steps: Vec<Step>,
    pub normal: Normal,
    /// `None` when the normal path works, so the ladder would not run.
    pub ladder: Option<Ladder>,
}

#[derive(Debug, Serialize)]
pub struct Step {
    pub name: String,
    pub ms: u64,
    pub note: String,
}

#[derive(Debug, Default, Serialize)]
pub struct Normal {
    /// `web page`, `platform post`, `file locker`...: only a web page can reach the ladder.
    pub kind: String,
    /// What the site's own API said, asked first as in the real job.
    pub site_api: Option<SiteApiResult>,
    pub ytdlp: Option<YtdlpResult>,
    pub sniff: Option<SniffResult>,
}

#[derive(Debug, Serialize)]
pub struct SiteApiResult {
    /// Host and path of the video the API names, if it names one.
    pub video: Option<String>,
    pub error: Option<String>,
    pub ms: u64,
}

fn kind_word(kind: JobKind) -> &'static str {
    match kind {
        JobKind::WebPage => "web page",
        JobKind::Platform => "platform post",
        JobKind::Locker => "file locker link",
        JobKind::Attachment => "attachment",
        JobKind::ArticleSibling => "article sibling",
        JobKind::Other => "other",
    }
}

#[derive(Debug, Serialize)]
pub struct YtdlpResult {
    pub ok: bool,
    pub extractor: Option<String>,
    pub title: Option<String>,
    pub duration_secs: Option<f64>,
    pub formats: usize,
    pub error: Option<String>,
    pub ms: u64,
}

#[derive(Debug, Serialize)]
pub struct SniffResult {
    pub ok: bool,
    /// Host and path of the best stream; never its query.
    pub stream: Option<String>,
    pub streams: usize,
    pub error: Option<String>,
    pub ms: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct Ladder {
    /// Whether a page with its text could be looked at.
    pub page_looked: bool,
    /// The snapshot's class (`strong`, `weak`, `none`, `protected`, ...) and its signals.
    pub class: Option<String>,
    pub signals: Vec<String>,
    pub family: Option<String>,
    pub recipes: Vec<RecipeRow>,
    pub live: Option<LiveRow>,
    /// Every address the gate looked at, verified ones first in rank order.
    pub candidates: Vec<CandRow>,
    pub llm: Option<LlmRow>,
}

#[derive(Debug, Serialize)]
pub struct RecipeRow {
    /// A built-in recipe by its name (`og_video`); any other as `site|player key` and its hash.
    pub name: String,
    pub hash: String,
    /// `site`, `player` or `generic` (built in).
    pub tier: String,
    pub origin: String,
    pub outcome: String,
    pub address: Option<String>,
    /// What the gate made of the address it produced.
    pub gate: Option<String>,
    pub ms: u64,
}

#[derive(Debug, Serialize)]
pub struct LiveRow {
    pub run: String,
    pub actions: Vec<String>,
    pub ms: u64,
}

#[derive(Debug, Serialize)]
pub struct CandRow {
    pub address: String,
    pub origin: String,
    /// 1 is the one the ladder would use; `None` for a rejected address.
    pub rank: Option<usize>,
    pub verified: bool,
    pub duration_secs: Option<f64>,
    pub height: Option<u32>,
    pub score: Option<i64>,
    pub checks: Vec<CheckRow>,
}

#[derive(Debug, Serialize)]
pub struct CheckRow {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Serialize)]
pub struct LlmRow {
    pub asked: bool,
    pub calls: i64,
    pub rounds: usize,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub outcome: String,
}

/// The ladder's world with the download taken away.
struct ProbeOnly(Arc<dyn Env>);

#[async_trait]
impl Env for ProbeOnly {
    async fn diagnose(&self, url: &str, secs: u64) -> Option<PageSnapshot> {
        self.0.diagnose(url, secs).await
    }
    async fn live_snapshot(&self, url: &str, actions: &[LiveAction], secs: u64) -> Option<(PageSnapshot, LiveRun, Vec<String>)> {
        self.0.live_snapshot(url, actions, secs).await
    }
    fn live_limits(&self) -> (u64, Duration) {
        self.0.live_limits()
    }
    fn model(&self) -> Option<&dyn RepairModel> {
        self.0.model()
    }
    fn fetcher(&self) -> &dyn Fetcher {
        self.0.fetcher()
    }
    fn prober(&self) -> &dyn Prober {
        self.0.prober()
    }
    fn resolver(&self) -> &dyn Resolver {
        self.0.resolver()
    }
    fn is_ad(&self, url: &str) -> bool {
        self.0.is_ad(url)
    }
    fn allow_loopback(&self) -> bool {
        self.0.allow_loopback()
    }
    async fn host_is_public(&self, host: &str, port: u16) -> bool {
        self.0.host_is_public(host, port).await
    }
    async fn site_api(&self, url: &str) -> std::result::Result<Option<String>, String> {
        self.0.site_api(url).await
    }
    async fn ytdl_probe(&self, url: &str, timeout: Duration) -> std::result::Result<LinkInfo, String> {
        self.0.ytdl_probe(url, timeout).await
    }
    async fn page_scan(&self, url: &str, timeout: Duration) -> omni_broadcast::downloader::PageScan {
        self.0.page_scan(url, timeout).await
    }
    async fn final_url(&self, url: &str) -> Option<String> {
        self.0.final_url(url).await
    }
    async fn sniff(&self, url: &str, secs: u64) -> SniffSeen {
        self.0.sniff(url, secs).await
    }
    async fn download(&self, _: Job, _: &str, _: Option<String>, _: &CancellationToken, _: DeliveryMode) -> Result<RunOutcome> {
        Err(anyhow::anyhow!("a dry run never downloads"))
    }
}

/// One sniff of the page within what is left of the deadline, recorded in the report.
async fn sniff_step(
    env: &Arc<dyn Env>,
    repo: &Repository,
    url: &str,
    deadline: Instant,
    steps: &mut Vec<Step>,
    normal: &mut Normal,
) -> (SniffSeen, Vec<omni_core::site_videos::Furniture>) {
    let mut seen = SniffSeen { error: Some("no time left".into()), ..Default::default() };
    let left = deadline.saturating_duration_since(Instant::now());
    if !left.is_zero() {
        let t = Instant::now();
        let secs = left.as_secs().clamp(1, SNIFF_SECS);
        seen = tokio::time::timeout(Duration::from_secs(secs) + LIVE_LAUNCH_ALLOWANCE + GRACE, env.sniff(url, secs))
            .await
            .unwrap_or_else(|_| SniffSeen { error: Some("the browser did not answer in time".into()), ..Default::default() });
        let sniff_ms = ms(t);
        normal.sniff = Some(SniffResult {
            ok: seen.stream.is_some(),
            stream: seen.stream.as_deref().map(shown),
            streams: seen.streams,
            error: seen.error.as_deref().map(clip),
            ms: sniff_ms,
        });
        steps.push(Step { name: "Sniffer".into(), ms: sniff_ms, note: if seen.stream.is_some() { "finds a stream".into() } else { "finds no stream".into() } });
    }
    let furniture = without_site_furniture(repo, url, &mut seen, normal, steps);
    (seen, furniture)
}

/// What the real job does with the sniff's streams before it chooses (P3.16): a video the site
/// shows on several of its articles is never the job's. Read only: the memory is a job's to fill,
/// not the Test URL's. The page counts as one of the articles. When nothing but such videos is
/// left the sniff is shown as finding no stream.
fn without_site_furniture(
    repo: &Repository,
    url: &str,
    seen: &mut SniffSeen,
    normal: &mut Normal,
    steps: &mut Vec<Step>,
) -> Vec<omni_core::site_videos::Furniture> {
    let all: Vec<String> = if seen.all.is_empty() { seen.stream.iter().cloned().collect() } else { seen.all.clone() };
    if all.is_empty() {
        return Vec::new();
    }
    let wide = crate::site_memory::recall(repo, url, Some(&seen.snapshot.clone().unwrap_or_else(|| PageSnapshot::new(url))), &all);
    let (kept, furniture) = omni_core::site_videos::split_site_wide(&all, &wide);
    if furniture.is_empty() {
        return furniture;
    }
    // Furniture took the article's own video and no own one is left: a held video is not the article's.
    let held = seen.held.clone();
    let (kept, dropped_held) = crate::site_memory::leave_nothing_held(furniture.iter().any(|f| !held.contains(&f.url)), kept, |u| held.iter().any(|h| h == u));
    seen.refused = dropped_held;
    if seen.stream.as_ref().is_some_and(|s| !kept.contains(s)) {
        seen.stream = kept.first().cloned();
    }
    if let Some(s) = normal.sniff.as_mut() {
        s.ok = seen.stream.is_some();
        s.stream = seen.stream.as_deref().map(shown);
    }
    if let Some(step) = steps.last_mut().filter(|s| s.name == "Sniffer") {
        step.note = format!(
            "{}; {} video(s) are ones the site shows on its other articles and are left out",
            if seen.stream.is_some() { "finds a stream" } else { "finds no stream" },
            furniture.len()
        );
    }
    furniture
}

/// The verdict for a page yt-dlp could not read and whose browser then finds only the site's furniture:
/// the real job ends without a video (yt-dlp has already failed, so there is no pick to fall back to).
fn furniture_verdict(furniture: &[omni_core::site_videos::Furniture]) -> String {
    format!(
        "The only videos the browser finds on this page ({}) are ones the site shows on its other articles this week. The real job takes none of them: it ends as NO_STREAM_FOUND and offers them to MCR.",
        furniture.iter().map(|f| shown(&f.url).split('/').next().unwrap_or("?").to_string()).collect::<Vec<_>>().join(", ")
    )
}

fn ms(since: Instant) -> u64 {
    since.elapsed().as_millis() as u64
}

/// Text from a tool or the gate for the report: every address in it cut to host and path
/// (yt-dlp says "Unsupported URL: <the whole link>"), and at most `TEXT_MAX` characters.
fn clip(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(i) = ["https://", "http://"].iter().filter_map(|p| rest.find(p)).min() {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        let end = tail.find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>')).unwrap_or(tail.len());
        let (addr, after) = tail.split_at(end);
        let addr = addr.trim_end_matches(['.', ',', ';', ':', ')', ']']);
        out.push_str(&shown(addr));
        out.push_str(&tail[addr.len()..end]);
        rest = after;
    }
    out.push_str(rest);
    out.chars().take(TEXT_MAX).collect()
}

/// Host and path of an address, never its query or fragment: a query may carry a token.
fn shown(url: &str) -> String {
    match omni_core::net::parse_public_url(url) {
        Ok((u, host)) => format!("{host}{}", u.path()).chars().take(TEXT_MAX).collect(),
        Err(_) => "?".to_string(),
    }
}

fn check_rows(checks: &[repair_gate::Check]) -> Vec<CheckRow> {
    checks.iter().map(|c| CheckRow { name: c.name.to_string(), ok: c.ok, detail: clip(&c.detail) }).collect()
}

fn verified_row(v: &Verified, rank: usize) -> CandRow {
    CandRow {
        address: shown(&v.candidate.url),
        origin: v.candidate.origin.clone(),
        rank: Some(rank),
        verified: true,
        duration_secs: Some(v.duration_secs),
        height: Some(v.height),
        score: Some(v.score),
        checks: check_rows(&v.checks),
    }
}

fn rejected_row(r: &repair_gate::Rejected) -> CandRow {
    CandRow {
        address: shown(&r.candidate.url),
        origin: r.candidate.origin.clone(),
        rank: None,
        verified: false,
        duration_secs: None,
        height: None,
        score: None,
        checks: check_rows(&r.checks),
    }
}

/// What the gate made of one address, for the recipe that produced it.
fn gate_word(url: &str, verified: &[Verified], rejected: &[repair_gate::Rejected]) -> String {
    if let Some(i) = verified.iter().position(|v| v.candidate.url == url) {
        return format!("verified, rank {}", i + 1);
    }
    let Some(r) = rejected.iter().find(|r| r.candidate.url == url) else { return "not checked".to_string() };
    match r.checks.iter().rev().find(|c| !c.ok) {
        Some(c) if r.is_conclusive() => format!("rejected by the {} check", c.name),
        _ => "not checked, or out of time".to_string(),
    }
}

fn ladder_verdict(mode: RepairMode, ladder: &Ladder) -> String {
    if !ladder.page_looked {
        return "The normal path fails and no page could be looked at (the browser did not give a complete page).".to_string();
    }
    match ladder.candidates.iter().find(|c| c.rank == Some(1)) {
        Some(best) => {
            let secs = best.duration_secs.map(mm_ss).unwrap_or_default();
            let what = match mode {
                RepairMode::Off => "repair is off, so nothing would be tried",
                RepairMode::Shadow => "shadow mode only records it",
                RepairMode::Hold => "it would be downloaded and held for MCR's approval",
            };
            format!("The normal path fails; the ladder verified {} ({secs}); {what}.", best.address.split('/').next().unwrap_or("?"))
        }
        None => "The normal path fails and no candidate verified.".to_string(),
    }
}

fn finish(
    started: Instant,
    host: String,
    use_llm: bool,
    mode: RepairMode,
    steps: Vec<Step>,
    normal: Normal,
    ladder: Option<Ladder>,
    verdict: String,
) -> DryRunReport {
    DryRunReport { host, use_llm, repair_mode: match mode { RepairMode::Off => "off", RepairMode::Shadow => "shadow", RepairMode::Hold => "hold" }.to_string(), total_ms: ms(started), verdict, steps, normal, ladder }
}

/// Run the normal path's probes and, when they fail, the repair ladder in probe-only form on
/// `url`, within `deadline`. `Err` is the reason the address may not be tested (not public).
/// Reads the recipe store and the MCR skip list; writes nothing but the model's call count.
pub async fn dry_run(ctx: &RepairCtx, url: &str, use_llm: bool, deadline: Instant) -> std::result::Result<DryRunReport, String> {
    let (parsed, host) = omni_core::net::parse_public_url(url)?;
    let started = Instant::now();
    let env: Arc<dyn Env> = Arc::new(ProbeOnly(ctx.env.clone()));
    let dry = RepairCtx { repo: ctx.repo.clone(), cfg: ctx.cfg.clone(), env: env.clone() };
    let mode = ctx.cfg.mode;
    let left = || deadline.saturating_duration_since(Instant::now());
    let mut steps: Vec<Step> = Vec::new();
    let mut normal = Normal::default();

    let t = Instant::now();
    let port = parsed.port_or_known_default().unwrap_or(443);
    if !env.host_is_public(&host, port).await {
        return Err(format!("host {host} does not resolve to a public address"));
    }
    steps.push(Step { name: "Host check".into(), ms: ms(t), note: "resolves to public addresses only".into() });

    // 0. What the real job asks first (plan P3.8): the site's own API names the video, and
    // yt-dlp is then asked about that video. A failed lookup just takes the usual way.
    let kind = kind_of(url, false, false);
    normal.kind = kind_word(kind).to_string();
    let mut probe_target = url.to_string();
    if kind == JobKind::WebPage || kind == JobKind::Platform {
        let t = Instant::now();
        let looked = tokio::time::timeout(Duration::from_secs(25), env.site_api(url)).await.unwrap_or_else(|_| Err("the site's API did not answer in time".to_string()));
        let api_ms = ms(t);
        let (video, error) = match looked {
            Ok(Some(v)) => {
                probe_target = v.clone();
                (Some(shown(&v)), None)
            }
            Ok(None) => (None, None),
            Err(e) => (None, Some(clip(&e))),
        };
        steps.push(Step {
            name: "Site API".into(),
            ms: api_ms,
            note: if video.is_some() { "names a video".into() } else if error.is_some() { "failed".into() } else { "has nothing for this page".into() },
        });
        normal.site_api = Some(SiteApiResult { video, error, ms: api_ms });
    }

    // 0b. yt-dlp's page scan, as the real job takes it for a page nothing named (P3.15): a page
    // yt-dlp reaches only through its generic scan is read by the browser first.
    let mut seen: Option<SniffSeen> = None;
    // The scan's own media addresses (what yt-dlp's pick would download), and the site's furniture the browser found.
    let mut scan_media: Vec<String> = Vec::new();
    // The scan's several videos, when it found more than one (the job's multi-video pick).
    let mut scan_videos: Vec<String> = Vec::new();
    let mut scan_webpage: Option<String> = None;
    let mut furniture_seen: Vec<omni_core::site_videos::Furniture> = Vec::new();
    if kind == JobKind::WebPage && probe_target == url && !left().is_zero() {
        let t = Instant::now();
        let cap = left().min(YTDL_MAX);
        let scan = tokio::time::timeout(cap + GRACE, env.page_scan(url, cap)).await.unwrap_or_default();
        let route = omni_broadcast::downloader::first_attempt_resolved(false, false, false, &scan, url, |u| {
            let env = env.clone();
            async move { tokio::time::timeout(Duration::from_secs(15), env.final_url(&u)).await.ok().flatten() }
        })
        .await;
        scan_media = scan.media_urls.clone();
        scan_videos = scan.videos.clone();
        scan_webpage = scan.webpage_url.clone();
        steps.push(Step {
            name: "Page scan".into(),
            ms: ms(t),
            note: if route == omni_broadcast::downloader::FirstAttempt::SnifferFirst { "yt-dlp reads the page only generically".into() } else { "yt-dlp has an extractor for it, or none".into() },
        });
        if route == omni_broadcast::downloader::FirstAttempt::SnifferFirst {
            let (mut first, furniture) = sniff_step(&env, &dry.repo, url, deadline, &mut steps, &mut normal).await;
            // A browser that ended on a video platform's page is taken as finding nothing:
            // yt-dlp follows that redirect and takes the post itself.
            if omni_broadcast::downloader::landed_on_platform(first.snapshot.as_ref().and_then(|s| s.final_url.as_deref())) {
                first.stream = None;
                // The report must say what the real job does with it, not "finds a stream".
                if let Some(s) = normal.sniff.as_mut() {
                    s.ok = false;
                    s.stream = None;
                }
                if let Some(step) = steps.last_mut().filter(|s| s.name == "Sniffer") {
                    step.note = "ends on a video platform's page; counts as no stream".into();
                }
            }
            // Nothing but the site's furniture is as good as nothing found: yt-dlp's own pick is asked next,
            // and refused if it is the same promo (P3.16).
            furniture_seen = furniture;
            if let Some(stream) = first.stream.as_deref() {
                let verdict = format!(
                    "yt-dlp reads this page only through its generic scan, which does not know where on the page each video sits, so the real job lets the browser choose: the sniffer's first choice is a stream on {}. Before downloading, the real job asks yt-dlp about its top choices and skips any that are gone or live, so it may take the next one; this test does not check that. yt-dlp's own pick is used only if the browser finds nothing (the dry run does not download).",
                    shown(stream).split('/').next().unwrap_or("?")
                );
                return Ok(finish(started, host, use_llm, mode, steps, normal, None, verdict));
            }
            seen = Some(first);
        }
    }

    // 1. The normal path, first part: yt-dlp, `--simulate`.
    let t = Instant::now();
    let cap = left().min(YTDL_MAX);
    let probed = tokio::time::timeout(cap + GRACE, env.ytdl_probe(&probe_target, cap))
        .await
        .unwrap_or_else(|_| Err("yt-dlp did not answer in time".to_string()));
    let ytdl_ms = ms(t);
    // yt-dlp's own pick is a video the site shows on its other articles: the real job does not
    // download it, and goes on to the browser as after any failed download (P3.16).
    let pick_is_furniture = probed.is_ok()
        && !(scan_media.is_empty() && scan_videos.is_empty())
        && {
            // Looked up as the job does: under the asked site, yt-dlp's page and where the browser ended; held videos
            // the browser's screen left out are refused too.
            let ended = seen.as_ref().and_then(|s| s.snapshot.as_ref()).and_then(|s| s.final_url.as_deref());
            let refused = seen.as_ref().map(|s| s.refused.clone()).unwrap_or_default();
            crate::site_memory::guard_scan(&dry.repo, url, scan_webpage.as_deref(), ended, scan_videos.clone(), &scan_media, &refused).only_furniture
        };
    let ytdl_ok = probed.is_ok() && !pick_is_furniture;
    let sniff_code = if pick_is_furniture {
        ErrorCode::NoStreamFound
    } else {
        probed.as_ref().err().map(|e| omni_broadcast::errors::classify_download_error(e)).unwrap_or(ErrorCode::UnsupportedUrl)
    };
    normal.ytdlp = Some(match probed {
        Ok(i) => YtdlpResult { ok: true, extractor: Some(i.extractor), title: i.title, duration_secs: i.duration_secs, formats: i.formats, error: None, ms: ytdl_ms },
        Err(e) => YtdlpResult { ok: false, extractor: None, title: None, duration_secs: None, formats: 0, error: Some(clip(&e)), ms: ytdl_ms },
    });
    steps.push(Step { name: "yt-dlp probe".into(), ms: ytdl_ms, note: if ytdl_ok { "reads the link".into() } else if pick_is_furniture { "reads the link, but its pick is a video the site shows on its other articles".into() } else { "fails".into() } });
    if pick_is_furniture && seen.is_some() {
        // The browser, asked first, found nothing, and yt-dlp's own pick is the site's furniture.
        let verdict = "The browser finds no video on this page, and yt-dlp's own pick is a video the site shows on its other articles this week: the real job takes none of them and ends as NO_STREAM_FOUND.".to_string();
        return Ok(finish(started, host, use_llm, mode, steps, normal, None, verdict));
    }
    if ytdl_ok {
        let verdict = if seen.is_some() && !furniture_seen.is_empty() {
            "The only videos the browser finds on this page are ones the site shows on its other articles this week, so it takes none of them: the normal path would download yt-dlp's own pick (which is not one of them). The repair ladder would not run.".to_string()
        } else if seen.is_some() {
            "The browser finds no video on this page, and yt-dlp reads it only generically: the normal path would download yt-dlp's own pick. The repair ladder would not run.".to_string()
        } else {
            "yt-dlp reads this link: the normal path would download it. The repair ladder would not run.".to_string()
        };
        return Ok(finish(started, host, use_llm, mode, steps, normal, None, verdict));
    }
    // The real job sniffs only where that can help: not a page that does not exist, not YouTube,
    // not a platform post unless yt-dlp's failure leaves the sniffer a chance.
    if seen.is_none() && !sniff_code.should_sniff(url) {
        let verdict = format!(
            "yt-dlp cannot read the link ({}), and the real job does not sniff it: a sniff cannot help here. The repair ladder runs only after a sniff that finds nothing, so it does not run either.",
            sniff_code.as_str()
        );
        return Ok(finish(started, host, use_llm, mode, steps, normal, None, verdict));
    }

    // 2. One sniff of the page for a stream (already done when the browser was asked first).
    let mut seen = match seen {
        Some(done) => done,
        None => {
            let (done, furniture) = sniff_step(&env, &dry.repo, url, deadline, &mut steps, &mut normal).await;
            furniture_seen = furniture;
            done
        }
    };
    if seen.stream.is_none() && !furniture_seen.is_empty() && seen.error.is_none() {
        let verdict = furniture_verdict(&furniture_seen);
        return Ok(finish(started, host, use_llm, mode, steps, normal, None, verdict));
    }
    if let Some(stream) = seen.stream.as_deref() {
        let verdict = format!(
            "yt-dlp cannot read the link, but the sniffer finds a stream ({}). The real job would download that stream, and would run the repair ladder only if that download failed (the dry run does not download, so it stops here). The real job also drops sniffed platform posts that turn out to have no video; this test does not check that.",
            shown(stream).split('/').next().unwrap_or("?")
        );
        return Ok(finish(started, host, use_llm, mode, steps, normal, None, verdict));
    }
    // Nothing found: the ladder runs only for an article page, never for a platform post or a locker.
    if kind != JobKind::WebPage {
        let verdict = format!(
            "yt-dlp cannot read the link and the sniffer finds no stream, and the repair ladder never runs for this kind of link ({}). The real job would fail or send it to review.",
            kind_word(kind)
        );
        return Ok(finish(started, host, use_llm, mode, steps, normal, None, verdict));
    }

    // 3. The ladder.
    let Some(domain) = omni_core::urlnorm::registrable_domain(url) else {
        let verdict = "The normal path fails and the address has no site name the ladder can use.".to_string();
        return Ok(finish(started, host, use_llm, mode, steps, normal, Some(Ladder::default()), verdict));
    };
    let mut ladder = Ladder::default();
    let tokio_deadline = tokio::time::Instant::from_std(deadline);

    // a. A page with its text: the sniff's own, else one diagnostic look.
    let t = Instant::now();
    let mut snap = seen.snapshot.take().filter(|s| s.html.is_some());
    if snap.is_none() && !left().is_zero() {
        let secs = left().as_secs().clamp(1, DIAGNOSE_SECS);
        snap = tokio::time::timeout_at(tokio_deadline, env.diagnose(url, secs)).await.ok().flatten().filter(|s| s.html.is_some());
    }
    steps.push(Step { name: "Page look".into(), ms: ms(t), note: if snap.is_some() { "complete".into() } else { "no complete page".into() } });
    let Some(snap) = snap else {
        let verdict = ladder_verdict(mode, &ladder);
        return Ok(finish(started, host, use_llm, mode, steps, normal, Some(ladder), verdict));
    };
    ladder.page_looked = true;
    let ev = evidence::classify(&snap, &[], None, |u| env.is_ad(u));
    ladder.class = Some(ev.class.as_str().to_string());
    ladder.signals = ev.signals.clone();
    let family = omni_browser::player_family(&snap);
    ladder.family = family.map(str::to_string);

    // b. The recipes, best first, on the page as the browser saw it.
    let t = Instant::now();
    let now = Utc::now();
    let ranked = dry.repo.recipes_for_site(&domain, family, now).unwrap_or_else(|e| {
        warn!("Test URL: the recipe store could not be read: {e:#}");
        Vec::new()
    });
    let prior = Prior { skips: dry.repo.repair_skip_patterns(&domain, now).unwrap_or_default(), ..Default::default() };
    let mut rows: Vec<RecipeRow> = Vec::new();
    let mut cands: Vec<Candidate> = Vec::new();
    // (row index, address) for every address a recipe produced.
    let mut produced: Vec<(usize, String)> = Vec::new();
    let mut live: Vec<(usize, &RankedRecipe, Recipe)> = Vec::new();
    let take = |rows: &mut Vec<RecipeRow>, produced: &mut Vec<(usize, String)>, cands: &mut Vec<Candidate>, i: usize, r: &RankedRecipe, page: &PageSnapshot, outcome: RecipeOutcome| {
        rows[i].outcome = match outcome {
            RecipeOutcome::Media { url, referer } => {
                let url = omni_core::net::parse_public_url(&url).map(|(u, _)| u.to_string()).unwrap_or(url);
                rows[i].address = Some(shown(&url));
                if prior.skipped(&url) {
                    "found an address MCR called a wrong video on this site; dropped".to_string()
                } else {
                    if !cands.iter().any(|c| c.url == url && c.referer == referer) {
                        let (position, in_article) = player_for(page, &url);
                        cands.push(Candidate { url: url.clone(), referer, origin: format!("recipe:{}", hash8(&r.hash)), position, in_article });
                    }
                    produced.push((i, url));
                    "found an address".to_string()
                }
            }
            RecipeOutcome::NeedsLivePage => "needs a page action (click or wait)".to_string(),
            RecipeOutcome::NoValue { at_step } => format!("found nothing (step {})", at_step + 1),
            RecipeOutcome::Rejected(why) => format!("rejected: {}", clip(&why)),
        };
    };
    for r in &ranked {
        let t1 = Instant::now();
        rows.push(RecipeRow {
            name: match r.scope_kind {
                ScopeKind::Generic => r.scope_key.clone(),
                other => format!("{} {} {}", other.as_str(), r.scope_key, hash8(&r.hash)),
            },
            hash: hash8(&r.hash).to_string(),
            tier: r.scope_kind.as_str().to_string(),
            origin: r.origin.as_str().to_string(),
            outcome: "not tried: out of time".into(),
            address: None,
            gate: None,
            ms: 0,
        });
        let i = rows.len() - 1;
        if Instant::now() >= deadline {
            continue;
        }
        let Ok(parsed) = Recipe::from_json(&r.recipe_json) else {
            rows[i].outcome = "stored recipe is unreadable".into();
            continue;
        };
        let slice = (Instant::now() + RECIPE_SLICE).min(deadline);
        let outcome = recipe::run(&parsed, &snap, env.fetcher(), slice).await;
        if outcome == RecipeOutcome::NeedsLivePage {
            rows[i].outcome = "not tried: needs a page action, and the one live page load was not available".into();
            live.push((i, r, parsed));
        } else {
            take(&mut rows, &mut produced, &mut cands, i, r, &snap, outcome);
        }
        rows[i].ms = ms(t1);
    }
    steps.push(Step { name: "Recipes".into(), ms: ms(t), note: format!("{} tried", ranked.len()) });

    // b2. The one live page load, with the gate's share of the budget kept.
    let mut live_facts: Option<GateFacts> = None;
    let mut live_tried = false;
    let room = left();
    if let (Some((_, first, _)), true) = (live.first(), room >= LIVE_MIN_LEFT + GATE_RESERVE) {
        let t = Instant::now();
        let actions = Recipe::from_json(&first.recipe_json).map(|p| recipe::live_actions(&p)).unwrap_or_default();
        let key = recipe::action_key(&actions);
        let (cap_secs, allowance) = env.live_limits();
        let secs = (room - GATE_RESERVE).as_secs().min(cap_secs).max(1);
        live_tried = true;
        let wait_until = (Instant::now() + Duration::from_secs(secs) + allowance).min(deadline);
        let got = tokio::time::timeout_at(tokio::time::Instant::from_std(wait_until), env.live_snapshot(url, &actions, secs)).await.ok().flatten();
        match got {
            None => ladder.live = Some(LiveRow { run: "the browser did not run".into(), actions: Vec::new(), ms: ms(t) }),
            Some((lsnap, run, outcomes)) => {
                ladder.live = Some(LiveRow { run: format!("{run:?}").to_lowercase(), actions: outcomes, ms: ms(t) });
                if run == LiveRun::Complete {
                    live_facts = Some(gate_facts(&lsnap, &domain));
                    for (i, r, parsed) in &live {
                        if recipe::action_key(&recipe::live_actions(parsed)) != key || Instant::now() >= deadline {
                            continue;
                        }
                        let slice = (Instant::now() + RECIPE_SLICE).min(deadline);
                        let outcome = recipe::run_after_live(parsed, &lsnap, env.fetcher(), slice).await;
                        take(&mut rows, &mut produced, &mut cands, *i, r, &lsnap, outcome);
                    }
                } else {
                    for (i, _, parsed) in &live {
                        if recipe::action_key(&recipe::live_actions(parsed)) == key {
                            rows[*i].outcome = format!("the page action could not be done ({})", format!("{run:?}").to_lowercase());
                        }
                    }
                }
            }
        }
        steps.push(Step { name: "Live page load".into(), ms: ms(t), note: ladder.live.as_ref().map(|l| l.run.clone()).unwrap_or_default() });
    }

    // c. The gate, for every candidate.
    let mut facts = gate_facts(&snap, &domain);
    if let Some(lf) = live_facts {
        facts.observed_hosts.extend(lf.observed_hosts);
        facts.ad_media.extend(lf.ad_media);
    }
    let t = Instant::now();
    let (mut verified, mut rejected) = (Vec::new(), Vec::new());
    if !cands.is_empty() {
        let is_ad = |u: &str| env.is_ad(u);
        (verified, rejected) = if env.allow_loopback() {
            repair_gate::verify_allowing_loopback_for_tests(cands, &facts, &is_ad, env.prober(), env.resolver(), deadline).await
        } else {
            repair_gate::verify(cands, &facts, &is_ad, env.prober(), env.resolver(), deadline).await
        };
    }
    for (i, u) in &produced {
        rows[*i].gate = Some(gate_word(u, &verified, &rejected));
    }
    steps.push(Step { name: "Gate".into(), ms: ms(t), note: format!("{} verified, {} rejected", verified.len(), rejected.len()) });
    ladder.recipes = rows;

    // d. The model rung: only when the admin ticked it, no recipe verified, and the cap and breaker allow.
    let mut llm_verified: Vec<Verified> = Vec::new();
    let mut llm_rejected: Vec<repair_gate::Rejected> = Vec::new();
    if use_llm && verified.is_empty() {
        let t = Instant::now();
        let mut row = LlmRow { asked: false, calls: 0, rounds: 0, tokens_in: 0, tokens_out: 0, outcome: String::new() };
        if !ctx.cfg.llm.active() || env.model().is_none() {
            row.outcome = "The model is not set up (repair.llm_enabled and repair.llm_model).".into();
        } else {
            let tried: Vec<String> = produced.iter().map(|(_, u)| u.clone()).collect();
            let mut stats = LlmStats::default();
            let flow = tokio::time::timeout_at(
                tokio::time::Instant::from_std(deadline + GRACE),
                llm_rung(&dry, NO_JOB, "", url, &domain, &snap, &tried, &prior, &CancellationToken::new(), deadline, live_tried, LLM_MAX_ROUNDS, true, &mut stats),
            )
            .await;
            row.asked = stats.calls > 0;
            (row.calls, row.rounds, row.tokens_in, row.tokens_out) = (stats.calls, stats.rounds, stats.tokens_in, stats.tokens_out);
            llm_rejected = std::mem::take(&mut stats.rejected);
            row.outcome = match flow {
                Err(_) => "out of time".to_string(),
                Ok(LlmFlow::Verified(v)) => {
                    llm_verified = v.verified;
                    "the model pointed at a video that verified".to_string()
                }
                Ok(_) if stats.reason.is_some() => format!("the model says nothing can be resolved here ({:?})", stats.reason.unwrap()),
                Ok(_) if stats.calls == 0 => {
                    "the model was not asked: today's call limit is used up, the site's model breaker is open, or too little time was left".to_string()
                }
                Ok(_) => "the model's pointers gave no verified video".to_string(),
            };
        }
        steps.push(Step { name: "Model".into(), ms: ms(t), note: format!("{} call(s)", row.calls) });
        ladder.llm = Some(row);
    }

    // e. Everything the gate looked at: verified in rank order (recipes', else the model's), then the rejected.
    let best: Vec<&Verified> = if verified.is_empty() { llm_verified.iter().collect() } else { verified.iter().collect() };
    ladder.candidates = best.iter().enumerate().map(|(i, v)| verified_row(v, i + 1)).collect();
    ladder.candidates.extend(rejected.iter().chain(llm_rejected.iter()).map(rejected_row));
    let verdict = ladder_verdict(mode, &ladder);
    info!("Test URL: dry run on {host}: {} candidate(s), {} verified", ladder.candidates.len(), best.len());
    Ok(finish(started, host, use_llm, mode, steps, normal, Some(ladder), verdict))
}

/// The daemon's tester behind the admin's «Test URL»: one run at a time in the process.
/// The web server starts before the workers' ladder world exists, so that is set later.
pub struct AdminTester {
    ctx: std::sync::OnceLock<Arc<RepairCtx>>,
    running: Arc<AtomicBool>,
    budget: Duration,
}

impl AdminTester {
    pub fn new() -> Self {
        Self { ctx: std::sync::OnceLock::new(), running: Arc::new(AtomicBool::new(false)), budget: TOTAL_BUDGET }
    }

    /// The ladder's world, once the daemon has built it.
    pub fn set_ctx(&self, ctx: Arc<RepairCtx>) {
        let _ = self.ctx.set(ctx);
    }

    /// Test only: a shorter budget.
    #[cfg(test)]
    pub fn with_budget(mut self, budget: Duration) -> Self {
        self.budget = budget;
        self
    }
}

/// Frees the run lock however the run ends: when its task finishes (the browser work it
/// waited for has ended), or, as a backstop, when it is dropped or panics.
struct RunGuard(Arc<AtomicBool>);

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

impl UrlTester for AdminTester {
    fn test_url<'a>(&'a self, url: &'a str, use_llm: bool) -> TestUrlFuture<'a> {
        Box::pin(async move {
            let Some(ctx) = self.ctx.get().cloned() else {
                return Err(TestUrlError::Refused("the daemon is still starting".into()));
            };
            if self.running.swap(true, Ordering::SeqCst) {
                return Err(TestUrlError::Busy);
            }
            let guard = RunGuard(self.running.clone());
            let (url, budget) = (url.to_string(), self.budget);
            // The run is its own task and holds the lock to its end: a closed tab does not
            // stop it, so a new run cannot start a second browser beside a running one.
            let run = tokio::spawn(async move {
                let _guard = guard;
                let deadline = Instant::now() + budget;
                tokio::time::timeout(budget + BACKSTOP, dry_run(&ctx, &url, use_llm, deadline)).await
            });
            match run.await {
                Ok(Ok(Ok(report))) => serde_json::to_value(report).map_err(|_| TestUrlError::Refused("the report could not be written".into())),
                Ok(Ok(Err(why))) => Err(TestUrlError::Refused(why)),
                Ok(Err(_)) => Err(TestUrlError::Refused("the test took too long".into())),
                Err(_) => Err(TestUrlError::Refused("the test ended abnormally".into())),
            }
        })
    }
}
