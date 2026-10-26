//! Self-refreshing site canaries (plan P10.4).
//!
//! A few recent articles that gave video, per frequent news site, are
//! re-checked each night and when a site starts failing. A site whose recent
//! articles no longer give video is *confirmed* broken before more
//! journalists' jobs hit it. The pool is drawn from the jobs' own history, so
//! nobody maintains a list; it is not `selfcheck_links`, which also decides
//! whether a yt-dlp update is kept. Nothing here updates yt-dlp: the 03:00
//! task is the only updater, and a canary run never overlaps it.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use tracing::{info, warn};

use omni_core::config::RepairMode;
use omni_core::dependencies::DependencyManager;
use omni_core::health::HealthState;
use omni_core::models::{CompletedPage, SiteCanary};
use omni_core::repository::Repository;
use omni_core::site_health::{self, SiteState};
use omni_web::state::{RunNow, SiteRunner};

use crate::selfcheck::LinkOutcome;

/// Jobs this recent feed the pool.
const POOL_DAYS: i64 = 30;
const PER_DOMAIN: usize = 3;
const MAX_DOMAINS: usize = 30;
/// All the checks of one pass together; the rest wait for the next night.
const NIGHTLY_BUDGET: Duration = Duration::from_secs(30 * 60);
/// One daytime check per site in this time.
const TRIGGER_GAP: Duration = Duration::from_secs(2 * 3600);
/// Looking up the newest yt-dlp release must not hold a check up.
const VERSION_LOOKUP: Duration = Duration::from_secs(60);
/// This many sites all failing in one pass looks like our outage, not theirs.
const OUTAGE_SITES: usize = 3;
/// All daytime heals of one local day together may make this many model calls.
const DAY_HEAL_LLM_CALLS: u32 = 3;
/// A site's self-heal may take this many times `repair.budget_secs` (one budget per canary page).
const HEAL_BUDGET_FACTOR: u32 = 3;

// ------------------------------------------------------------------ pool

/// Which links to add to and remove from the pool. `jobs` are the recent
/// completed page jobs; `existing` is the pool now. Per site (registrable
/// domain) the 3 newest links stay; only the 30 sites with the most jobs
/// have any; video platforms have none. A link recorded as gone stays as a
/// marker (it is never added again and does not count toward the 3) until
/// it is older than the jobs that could re-offer it. So never more than 90
/// live rows.
pub(crate) fn plan_canaries(
    jobs: &[CompletedPage],
    existing: &[SiteCanary],
    is_platform: impl Fn(&str) -> bool,
) -> (Vec<(String, String, i64)>, Vec<i64>) {
    let mut per_domain: HashMap<String, Vec<&CompletedPage>> = HashMap::new();
    for job in jobs {
        if is_platform(&job.url) {
            continue;
        }
        if let Some(domain) = site_health::site_for_job(&job.url, false) {
            per_domain.entry(domain).or_default().push(job);
        }
    }
    let mut ranked: Vec<(&String, usize)> = per_domain.iter().map(|(d, j)| (d, j.len())).collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
    let top: Vec<&String> = ranked.into_iter().take(MAX_DOMAINS).map(|(d, _)| d).collect();

    // Every link already in the pool, gone or not, is never added twice.
    let have: HashSet<&str> = existing.iter().map(|c| c.url.as_str()).collect();
    let marker_cutoff = Utc::now() - chrono::Duration::days(POOL_DAYS);
    let mut add = Vec::new();
    let mut keep_ids: HashSet<i64> = HashSet::new();
    for domain in top {
        // (time, url, existing id or the job to add), newest first.
        let mut cands: Vec<(DateTime<Utc>, &str, Result<i64, i64>)> = Vec::new();
        for c in existing.iter().filter(|c| &c.domain == domain) {
            if c.is_gone() {
                if c.added_at.map_or(false, |t| t >= marker_cutoff) {
                    keep_ids.insert(c.id);
                }
                continue;
            }
            cands.push((c.added_at.unwrap_or(DateTime::<Utc>::MIN_UTC), &c.url, Ok(c.id)));
        }
        let mut seen: HashSet<&str> = HashSet::new();
        for job in &per_domain[domain] {
            if have.contains(job.url.as_str()) {
                continue;
            }
            // The same link finished twice: the newer finish counts once.
            match cands.iter_mut().find(|c| c.1 == job.url && c.2.is_err()) {
                Some(c) if job.finished_at > c.0 => {
                    *c = (job.finished_at, &job.url, Err(job.job_id));
                }
                Some(_) => {}
                None => {
                    if seen.insert(&job.url) {
                        cands.push((job.finished_at, &job.url, Err(job.job_id)));
                    }
                }
            }
        }
        cands.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        for (_, url, what) in cands.into_iter().take(PER_DOMAIN) {
            match what {
                Ok(id) => {
                    keep_ids.insert(id);
                }
                Err(job_id) => add.push((domain.clone(), url.to_string(), job_id)),
            }
        }
    }
    let remove = existing.iter().filter(|c| !keep_ids.contains(&c.id)).map(|c| c.id).collect();
    (add, remove)
}

/// Bring the pool in line with the last 30 days of completed jobs. Returns
/// (added, removed).
pub fn sync_pool(repo: &Repository) -> anyhow::Result<(usize, usize)> {
    let jobs = repo.completed_pages_since(POOL_DAYS)?;
    let existing = repo.list_canaries()?;
    let (add, remove) = plan_canaries(&jobs, &existing, omni_broadcast::downloader::is_video_platform);
    for id in &remove {
        repo.delete_canary(*id)?;
    }
    let mut added = 0;
    for (domain, url, job_id) in &add {
        let at = jobs.iter().find(|j| j.job_id == *job_id).map(|j| j.finished_at).unwrap_or_else(Utc::now);
        if repo.add_canary(domain, url, Some(*job_id), at)? {
            added += 1;
        }
    }
    Ok((added, remove.len()))
}

// ----------------------------------------------------------------- check

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SiteCheck {
    Pass,
    Fail(String),
    /// Nothing is known: no live article, no answer, no time left, or the
    /// failure looks like ours.
    Unknown,
}

/// What the check does to the site's state; `None` leaves it as it is. Only
/// a canary confirms "broken", and only from "degraded": a site that is
/// merely slow to fail is not condemned by one night's check.
pub(crate) fn site_after_check(state: SiteState, check: &SiteCheck) -> Option<SiteState> {
    match (check, state) {
        (SiteCheck::Pass, _) => Some(SiteState::Healthy),
        (SiteCheck::Fail(_), SiteState::Degraded) => Some(SiteState::Broken),
        _ => None,
    }
}

/// Whether a daytime check is started for a site's change of state: only a
/// site that just turned degraded, and only with repair on.
pub(crate) fn should_trigger(mode: RepairMode, before: SiteState, after: SiteState) -> bool {
    mode != RepairMode::Off && before == SiteState::Healthy && after == SiteState::Degraded
}

/// A failure is the site's only while our own browser works.
pub(crate) fn believe_failure(check: SiteCheck, browser_ok: bool) -> SiteCheck {
    match check {
        SiteCheck::Fail(_) if !browser_ok => SiteCheck::Unknown,
        other => other,
    }
}

/// Three or more decided sites all failing in one pass is more likely our
/// network, browser or yt-dlp than three sites changing at once: change
/// nothing. Undecided (Unknown) sites say nothing either way.
pub(crate) fn outage_suspected(checks: &[&SiteCheck]) -> bool {
    let decided: Vec<&&SiteCheck> = checks.iter().filter(|c| ***c != SiteCheck::Unknown).collect();
    decided.len() >= OUTAGE_SITES && decided.iter().all(|c| matches!(***c, SiteCheck::Fail(_)))
}

