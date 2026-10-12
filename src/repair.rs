//! The repair ladder, rung one: recipes (plan P10.11a).
//!
//! Runs only after a web job's normal path has finished in failure AND the
//! page showed a video that was not resolved (`VIDEO_UNRESOLVED`). It tries
//! the learned and shipped recipes on the page the browser saw, checks every
//! candidate with the repair gate, and then, by `repair.mode`, either only
//! records what it would have used (shadow) or downloads the best verified
//! candidate through the unchanged pipeline in `DeliveryMode::Hold`, so MCR
//! approves it before anything reaches the watchfolder. Never creates a
//! job, never sets a status (the worker does), writes nothing once the lease
//! is gone, and spends a bounded time (`repair.budget_secs`) before the download.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use omni_broadcast::errors::ErrorCode;
use omni_broadcast::repair::JobKind;
use omni_broadcast::pipeline::{BroadcastEngine, DeliveryMode, LeaseLost, RunOutcome};
use omni_broadcast::repair_gate::{self, Candidate, GateFacts, Prober, Resolver, ToolProber, Verified};
use omni_browser::recipe::{self, Fetcher, Recipe, RecipeOutcome};
use omni_browser::{HttpFetcher, PageSnapshot, StreamSniffer};
use omni_core::config::RepairMode;
use omni_core::models::{Job, JobStage};
use omni_core::recipe_score::Event;
use omni_core::repository::{AttemptEnd, Repository};

/// The browser user agent the download of a repaired video presents.
const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
/// One recipe may not use more than this of the budget.
const RECIPE_SLICE: Duration = Duration::from_secs(10);
/// The page look the ladder may ask for when the sniff left no page text.
const DIAGNOSE_SECS: u64 = 30;
/// Safety net around steps a-d: they end at the deadline by themselves.
const GRACE: Duration = Duration::from_secs(2);
/// Largest `candidates_json` of one attempt.
const CANDIDATES_JSON_MAX: usize = 32 * 1024;
/// Downloads tried for one job, however many candidates verified.
const MAX_DOWNLOADS: usize = 2;
/// The time allowed to resolve a host just before the download.
const CONNECT_CHECK: Duration = Duration::from_secs(5);

/// What `run_job` does with the ladder's answer.
#[derive(Debug, PartialEq, Eq)]
pub enum Disposition {
    /// A verified video is held: park the job for MCR, no site-health record.
    Held,
    /// The job is no longer ours: write nothing more.
    Lost,
    /// End the job (REQUIRES_REVIEW) with this code and message instead of the one it had.
    Finish { code: ErrorCode, message: String },
    /// The job ends exactly as it would have without the ladder.
    Unchanged,
}

/// The ladder's answer in `mode` as what the worker does. Only Hold changes how a
/// job ends; shadow never does, not even for a spent budget.
pub fn disposition(result: &LadderResult, mode: RepairMode, budget: Duration) -> Disposition {
    match result {
        LadderResult::Held { .. } => Disposition::Held,
        LadderResult::Lost => Disposition::Lost,
        LadderResult::Exhausted if mode == RepairMode::Hold => Disposition::Finish {
            code: ErrorCode::RepairExhausted,
            message: format!("Η αυτόματη επισκευή δεν ολοκληρώθηκε μέσα σε {} δευτερόλεπτα", budget.as_secs()),
        },
        LadderResult::Exhausted | LadderResult::ShadowFound { .. } | LadderResult::NothingVerified => Disposition::Unchanged,
    }
}

/// The code site health counts a finished job under: a repair that ran out of
/// time was an unresolved video.
pub fn site_code(code: ErrorCode) -> ErrorCode {
    if code == ErrorCode::RepairExhausted {
        ErrorCode::VideoUnresolved
    } else {
        code
    }
}

/// What the ladder settled on.
#[derive(Debug)]
pub enum LadderResult {
    /// The best verified video was downloaded, checked and is held for MCR.
    Held { recipe_hash: String },
    /// Shadow mode: this is what would have been used. Nothing was downloaded.
    ShadowFound { verified: Vec<Verified> },
    /// No candidate passed the gate, or the downloads of those that did failed.
    NothingVerified,
    /// The time budget ran out before anything verified.
    Exhausted,
    /// The job is no longer this worker's (cancelled, lease gone). Write nothing.
    Lost,
}

