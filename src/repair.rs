//! The repair ladder: recipes (plan P10.11a-c), then a model that only points (P10.12).
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
use omni_broadcast::card::{self, ImageEvidence, ImageSource};
use omni_broadcast::repair::JobKind;
use omni_broadcast::pipeline::{BroadcastEngine, DeliveryMode, LeaseLost, RunOutcome};
use omni_broadcast::repair_gate::{self, Candidate, GateFacts, Prober, Resolver, ToolProber, Verified};
use omni_browser::recipe::{self, Fetcher, LiveAction, Recipe, RecipeOutcome};
use omni_browser::repair_llm::{self, Assessment, Reason, RoundNote};
use omni_browser::sniffer::{classify_live_run, LiveRun};
use omni_browser::{HttpFetcher, PageSnapshot, StreamSniffer};
use omni_core::config::RepairMode;
use omni_core::models::{Job, JobStage};
use omni_core::recipe_score::{Event, InsertOutcome, NewRecipe, Origin, RankedRecipe, ScopeKind};
use omni_email::llm::{LlmClient, Usage};
use omni_core::repository::{AttemptEnd, RepairAttempt, Repository};

/// The browser user agent the download of a repaired video presents.
const BROWSER_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";
/// One recipe may not use more than this of the budget.
const RECIPE_SLICE: Duration = Duration::from_secs(10);
/// The page look the ladder may ask for when the sniff left no page text.
const DIAGNOSE_SECS: u64 = 30;
/// Safety net around steps a-d: they end at the deadline by themselves.
const GRACE: Duration = Duration::from_secs(2);
/// A live page load (P10.11c) starts only with at least this much budget left.
const LIVE_MIN_LEFT: Duration = Duration::from_secs(30);
/// The most one live page load may take.
const LIVE_SECS: u64 = 40;
/// Budget the gate keeps for itself: a live load never takes it.
const GATE_RESERVE: Duration = Duration::from_secs(20);
/// Time past a live run's own seconds that its browser launch may take.
const LIVE_LAUNCH_ALLOWANCE: Duration = Duration::from_secs(5);
/// Largest `candidates_json` of one attempt.
const CANDIDATES_JSON_MAX: usize = 32 * 1024;
/// Downloads tried for one job, however many candidates verified.
const MAX_DOWNLOADS: usize = 2;
/// The time allowed to resolve a host just before the download.
const CONNECT_CHECK: Duration = Duration::from_secs(5);
/// The model rung starts a round only with the gate's reserve and this much more left.
const LLM_MIN_ROUND: Duration = Duration::from_secs(15);
/// Rounds one ladder run may ask the model.
const LLM_MAX_ROUNDS: usize = 3;
/// The same page bundle is not sent again within this time.
const LLM_RESEND_HOURS: i64 = 24;
/// After this many misses on a site within 24 h the model rung closes for it for 24 h.
const LLM_BREAKER_MISSES: u32 = 3;

/// What `run_job` does with the ladder's answer.
#[derive(Debug, PartialEq, Eq)]
pub enum Disposition {
    /// A verified video is held: park the job for MCR, no site-health record.
    Held,
    /// The job is no longer ours: write nothing more.
    Lost,
    /// End the job (REQUIRES_REVIEW) with this code and message instead of the one it had.
    Finish { code: ErrorCode, message: String },
    /// The job ends as it would have, and its message gains this (the model's reason, in Greek).
    Annotate(String),
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
        LadderResult::Unresolvable { reason } if mode == RepairMode::Hold => Disposition::Annotate(reason.greek().to_string()),
        LadderResult::Exhausted
        | LadderResult::ShadowFound { .. }
        | LadderResult::NothingVerified
        | LadderResult::Unresolvable { .. } => Disposition::Unchanged,
    }
}