/// `text` with every web address cut down to its host, so a detail line can
/// go to the audit log without article paths or query strings.
pub(crate) fn hosts_only(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("http://").into_iter().chain(rest.find("https://")).min() {
        out.push_str(&rest[..at]);
        let url = &rest[at..];
        let end = url.find(|c: char| c.is_whitespace() || matches!(c, ')' | '"' | '\'' | '>' | ',')).unwrap_or(url.len());
        let host = url[..end]
            .split("://")
            .nth(1)
            .unwrap_or("")
            .split(|c| matches!(c, '/' | '?' | '#'))
            .next()
            .unwrap_or("");
        out.push_str(host);
        rest = &url[end..];
    }
    out.push_str(rest);
    out
}

/// The outside questions a check asks, so a test can answer them.
pub(crate) trait SiteProbe {
    async fn page_status(&self, url: &str) -> Option<u16>;
    async fn check_link(&self, url: &str) -> LinkOutcome;
    /// Does our own browser work (a failure is believed only if it does)?
    async fn browser_works(&self, health: &HealthState) -> bool;
}

pub(crate) struct LiveProbe {
    pub ytdl: PathBuf,
    /// Where what the browser sees is remembered for the site (P3.16).
    pub repo: Repository,
    pub scratch: PathBuf,
}

impl SiteProbe for LiveProbe {
    async fn page_status(&self, url: &str) -> Option<u16> {
        omni_browser::pages::page_status(url).await
    }
    async fn check_link(&self, url: &str) -> LinkOutcome {
        crate::selfcheck::check_link_recording(&self.ytdl, &self.repo, &self.scratch, url).await
    }
    async fn browser_works(&self, health: &HealthState) -> bool {
        crate::selfcheck::check_browser(health).await
    }
}

/// Ask a site's canaries, newest first, whether it still gives video. The
/// first that does is a pass.
/// - 404/410: the article was taken down; the row is marked gone (kept, so
///   the link is not added again) and counts neither way.
/// - no answer at all, or a 5xx: we cannot tell; skipped, counts neither way.
/// - past `deadline`: stop; whatever is undecided is Unknown.
pub(crate) async fn check_site(
    repo: &Repository,
    probe: &impl SiteProbe,
    domain: &str,
    deadline: Instant,
) -> SiteCheck {
    let canaries = match repo.list_canaries_for(domain) {
        Ok(c) => c,
        Err(e) => {
            warn!("Canary: could not read the canaries of {domain}: {e:#}");
            return SiteCheck::Unknown;
        }
    };
    let mut last_detail: Option<String> = None;
    for canary in canaries.iter().filter(|c| !c.is_gone()) {
        if Instant::now() >= deadline {
            info!("Canary: out of time while checking {domain}");
            break;
        }
        match probe.page_status(&canary.url).await {
            Some(404 | 410) => {
                info!("Canary: an article of {domain} is gone (404/410); marked");
                let _ = repo.mark_canary_gone(canary.id);
                continue;
            }
            None => continue,
            Some(s) if s >= 500 => continue,
            Some(_) => {}
        }
        // Past the deadline the check is dropped (which kills its yt-dlp):
        // undecided, not failed.
        let left = deadline.saturating_duration_since(Instant::now());
        let Ok(outcome) = tokio::time::timeout(left, probe.check_link(&canary.url)).await else {
            info!("Canary: out of time while checking {domain}");
            break;
        };
        if outcome.inconclusive {
            info!("Canary: the only videos of an article of {domain} are the site's own furniture; undecided");
            continue;
        }
        if let Err(e) = repo.record_canary_result(canary.id, outcome.ok, &outcome.detail) {
            warn!("Canary: result for {domain} not stored: {e:#}");
        }
        if outcome.ok {
            return SiteCheck::Pass;
        }
        last_detail = Some(outcome.detail);
    }
    match last_detail {
        Some(d) => SiteCheck::Fail(d),
        None => SiteCheck::Unknown,
    }
}

/// Store what a check means for the site. Returns (before, after) when the
/// state changed.
/// A verdict older than the site's last change or failure is stale (a job
/// failed, or the state moved, while the nightly pass was still running):
/// it is dropped, not applied over newer evidence.
fn apply_check(
    repo: &Repository,
    health: &HealthState,
    domain: &str,
    check: &SiteCheck,
    checked_at: DateTime<Utc>,
) -> Option<(SiteState, SiteState)> {
    let row = repo.get_site(domain).ok().flatten()?;
    if row.state_since.is_some_and(|t| t > checked_at) || row.last_fail_at.is_some_and(|t| t > checked_at) {
        info!("Canary: the verdict for {domain} is older than its latest change; not applied");
        return None;
    }
    if *check == SiteCheck::Pass {
        if let Err(e) = repo.reset_site_failures(domain) {
            warn!("Canary: failures of {domain} not cleared: {e:#}");
        }
    }
    let after = site_after_check(row.state, check).filter(|a| *a != row.state)?;
    if let Err(e) = repo.set_site_state(domain, after) {
        warn!("Canary: state of {domain} not stored: {e:#}");
        return None;
    }
    match (row.state, after) {
        (SiteState::Degraded, SiteState::Broken) => {
            let detail = match check {
                SiteCheck::Fail(d) => hosts_only(d).chars().take(300).collect::<String>(),
                _ => String::new(),
            };
            warn!("Canary: {domain} confirmed broken");
            let _ = repo.log_audit(
                "WARN",
                "SYSTEM",
                &format!("Site {domain} confirmed broken: its recent articles no longer give video ({detail})"),
            );
        }
        (SiteState::Broken, SiteState::Healthy) => {
            info!("Canary: {domain} works again");
            let _ = repo.log_audit("INFO", "SYSTEM", &format!("Site {domain} works again"));
        }
        _ => {}
    }
    site_health::refresh_health(repo, health);
    Some((row.state, after))
}

// ------------------------------------------------------------ rate limits

/// At most one daytime check per site per [`TRIGGER_GAP`].
#[derive(Default)]
pub(crate) struct TriggerLimiter {
    last: HashMap<String, Instant>,
}

impl TriggerLimiter {
    /// Whether a check of `domain` may start now; if so, it is counted.
    pub fn allow(&mut self, domain: &str, now: Instant) -> bool {
        if let Some(t) = self.last.get(domain) {
            if now.saturating_duration_since(*t) < TRIGGER_GAP {
                return false;
            }
        }
        self.last.insert(domain.to_string(), now);
        true
    }
}

// -------------------------------------------------------------- the runner

/// Everything a canary check needs, shared by the workers and the scheduler.
pub struct CanaryEnv {
    pub repo: Repository,
    pub health: HealthState,
    pub bin_dir: PathBuf,
    /// Where a run's copy of a site's cookie jar is written (the install's temp directory).
    pub scratch: PathBuf,
    pub channel: String,
    /// One canary run at a time, process-wide. The 03:00 yt-dlp task holds
    /// it for its whole run too: a canary's yt-dlp has no gate pass.
    run_lock: Arc<tokio::sync::Mutex<()>>,
    limiter: Mutex<TriggerLimiter>,
    /// Daytime checks started (a test hook as much as a statistic).
    spawned: AtomicUsize,
    /// The repair ladder's world, set once at start-up: the self-heal (P10.14) needs it.
    heal: OnceLock<Arc<crate::repair::RepairCtx>>,
    /// At most one daytime self-heal per site per [`TRIGGER_GAP`].
    heal_limiter: Mutex<TriggerLimiter>,
    /// The last nightly pass looked like our own outage.
    outage: AtomicBool,
    /// Model calls the daytime heals made on this local day: (date, calls). In memory only.
    day_llm: Mutex<(String, u32)>,
}