/// Addresses the normal path already tried, and videos MCR already rejected.
#[derive(Debug, Clone, Default)]
pub struct Prior {
    /// A candidate equal to one of these is dropped, unless its referer differs.
    pub tried: Vec<Tried>,
    /// Addresses of videos MCR rejected for this job: never offered again.
    pub rejected_by_mcr: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Tried {
    pub url: String,
    pub referer: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RepairCfg {
    pub mode: RepairMode,
    pub budget: Duration,
    pub hold_hours: i64,
    pub held_max_bytes: u64,
}

/// Run `work` to its own end even when the caller stops waiting (the ladder
/// budget, a cancel). A dropped browser run would leave Chrome to chromiumoxide
/// kill-on-drop with its profile removed under it; left to finish, the run
/// stops at its own budget and closes the browser and removes the profile itself.
async fn detached<T: Send + 'static>(work: impl std::future::Future<Output = T> + Send + 'static) -> Option<T> {
    tokio::spawn(work).await.ok()
}

/// What the ladder needs from the outside world; the real one is [`RealEnv`].
#[async_trait]
pub trait Env: Send + Sync {
    /// One diagnostic look at the page; `Some` only when the run was complete.
    async fn diagnose(&self, url: &str, secs: u64) -> Option<PageSnapshot>;
    fn fetcher(&self) -> &dyn Fetcher;
    fn prober(&self) -> &dyn Prober;
    fn resolver(&self) -> &dyn Resolver;
    fn is_ad(&self, url: &str) -> bool;
    /// Test only: accept a loopback answer from the resolver.
    fn allow_loopback(&self) -> bool {
        false
    }
    /// Just before a download: does `host` still resolve to public addresses only?
    async fn host_is_public(&self, host: &str, port: u16) -> bool;
    /// The unchanged pipeline, for one verified address.
    async fn download(
        &self,
        job: Job,
        owner: &str,
        referer: Option<String>,
        cancel: &CancellationToken,
        mode: DeliveryMode,
    ) -> Result<RunOutcome>;
}

/// Everything a worker needs to run the ladder.
pub struct RepairCtx {
    pub repo: Repository,
    pub cfg: RepairCfg,
    pub env: Arc<dyn Env>,
}

/// The real world: the headless browser, the network, the tools, the pipeline.
pub struct RealEnv {
    engine: Arc<BroadcastEngine>,
    fetcher: HttpFetcher,
    prober: ToolProber,
    resolver: repair_gate::SystemResolver,
}

impl RealEnv {
    pub fn new(engine: Arc<BroadcastEngine>, ffprobe: std::path::PathBuf, ytdl: std::path::PathBuf) -> Self {
        Self {
            engine,
            fetcher: HttpFetcher::new(),
            prober: ToolProber::new(ffprobe, Some(ytdl)),
            resolver: repair_gate::SystemResolver,
        }
    }
}

#[async_trait]
impl Env for RealEnv {
    async fn diagnose(&self, url: &str, secs: u64) -> Option<PageSnapshot> {
        let url = url.to_string();
        let (snap, diag) = detached(async move { StreamSniffer::diagnose(&url, secs).await }).await?;
        diag.complete.then_some(snap)
    }
    fn fetcher(&self) -> &dyn Fetcher {
        &self.fetcher
    }
    fn prober(&self) -> &dyn Prober {
        &self.prober
    }
    fn resolver(&self) -> &dyn Resolver {
        &self.resolver
    }
    fn is_ad(&self, url: &str) -> bool {
        StreamSniffer::is_ad_or_tracking(url)
    }
    async fn host_is_public(&self, host: &str, port: u16) -> bool {
        omni_core::net::resolve_public(host, port, CONNECT_CHECK).await.is_ok()
    }
    async fn download(
        &self,
        job: Job,
        owner: &str,
        referer: Option<String>,
        cancel: &CancellationToken,
        mode: DeliveryMode,
    ) -> Result<RunOutcome> {
        self.engine
            .process_job_mode(job, owner, referer.as_deref(), Some(BROWSER_UA), None, Some(cancel.clone()), mode)
            .await
    }
}

/// What kind of job this is, for [`omni_broadcast::repair::eligible`].
pub fn job_kind(job: &Job, url: &str) -> JobKind {
    use omni_browser::agent::{ComputerUseAgentPlaceholder, FileLockerResolver};
    if url.starts_with("attachment://") {
        return JobKind::Attachment;
    }
    if job.extraction_method.as_deref() == Some("locker") || ComputerUseAgentPlaceholder::new(None, None).can_handle(url) {
        return JobKind::Locker;
    }
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return JobKind::Other;
    }
    if omni_broadcast::downloader::is_video_platform(url) {
        return JobKind::Platform;
    }
    if job.parent_job_id.is_some() {
        return JobKind::ArticleSibling;
    }
    JobKind::WebPage
}