/// `message` with the model's reason added (what MCR reads on the card).
pub fn annotate(message: &str, reason_el: &str) -> String {
    if message.trim().is_empty() {
        format!("Η αυτόματη επισκευή: {reason_el}")
    } else {
        format!("{message} (Η αυτόματη επισκευή: {reason_el})")
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
    /// The model said the page has nothing to resolve (DRM, login, a click, no media data)
    /// and nothing else verified.
    Unresolvable { reason: Reason },
}

/// Addresses the normal path already tried, and videos MCR already rejected.
#[derive(Debug, Clone, Default)]
pub struct Prior {
    /// A candidate equal to one of these is dropped, unless its referer differs.
    pub tried: Vec<Tried>,
    /// Addresses of videos MCR rejected for this job: never offered again.
    pub rejected_by_mcr: Vec<String>,
    /// `urlnorm::skip_pattern`s MCR called a wrong video on this site (30 days):
    /// a candidate with one is dropped before the gate, with no recipe event.
    pub skips: Vec<String>,
}

impl Prior {
    /// MCR called this address a wrong video on this site.
    fn skipped(&self, url: &str) -> bool {
        omni_core::urlnorm::skip_pattern(url).is_some_and(|p| self.skips.contains(&p))
    }
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
    pub llm: LlmCfg,
}

/// The model rung's settings (`repair.llm_*`). Off unless enabled AND a model is named.
#[derive(Debug, Clone)]
pub struct LlmCfg {
    pub enabled: bool,
    pub model: Option<String>,
    /// One model call may take this long.
    pub timeout: Duration,
    pub max_calls_per_day: u32,
}

impl Default for LlmCfg {
    fn default() -> Self {
        Self { enabled: false, model: None, timeout: Duration::from_secs(60), max_calls_per_day: 15 }
    }
}

impl LlmCfg {
    /// The rung runs only with both set; it never falls back to the mail model.
    pub fn active(&self) -> bool {
        self.enabled && self.model.as_deref().is_some_and(|m| !m.trim().is_empty())
    }
}

/// The model the repair rung asks. It returns the JSON answer and the tokens used.
#[async_trait]
pub trait RepairModel: Send + Sync {
    async fn ask(&self, system: &str, user: &str, schema: &serde_json::Value) -> Result<(serde_json::Value, Usage)>;
}

/// The real model: the OpenAI-compatible client of the mail assist (same endpoint, key and
/// auth) with the repair model name, and the contact-detail redaction the assist applies to
/// an online provider.
pub struct LlmModel {
    client: LlmClient,
    max_tokens: u32,
    redact: bool,
}

impl LlmModel {
    pub fn new(client: LlmClient, max_tokens: u32) -> Self {
        let redact = client.is_online();
        Self { client, max_tokens, redact }
    }

    /// Test only: force the redaction on or off whatever the endpoint is.
    #[cfg(test)]
    fn with_redaction(mut self, on: bool) -> Self {
        self.redact = on;
        self
    }
}

#[async_trait]
impl RepairModel for LlmModel {
    async fn ask(&self, system: &str, user: &str, schema: &serde_json::Value) -> Result<(serde_json::Value, Usage)> {
        // Page text may hold an address or a phone number: online, they stay home.
        if self.redact {
            let clean = omni_email::assist::redact_contacts(user);
            self.client.chat_json_usage(system, &clean, schema, self.max_tokens).await
        } else {
            self.client.chat_json_usage(system, user, schema, self.max_tokens).await
        }
    }
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
    /// One live page load that performs a recipe's actions (P10.11c): the page
    /// afterwards, what the run can say about the recipe, and what each action
    /// did. `None` when no live load ran (the browser did not start). The
    /// default sees nothing.
    async fn live_snapshot(&self, _url: &str, _actions: &[LiveAction], _secs: u64) -> Option<(PageSnapshot, LiveRun, Vec<String>)> {
        None
    }
    /// Test only: the longest live run, and how long past it the ladder waits.
    fn live_limits(&self) -> (u64, Duration) {
        (LIVE_SECS, LIVE_LAUNCH_ALLOWANCE)
    }
    /// The model of the LLM rung (P10.12); `None`: there is none.
    fn model(&self) -> Option<&dyn RepairModel> {
        None
    }
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
    /// What the approval card's picture comparison needs (P10.11b): ffmpeg and
    /// a source of article pictures. `None`: the card gets no picture.
    fn card_tools(&self) -> Option<(std::path::PathBuf, &dyn ImageSource)> {
        None
    }
    /// The time the card step may take (not part of the repair budget).
    fn card_cap(&self) -> Duration {
        card::CARD_CAP
    }
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
    images: HttpImages,
    ffmpeg: std::path::PathBuf,
    prober: ToolProber,
    resolver: repair_gate::SystemResolver,
    model: Option<LlmModel>,
}

/// Article pictures over the same public-address rule as a recipe fetch, up to 2 MiB.
pub struct HttpImages(pub HttpFetcher);

#[async_trait]
impl ImageSource for HttpImages {
    async fn get_image(&self, url: &str, deadline: Instant) -> Result<Vec<u8>> {
        self.0.get_image_bytes(url, deadline).await
    }
}

impl RealEnv {
    pub fn new(
        engine: Arc<BroadcastEngine>,
        ffprobe: std::path::PathBuf,
        ytdl: std::path::PathBuf,
        ffmpeg: std::path::PathBuf,
    ) -> Self {
        Self {
            engine,
            fetcher: HttpFetcher::new(),
            images: HttpImages(HttpFetcher::new()),
            ffmpeg,
            prober: ToolProber::new(ffprobe, Some(ytdl)),
            resolver: repair_gate::SystemResolver,
            model: None,
        }
    }

    /// The model the LLM rung asks.
    pub fn with_model(mut self, model: LlmModel) -> Self {
        self.model = Some(model);
        self
    }
}

#[async_trait]
impl Env for RealEnv {
    async fn diagnose(&self, url: &str, secs: u64) -> Option<PageSnapshot> {
        let url = url.to_string();
        let (snap, diag) = detached(async move { StreamSniffer::diagnose(&url, secs).await }).await?;
        diag.complete.then_some(snap)
    }
    async fn live_snapshot(&self, url: &str, actions: &[LiveAction], secs: u64) -> Option<(PageSnapshot, LiveRun, Vec<String>)> {
        let (url, acts) = (url.to_string(), actions.to_vec());
        let (snap, diag) = detached(async move { StreamSniffer::diagnose_live(&url, secs, &acts).await }).await?;
        live_result(snap, &diag, actions)
    }
    fn model(&self) -> Option<&dyn RepairModel> {
        self.model.as_ref().map(|m| m as &dyn RepairModel)
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
    fn card_tools(&self) -> Option<(std::path::PathBuf, &dyn ImageSource)> {
        Some((self.ffmpeg.clone(), &self.images))
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

/// A finished live run as the ladder reads it. `None` when the browser never
/// started (no action has an outcome): no live load ran, so none is recorded.
fn live_result(snap: PageSnapshot, diag: &omni_browser::evidence::Diagnostic, actions: &[LiveAction]) -> Option<(PageSnapshot, LiveRun, Vec<String>)> {
    if diag.live_actions.is_empty() {
        return None;
    }
    Some((snap, classify_live_run(actions, diag), diag.live_actions.clone()))
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
    // The model breaker (P10.12) closes only the model rung, inside the ladder:
    // the recipe rung still runs for a site whose breaker is open.
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
    let prior = Prior { tried, rejected_by_mcr: rejected_before(&ctx.repo, job)?, skips: skips_of_site(&ctx.repo, job, orig_url)? };
    Some(run(ctx, job, owner, orig_url, snapshot.clone(), &prior, cancel).await)
}

/// The wrong-video patterns MCR set for this job's site. `None` when the store
/// cannot say: better no repair than a wrong video offered again.
fn skips_of_site(repo: &Repository, job: &Job, orig_url: &str) -> Option<Vec<String>> {
    let Some(domain) = omni_core::urlnorm::registrable_domain(orig_url) else { return Some(Vec::new()) };
    match repo.repair_skip_patterns(&domain, Utc::now()) {
        Ok(v) => Some(v),
        Err(e) => {
            warn!("Job #{}: the repair skip list could not be read, no repair: {e:#}", job.id);
            None
        }
    }
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
///
/// An entry a stored recipe produced also carries that recipe's `recipe_id` and
/// full `recipe_hash` (from `recipes`, address -> (id, hash)), so «Επόμενη
/// επιλογή» can name the recipe behind a later candidate (P10.13).
fn verified_json(list: &[Verified], recipes: &HashMap<String, (i64, String)>) -> String {
    let mut items: Vec<serde_json::Value> = list
        .iter()
        .map(|v| {
            let link = recipes.get(&v.candidate.url);
            serde_json::json!({
                "url": v.candidate.url,
                "referer": v.candidate.referer,
                "origin": v.candidate.origin,
                "recipe_id": link.map(|l| l.0),
                "recipe_hash": link.map(|l| l.1.as_str()),
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
        /// The page the recipes ran on (the card picture comes from it).
        snap: Box<PageSnapshot>,
        /// A live page load ran: "recipes_live:" and what each action did.
        live: Option<String>,
        /// A live page load was tried (it may have failed): the one live load is spent.
        live_tried: bool,
    },
}

/// One recipe that produced an address.
struct Ran {
    id: i64,
    url: String,
}

/// What the recipes have produced so far.
#[derive(Default)]
struct Acc {
    cands: Vec<Candidate>,
    by_url: HashMap<String, (i64, String)>,
    ran: Vec<Ran>,
    /// The budget cut the search short.
    cut: bool,
}

/// Take one recipe's outcome on `page` (the passive page, or the one the live
/// load produced) into the candidates, or record its miss.
fn absorb(
    ctx: &RepairCtx,
    acc: &mut Acc,
    prior: &Prior,
    r: &RankedRecipe,
    page: &PageSnapshot,
    outcome: RecipeOutcome,
    domain: &str,
    deadline: Instant,
) {
    match outcome {
        RecipeOutcome::Media { url, referer } => {
            let url = omni_core::net::parse_public_url(&url).map(|(u, _)| u.to_string()).unwrap_or(url);
            let already = prior.tried.iter().any(|t| equal_address(&t.url, &url) && t.referer == referer)
                || prior.rejected_by_mcr.iter().any(|u| equal_address(u, &url));
            // Not the recipe fault: the normal path tried this address, or
            // MCR rejected it before (the desk records that itself), or called
            // it a wrong video on this site: dropped before the gate.
            if already || prior.skipped(&url) {
                return;
            }
            acc.by_url.entry(url.clone()).or_insert_with(|| (r.id, r.hash.clone()));
            if !acc.cands.iter().any(|c| c.url == url && c.referer == referer) {
                let (position, in_article) = player_for(page, &url);
                acc.cands.push(Candidate {
                    url: url.clone(),
                    referer,
                    origin: format!("recipe:{}", hash8(&r.hash)),
                    position,
                    in_article,
                });
            }
            acc.ran.push(Ran { id: r.id, url });
        }
        // A live recipe is never passed here; nothing to learn from it.
        RecipeOutcome::NeedsLivePage => {}
        // Not the recipe's fault: another evaluation held the one permit, or time ran out.
        RecipeOutcome::Rejected(why) if why == "busy" => {}
        RecipeOutcome::Rejected(why) if why == "deadline exceeded" => acc.cut = acc.cut || Instant::now() >= deadline,
        RecipeOutcome::NoValue { .. } | RecipeOutcome::Rejected(_) => no_candidate(ctx, r.id, domain),
    }
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
    let mut acc = Acc::default();
    // Recipes that need a click or a wait: held back for the one live load.
    let mut live: Vec<(&RankedRecipe, Recipe)> = Vec::new();
    for r in &ranked {
        if lost(ctx, job.id, owner, cancel) {
            return Phase::Lost;
        }
        if Instant::now() >= deadline {
            acc.cut = true;
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
        if outcome == RecipeOutcome::NeedsLivePage {
            live.push((r, parsed));
            continue;
        }
        absorb(ctx, &mut acc, prior, r, &snap, outcome, domain, deadline);
    }

    // b2. Recipes with live actions: at most one live page load per ladder
    // run, and only with enough budget left for it. The first such recipe
    // (best first) sets the actions; every other with the same actions is read
    // from the same page, the rest wait for another run.
    let mut live_note: Option<String> = None;
    let mut live_facts: Option<GateFacts> = None;
    let mut live_tried = false;
    let left = deadline.saturating_duration_since(Instant::now());
    // The gate keeps its own share of the budget: the live load never takes it.
    if let (Some((first, _)), true) = (live.first(), left >= LIVE_MIN_LEFT + GATE_RESERVE) {
        if lost(ctx, job.id, owner, cancel) {
            return Phase::Lost;
        }
        let actions = Recipe::from_json(&first.recipe_json).map(|p| recipe::live_actions(&p)).unwrap_or_default();
        let key = recipe::action_key(&actions);
        let (cap_secs, allowance) = env.live_limits();
        let secs = (left - GATE_RESERVE).as_secs().min(cap_secs).max(1);
        live_tried = true;
        // The run ends itself at `secs` after its browser started; the launch is
        // outside that, so the wait is bounded here too, never by the ladder deadline.
        let wait_until = (Instant::now() + Duration::from_secs(secs) + allowance).min(deadline);
        let seen = tokio::select! {
            r = tokio::time::timeout_at(tokio::time::Instant::from_std(wait_until), env.live_snapshot(orig_url, &actions, secs)) => r.ok().flatten(),
            _ = cancel.cancelled() => return Phase::Lost,
        };
        if lost(ctx, job.id, owner, cancel) {
            return Phase::Lost;
        }
        match seen {
            None => {}
            Some((lsnap, run, outcomes)) => {
                live_note = Some(format!("recipes_live:{}", outcomes.join(",")));
                info!("Job #{}: repair live load on {}: {run:?}", job.id, domain);
                // Only a complete run can produce candidates. A missing click
                // target on an otherwise clean page is a conclusive miss for every
                // recipe with these actions; anything else says nothing about them.
                if run == LiveRun::Missing {
                    for (r, parsed) in &live {
                        if recipe::action_key(&recipe::live_actions(parsed)) == key {
                            no_candidate(ctx, r.id, domain);
                        }
                    }
                }
                if run == LiveRun::Complete {
                    live_facts = Some(gate_facts(&lsnap, domain));
                    for (r, parsed) in &live {
                        if recipe::action_key(&recipe::live_actions(parsed)) != key {
                            continue;
                        }
                        if lost(ctx, job.id, owner, cancel) {
                            return Phase::Lost;
                        }
                        if Instant::now() >= deadline {
                            acc.cut = true;
                            break;
                        }
                        let slice = (Instant::now() + RECIPE_SLICE).min(deadline);
                        let outcome = tokio::select! {
                            o = recipe::run_after_live(parsed, &lsnap, env.fetcher(), slice) => o,
                            _ = cancel.cancelled() => return Phase::Lost,
                        };
                        absorb(ctx, &mut acc, prior, r, &lsnap, outcome, domain, deadline);
                    }
                }
            }
        }
    }
    let Acc { cands, by_url, ran, mut cut } = acc;
    if cands.is_empty() {
        return Phase::Found { verified: Vec::new(), recipes: by_url, cut, snap: Box::new(snap), live: live_note, live_tried };
    }
    if lost(ctx, job.id, owner, cancel) {
        return Phase::Lost;
    }

    // c. The gate, for every candidate.
    let mut facts = gate_facts(&snap, domain);
    if let Some(lf) = live_facts {
        // What the page requested after the live actions counts as observed.
        facts.observed_hosts.extend(lf.observed_hosts);
        facts.ad_media.extend(lf.ad_media);
    }
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
    Phase::Found { verified, recipes: by_url, cut, snap: Box::new(snap), live: live_note, live_tried }
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
    let result = ladder(ctx, job, owner, orig_url, snapshot, prior, cancel, started, attempt, &mut end).await;
    let outcome = match &result {
        LadderResult::Held { .. } => "held".to_string(),
        LadderResult::ShadowFound { .. } => "shadow_found".to_string(),
        LadderResult::NothingVerified => end.outcome.take().unwrap_or_else(|| "nothing_verified".to_string()),
        LadderResult::Exhausted => "exhausted".to_string(),
        LadderResult::Lost => "lost".to_string(),
        LadderResult::Unresolvable { .. } => "not_resolvable".to_string(),
    };
    end.outcome = Some(outcome);
    end.duration_ms = Some(started.elapsed().as_millis() as i64);
    if let Some(id) = attempt {
        // Bookkeeping of the ladder's own table, not a write on the job.
        if let Err(e) = ctx.repo.finish_attempt(id, &end, Utc::now()) {
            warn!("Job #{}: the repair attempt could not be closed: {e:#}", job.id);
        }
    }
    // A model rung that found nothing is a miss; three on one site within 24 h
    // close the rung for it for 24 h (a canary pass or a new yt-dlp build reopens it).
    let found = matches!(result, LadderResult::Held { .. } | LadderResult::ShadowFound { .. } | LadderResult::Lost);
    if end.llm_calls > 0 && !found {
        if let Some(domain) = end.domain.as_deref() {
            let now = Utc::now();
            match ctx.repo.llm_misses_since(domain, now - chrono::Duration::hours(LLM_RESEND_HOURS)) {
                Ok(n) if n >= LLM_BREAKER_MISSES => {
                    if let Err(e) = ctx.repo.open_llm_breaker(domain, now + chrono::Duration::hours(LLM_RESEND_HOURS)) {
                        warn!("Job #{}: the repair model breaker could not be opened: {e:#}", job.id);
                    } else {
                        info!("Job #{}: the repair model is not asked about {domain} for 24 h ({n} misses)", job.id);
                    }
                }
                Ok(_) => {}
                Err(e) => warn!("Job #{}: the repair model misses could not be counted: {e:#}", job.id),
            }
        }
    }
    result
}

/// The held video's directory: the parent of the held row's file.
fn held_dir_of(repo: &Repository, job_id: i64) -> Option<std::path::PathBuf> {
    let held = repo.waiting_held(job_id).ok()??;
    std::path::Path::new(&held.path).parent().map(std::path::Path::to_path_buf)
}

/// Right after a download was held: the article's picture and the preview frame
/// closest to it, for MCR's card (P10.11b). Best effort and bounded by its own
/// 45 s cap (not part of the repair budget): any failure leaves the hold exactly
/// as it was. Writes only while the lease is still ours.
async fn card_step(
    ctx: &RepairCtx,
    job_id: i64,
    owner: &str,
    attempt: Option<i64>,
    snap: &PageSnapshot,
    facts: &GateFacts,
    duration: f64,
    cancel: &CancellationToken,
) {
    let Some((ffmpeg, source)) = ctx.env.card_tools() else { return };
    let candidates = omni_browser::evidence::article_images(snap);
    if candidates.is_empty() || lost(ctx, job_id, owner, cancel) {
        return;
    }
    let Some(dir) = held_dir_of(&ctx.repo, job_id) else { return };
    let work = card::card_evidence_capped(&ffmpeg, &dir, &candidates, facts, duration, source, ctx.env.card_cap());
    let ev = tokio::select! {
        ev = work => ev,
        _ = cancel.cancelled() => {
            // Stopped part-way: nothing of it is recorded, so nothing of it stays.
            card::remove_dir_retrying(&dir.join(card::WORK_DIR)).await;
            for f in [omni_broadcast::preview::ARTICLE_FILE, omni_broadcast::preview::FRAME_FILE] {
                let _ = std::fs::remove_file(dir.join(f));
            }
            return;
        }
    };
    record_card(ctx, job_id, owner, attempt, &dir, &ev);
}

/// Write what the card step found on the held attempt, if the lease is ours.
/// Without it the picture files are removed too: nothing of a lost job stays.
fn record_card(ctx: &RepairCtx, job_id: i64, owner: &str, attempt: Option<i64>, dir: &std::path::Path, ev: &ImageEvidence) {
    if ev.host.is_none() {
        return;
    }
    let written = attempt.is_some_and(|id| {
        ctx.repo
            .record_card_evidence(id, job_id, owner, ev.distance.map(i64::from), ev.frame_secs, ev.host.as_deref())
            .unwrap_or_else(|e| {
                warn!("Job #{job_id}: the card evidence could not be recorded: {e:#}");
                false
            })
    });
    if !written {
        for f in [omni_broadcast::preview::ARTICLE_FILE, omni_broadcast::preview::FRAME_FILE] {
            let _ = std::fs::remove_file(dir.join(f));
        }
    }
}

/// What the model rung cost and concluded (kept outside the rung so a timeout cannot lose it).
#[derive(Default)]
struct LlmStats {
    rounds: usize,
    calls: i64,
    tokens_in: i64,
    tokens_out: i64,
    /// The model's verdict when it said nothing can be resolved.
    reason: Option<Reason>,
    /// The budget ran out between rounds.
    cut: bool,
}

/// An address the model pointed at: the hash its method name carries (of the recipe
/// scope when one reproduced it, else of the pointer) and that recipe.
struct LlmFound {
    hash: String,
    recipe: Option<Recipe>,
}

struct LlmVerified {
    verified: Vec<Verified>,
    found: HashMap<String, LlmFound>,
    /// The gate's facts, with the hosts a click page showed merged in.
    facts: GateFacts,
}

enum LlmFlow {
    Lost,
    Verified(LlmVerified),
    Nothing,
}

/// Store the model's recipe (scope: this site) as a candidate. `None`: the store
/// refused it or failed; the hold stands either way.
fn store_llm_recipe(ctx: &RepairCtx, domain: &str, snap: &PageSnapshot, rec: &Recipe) -> Option<(i64, String)> {
    let hash = recipe::scope_hash("site", domain, rec);
    let new = NewRecipe {
        scope_kind: ScopeKind::Site,
        scope_key: domain.to_string(),
        family: omni_browser::player_family(snap).map(str::to_string),
        recipe_json: rec.canonical_json(),
        hash: hash.clone(),
        steps_hash: recipe::steps_hash(rec),
        dsl_version: rec.dsl_version,
        origin: Origin::Llm,
    };
    match ctx.repo.insert_recipe(&new, Utc::now()) {
        // A merge can land on another recipe (a player recipe with the same steps): the
        // hash is the stored recipe's own, so MCR's decision finds it.
        Ok(InsertOutcome::Inserted(id) | InsertOutcome::Merged(id) | InsertOutcome::Revived(id)) => match ctx.repo.recipe_hash_of(id) {
            Ok(Some(stored)) => Some((id, stored)),
            _ => None,
        },
        Ok(InsertOutcome::Refused(_) | InsertOutcome::OverCap) => {
            info!("the model's recipe for {domain} was not stored (refused or the store is full)");
            None
        }
        Err(e) => {
            warn!("the model's recipe for {domain} could not be stored: {e:#}");
            None
        }
    }
}

/// The reason line of one gate rejection: the failing check, hosts only.
fn rejection_note(round: usize, r: &repair_gate::Rejected) -> RoundNote {
    let why = r
        .checks
        .iter()
        .rev()
        .find(|c| !c.ok)
        .map(|c| format!("{}: {}", c.name, c.detail))
        .unwrap_or_else(|| "not checked".to_string());
    RoundNote { what: format!("round {round}: an address you pointed at was rejected"), host: Some(host_of(&r.candidate.url)), reason: why }
}

/// The model rung (P10.12): up to three rounds in which the model points at page
/// items, our code computes the addresses and the gate checks them. Never creates
/// a URL from the answer's text; stops at the first verified candidate.
async fn llm_rung(
    ctx: &RepairCtx,
    job: &Job,
    owner: &str,
    orig_url: &str,
    domain: &str,
    snap: &PageSnapshot,
    recipe_urls: &[String],
    prior: &Prior,
    cancel: &CancellationToken,
    deadline: Instant,
    live_used: bool,
    stats: &mut LlmStats,
) -> LlmFlow {
    let job_id = job.id;
    let cfg = &ctx.cfg.llm;
    let Some(model) = ctx.env.model().filter(|_| cfg.active()) else {
        return LlmFlow::Nothing;
    };
    let now = Utc::now();
    match ctx.repo.llm_breaker_open(domain, now) {
        Ok(false) => {}
        Ok(true) => {
            info!("Job #{job_id}: the repair model is shut for {domain} (breaker open)");
            return LlmFlow::Nothing;
        }
        Err(e) => {
            warn!("Job #{job_id}: the repair model breaker could not be read, no model call: {e:#}");
            return LlmFlow::Nothing;
        }
    }
    if deadline.saturating_duration_since(Instant::now()) < GATE_RESERVE + LLM_MIN_ROUND {
        return LlmFlow::Nothing;
    }
    let mut notes = vec![RoundNote { what: "recipes".into(), host: None, reason: "no recipe produced a verified video".into() }];
    // The notes are not part of the hash: the same page is the same bundle.
    let first = repair_llm::build_bundle(snap, &notes);
    match ctx.repo.llm_bundle_sent_since(&first.hash, now - chrono::Duration::hours(LLM_RESEND_HOURS)) {
        Ok(false) => {}
        Ok(true) => {
            info!("Job #{job_id}: the same page bundle went to the repair model in the last 24 h, not sent again");
            return LlmFlow::Nothing;
        }
        Err(e) => {
            warn!("Job #{job_id}: the repair model history could not be read, no model call: {e:#}");
            return LlmFlow::Nothing;
        }
    }

    let schema = repair_llm::answer_schema();
    let mut first = Some(first);
    let mut live_snap: Option<PageSnapshot> = None;
    let mut facts = gate_facts(snap, domain);
    let mut tried: Vec<String> = recipe_urls.to_vec();
    // The click this run performed, and whether its one live load is spent.
    let mut clicked: Option<String> = None;
    let mut live_spent = live_used;
    let mut click_attempted = false;

    for round in 1..=LLM_MAX_ROUNDS {
        if lost(ctx, job_id, owner, cancel) {
            return LlmFlow::Lost;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left < GATE_RESERVE + LLM_MIN_ROUND {
            stats.cut = true;
            break;
        }
        // The day's cap is reserved before the call, atomically, over all jobs.
        let day = chrono::Local::now().format("%Y-%m-%d").to_string();
        match ctx.repo.reserve_llm_call(&day, cfg.max_calls_per_day) {
            Ok(true) => {}
            Ok(false) => {
                info!("Job #{job_id}: the repair model's calls for today are used up");
                break;
            }
            Err(e) => {
                warn!("Job #{job_id}: the repair model call could not be reserved: {e:#}");
                break;
            }
        }
        stats.calls += 1;
        stats.rounds = round;
        let bundle = match first.take() {
            Some(b) => {
                if let Err(e) = ctx.repo.record_llm_bundle(&b.hash, domain, Utc::now()) {
                    warn!("Job #{job_id}: the repair model bundle could not be recorded: {e:#}");
                }
                b
            }
            None => repair_llm::build_bundle(live_snap.as_ref().unwrap_or(snap), &notes),
        };
        let timeout = cfg.timeout.min(left - GATE_RESERVE);
        let asked = tokio::select! {
            r = tokio::time::timeout(timeout, model.ask(repair_llm::REPAIR_PROMPT_V1, &bundle.text, &schema)) => r,
            _ = cancel.cancelled() => return LlmFlow::Lost,
        };
        let (answer, usage) = match asked {
            Ok(Ok(x)) => x,
            Ok(Err(e)) => {
                warn!("Job #{job_id}: the repair model call failed");
                tracing::debug!("Job #{job_id}: repair model failure: {e:#}");
                break;
            }
            Err(_) => {
                warn!("Job #{job_id}: the repair model call timed out");
                break;
            }
        };
        stats.tokens_in += i64::from(usage.prompt_tokens);
        stats.tokens_out += i64::from(usage.completion_tokens);
        if let Err(e) = ctx.repo.add_llm_tokens(&day, usage.prompt_tokens, usage.completion_tokens) {
            warn!("Job #{job_id}: the repair model tokens could not be counted: {e:#}");
        }
        if lost(ctx, job_id, owner, cancel) {
            return LlmFlow::Lost;
        }

        let cur: &PageSnapshot = live_snap.as_ref().unwrap_or(snap);
        let resolved = repair_llm::resolve_answer(&answer, &bundle, cur);
        // "Needs a click" with a click to make and the one live load free is a click round,
        // not an end; every other not_resolvable ends the rung with its reason.
        let not_resolvable = resolved.assessment == Assessment::NotResolvable;
        if not_resolvable {
            let click_round = resolved.reason == Some(Reason::NeedsClick)
                && resolved.click.is_some()
                && !live_spent
                && !click_attempted
                && round < LLM_MAX_ROUNDS;
            if !click_round {
                stats.reason = resolved.reason;
                break;
            }
        }
        for why in &resolved.dropped {
            tracing::debug!("Job #{job_id}: repair model answer, round {round}: {why}");
        }
        // Steps MCR rejected on this site stay barred: nothing built on them is offered.
        let mut recipe_in = resolved.recipe.clone();
        let barred = recipe_in
            .as_ref()
            .is_some_and(|r| ctx.repo.recipe_steps_barred(&recipe::steps_hash(r), domain).unwrap_or(false));
        if barred {
            recipe_in = None;
            notes.push(RoundNote { what: format!("round {round}"), host: None, reason: "that recipe was rejected by MCR on this site".into() });
        }
        let page_ref = cur.final_url.clone().unwrap_or_else(|| cur.page_url.clone());
        let fresh: Vec<&repair_llm::LlmCandidate> = if barred || not_resolvable {
            Vec::new()
        } else {
            resolved
                .candidates
                .iter()
                .filter(|c| {
                    !(prior.tried.iter().any(|t| equal_address(&t.url, &c.url) && t.referer.as_deref() == Some(page_ref.as_str()))
                        || prior.rejected_by_mcr.iter().any(|u| equal_address(u, &c.url))
                        || prior.skipped(&c.url)
                        || tried.iter().any(|u| equal_address(u, &c.url)))
                })
                .collect()
        };

        if !fresh.is_empty() {
            // Each address is read again by the recipe the model offered, if it gave one;
            // only an exact match keeps the recipe, and only for that address.
            let mut found: HashMap<String, LlmFound> = HashMap::new();
            let mut cands: Vec<Candidate> = Vec::new();
            for c in &fresh {
                let mut reproduced: Option<Recipe> = None;
                if let Some(rec) = &recipe_in {
                    let acts = recipe::live_actions(rec);
                    // No actions: the recipe has to work on the page as every later run
                    // sees it. The click it names: on the page after exactly that click.
                    let on: Option<&PageSnapshot> = if acts.is_empty() {
                        Some(snap)
                    } else if clicked
                        .as_ref()
                        .is_some_and(|css| recipe::action_key(&acts) == recipe::action_key(&[LiveAction::Click(css.clone())]))
                    {
                        Some(cur)
                    } else {
                        None
                    };
                    if let Some(on) = on {
                        let slice = (Instant::now() + RECIPE_SLICE).min(deadline);
                        let ok = tokio::select! {
                            ok = repair_llm::recipe_reproduces(rec, on, &c.url, ctx.env.fetcher(), slice) => ok,
                            _ = cancel.cancelled() => return LlmFlow::Lost,
                        };
                        // The recipe must depend on the page: without the item the address came
                        // from, it must no longer give the address. (If another item carries the
                        // same address the recipe is not kept: one place to depend on.) Not
                        // checkable on a page other than the one the bundle was built from: not kept.
                        let page_dependent = ok && std::ptr::eq(on, cur) && match bundle.item(&c.from) {
                            Some(item) => {
                                let blank = repair_llm::blank_item(cur, item);
                                let still = tokio::select! {
                                    v = repair_llm::recipe_reproduces(rec, &blank, &c.url, ctx.env.fetcher(), slice) => v,
                                    _ = cancel.cancelled() => return LlmFlow::Lost,
                                };
                                // And it must follow the item: with the address stem changed in the
                                // item it must give the changed address (nothing is written into it).
                                !still && tokio::select! {
                                    v = repair_llm::follow_check(rec, cur, item, &c.url, ctx.env.fetcher(), slice) => v,
                                    _ = cancel.cancelled() => return LlmFlow::Lost,
                                }
                            }
                            None => false,
                        };
                        if page_dependent {
                            reproduced = Some(rec.clone());
                        }
                    }
                }
                let hash = match &reproduced {
                    Some(r) => recipe::scope_hash("site", domain, r),
                    None => repair_llm::candidate_hash(c),
                };
                let (position, in_article) = player_for(cur, &c.url);
                cands.push(Candidate {
                    url: c.url.clone(),
                    referer: Some(page_ref.clone()),
                    origin: format!("llm:{}", hash8(&hash)),
                    position,
                    in_article,
                });
                found.insert(c.url.clone(), LlmFound { hash, recipe: reproduced });
            }
            tried.extend(cands.iter().map(|c| c.url.clone()));
            let is_ad = |u: &str| ctx.env.is_ad(u);
            let gate = async {
                if ctx.env.allow_loopback() {
                    repair_gate::verify_allowing_loopback_for_tests(cands, &facts, &is_ad, ctx.env.prober(), ctx.env.resolver(), deadline).await
                } else {
                    repair_gate::verify(cands, &facts, &is_ad, ctx.env.prober(), ctx.env.resolver(), deadline).await
                }
            };
            let (verified, rejected) = tokio::select! {
                r = gate => r,
                _ = cancel.cancelled() => return LlmFlow::Lost,
            };
            if lost(ctx, job_id, owner, cancel) {
                return LlmFlow::Lost;
            }
            if !verified.is_empty() {
                found.retain(|u, _| verified.iter().any(|v| &v.candidate.url == u));
                return LlmFlow::Verified(LlmVerified { verified, found, facts });
            }
            notes.extend(rejected.iter().map(|r| rejection_note(round, r)));
        }

        // A click: once per ladder run, with the budget for it and for the gate.
        let click = resolved.click.clone().filter(|_| !live_spent && !click_attempted && round < LLM_MAX_ROUNDS);
        if let Some(css) = click {
            let left = deadline.saturating_duration_since(Instant::now());
            if left >= LIVE_MIN_LEFT + GATE_RESERVE {
                click_attempted = true;
                let actions = vec![LiveAction::Click(css.clone())];
                let (cap_secs, allowance) = ctx.env.live_limits();
                let secs = (left - GATE_RESERVE).as_secs().min(cap_secs).max(1);
                let wait_until = (Instant::now() + Duration::from_secs(secs) + allowance).min(deadline);
                let seen = tokio::select! {
                    r = tokio::time::timeout_at(tokio::time::Instant::from_std(wait_until), ctx.env.live_snapshot(orig_url, &actions, secs)) => r.ok().flatten(),
                    _ = cancel.cancelled() => return LlmFlow::Lost,
                };
                if lost(ctx, job_id, owner, cancel) {
                    return LlmFlow::Lost;
                }
                match seen {
                    Some((lsnap, run, _)) => {
                        live_spent = true;
                        info!("Job #{job_id}: repair live load on {domain} for the model's click: {run:?}");
                        if run == LiveRun::Complete {
                            let lf = gate_facts(&lsnap, domain);
                            facts.observed_hosts.extend(lf.observed_hosts);
                            facts.ad_media.extend(lf.ad_media);
                            clicked = Some(css);
                            live_snap = Some(lsnap);
                            notes.push(RoundNote { what: format!("round {round}"), host: None, reason: "your click was done; the page below is the page after it".into() });
                        } else {
                            notes.push(RoundNote { what: format!("round {round}"), host: None, reason: "your click could not be done on the page".into() });
                        }
                    }
                    None => notes.push(RoundNote { what: format!("round {round}"), host: None, reason: "the click could not be tried".into() }),
                }
                continue;
            }
        }
        if fresh.is_empty() {
            // Nothing new and nothing to click: another round would get the same answer.
            if not_resolvable {
                stats.reason = resolved.reason;
            }
            break;
        }
    }
    LlmFlow::Nothing
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
    attempt: Option<i64>,
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
    let (mut verified, recipes, mut cut, snap, live_used) = match phase {
        Phase::Lost => return LadderResult::Lost,
        Phase::Exhausted => return LadderResult::Exhausted,
        Phase::NoPage => {
            end.outcome = Some("no_page".into());
            return LadderResult::NothingVerified;
        }
        Phase::Found { verified, recipes, cut, snap, live, live_tried } => {
            let used = live_tried;
            if let Some(note) = live {
                end.rung = Some(note);
            }
            (verified, recipes, cut, snap, used)
        }
    };
    if lost(ctx, job_id, owner, cancel) {
        return LadderResult::Lost;
    }

    // d. What was found, in rank order, for MCR and for what comes next.
    let mut facts = gate_facts(&snap, &domain);
    end.expected_secs = facts.expected_secs;

    // d2. The recipes found nothing that verified: ask the model to point (P10.12).
    let mut llm_found: HashMap<String, LlmFound> = HashMap::new();
    let mut unresolvable: Option<Reason> = None;
    if verified.is_empty() && ctx.cfg.llm.active() {
        let tried: Vec<String> = recipes.keys().cloned().collect();
        let mut stats = LlmStats::default();
        let flow = tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline + GRACE),
            llm_rung(ctx, job, owner, orig_url, &domain, &snap, &tried, prior, cancel, deadline, live_used, &mut stats),
        )
        .await;
        end.llm_calls = stats.calls;
        end.tokens_in = stats.tokens_in;
        end.tokens_out = stats.tokens_out;
        let verdict = match flow {
            Err(_) => {
                cut = true;
                "out of time"
            }
            Ok(LlmFlow::Lost) => return LadderResult::Lost,
            Ok(LlmFlow::Verified(v)) => {
                verified = v.verified;
                llm_found = v.found;
                facts = v.facts;
                "found"
            }
            Ok(LlmFlow::Nothing) => {
                cut = cut || stats.cut;
                unresolvable = stats.reason;
                if stats.reason.is_some() { "not resolvable" } else { "nothing" }
            }
        };
        if stats.calls > 0 {
            info!(
                "Job #{job_id}: repair model rung: {verdict}, {} round(s), {} call(s), {} tokens in, {} out",
                stats.rounds, stats.calls, stats.tokens_in, stats.tokens_out
            );
        }
        if lost(ctx, job_id, owner, cancel) {
            return LadderResult::Lost;
        }
    }
    end.candidates_json = Some(verified_json(&verified, &recipes));
    let Some(best) = verified.first() else {
        if ctx.cfg.mode == RepairMode::Shadow {
            note(ctx, job_id, "INFO", "Η αυτόματη επισκευή δεν βρήκε επαληθευμένο βίντεο");
        }
        if let Some(reason) = unresolvable {
            return LadderResult::Unresolvable { reason };
        }
        return if cut { LadderResult::Exhausted } else { LadderResult::NothingVerified };
    };
    let (top_id, top_hash) = recipes.get(&best.candidate.url).cloned().unwrap_or((0, String::new()));
    if llm_found.contains_key(&best.candidate.url) {
        end.rung = Some("llm".into());
    }

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
        // The method names the recipe behind the candidate being downloaded now.
        let (mut recipe_id, mut recipe_hash) = recipes.get(&v.candidate.url).cloned().unwrap_or((0, String::new()));
        let from_llm = llm_found.get(&v.candidate.url);
        let method = match from_llm {
            Some(l) => format!("repair:llm:{}", hash8(&l.hash)),
            None => format!("repair:recipe:{}", hash8(&recipe_hash)),
        };
        let url = match hold_one(ctx, job, owner, &v.candidate.url, v.candidate.referer.clone(), &method, mode, cancel).await {
            HoldStep::Held { url } => url,
            HoldStep::Skipped | HoldStep::Failed => continue,
            HoldStep::Lost => return LadderResult::Lost,
            HoldStep::Stop => return LadderResult::NothingVerified,
        };
        let host = host_of(&v.candidate.url);
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
        // The model's pointer is stored as a recipe only now that a video is
        // really held, and only when the recipe reproduces the address.
        if let Some(l) = from_llm {
            end.rung = Some("llm".into());
            if lost(ctx, job_id, owner, cancel) {
                return LadderResult::Lost;
            }
            recipe_hash = l.hash.clone();
            (recipe_id, end.recipe_hash) = match &l.recipe {
                Some(rec) => match store_llm_recipe(ctx, &domain, &snap, rec) {
                    Some((id, hash)) => (id, Some(hash)),
                    None => (0, None),
                },
                None => (0, None),
            };
        } else {
            end.recipe_hash = (!recipe_hash.is_empty()).then(|| recipe_hash.clone());
        }
        // What MCR decision will find: the candidate actually held.
        end.recipe_id = (recipe_id != 0).then_some(recipe_id);
        end.held_url = Some(url);
        end.next_index = index as i64;
        card_step(ctx, job_id, owner, attempt, &snap, &facts, v.duration_secs, cancel).await;
        return LadderResult::Held { recipe_hash };
    }
    // No download worked: the job is as it was, without the repair mark.
    restore(ctx);
    end.outcome = Some("download_failed".into());
    LadderResult::NothingVerified
}

/// What the next-candidate path (P10.13) came to.
#[derive(Debug, PartialEq, Eq)]
pub enum NextResult {
    /// The next verified video is held for MCR: park the job.
    Held,
    /// The job is no longer this worker's: write nothing more.
    Lost,
    /// It was not held (the download failed, or the address was rejected or skipped
    /// meanwhile): the job ends in review; no further candidate is tried by itself.
    Failed,
}

/// The Greek message of a job whose next choice did not come.
pub const NEXT_FAILED_EL: &str = "Η επόμενη επιλογή δεν κατέβηκε";

/// The message of a job whose pending next choice found repair switched off.
pub const REPAIR_OFF_EL: &str = "Η αυτόματη επισκευή είναι απενεργοποιημένη";

/// MCR's pending «Επόμενη επιλογή» for this job: its latest repair attempt is a
/// `next` one nobody has finished.
pub fn open_next(repo: &Repository, job_id: i64) -> Result<Option<RepairAttempt>> {
    Ok(repo.latest_attempt(job_id)?.filter(|a| a.rung.as_deref() == Some("next") && a.outcome.is_none()))
}

/// «Επόμενη επιλογή» (P10.13): hold the candidate MCR's choice named, from the
/// list the earlier run already verified. No page look, no recipe, no model, no
/// normal download: only the hold step the ladder uses. A lost lease writes
/// nothing (the attempt stays open: a new lease does it again, a retry drops it).
pub async fn run_next(
    ctx: &RepairCtx,
    job: &Job,
    owner: &str,
    evidence_dir: &std::path::Path,
    attempt: &RepairAttempt,
    cancel: &CancellationToken,
) -> NextResult {
    let started = Instant::now();
    let mut end = AttemptEnd {
        rung: Some("next".into()),
        domain: attempt.domain.clone(),
        candidates_json: attempt.candidates_json.clone(),
        next_index: attempt.next_index,
        ..Default::default()
    };
    let result = next_step(ctx, job, owner, evidence_dir, attempt, cancel, &mut end).await;
    if result != NextResult::Lost {
        end.outcome = Some(match result {
            NextResult::Held => "held".to_string(),
            _ => end.outcome.take().unwrap_or_else(|| "next_failed".to_string()),
        });
        end.duration_ms = Some(started.elapsed().as_millis() as i64);
        // Bookkeeping of the ladder's own table, not a write on the job.
        if let Err(e) = ctx.repo.finish_attempt(attempt.id, &end, Utc::now()) {
            warn!("Job #{}: the next-choice attempt could not be closed: {e:#}", job.id);
        }
    }
    result
}

/// Is this address one MCR rejected for the job, or called a wrong video on its
/// site? `None` when the store cannot say.
fn next_barred(repo: &Repository, job_id: i64, domain: &str, url: &str) -> Option<bool> {
    let rejected = repo.rejected_held_urls(job_id).ok()?;
    let skips = repo.repair_skip_patterns(domain, Utc::now()).ok()?;
    Some(rejected.iter().any(|u| equal_address(u, url)) || omni_core::urlnorm::skip_pattern(url).is_some_and(|p| skips.contains(&p)))
}

async fn next_step(
    ctx: &RepairCtx,
    job: &Job,
    owner: &str,
    evidence_dir: &std::path::Path,
    attempt: &RepairAttempt,
    cancel: &CancellationToken,
    end: &mut AttemptEnd,
) -> NextResult {
    let job_id = job.id;
    let restore = || {
        let _ = ctx.repo.set_leased_extraction_method(job_id, owner, job.extraction_method.as_deref());
    };
    let list: Vec<serde_json::Value> = attempt.candidates_json.as_deref().and_then(|t| serde_json::from_str(t).ok()).unwrap_or_default();
    let entry = usize::try_from(attempt.next_index).ok().and_then(|i| list.get(i));
    let (Some(entry), Some(domain)) = (entry, attempt.domain.as_deref()) else {
        warn!("Job #{job_id}: the next choice names no candidate");
        return NextResult::Failed;
    };
    let Some(url) = entry.get("url").and_then(serde_json::Value::as_str) else { return NextResult::Failed };
    let referer = entry.get("referer").and_then(serde_json::Value::as_str).map(str::to_string);
    let origin = entry.get("origin").and_then(serde_json::Value::as_str).unwrap_or("");
    let duration = entry.get("duration_secs").and_then(serde_json::Value::as_f64).unwrap_or(0.0);
    if lost(ctx, job_id, owner, cancel) {
        return NextResult::Lost;
    }
    // MCR may have rejected or skipped this address since the choice was made.
    let Some(barred) = next_barred(&ctx.repo, job_id, domain, url) else {
        warn!("Job #{job_id}: the repair history could not be read, the next choice is not downloaded");
        return NextResult::Failed;
    };
    if barred {
        note(ctx, job_id, "WARN", &format!("Η επόμενη επιλογή ({}) έχει απορριφθεί από το MCR, δεν κατεβαίνει", host_of(url)));
        end.outcome = Some("next_skipped".into());
        return NextResult::Failed;
    }
    // The method names the recipe behind the candidate, as the ladder's does.
    let tag = |t: &str| t.chars().filter(char::is_ascii_alphanumeric).collect::<String>();
    let method = match origin.split_once(':') {
        Some(("recipe", h)) => format!("repair:recipe:{}", tag(h)),
        Some(("llm", h)) => format!("repair:llm:{}", tag(h)),
        _ => "repair:next".to_string(),
    };
    let mode = DeliveryMode::Hold { hold_hours: ctx.cfg.hold_hours, held_max_bytes: ctx.cfg.held_max_bytes };
    let held_url = match hold_one(ctx, job, owner, url, referer, &method, mode, cancel).await {
        HoldStep::Held { url } => url,
        HoldStep::Lost => return NextResult::Lost,
        HoldStep::Skipped | HoldStep::Failed => {
            restore();
            return NextResult::Failed;
        }
        HoldStep::Stop => return NextResult::Failed,
    };
    note(
        ctx,
        job_id,
        "INFO",
        &format!("Η επόμενη επιλογή: {} ({}, {origin}), περιμένει έγκριση από το MCR", host_of(url), mm_ss(duration)),
    );
    end.recipe_id = entry.get("recipe_id").and_then(serde_json::Value::as_i64).filter(|id| *id != 0);
    end.recipe_hash = entry.get("recipe_hash").and_then(serde_json::Value::as_str).map(str::to_string);
    end.held_url = Some(held_url);
    // The length the page stated is the earlier run's; the page's own evidence, when saved, says it again.
    end.expected_secs = ctx.repo.latest_held_attempt(job_id).ok().flatten().and_then(|a| a.expected_secs);
    // The card's picture: only from the page evidence the job saved (no page look here).
    let saved = tokio::fs::read_to_string(evidence_dir.join(format!("{job_id}.json"))).await.ok();
    if let Some(snap) = saved.and_then(|t| serde_json::from_str::<PageSnapshot>(&t).ok()) {
        let facts = gate_facts(&snap, domain);
        end.expected_secs = facts.expected_secs.or(end.expected_secs);
        card_step(ctx, job_id, owner, Some(attempt.id), &snap, &facts, duration, cancel).await;
    }
    NextResult::Held
}

/// What one held download of a verified address came to.
enum HoldStep {
    /// Held, from this address; the caller records it.
    Held { url: String },
    /// Not tried: the address or its host is no longer one to use.
    Skipped,
    /// The download failed (noted on the job).
    Failed,
    /// The job is no longer this worker's: write nothing more.
    Lost,
    /// Stop here: the repair mark could not be written, or the pipeline delivered.
    Stop,
}

/// Download one verified address through the unchanged pipeline, held for MCR
/// (the ladder's hold step and the next-candidate path share it). DNS can change
/// between the gate and now: the host must still resolve to public addresses
/// only, and the pipeline gets the gate's own address. `method` is written as the
/// job's `extraction_method` first; on a failure it is put back.
async fn hold_one(
    ctx: &RepairCtx,
    job: &Job,
    owner: &str,
    candidate_url: &str,
    referer: Option<String>,
    method: &str,
    mode: DeliveryMode,
    cancel: &CancellationToken,
) -> HoldStep {
    let job_id = job.id;
    let restore = || {
        let _ = ctx.repo.set_leased_extraction_method(job_id, owner, job.extraction_method.as_deref());
    };
    if lost(ctx, job_id, owner, cancel) {
        return HoldStep::Lost;
    }
    let host = host_of(candidate_url);
    let Ok((url, host_name)) = omni_core::net::parse_public_url(candidate_url) else {
        return HoldStep::Skipped;
    };
    // A rooted name ("host.") could miss an address pin; never used.
    if url.host_str().is_some_and(|h| h.ends_with('.')) {
        return HoldStep::Skipped;
    }
    let port = url.port_or_known_default().unwrap_or(443);
    if host_name.parse::<std::net::IpAddr>().is_err() && !ctx.env.host_is_public(&host_name, port).await {
        note(ctx, job_id, "WARN", &format!("Η αυτόματη επισκευή: το {host} δεν δείχνει πια σε δημόσια διεύθυνση, παραλείφθηκε"));
        return HoldStep::Skipped;
    }
    if lost(ctx, job_id, owner, cancel) {
        return HoldStep::Lost;
    }
    match ctx.repo.set_leased_extraction_method(job_id, owner, Some(method)) {
        Ok(true) => {}
        Ok(false) => return HoldStep::Lost,
        Err(e) => {
            warn!("Job #{job_id}: the repair method could not be noted: {e:#}");
            restore();
            return HoldStep::Stop;
        }
    }
    let mut repaired = job.clone();
    repaired.url = url.to_string();
    match ctx.env.download(repaired, owner, referer, cancel, mode).await {
        Ok(RunOutcome::Held) => HoldStep::Held { url: url.to_string() },
        Ok(RunOutcome::Delivered) => {
            // Hold never delivers; if it ever did, say so loudly and stop.
            warn!("Job #{job_id}: a held repair download reported a delivery; stopping the ladder");
            note(ctx, job_id, "ERROR", "Η αυτόματη επισκευή: απροσδόκητη παράδοση, διακόπηκε");
            restore();
            HoldStep::Stop
        }
        Err(e) => {
            if e.downcast_ref::<LeaseLost>().is_some() || lost(ctx, job_id, owner, cancel) {
                return HoldStep::Lost;
            }
            let code: ErrorCode = crate::classify_pipeline_error(&e);
            info!("Job #{job_id}: repair download from {host} failed: {code}");
            note(ctx, job_id, "WARN", &format!("Η αυτόματη επισκευή: η λήψη από {host} απέτυχε ({})", code.as_str()));
            HoldStep::Failed
        }
    }
}

#[cfg(test)]
#[path = "repair_tests.rs"]
mod tests;