impl CanaryEnv {
    pub fn new(repo: Repository, health: HealthState, bin_dir: PathBuf, channel: String) -> Self {
        Self {
            repo,
            health,
            scratch: bin_dir.join("..").join("temp"),
            bin_dir,
            channel,
            run_lock: Arc::new(tokio::sync::Mutex::new(())),
            limiter: Mutex::new(TriggerLimiter::default()),
            spawned: AtomicUsize::new(0),
            heal: OnceLock::new(),
            heal_limiter: Mutex::new(TriggerLimiter::default()),
            outage: AtomicBool::new(false),
            day_llm: Mutex::new((String::new(), 0)),
        }
    }

    /// Calls of the daytime heals still allowed today (the count restarts with the local date).
    fn day_llm_left(&self) -> u32 {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let mut g = self.day_llm.lock().unwrap_or_else(|e| e.into_inner());
        if g.0 != today {
            *g = (today, 0);
        }
        DAY_HEAL_LLM_CALLS.saturating_sub(g.1)
    }

    fn add_day_llm(&self, n: u32) {
        self.day_llm.lock().unwrap_or_else(|e| e.into_inner()).1 += n;
    }

    /// Test hook: model calls the daytime heals made today.
    #[cfg(test)]
    pub(crate) fn day_llm_used(&self) -> u32 {
        DAY_HEAL_LLM_CALLS - self.day_llm_left()
    }

    #[cfg(test)]
    pub(crate) fn set_day_llm_used_for_tests(&self, n: u32) {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        *self.day_llm.lock().unwrap() = (today, n);
    }

    /// Test hook: pretend the last nightly pass looked like our own outage.
    #[cfg(test)]
    pub(crate) fn mark_outage(&self, on: bool) {
        self.outage.store(on, Ordering::SeqCst);
    }

    /// Where the cookie jars of a run are written; the daemon gives its temp directory.
    pub fn with_scratch(mut self, scratch: PathBuf) -> Self {
        self.scratch = scratch;
        self
    }

    /// Give the self-heal the repair ladder's world. Once; later calls change nothing.
    pub fn set_heal(&self, ctx: Arc<crate::repair::RepairCtx>) {
        let _ = self.heal.set(ctx);
    }

    /// The run lock itself, for the web crate's yt-dlp and Deno installs (P9.3).
    pub fn run_lock_handle(&self) -> Arc<tokio::sync::Mutex<()>> {
        self.run_lock.clone()
    }

    /// Wait for any canary run to end and keep new ones out while held.
    pub async fn lock_run(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.run_lock.clone().lock_owned().await
    }

    /// A site just went from healthy to degraded in the daytime: check it in
    /// the background. Never waits; the job that noticed is not delayed.
    pub fn trigger(self: &Arc<Self>, domain: &str) {
        let Ok(guard) = self.run_lock.clone().try_lock_owned() else {
            info!("Canary: another check is running; {domain} waits for the nightly one");
            return;
        };
        if !self.limiter.lock().unwrap_or_else(|e| e.into_inner()).allow(domain, Instant::now()) {
            info!("Canary: {domain} was checked less than 2 hours ago");
            return;
        }
        self.spawned.fetch_add(1, Ordering::SeqCst);
        let env = self.clone();
        let domain = domain.to_string();
        tokio::spawn(async move {
            let _guard = guard;
            let probe = env.live_probe();
            let check = check_domain(&env, &probe, &domain, Instant::now() + NIGHTLY_BUDGET).await;
            let broken = settle(&env, &domain, &check, Utc::now()).await;
            // Confirmed broken in the daytime: try to find a way before a journalist's job needs it.
            if broken {
                if let Some(ctx) = env.heal.get() {
                    heal_by_day(&env, ctx, &domain).await;
                }
            }
        });
    }

    /// The admin's «Run now» (P10.15a): check one site in the background, and
    /// when it is broken afterwards (and repair is on) probe its canary pages.
    /// Not held to the daytime rate limit: an admin asked. Never waits.
    pub fn run_now(self: &Arc<Self>, domain: &str) -> RunNow {
        let Ok(guard) = self.run_lock.clone().try_lock_owned() else {
            return RunNow::Busy;
        };
        let env = self.clone();
        let domain = domain.to_string();
        tokio::spawn(async move {
            let _guard = guard;
            let probe = env.live_probe();
            manual_check(&env, &probe, &domain).await;
        });
        RunNow::Started
    }

    fn ytdl(&self) -> PathBuf {
        DependencyManager::new(&self.bin_dir).ytdl_live()
    }

    /// The real outside world for a check, with the memory of what the browser sees on a site's pages.
    fn live_probe(&self) -> LiveProbe {
        LiveProbe { ytdl: self.ytdl(), repo: self.repo.clone(), scratch: self.scratch.clone() }
    }
}

/// What «Run now» does under the canary lock: check the site, store what that
/// means, say so in the audit log, and, when the site is broken afterwards,
/// probe its canary pages (a no-op with repair off or a job running).
async fn manual_check(env: &CanaryEnv, probe: &impl SiteProbe, domain: &str) {
    let check = check_domain(env, probe, domain, Instant::now() + NIGHTLY_BUDGET).await;
    settle(env, domain, &check, Utc::now()).await;
    let said = match &check {
        SiteCheck::Pass => "works",
        SiteCheck::Fail(_) => "no recent article gives video",
        SiteCheck::Unknown => "undecided",
    };
    let _ = env.repo.log_audit("INFO", "ADMIN", &format!("Manual check of {domain}: {said}"));
    let broken = matches!(env.repo.get_site(domain), Ok(Some(row)) if row.state == SiteState::Broken);
    if broken {
        if let Some(ctx) = env.heal.get() {
            heal_now(env, ctx, domain, false).await;
        }
    }
}

/// One «Run now» per site in this time (in memory).
const RUN_NOW_COOLDOWN: Duration = Duration::from_secs(10 * 60);

/// The web panel's way in to [`CanaryEnv::run_now`], with a cooldown per site.
pub struct AdminRunner {
    env: Arc<CanaryEnv>,
    started: Mutex<HashMap<String, Instant>>,
}

impl AdminRunner {
    pub fn new(env: Arc<CanaryEnv>) -> Self {
        Self { env, started: Mutex::new(HashMap::new()) }
    }
}

impl SiteRunner for AdminRunner {
    fn run_now(&self, domain: &str) -> RunNow {
        let mut started = self.started.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = started.get(domain) {
            let left = RUN_NOW_COOLDOWN.saturating_sub(at.elapsed());
            if !left.is_zero() {
                return RunNow::Cooldown(left.as_secs().max(1));
            }
        }
        let answer = self.env.run_now(domain);
        if answer == RunNow::Started {
            started.insert(domain.to_string(), Instant::now());
        }
        answer
    }
}

/// Run `fut` while no canary run is going and none can start: the 03:00
/// yt-dlp task, whose swap a canary's yt-dlp (no gate pass) must not overlap.
pub async fn with_canary_lock<F: std::future::Future>(env: &CanaryEnv, fut: F) -> F::Output {
    let _guard = env.lock_run().await;
    fut.await
}