impl Tried {
    /// Every address the normal path already used for this job: the link, the
    /// video the page scan named, and the streams the sniffer found.
    pub fn of(
        job: &Job,
        orig_url: &str,
        page_referer: Option<&str>,
        sniffed: &[String],
        sniff_referer: Option<&str>,
    ) -> Vec<Tried> {
        let mut tried = vec![Tried { url: orig_url.to_string(), referer: page_referer.map(str::to_string) }];
        if job.url != orig_url {
            tried.push(Tried { url: job.url.clone(), referer: page_referer.map(str::to_string) });
        }
        tried.extend(sniffed.iter().map(|u| Tried { url: u.clone(), referer: sniff_referer.map(str::to_string) }));
        tried
    }
}

/// Every address MCR rejected for this job in any earlier run (an expired or
/// approved hold is not among them). Never offered again. `None` when the
/// store cannot say: better no repair than a wrong video held again.
fn rejected_before(repo: &Repository, job: &Job) -> Option<Vec<String>> {
    match repo.rejected_held_urls(job.id) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!("Job #{}: the repair history could not be read, no repair: {e:#}", job.id);
            None
        }
    }
}

/// The one place the worker asks for repair, after a web job's normal path
/// ended in failure with `code`. `None` (and nothing touched, nothing
/// recorded) unless the failure is an unresolved video on a web page, repair
/// is on, and MCR has not already rejected this video.
pub async fn after_failure(
    ctx: &RepairCtx,
    job: &Job,
    owner: &str,
    orig_url: &str,
    code: ErrorCode,
    snapshot: &Option<PageSnapshot>,
    tried: Vec<Tried>,
    cancel: &CancellationToken,
) -> Option<LadderResult> {
    // The site breaker arrives with P10.12; until then it is never open.
    let site_breaker_open = false;
    // A video MCR rejected is dropped from the candidates inside the ladder
    // (the top candidate is not known before it runs).
    let already_rejected_same = false;
    if !omni_broadcast::repair::eligible(ctx.cfg.mode, code, job_kind(job, orig_url), site_breaker_open, already_rejected_same)
        || cancel.is_cancelled()
        || !crate::still_ours(&ctx.repo, job.id, owner)
    {
        return None;
    }
    let prior = Prior { tried, rejected_by_mcr: rejected_before(&ctx.repo, job)? };
    Some(run(ctx, job, owner, orig_url, snapshot.clone(), &prior, cancel).await)
}

fn lost(ctx: &RepairCtx, job_id: i64, owner: &str, cancel: &CancellationToken) -> bool {
    cancel.is_cancelled() || !crate::still_ours(&ctx.repo, job_id, owner)
}

fn note(ctx: &RepairCtx, job_id: i64, level: &str, text: &str) {
    if let Err(e) = ctx.repo.record_event(job_id, level, Some(JobStage::Extract), text) {
        warn!("Job #{job_id}: could not note the repair step: {e:#}");
    }
}

fn host_of(url: &str) -> String {
    omni_core::net::parse_public_url(url).map(|(_, h)| h).unwrap_or_else(|_| "?".to_string())
}

fn mm_ss(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    format!("{:02}:{:02}", s / 60, s % 60)
}

fn hash8(hash: &str) -> &str {
    hash.get(..8).unwrap_or(hash)
}