/// Check one site. A failure is believed only while the browser itself works.
async fn check_domain(env: &CanaryEnv, probe: &impl SiteProbe, domain: &str, deadline: Instant) -> SiteCheck {
    let mut check = check_site(&env.repo, probe, domain, deadline).await;
    if matches!(check, SiteCheck::Fail(_)) {
        let left = deadline.saturating_duration_since(Instant::now());
        let browser_ok = tokio::time::timeout(left, probe.browser_works(&env.health)).await.unwrap_or(false);
        if !browser_ok {
            info!("Canary: {domain} failed but the browser is down; not held against the site");
        }
        check = believe_failure(check, browser_ok);
    }
    info!("Canary: {domain}: {}", match &check {
        SiteCheck::Pass => "works",
        SiteCheck::Fail(_) => "no recent article gives video",
        SiteCheck::Unknown => "undecided",
    });
    check
}

/// Store what a check means; when the site just turned broken, say whether a
/// newer yt-dlp is waiting for 03:00.
/// Returns whether the site was just confirmed broken.
async fn settle(env: &CanaryEnv, domain: &str, check: &SiteCheck, checked_at: DateTime<Utc>) -> bool {
    if matches!(apply_check(&env.repo, &env.health, domain, check, checked_at), Some((_, SiteState::Broken))) {
        announce_newer_ytdl(env).await;
        return true;
    }
    false
}

/// A site is broken: is there a newer yt-dlp? Only said, never installed
/// here; the 03:00 update is the only updater. Unknown on error or timeout.
async fn announce_newer_ytdl(env: &CanaryEnv) {
    let dep_mgr = DependencyManager::new(&env.bin_dir);
    match tokio::time::timeout(VERSION_LOOKUP, dep_mgr.check_ytdl_update(&env.channel)).await {
        Ok(Ok((true, _local, remote))) => {
            info!("Canary: newer yt-dlp {remote} exists; it will be tried at 03:00");
            let _ = env.repo.log_audit(
                "INFO",
                "SYSTEM",
                &format!("Newer yt-dlp {remote} exists; it will be tried at 03:00"),
            );
        }
        Ok(Ok(_)) => {}
        Ok(Err(e)) => warn!("Canary: could not look for a newer yt-dlp: {e:#}"),
        Err(_) => warn!("Canary: looking for a newer yt-dlp took over 60 s"),
    }
}

/// The nightly pass: every site that has canaries, one at a time, within the
/// time budget. Verdicts are applied together at the end, so a pass where
/// everything failed can be recognised as an outage. Returns (checked,
/// skipped).
pub async fn nightly(env: &CanaryEnv) -> (usize, usize) {
    let probe = env.live_probe();
    nightly_with(env, &probe).await
}

async fn nightly_with(env: &CanaryEnv, probe: &impl SiteProbe) -> (usize, usize) {
    let domains = env.repo.canary_domains().unwrap_or_default();
    let deadline = Instant::now() + NIGHTLY_BUDGET;
    let mut results: Vec<(String, SiteCheck, DateTime<Utc>)> = Vec::new();
    let mut skipped = 0;
    for (i, domain) in domains.iter().enumerate() {
        if Instant::now() >= deadline {
            skipped = domains.len() - i;
            warn!("Canary: nightly budget used up; {skipped} site(s) not checked tonight");
            break;
        }
        let _guard = env.run_lock.lock().await;
        let check = check_domain(env, probe, domain, deadline).await;
        results.push((domain.clone(), check, Utc::now()));
    }
    let checked = results.len();
    let all: Vec<&SiteCheck> = results.iter().map(|(_, c, _)| c).collect();
    let outage = outage_suspected(&all);
    env.outage.store(outage, Ordering::SeqCst);
    if outage {
        warn!("Canary: all {checked} sites failed; treated as an outage on our side, no site changed");
        let _ = env.repo.log_audit(
            "WARN",
            "SYSTEM",
            &format!("Canary check: all {checked} sites failed at once; assumed to be our network or tools, no site was marked"),
        );
    } else {
        for (domain, check, at) in &results {
            settle(env, domain, check, *at).await;
        }
        info!("Canary: {checked} site(s) checked");
    }
    (checked, skipped)
}

// ------------------------------------------------------------- self-heal

/// The live canary pages of a site, newest first.
fn live_pages(repo: &Repository, domain: &str) -> Vec<String> {
    repo.list_canaries_for(domain).unwrap_or_default().into_iter().filter(|c| !c.is_gone()).map(|c| c.url).collect()
}

/// Sites whose canaries say they are broken, the one with the most recent jobs first.
fn broken_sites(repo: &Repository) -> Vec<String> {
    let mut rows: Vec<_> = repo.list_sites_not_healthy().unwrap_or_default().into_iter().filter(|r| r.state == SiteState::Broken).collect();
    rows.sort_by(|a, b| {
        let at = |r: &site_health::SiteRow| r.last_ok_at.max(r.last_fail_at);
        at(b).cmp(&at(a)).then_with(|| a.domain.cmp(&b.domain))
    });
    rows.into_iter().map(|r| r.domain).collect()
}

/// What one site's self-heal says, to the audit log: hosts and outcomes only.
fn announce_heal(repo: &Repository, heal: &crate::repair::SiteHeal) {
    if heal.found.is_some() {
        info!("Self-heal: {}: a new way to find the video was found; the next job there is held for MCR", heal.domain);
        let _ = repo.log_audit(
            "INFO",
            "SYSTEM",
            &format!("Site {}: a new way to find its video was found on its canary pages; the next job there waits for MCR's approval", heal.domain),
        );
    }
}

/// After the nightly canaries settled: every site still broken gets its canary
/// pages probed, one site at a time under the canary lock (never while the
/// yt-dlp updater holds it), within the nightly budget and three repair budgets
/// per site. Nothing runs with repair off, nor after a pass that looked like our
/// own outage. Returns the number of sites probed.
pub async fn heal_nightly(env: &CanaryEnv) -> usize {
    match env.heal.get() {
        Some(ctx) => heal_nightly_with(env, ctx).await,
        None => 0,
    }
}

pub(crate) async fn heal_nightly_with(env: &CanaryEnv, ctx: &crate::repair::RepairCtx) -> usize {
    use crate::repair::{record_report, SiteHeal};
    if ctx.cfg.mode == RepairMode::Off {
        return 0;
    }
    let broken = broken_sites(&env.repo);
    if broken.is_empty() {
        return 0;
    }
    // A late start (the machine was off at the usual hour) must not run the browser
    // beside live jobs; the next night tries again. No report: nothing was decided.
    match env.repo.running_job_ids() {
        Ok(ids) if ids.is_empty() => {}
        Ok(_) => {
            info!("Self-heal: skipped tonight, a job is running");
            return 0;
        }
        Err(e) => {
            warn!("Self-heal: the running jobs could not be counted, skipped: {e:#}");
            return 0;
        }
    }
    if env.outage.load(Ordering::SeqCst) {
        warn!("Self-heal: the last canary pass looked like our own outage; no site is probed");
        for domain in &broken {
            let heal = SiteHeal { domain: domain.clone(), found: None, outcome: "outage", detail: "all canary sites failed at once".into() };
            record_report(&env.repo, &heal, Utc::now());
        }
        return 0;
    }
    let deadline = Instant::now() + NIGHTLY_BUDGET;
    let mut probed = 0;
    // The run's model calls, shared by all its sites.
    let llm_left = std::sync::atomic::AtomicU32::new(crate::repair::PROBE_LLM_CALLS);
    for domain in &broken {
        if Instant::now() >= deadline {
            warn!("Self-heal: the nightly budget is used up; the remaining broken site(s) wait for tomorrow");
            break;
        }
        let pages = live_pages(&env.repo, domain);
        if pages.is_empty() {
            continue;
        }
        let _guard = env.run_lock.lock().await;
        let site_deadline = (Instant::now() + ctx.cfg.budget * HEAL_BUDGET_FACTOR).min(deadline);
        let heal = crate::repair::probe_site_with(ctx, domain, &pages, Utc::now(), site_deadline, &llm_left).await;
        record_report(&env.repo, &heal, Utc::now());
        announce_heal(&env.repo, &heal);
        probed += 1;
    }
    probed
}

/// A site was just confirmed broken in the daytime: probe its canary pages once,
/// only with no job running and at most once per site per [`TRIGGER_GAP`]. The
/// caller holds the canary lock.
pub(crate) async fn heal_by_day(env: &CanaryEnv, ctx: &crate::repair::RepairCtx, domain: &str) -> bool {
    heal_now(env, ctx, domain, true).await
}

/// [`heal_by_day`]; `limited` false (an admin's «Run now») skips the once-per-2-hours limit only.
pub(crate) async fn heal_now(env: &CanaryEnv, ctx: &crate::repair::RepairCtx, domain: &str, limited: bool) -> bool {
    use crate::repair::record_report;
    if ctx.cfg.mode == RepairMode::Off {
        return false;
    }
    match env.repo.running_job_ids() {
        Ok(ids) if ids.is_empty() => {}
        Ok(_) => {
            info!("Self-heal: {domain} waits for tonight: a job is running");
            return false;
        }
        Err(e) => {
            warn!("Self-heal: the running jobs could not be counted: {e:#}");
            return false;
        }
    }
    let pages = live_pages(&env.repo, domain);
    if pages.is_empty() || (limited && !env.heal_limiter.lock().unwrap_or_else(|e| e.into_inner()).allow(domain, Instant::now())) {
        return false;
    }
    let deadline = Instant::now() + (ctx.cfg.budget * HEAL_BUDGET_FACTOR).min(NIGHTLY_BUDGET);
    // One model call for this heal, and a few for the whole day: a daytime heal must not
    // drain the day's cap across several broken sites.
    let budget = crate::repair::DAY_PROBE_LLM_CALLS.min(env.day_llm_left());
    let left = std::sync::atomic::AtomicU32::new(budget);
    let heal = crate::repair::probe_site_with(ctx, domain, &pages, Utc::now(), deadline, &left).await;
    env.add_day_llm(budget - left.load(Ordering::SeqCst).min(budget));
    record_report(&env.repo, &heal, Utc::now());
    announce_heal(&env.repo, &heal);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration as Dur;
    use omni_core::site_health::SiteOutcome;
    use std::collections::HashMap as Map;

    fn page(id: i64, url: &str, age_h: i64) -> CompletedPage {
        CompletedPage { job_id: id, url: url.into(), finished_at: Utc::now() - Dur::hours(age_h) }
    }

    fn canary(id: i64, domain: &str, url: &str, age_h: i64) -> SiteCanary {
        SiteCanary {
            id,
            domain: domain.into(),
            url: url.into(),
            job_id: None,
            added_at: Some(Utc::now() - Dur::hours(age_h)),
            last_checked_at: None,
            last_ok: None,
            last_detail: None,
            fail_count: 0,
        }
    }

    fn gone(id: i64, domain: &str, url: &str, age_h: i64) -> SiteCanary {
        SiteCanary { last_ok: Some(false), last_detail: Some("gone".into()), ..canary(id, domain, url, age_h) }
    }

    fn platform(u: &str) -> bool {
        omni_broadcast::downloader::is_video_platform(u)
    }

    fn soon() -> Instant {
        Instant::now() + Duration::from_secs(600)
    }

    #[test]
    fn three_newest_per_site_and_the_rest_dropped() {
        let jobs: Vec<CompletedPage> =
            (1..=5).map(|i| page(i, &format!("https://news.example/a{i}"), i * 10)).collect();
        let (add, remove) = plan_canaries(&jobs, &[], platform);
        let urls: Vec<&str> = add.iter().map(|a| a.1.as_str()).collect();
        assert_eq!(add.len(), 3);
        for want in ["https://news.example/a1", "https://news.example/a2", "https://news.example/a3"] {
            assert!(urls.contains(&want), "{want} missing in {urls:?}");
        }
        assert!(add.iter().all(|a| a.0 == "news.example"));
        assert!(remove.is_empty());

        // An older existing canary yields to three newer jobs, oldest first.
        let existing = vec![canary(7, "news.example", "https://news.example/old", 900)];
        let (add, remove) = plan_canaries(&jobs, &existing, platform);
        assert_eq!(add.len(), 3);
        assert_eq!(remove, vec![7]);

        // A newer existing one stays and only two are added.
        let existing = vec![canary(8, "news.example", "https://news.example/new", 1)];
        let (add, remove) = plan_canaries(&jobs, &existing, platform);
        assert_eq!(add.len(), 2);
        assert!(remove.is_empty());
    }

    #[test]
    fn at_most_thirty_sites_and_ninety_rows() {
        let mut jobs = Vec::new();
        let mut id = 0;
        for d in 0..40 {
            // Site d has (d + 1) jobs: the most active sites win.
            for k in 0..=d {
                id += 1;
                jobs.push(page(id, &format!("https://site{d}.example/{k}"), 1 + k));
            }
        }
        let (add, remove) = plan_canaries(&jobs, &[], platform);
        assert!(add.len() <= 90, "{}", add.len());
        let domains: HashSet<&str> = add.iter().map(|a| a.0.as_str()).collect();
        assert_eq!(domains.len(), 30);
        assert!(domains.contains("site39.example") && domains.contains("site10.example"));
        assert!(!domains.contains("site9.example"), "a quiet site must not take a slot");
        assert!(remove.is_empty());
    }

    #[test]
    fn platforms_are_skipped_and_dropped_sites_are_removed() {
        let jobs = vec![
            page(1, "https://www.youtube.com/watch?v=abc", 1),
            page(2, "https://www.facebook.com/watch?v=1", 2),
            page(3, "https://news.example/a", 3),
        ];
        let existing = vec![
            canary(4, "gone.example", "https://gone.example/x", 5),
            canary(5, "news.example", "https://news.example/a", 3),
        ];
        let (add, remove) = plan_canaries(&jobs, &existing, platform);
        assert!(add.is_empty(), "{add:?}");
        assert_eq!(remove, vec![4], "a site with no recent jobs leaves the pool");
    }

    #[test]
    fn a_gone_link_is_not_re_added_and_does_not_take_a_slot() {
        let jobs: Vec<CompletedPage> =
            (1..=4).map(|i| page(i, &format!("https://news.example/a{i}"), i)).collect();
        // a1 (the newest job) is recorded as gone: never offered again, and
        // three live ones are still wanted.
        let existing = vec![gone(9, "news.example", "https://news.example/a1", 1)];
        let (add, remove) = plan_canaries(&jobs, &existing, platform);
        let urls: Vec<&str> = add.iter().map(|a| a.1.as_str()).collect();
        assert_eq!(urls.len(), 3, "{urls:?}");
        assert!(!urls.contains(&"https://news.example/a1"), "{urls:?}");
        assert!(remove.is_empty(), "the marker stays: {remove:?}");

        // A marker older than the pool window, or of a dropped site, goes.
        let old = vec![gone(9, "news.example", "https://news.example/a1", 24 * 40)];
        assert_eq!(plan_canaries(&jobs, &old, platform).1, vec![9]);
        let elsewhere = vec![gone(10, "other.example", "https://other.example/x", 1)];
        assert_eq!(plan_canaries(&jobs, &elsewhere, platform).1, vec![10]);
    }

    #[test]
    fn site_after_check_table() {
        use SiteState::*;
        let fail = SiteCheck::Fail("x".into());
        for s in [Healthy, Degraded, Broken] {
            assert_eq!(site_after_check(s, &SiteCheck::Pass), Some(Healthy), "pass from {s:?}");
            assert_eq!(site_after_check(s, &SiteCheck::Unknown), None, "unknown from {s:?}");
        }
        assert_eq!(site_after_check(Degraded, &fail), Some(Broken));
        assert_eq!(site_after_check(Healthy, &fail), None);
        assert_eq!(site_after_check(Broken, &fail), None);
    }

    #[test]
    fn only_a_newly_degraded_site_with_repair_on_is_checked_by_day() {
        use SiteState::*;
        for mode in [RepairMode::Off, RepairMode::Shadow, RepairMode::Hold] {
            for before in [Healthy, Degraded, Broken] {
                for after in [Healthy, Degraded, Broken] {
                    let want = mode != RepairMode::Off && before == Healthy && after == Degraded;
                    assert_eq!(should_trigger(mode, before, after), want, "{mode:?} {before:?}->{after:?}");
                }
            }
        }
    }

    #[test]
    fn a_failure_while_the_browser_is_down_is_unknown() {
        let f = SiteCheck::Fail("x".into());
        assert_eq!(believe_failure(f.clone(), false), SiteCheck::Unknown);
        assert_eq!(believe_failure(f.clone(), true), f);
        assert_eq!(believe_failure(SiteCheck::Pass, false), SiteCheck::Pass);
    }

    #[test]
    fn three_failures_in_one_pass_look_like_an_outage_but_not_two_or_a_mix() {
        let f = SiteCheck::Fail("x".into());
        let p = SiteCheck::Pass;
        let u = SiteCheck::Unknown;
        assert!(outage_suspected(&[&f, &f, &f]));
        assert!(outage_suspected(&[&f, &f, &f, &f]));
        assert!(!outage_suspected(&[&f, &f]), "two sites can change together");
        assert!(!outage_suspected(&[&f, &f, &p]));
        assert!(outage_suspected(&[&f, &f, &f, &u]), "unknowns are ignored");
        assert!(!outage_suspected(&[&f, &f, &u]), "two decided are not enough");
        assert!(!outage_suspected(&[&f, &f, &f, &p]));
        assert!(!outage_suspected(&[&u, &u, &u]));
        assert!(!outage_suspected(&[]));
    }

    #[test]
    fn addresses_in_a_detail_line_are_cut_to_their_host() {
        let d = hosts_only(
            "the page's video (https://cdn.news.example/v/1.m3u8?token=abc) could not be read: \
             see http://other.example/a/b#x, and https://third.example",
        );
        assert_eq!(
            d,
            "the page's video (cdn.news.example) could not be read: see other.example, and third.example"
        );
        assert!(!d.contains("token") && !d.contains("m3u8") && !d.contains("://"));
        assert_eq!(hosts_only("no video found on the page"), "no video found on the page");
    }

    struct Fake {
        status: Map<&'static str, u16>,
        ok: Map<&'static str, bool>,
        asked: Mutex<Vec<String>>,
        /// Time `check_link` takes.
        delay: Duration,
    }

    impl SiteProbe for Fake {
        async fn page_status(&self, url: &str) -> Option<u16> {
            self.status.get(url).copied()
        }
        async fn check_link(&self, url: &str) -> LinkOutcome {
            self.asked.lock().unwrap().push(url.to_string());
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            let ok = *self.ok.get(url).unwrap_or(&false);
            LinkOutcome { inconclusive: url.contains("furniture"), ok, detail: if ok { "found it".into() } else { format!("no video at {url}") } }
        }
        async fn browser_works(&self, _health: &HealthState) -> bool {
            true
        }
    }

    /// Every page answers 200 unless `status` says otherwise.
    fn fake(status: &[(&'static str, u16)], ok: &[(&'static str, bool)]) -> Fake {
        Fake { status: status.iter().cloned().collect(), ok: ok.iter().cloned().collect(), asked: Mutex::new(Vec::new()), delay: Duration::ZERO }
    }

    /// Listed oldest first, so the last one is the newest.
    fn pool(repo: &Repository, urls: &[&str]) {
        let t0 = Utc::now() - Dur::hours(10);
        for (i, u) in urls.iter().enumerate() {
            repo.add_canary("news.example", u, None, t0 + Dur::hours(i as i64)).unwrap();
        }
    }

    /// Keeps the test database file until the test ends, then removes it.
    struct TempDb(PathBuf);

    impl Drop for TempDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
            for ext in ["-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{ext}", self.0.display()));
            }
        }
    }

    /// The guard comes first so a test binds it first and it drops last, after
    /// the database is closed; Windows cannot delete a file that is still open.
    fn repo() -> (TempDb, Repository) {
        static N: AtomicUsize = AtomicUsize::new(0);
        let n = N.fetch_add(1, Ordering::SeqCst);
        // Windows reuses process ids, and a database file that is still open when
        // TempDb drops is not deleted: a pid-and-counter name reopened an older
        // run's database, whose rows broke the counts (gate 2026-10-19). The
        // clock makes every run's name its own.
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let path = std::env::temp_dir().join(format!("omni-canary-test-{}-{n}-{nanos}.db", std::process::id()));
        assert!(!path.exists(), "a fresh test database must not exist yet: {}", path.display());
        let db = Repository::new(&path).unwrap();
        (TempDb(path), db)
    }

    const OK200: u16 = 200;

    fn later() -> DateTime<Utc> {
        Utc::now() + Dur::seconds(1)
    }

    fn degrade(repo: &Repository, domain: &str) {
        for _ in 0..6 {
            repo.record_site_outcome(domain, SiteOutcome::Success).unwrap();
        }
        for _ in 0..3 {
            repo.record_site_outcome(domain, SiteOutcome::SiteFailure("NO_STREAM_FOUND".into())).unwrap();
        }
    }

    #[tokio::test]
    async fn a_gone_article_is_marked_and_never_counts_as_failure() {
        let (_f, repo) = repo();
        pool(&repo, &["https://news.example/live", "https://news.example/gone"]);
        // Newest is "gone" (404); the older one fails.
        let p = fake(&[("https://news.example/gone", 404), ("https://news.example/live", OK200)], &[]);
        let check = check_site(&repo, &p, "news.example", soon()).await;
        assert_eq!(check, SiteCheck::Fail("no video at https://news.example/live".into()));
        assert_eq!(*p.asked.lock().unwrap(), vec!["https://news.example/live".to_string()], "a 404 page is not probed");
        let rows = repo.list_canaries_for("news.example").unwrap();
        assert_eq!(rows.len(), 2, "the gone row is kept as a marker");
        let g = rows.iter().find(|c| c.url.ends_with("/gone")).unwrap();
        assert!(g.is_gone());
        assert_eq!(g.fail_count, 0);
        assert_eq!(rows.iter().find(|c| c.url.ends_with("/live")).unwrap().fail_count, 1);

        // 410 as well; a gone row is skipped on the next run; nothing live is Unknown.
        let p = fake(&[("https://news.example/live", 410)], &[]);
        assert_eq!(check_site(&repo, &p, "news.example", soon()).await, SiteCheck::Unknown);
        let p = fake(&[], &[]);
        assert_eq!(check_site(&repo, &p, "news.example", soon()).await, SiteCheck::Unknown);
        assert!(p.asked.lock().unwrap().is_empty(), "gone rows are never checked");
        assert_eq!(check_site(&repo, &p, "empty.example", soon()).await, SiteCheck::Unknown);
    }

    #[tokio::test]
    async fn no_answer_or_a_server_error_is_unknown_not_a_failure() {
        let (_f, repo) = repo();
        pool(&repo, &["https://news.example/a", "https://news.example/b", "https://news.example/c"]);
        // a: no answer at all (not in the map); b: 503; c: 502.
        let p = fake(&[("https://news.example/b", 503), ("https://news.example/c", 502)], &[]);
        assert_eq!(check_site(&repo, &p, "news.example", soon()).await, SiteCheck::Unknown);
        assert!(p.asked.lock().unwrap().is_empty(), "an unreachable page is not probed");
        let rows = repo.list_canaries_for("news.example").unwrap();
        assert_eq!(rows.len(), 3, "nothing is deleted");
        assert!(rows.iter().all(|c| c.last_ok.is_none() && c.fail_count == 0));
    }

    #[tokio::test]
    async fn the_first_pass_stops_the_loop() {
        let (_f, repo) = repo();
        pool(&repo, &["https://news.example/a", "https://news.example/b", "https://news.example/c"]);
        let all200: Vec<(&'static str, u16)> =
            ["a", "b", "c"].iter().map(|x| (&*Box::leak(format!("https://news.example/{x}").into_boxed_str()), OK200)).collect();
        // Newest first: c fails, b passes, a is never asked.
        let p = fake(&all200, &[("https://news.example/b", true)]);
        assert_eq!(check_site(&repo, &p, "news.example", soon()).await, SiteCheck::Pass);
        assert_eq!(
            *p.asked.lock().unwrap(),
            vec!["https://news.example/c".to_string(), "https://news.example/b".to_string()]
        );
        let rows = repo.list_canaries_for("news.example").unwrap();
        let by = |u: &str| rows.iter().find(|c| c.url == u).unwrap().clone();
        assert_eq!(by("https://news.example/b").last_ok, Some(true));
        assert_eq!(by("https://news.example/c").last_ok, Some(false));
        assert_eq!(by("https://news.example/a").last_ok, None);
    }

    #[tokio::test]
    async fn all_live_canaries_failing_is_a_fail_with_the_last_detail() {
        let (_f, repo) = repo();
        pool(&repo, &["https://news.example/a", "https://news.example/b"]);
        let p = fake(&[("https://news.example/a", OK200), ("https://news.example/b", OK200)], &[]);
        let check = check_site(&repo, &p, "news.example", soon()).await;
        // The last one asked is the oldest.
        assert_eq!(check, SiteCheck::Fail("no video at https://news.example/a".into()));
    }

    #[tokio::test]
    async fn a_page_whose_only_videos_are_site_furniture_is_undecided_not_failed() {
        let (_f, repo) = repo();
        let page = "https://news.example/furniture-only";
        pool(&repo, &[page]);
        let p = fake(&[(page, OK200)], &[]);
        assert_eq!(check_site(&repo, &p, "news.example", soon()).await, SiteCheck::Unknown);
        let rows = repo.list_canaries_for("news.example").unwrap();
        assert!(rows.iter().all(|c| c.last_ok.is_none()), "no result is stored, so no failure is counted");
        assert_eq!(p.asked.lock().unwrap().len(), 1, "it was asked");

        // Control: a page with no video at all still fails as before.
        let (_f2, repo2) = self::repo();
        pool(&repo2, &["https://news.example/a"]);
        let p2 = fake(&[("https://news.example/a", OK200)], &[]);
        assert!(matches!(check_site(&repo2, &p2, "news.example", soon()).await, SiteCheck::Fail(_)));
    }

    #[tokio::test]
    async fn a_check_past_its_deadline_decides_nothing() {
        let (_f, repo) = repo();
        pool(&repo, &["https://news.example/a", "https://news.example/b"]);
        let p = fake(&[("https://news.example/a", OK200), ("https://news.example/b", OK200)], &[]);
        let past = Instant::now() - Duration::from_secs(1);
        assert_eq!(check_site(&repo, &p, "news.example", past).await, SiteCheck::Unknown);
        assert!(p.asked.lock().unwrap().is_empty());
    }

    #[test]
    fn a_degraded_site_is_confirmed_broken_and_a_pass_heals_it() {
        let (_f, repo) = repo();
        let health = HealthState::new();
        for _ in 0..6 {
            repo.record_site_outcome("news.example", SiteOutcome::Success).unwrap();
        }
        for _ in 0..3 {
            repo.record_site_outcome("news.example", SiteOutcome::SiteFailure("NO_STREAM_FOUND".into())).unwrap();
        }
        assert_eq!(repo.get_site("news.example").unwrap().unwrap().state, SiteState::Degraded);

        let fail = SiteCheck::Fail("no video at https://news.example/a/b?token=zzz".into());
        assert_eq!(apply_check(&repo, &health, "news.example", &SiteCheck::Unknown, later()), None);
        assert_eq!(
            apply_check(&repo, &health, "news.example", &fail, later()),
            Some((SiteState::Degraded, SiteState::Broken))
        );
        assert_eq!(apply_check(&repo, &health, "news.example", &fail, later()), None, "broken stays broken");
        assert_eq!(
            apply_check(&repo, &health, "news.example", &SiteCheck::Pass, later()),
            Some((SiteState::Broken, SiteState::Healthy))
        );
        let row = repo.get_site("news.example").unwrap().unwrap();
        assert_eq!((row.state, row.consecutive_fail), (SiteState::Healthy, 0));
        let audit: Vec<String> = repo.get_recent_audit_logs(10).unwrap().into_iter().map(|a| a.message).collect();
        let broken = audit.iter().find(|m| m.contains("news.example confirmed broken")).expect("broken line");
        assert!(!broken.contains("token") && !broken.contains("/a/b") && !broken.contains("://"), "{broken}");
        assert!(audit.iter().any(|m| m.contains("news.example works again")), "{audit:?}");
    }

    #[test]
    fn a_second_trigger_within_two_hours_is_refused() {
        let mut l = TriggerLimiter::default();
        let t = Instant::now();
        assert!(l.allow("a.example", t));
        assert!(!l.allow("a.example", t + Duration::from_secs(60)), "same site, a minute later");
        assert!(!l.allow("a.example", t + Duration::from_secs(2 * 3600 - 1)));
        assert!(l.allow("b.example", t + Duration::from_secs(60)), "another site is its own");
        assert!(l.allow("a.example", t + Duration::from_secs(2 * 3600)), "after two hours again");
    }

    fn env() -> (TempDb, Arc<CanaryEnv>) {
        let (db, repo) = repo();
        (db, Arc::new(CanaryEnv::new(repo, HealthState::new(), PathBuf::from("."), "stable".into())))
    }

    #[tokio::test]
    async fn a_check_that_outlives_its_deadline_is_undecided_not_failed() {
        let (_f, repo) = repo();
        pool(&repo, &["https://news.example/a"]);
        let mut p = fake(&[("https://news.example/a", OK200)], &[]);
        p.delay = Duration::from_secs(30);
        let deadline = Instant::now() + Duration::from_millis(100);
        let started = Instant::now();
        assert_eq!(check_site(&repo, &p, "news.example", deadline).await, SiteCheck::Unknown);
        assert!(started.elapsed() < Duration::from_secs(5), "the slow check was dropped, not awaited");
    }

    #[test]
    fn a_verdict_older_than_the_sites_latest_change_is_not_applied() {
        let (_f, repo) = repo();
        let health = HealthState::new();
        degrade(&repo, "news.example");
        let fail = SiteCheck::Fail("no video".into());
        // The check finished before the last job failure / state change.
        let stale = Utc::now() - Dur::hours(1);
        assert_eq!(apply_check(&repo, &health, "news.example", &fail, stale), None);
        assert_eq!(repo.get_site("news.example").unwrap().unwrap().state, SiteState::Degraded);
        assert_eq!(apply_check(&repo, &health, "news.example", &SiteCheck::Pass, stale), None);
        assert_eq!(repo.get_site("news.example").unwrap().unwrap().consecutive_fail, 3, "a stale pass clears nothing");
        // A verdict that finished after it counts.
        assert_eq!(
            apply_check(&repo, &health, "news.example", &fail, later()),
            Some((SiteState::Degraded, SiteState::Broken))
        );
    }

    #[tokio::test]
    async fn the_update_task_waits_while_a_canary_run_holds_the_lock() {
        let (_db, env) = env();
        let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let held = env.lock_run().await; // a canary run is going
        let task = {
            let (env, ran) = (env.clone(), ran.clone());
            tokio::spawn(async move {
                with_canary_lock(&env, async { ran.store(true, Ordering::SeqCst) }).await;
            })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!ran.load(Ordering::SeqCst), "the swap must wait for the canary run");
        drop(held);
        task.await.unwrap();
        assert!(ran.load(Ordering::SeqCst));
        // And while it runs no canary run can start.
        let blocked = with_canary_lock(&env, async { env.run_lock.clone().try_lock_owned().is_err() }).await;
        assert!(blocked);
    }

    /// One canary article for `domain`; returns its page-status entry.
    fn pool_for(repo: &Repository, domain: &str) -> (&'static str, u16) {
        let url = format!("https://{domain}/a");
        repo.add_canary(domain, &url, None, Utc::now() - Dur::hours(1)).unwrap();
        (Box::leak(url.into_boxed_str()), OK200)
    }

    #[tokio::test]
    async fn a_nightly_pass_where_every_site_failed_changes_nothing_and_says_so() {
        let (_db, env) = env();
        let mut statuses = Vec::new();
        for d in ["a.example", "b.example", "c.example"] {
            degrade(&env.repo, d);
            statuses.push(pool_for(&env.repo, d));
        }
        let p = fake(&statuses, &[]); // every canary fails
        assert_eq!(nightly_with(&env, &p).await, (3, 0));
        for d in ["a.example", "b.example", "c.example"] {
            assert_eq!(env.repo.get_site(d).unwrap().unwrap().state, SiteState::Degraded, "{d} must not be marked");
        }
        let audit: Vec<String> = env.repo.get_recent_audit_logs(10).unwrap().into_iter().map(|a| a.message).collect();
        assert!(audit.iter().any(|m| m.contains("all 3 sites failed at once")), "{audit:?}");
    }

    #[tokio::test]
    async fn a_nightly_pass_that_is_not_an_outage_applies_each_verdict() {
        let (_db, env) = env();
        let mut statuses = Vec::new();
        let mut ok = Vec::new();
        for d in ["a.example", "b.example", "c.example"] {
            degrade(&env.repo, d);
            env.repo.set_site_state(d, SiteState::Broken).unwrap();
            let st = pool_for(&env.repo, d);
            statuses.push(st);
            ok.push((st.0, true));
        }
        let p = fake(&statuses, &ok); // all three work again
        nightly_with(&env, &p).await;
        for d in ["a.example", "b.example", "c.example"] {
            assert_eq!(env.repo.get_site(d).unwrap().unwrap().state, SiteState::Healthy, "{d}");
        }
    }

    #[tokio::test]
    async fn a_daytime_trigger_spawns_once_when_free_and_never_while_a_run_holds_the_lock() {
        let (_db, env) = env();

        // Lock free: the trigger counts and spawns; a second one within 2 h does not.
        env.trigger("free.example");
        assert_eq!(env.spawned.load(Ordering::SeqCst), 1);
        assert!(!env.limiter.lock().unwrap().allow("free.example", Instant::now()), "stamped");
        // The spawned check holds the lock briefly; wait for it to finish.
        drop(env.lock_run().await);
        env.trigger("free.example");
        assert_eq!(env.spawned.load(Ordering::SeqCst), 1, "same site within 2 h");

        // A run holds the lock: refused, and nothing is stamped.
        let held = env.lock_run().await;
        env.trigger("busy.example");
        assert_eq!(env.spawned.load(Ordering::SeqCst), 1);
        assert!(env.limiter.lock().unwrap().allow("busy.example", Instant::now()));
        drop(held);
    }


    #[tokio::test]
    async fn a_manual_check_confirms_a_degraded_site_broken_and_a_pass_restores_it() {
        let (_db, env) = env();
        degrade(&env.repo, "news.example");
        pool(&env.repo, &["https://news.example/a"]);
        let failing = fake(&[("https://news.example/a", OK200)], &[("https://news.example/a", false)]);
        manual_check(&env, &failing, "news.example").await;
        assert_eq!(env.repo.get_site("news.example").unwrap().unwrap().state, SiteState::Broken);

        let works = fake(&[("https://news.example/a", OK200)], &[("https://news.example/a", true)]);
        manual_check(&env, &works, "news.example").await;
        assert_eq!(env.repo.get_site("news.example").unwrap().unwrap().state, SiteState::Healthy);

        let said: Vec<String> = env.repo.get_recent_audit_logs(20).unwrap().into_iter().filter(|a| a.category == "ADMIN").map(|a| a.message).collect();
        assert!(said.iter().any(|m| m == "Manual check of news.example: works"), "{said:?}");
        assert!(said.iter().any(|m| m == "Manual check of news.example: no recent article gives video"), "{said:?}");
    }

    #[tokio::test]
    async fn run_now_starts_when_the_lock_is_free_and_says_busy_while_a_run_holds_it() {
        let (_db, env) = env();
        // A site with no canaries is undecided without any outside call.
        assert_eq!(env.run_now("quiet.example"), RunNow::Started);
        // The check holds the lock until it is done.
        drop(env.lock_run().await);
        let said: Vec<String> = env.repo.get_recent_audit_logs(5).unwrap().into_iter().map(|a| a.message).collect();
        assert_eq!(said, ["Manual check of quiet.example: undecided"]);

        let held = env.lock_run().await;
        assert_eq!(env.run_now("quiet.example"), RunNow::Busy, "a canary run or the yt-dlp update holds it");
        drop(held);
        assert!(env.repo.get_recent_audit_logs(5).unwrap().len() == 1, "the refused request ran nothing");
    }

    #[tokio::test]
    async fn run_now_from_the_panel_has_a_ten_minute_cooldown_per_site_and_a_refusal_does_not_start_it() {
        let (_db, env) = env();
        let runner = AdminRunner::new(env.clone());
        // Refused while the lock is held: no cooldown starts.
        let held = env.lock_run().await;
        assert_eq!(runner.run_now("quiet.example"), RunNow::Busy);
        drop(held);
        assert_eq!(runner.run_now("quiet.example"), RunNow::Started);
        drop(env.lock_run().await);
        match runner.run_now("quiet.example") {
            RunNow::Cooldown(secs) => assert!((590..=600).contains(&secs), "about ten minutes left: {secs}"),
            other => panic!("expected a cooldown, got {other:?}"),
        }
        assert_eq!(runner.run_now("other.example"), RunNow::Started, "another site is not held back");
        drop(env.lock_run().await);
    }
    #[tokio::test]
    async fn the_nightly_update_task_keeps_canary_runs_out_while_it_holds_the_lock() {
        let (_db, env) = env();
        let held = env.lock_run().await;
        assert!(env.run_lock.clone().try_lock_owned().is_err(), "a canary run cannot start under the swap");
        drop(held);
        assert!(env.run_lock.clone().try_lock_owned().is_ok());
    }
}