/// The gate's view of the page: every observed host, plus `host:port` for each
/// network address with an explicit non-default port (the gate allows such a
/// port only when the page itself used that host and port).
fn gate_facts(snap: &PageSnapshot, domain: &str) -> GateFacts {
    let mut hosts = snap.observed_hosts();
    for n in &snap.network {
        if let Ok((url, _)) = omni_core::net::parse_public_url(&n.url) {
            if let (Some(port), Some(host)) = (url.port(), url.host_str()) {
                hosts.insert(format!("{}:{port}", host.to_ascii_lowercase().trim_end_matches('.')));
            }
        }
    }
    GateFacts {
        observed_hosts: hosts,
        page_domain: Some(domain.to_string()),
        ad_media: snap.ad_media.iter().cloned().collect(),
        expected_secs: omni_browser::evidence::expected_duration_secs(snap),
    }
}

/// The player block that contains this address, if any: its place in the
/// page and whether it sits in the article body.
fn player_for(snap: &PageSnapshot, url: &str) -> (Option<usize>, Option<bool>) {
    let tail = url.split_once(':').map(|(_, t)| t).unwrap_or(url);
    for block in &snap.players {
        let html = block.html.replace("\\/", "/").replace("&amp;", "&");
        if html.contains(url) || html.contains(tail) {
            return (Some(block.index), Some(block.in_article));
        }
    }
    (None, None)
}

/// The verified candidates in rank order, with every check, as stored on the attempt.
fn verified_json(list: &[Verified]) -> String {
    let mut items: Vec<serde_json::Value> = list
        .iter()
        .map(|v| {
            serde_json::json!({
                "url": v.candidate.url,
                "referer": v.candidate.referer,
                "origin": v.candidate.origin,
                "duration_secs": v.duration_secs,
                "height": v.height,
                "score": v.score,
                "checks": v.checks.iter().map(|c| serde_json::json!({"name": c.name, "ok": c.ok, "detail": c.detail})).collect::<Vec<_>>(),
            })
        })
        .collect();
    loop {
        let text = serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string());
        if text.len() <= CANDIDATES_JSON_MAX || items.is_empty() {
            return text;
        }
        // Rank order is kept: the weakest entries go first.
        items.pop();
    }
}

fn equal_address(a: &str, b: &str) -> bool {
    omni_core::urlnorm::normalize(a) == omni_core::urlnorm::normalize(b)
}

/// What steps a-d found.
enum Phase {
    Lost,
    Exhausted,
    NoPage,
    Found {
        verified: Vec<Verified>,
        /// Address -> the first recipe (id, full hash) that produced it.
        recipes: HashMap<String, (i64, String)>,
        /// The budget cut the search short.
        cut: bool,
    },
}

/// One recipe that produced an address.
struct Ran {
    id: i64,
    url: String,
}

fn no_candidate(ctx: &RepairCtx, id: i64, domain: &str) {
    if let Err(e) = ctx.repo.recipe_event(id, domain, Event::NoCandidate, Utc::now()) {
        warn!("recipe #{id}: could not record the miss on {domain}: {e:#}");
    }
}

/// Steps a-d: the page, the recipes, the gate.
async fn find_and_verify(
    ctx: &RepairCtx,
    job: &Job,
    owner: &str,
    orig_url: &str,
    domain: &str,
    snapshot: Option<PageSnapshot>,
    prior: &Prior,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Phase {
    let env = &ctx.env;
    let tokio_deadline = tokio::time::Instant::from_std(deadline);

    // a. A page with its text. A sniff that found a stream which then
    // failed leaves none: look once more.
    let mut snap = snapshot;
    if snap.as_ref().map_or(true, |s| s.html.is_none()) {
        if lost(ctx, job.id, owner, cancel) {
            return Phase::Lost;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Phase::Exhausted;
        }
        let secs = left.as_secs().clamp(1, DIAGNOSE_SECS);
        let seen = tokio::select! {
            r = tokio::time::timeout_at(tokio_deadline, env.diagnose(orig_url, secs)) => r.ok().flatten(),
            _ = cancel.cancelled() => return Phase::Lost,
        };
        if seen.as_ref().map_or(false, |s| s.html.is_some()) {
            snap = seen;
        }
    }
    let Some(snap) = snap.filter(|s| s.html.is_some()) else {
        return if Instant::now() >= deadline { Phase::Exhausted } else { Phase::NoPage };
    };
    if lost(ctx, job.id, owner, cancel) {
        return Phase::Lost;
    }

    // b. Recipes, best first: this site's, the player family's, the shipped ones.
    let family = omni_browser::player_family(&snap);
    let ranked = match ctx.repo.recipes_for_site(domain, family, Utc::now()) {
        Ok(r) => r,
        Err(e) => {
            warn!("Job #{}: the recipe store could not be read: {e:#}", job.id);
            return Phase::NoPage;
        }
    };
    let mut cands: Vec<Candidate> = Vec::new();
    let mut by_url: HashMap<String, (i64, String)> = HashMap::new();
    let mut ran: Vec<Ran> = Vec::new();
    let mut cut = false;
    for r in &ranked {
        if lost(ctx, job.id, owner, cancel) {
            return Phase::Lost;
        }
        if Instant::now() >= deadline {
            cut = true;
            break;
        }
        let Ok(parsed) = Recipe::from_json(&r.recipe_json) else {
            no_candidate(ctx, r.id, domain);
            continue;
        };
        let slice = (Instant::now() + RECIPE_SLICE).min(deadline);
        let outcome = tokio::select! {
            o = recipe::run(&parsed, &snap, env.fetcher(), slice) => o,
            _ = cancel.cancelled() => return Phase::Lost,
        };
        match outcome {
            RecipeOutcome::Media { url, referer } => {
                let url = omni_core::net::parse_public_url(&url).map(|(u, _)| u.to_string()).unwrap_or(url);
                let already = prior.tried.iter().any(|t| equal_address(&t.url, &url) && t.referer == referer)
                    || prior.rejected_by_mcr.iter().any(|u| equal_address(u, &url));
                // Not the recipe fault: the normal path tried this address, or
                // MCR rejected it before (the desk records that itself).
                if already {
                    continue;
                }
                by_url.entry(url.clone()).or_insert_with(|| (r.id, r.hash.clone()));
                if !cands.iter().any(|c| c.url == url && c.referer == referer) {
                    let (position, in_article) = player_for(&snap, &url);
                    cands.push(Candidate {
                        url: url.clone(),
                        referer,
                        origin: format!("recipe:{}", hash8(&r.hash)),
                        position,
                        in_article,
                    });
                }
                ran.push(Ran { id: r.id, url });
            }
            RecipeOutcome::NeedsLivePage => {}
            // Not the recipe's fault: another evaluation held the one permit, or time ran out.
            RecipeOutcome::Rejected(why) if why == "busy" => {}
            RecipeOutcome::Rejected(why) if why == "deadline exceeded" => cut = cut || Instant::now() >= deadline,
            RecipeOutcome::NoValue { .. } | RecipeOutcome::Rejected(_) => no_candidate(ctx, r.id, domain),
        }
    }
    if cands.is_empty() {
        return Phase::Found { verified: Vec::new(), recipes: by_url, cut };
    }
    if lost(ctx, job.id, owner, cancel) {
        return Phase::Lost;
    }

    // c. The gate, for every candidate.
    let facts = gate_facts(&snap, domain);
    let is_ad = |u: &str| env.is_ad(u);
    let gate = async {
        if env.allow_loopback() {
            repair_gate::verify_allowing_loopback_for_tests(cands, &facts, &is_ad, env.prober(), env.resolver(), deadline).await
        } else {
            repair_gate::verify(cands, &facts, &is_ad, env.prober(), env.resolver(), deadline).await
        }
    };
    let (verified, rejected) = tokio::select! {
        r = gate => r,
        _ = cancel.cancelled() => return Phase::Lost,
    };
    if lost(ctx, job.id, owner, cancel) {
        return Phase::Lost;
    }
    // A recipe whose every address was looked at by the gate and failed has
    // "tried, no verified candidate". A candidate the gate never probed (a
    // better one was accepted first) or that ran out of time says nothing.
    if Instant::now() >= deadline {
        cut = true;
    }
    for r in &ran {
        let verdict = rejected.iter().find(|x| x.candidate.url == r.url);
        if !verified.iter().any(|v| v.candidate.url == r.url) && verdict.is_some_and(|x| x.is_conclusive()) {
            no_candidate(ctx, r.id, domain);
        }
    }
    Phase::Found { verified, recipes: by_url, cut }
}

/// Run the ladder once for a failed web job.
pub async fn run(
    ctx: &RepairCtx,
    job: &Job,
    owner: &str,
    orig_url: &str,
    snapshot: Option<PageSnapshot>,
    prior: &Prior,
    cancel: &CancellationToken,
) -> LadderResult {
    let started = Instant::now();
    let mode = match ctx.cfg.mode {
        RepairMode::Off => return LadderResult::NothingVerified,
        RepairMode::Shadow => "shadow",
        RepairMode::Hold => "hold",
    };
    let attempt = match ctx.repo.start_attempt(job.id, mode, Utc::now()) {
        Ok(id) => Some(id),
        Err(e) => {
            warn!("Job #{}: the repair attempt could not be recorded: {e:#}", job.id);
            None
        }
    };
    let mut end = AttemptEnd { rung: Some("recipes".into()), ..Default::default() };
    let result = ladder(ctx, job, owner, orig_url, snapshot, prior, cancel, started, &mut end).await;
    let outcome = match &result {
        LadderResult::Held { .. } => "held".to_string(),
        LadderResult::ShadowFound { .. } => "shadow_found".to_string(),
        LadderResult::NothingVerified => end.outcome.take().unwrap_or_else(|| "nothing_verified".to_string()),
        LadderResult::Exhausted => "exhausted".to_string(),
        LadderResult::Lost => "lost".to_string(),
    };
    end.outcome = Some(outcome);
    end.duration_ms = Some(started.elapsed().as_millis() as i64);
    if let Some(id) = attempt {
        // Bookkeeping of the ladder's own table, not a write on the job.
        if let Err(e) = ctx.repo.finish_attempt(id, &end, Utc::now()) {
            warn!("Job #{}: the repair attempt could not be closed: {e:#}", job.id);
        }
    }
    result
}

async fn ladder(
    ctx: &RepairCtx,
    job: &Job,
    owner: &str,
    orig_url: &str,
    snapshot: Option<PageSnapshot>,
    prior: &Prior,
    cancel: &CancellationToken,
    started: Instant,
    end: &mut AttemptEnd,
) -> LadderResult {
    let job_id = job.id;
    let deadline = started + ctx.cfg.budget;
    let Some(domain) = omni_core::urlnorm::registrable_domain(orig_url) else {
        end.outcome = Some("no_domain".into());
        return LadderResult::NothingVerified;
    };
    end.domain = Some(domain.clone());
    if lost(ctx, job_id, owner, cancel) {
        return LadderResult::Lost;
    }

    let phase = tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline + GRACE),
        find_and_verify(ctx, job, owner, orig_url, &domain, snapshot, prior, cancel, deadline),
    )
    .await
    .unwrap_or(Phase::Exhausted);
    let (verified, recipes, cut) = match phase {
        Phase::Lost => return LadderResult::Lost,
        Phase::Exhausted => return LadderResult::Exhausted,
        Phase::NoPage => {
            end.outcome = Some("no_page".into());
            return LadderResult::NothingVerified;
        }
        Phase::Found { verified, recipes, cut } => (verified, recipes, cut),
    };
    if lost(ctx, job_id, owner, cancel) {
        return LadderResult::Lost;
    }

    // d. What was found, in rank order, for MCR and for what comes next.
    end.candidates_json = Some(verified_json(&verified));
    let Some(best) = verified.first() else {
        if ctx.cfg.mode == RepairMode::Shadow {
            note(ctx, job_id, "INFO", "Η αυτόματη επισκευή δεν βρήκε επαληθευμένο βίντεο");
        }
        return if cut { LadderResult::Exhausted } else { LadderResult::NothingVerified };
    };
    let (top_id, top_hash) = recipes.get(&best.candidate.url).cloned().unwrap_or((0, String::new()));

    // Shadow: only say what would have been used.
    if ctx.cfg.mode == RepairMode::Shadow {
        end.recipe_id = (top_id != 0).then_some(top_id);
        end.recipe_hash = (!top_hash.is_empty()).then(|| top_hash.clone());
        note(
            ctx,
            job_id,
            "INFO",
            &format!(
                "Η αυτόματη επισκευή θα χρησιμοποιούσε: {} ({}, {})",
                host_of(&best.candidate.url),
                mm_ss(best.duration_secs),
                best.candidate.origin
            ),
        );
        info!("Job #{job_id}: repair (shadow) would use {}", host_of(&best.candidate.url));
        return LadderResult::ShadowFound { verified };
    }

    // Hold: download the best one through the unchanged pipeline, held for MCR.
    let mode = DeliveryMode::Hold { hold_hours: ctx.cfg.hold_hours, held_max_bytes: ctx.cfg.held_max_bytes };
    let restore = |ctx: &RepairCtx| {
        let _ = ctx.repo.set_leased_extraction_method(job_id, owner, job.extraction_method.as_deref());
    };
    for (index, v) in verified.iter().enumerate().take(MAX_DOWNLOADS) {
        if lost(ctx, job_id, owner, cancel) {
            return LadderResult::Lost;
        }
        let host = host_of(&v.candidate.url);
        // DNS can change between the gate and now: the host must still resolve
        // to public addresses only, and the pipeline gets the gate own address.
        let Ok((url, host_name)) = omni_core::net::parse_public_url(&v.candidate.url) else {
            continue;
        };
        // A rooted name ("host.") could miss an address pin; never used.
        if url.host_str().is_some_and(|h| h.ends_with('.')) {
            continue;
        }
        let port = url.port_or_known_default().unwrap_or(443);
        if host_name.parse::<std::net::IpAddr>().is_err() && !ctx.env.host_is_public(&host_name, port).await {
            note(ctx, job_id, "WARN", &format!("Η αυτόματη επισκευή: το {host} δεν δείχνει πια σε δημόσια διεύθυνση, παραλείφθηκε"));
            continue;
        }
        if lost(ctx, job_id, owner, cancel) {
            return LadderResult::Lost;
        }
        // The method names the recipe behind the candidate being downloaded now.
        let (recipe_id, recipe_hash) = recipes.get(&v.candidate.url).cloned().unwrap_or((0, String::new()));
        let method = format!("repair:recipe:{}", hash8(&recipe_hash));
        match ctx.repo.set_leased_extraction_method(job_id, owner, Some(&method)) {
            Ok(true) => {}
            Ok(false) => return LadderResult::Lost,
            Err(e) => {
                warn!("Job #{job_id}: the repair method could not be noted: {e:#}");
                restore(ctx);
                return LadderResult::NothingVerified;
            }
        }
        let mut repaired = job.clone();
        repaired.url = url.to_string();
        match ctx.env.download(repaired, owner, v.candidate.referer.clone(), cancel, mode).await {
            Ok(RunOutcome::Held) => {
                note(
                    ctx,
                    job_id,
                    "INFO",
                    &format!(
                        "Η αυτόματη επισκευή βρήκε βίντεο: {host} ({}, {}), περιμένει έγκριση από το MCR",
                        mm_ss(v.duration_secs),
                        v.candidate.origin
                    ),
                );
                // What MCR decision will find: the candidate actually held.
                end.recipe_id = (recipe_id != 0).then_some(recipe_id);
                end.recipe_hash = (!recipe_hash.is_empty()).then(|| recipe_hash.clone());
                end.held_url = Some(url.to_string());
                end.next_index = index as i64;
                return LadderResult::Held { recipe_hash };
            }
            Ok(RunOutcome::Delivered) => {
                // Hold never delivers; if it ever did, say so loudly and stop.
                warn!("Job #{job_id}: a held repair download reported a delivery; stopping the ladder");
                note(ctx, job_id, "ERROR", "Η αυτόματη επισκευή: απροσδόκητη παράδοση, διακόπηκε");
                restore(ctx);
                return LadderResult::NothingVerified;
            }
            Err(e) => {
                if e.downcast_ref::<LeaseLost>().is_some() || lost(ctx, job_id, owner, cancel) {
                    return LadderResult::Lost;
                }
                let code: ErrorCode = crate::classify_pipeline_error(&e);
                info!("Job #{job_id}: repair download from {host} failed: {code}");
                note(ctx, job_id, "WARN", &format!("Η αυτόματη επισκευή: η λήψη από {host} απέτυχε ({})", code.as_str()));
            }
        }
    }
    // No download worked: the job is as it was, without the repair mark.
    restore(ctx);
    end.outcome = Some("download_failed".into());
    LadderResult::NothingVerified
}

#[cfg(test)]
#[path = "repair_tests.rs"]
mod tests;
