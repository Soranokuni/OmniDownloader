//! The end-to-end benchmark runner (plan P6.4): the stored 65-link corpus run
//! through extraction only, or through download, transcode, rewrap and the
//! compliance check as well, with a report of timings, thresholds and the change
//! from the previous run of the same mode.
//!
//! A run is a measurement, not a job. It creates no queue row and uses no job
//! number, delivers nothing, touches neither the watchfolder nor the archive, and
//! works only in `temp/benchmark/{run}/{target}`, which is removed as soon as the
//! target is done. Stored results carry the host of the stream found, never its
//! address; the report names a target by number, domain and category.

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use clap::Subcommand;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{info, warn};

use omni_broadcast::downloader::{first_attempt_resolved, Choice, Probe, DownloadError, DownloadOpts, Downloader, FirstAttempt, PageScan};
use omni_broadcast::errors::{classify_download_error, ErrorCode};
use omni_broadcast::rewrapper::Rewrapper;
use omni_broadcast::transcoder::Transcoder;
use omni_core::benchmark::{self, Mode, ResultRow, Summary, TargetRow, CORPUS};
use omni_core::paths::AppPaths;
use omni_core::repository::{BenchStart, Repository};
use omni_core::update_gate::{GatePass, UpdateGate};
use omni_web::state::{BenchRequest, BenchStartError, BenchmarkStarter};

/// Targets run at once unless told otherwise.
pub const DEFAULT_PARALLEL: usize = 2;
/// The most that may run at once.
pub const MAX_PARALLEL: usize = 4;
/// A command-line run whose heartbeat is older than this was left by a process that died
/// (the heartbeat ticks every minute). A daemon's run does not wait: it dies with the daemon.
pub const STALE_AFTER_MINUTES: i64 = 5;
/// How often a running run says it is alive.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(60);
/// How often a run that waits for on-air jobs looks again.
const POLL_EVERY: Duration = Duration::from_secs(15);
/// A source longer than this is not transcoded: a benchmark is not the place for an hour of video.
pub const MAX_SOURCE_SECS: f64 = 600.0;
/// Finding the video may take this long, whatever the tools inside do.
const EXTRACT_BUDGET: Duration = Duration::from_secs(120);
/// yt-dlp's share of the extraction.
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);
/// The sniffer's share, in seconds.
const SNIFF_SECS: u64 = 25;
/// One download may take this long.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(15 * 60);
/// Encodes at once, however many targets run: the same default as the daemon's workers.
const ENCODES_AT_ONCE: usize = 2;

/// An error code (`ErrorCode::as_str`), never a message: a message may quote an address.
pub type StageResult<T> = std::result::Result<T, String>;

/// What extraction found. The stream address lives in memory only: it goes to
/// the download and no further; the run stores its host.
pub struct Extracted {
    /// `site-api`, `yt-dlp` or `sniffer`.
    pub method: String,
    pub candidates: i64,
    /// The video's length when extraction already knows it (yt-dlp does for most sites).
    pub duration_secs: Option<f64>,
    pub stream: String,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    pub cookies: Option<String>,
}

/// The four stages of a full run. The real ones call the tools the daemon's
/// workers use; tests use fakes.
#[async_trait]
pub trait BenchStages: Send + Sync {
    async fn extract(&self, target: &TargetRow) -> StageResult<Extracted>;
    /// Into `dir`, which is this target's own.
    async fn download(&self, target: &TargetRow, found: &Extracted, dir: &Path) -> StageResult<PathBuf>;
    /// The downloaded file's duration in seconds (a probe); `Err` is a file that cannot be read.
    async fn source_secs(&self, target: &TargetRow, source: &Path) -> StageResult<f64>;
    /// Transcode to the intermediate and rewrap it to RDD9 OP1a; returns the file and the source duration.
    async fn transcode(&self, target: &TargetRow, source: &Path, duration_secs: f64, dir: &Path) -> StageResult<(PathBuf, f64)>;
    /// The compliance check; `Ok(false)` is a file that failed it.
    async fn verify(&self, target: &TargetRow, mxf: &Path, duration_secs: f64) -> StageResult<bool>;
}

// ------------------------------------------------------------------ the run

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub mode: Mode,
    /// Target numbers, each in the corpus.
    pub targets: Vec<i64>,
    pub parallel: usize,
    /// Run although a job is running.
    pub force: bool,
}

#[derive(Debug)]
pub enum BeginError {
    /// Not now, and why (short, names no address).
    Refused(String),
    Failed(anyhow::Error),
}

/// Called when a target is done: the target, its result, how many are done, how many there are.
pub type OnTarget = Arc<dyn Fn(&TargetRow, &ResultRow, usize, usize) + Send + Sync>;

pub struct Benchmark {
    repo: Repository,
    paths: AppPaths,
    stages: Arc<dyn BenchStages>,
    /// Encodes in progress; taken before the on-air check so a waiting target holds its place.
    encoders: Arc<Semaphore>,
    poll: Duration,
    /// `daemon` or `cli`: who answers for the run if the process dies.
    owner: &'static str,
    /// The tool updater's gate; `None` where there is none (the command line).
    gate: Option<UpdateGate>,
}

/// What every target of one run shares: the courtesy to on-air jobs and the encoder slots.
struct Courtesy {
    repo: Repository,
    run_id: i64,
    /// `--force`: never wait.
    force: bool,
    poll: Duration,
    /// Targets waiting now; the run reads "paused" while there is one.
    waiting: AtomicUsize,
    encoders: Arc<Semaphore>,
    gate: Option<UpdateGate>,
}

impl Courtesy {
    /// A pass of the update gate, waiting while an update is pending (`None` without a gate).
    async fn pass(&self) -> Option<GatePass> {
        let gate = self.gate.as_ref()?;
        loop {
            if let Some(pass) = gate.try_enter() {
                return Some(pass);
            }
            tokio::time::sleep(self.poll).await;
        }
    }

    /// Return when no job is RUNNING (at once with `force`). The run's heartbeat stays
    /// fresh and its state reads paused while any target waits.
    async fn wait_for_quiet(&self, idx: i64) {
        if self.force {
            return;
        }
        let mut paused_here = false;
        loop {
            match self.repo.running_job_ids() {
                Ok(ids) if ids.is_empty() => break,
                Ok(ids) => {
                    if !paused_here {
                        paused_here = true;
                        if self.waiting.fetch_add(1, Ordering::SeqCst) == 0 {
                            info!("Benchmark run {}: paused for {} on-air job(s); target #{idx} waits", self.run_id, ids.len());
                        }
                    }
                    let _ = self.repo.benchmark_heartbeat(self.run_id, true, Utc::now());
                }
                // Cannot tell: do not block the run on it.
                Err(_) => break,
            }
            tokio::time::sleep(self.poll).await;
        }
        if paused_here && self.waiting.fetch_sub(1, Ordering::SeqCst) == 1 {
            info!("Benchmark run {}: no on-air job any more; resuming", self.run_id);
            let _ = self.repo.benchmark_heartbeat(self.run_id, false, Utc::now());
        }
    }

    fn heartbeat(&self) {
        let paused = self.waiting.load(Ordering::SeqCst) > 0;
        if let Err(e) = self.repo.benchmark_heartbeat(self.run_id, paused, Utc::now()) {
            warn!("Benchmark run {}: heartbeat not stored: {e:#}", self.run_id);
        }
    }
}

impl Benchmark {
    pub fn new(repo: Repository, paths: AppPaths, stages: Arc<dyn BenchStages>, owner: &'static str) -> Self {
        Self { repo, paths, stages, encoders: Arc::new(Semaphore::new(ENCODES_AT_ONCE)), poll: POLL_EVERY, owner, gate: None }
    }

    /// Take a pass of the updater's gate around every yt-dlp call, as the workers do, so the
    /// nightly swap of yt-dlp waits for a run and a run waits for the swap.
    pub fn with_gate(mut self, gate: UpdateGate) -> Self {
        self.gate = Some(gate);
        self
    }

    /// How often a run that waits for on-air jobs looks again (tests shorten it).
    #[cfg(test)]
    pub fn with_poll(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    /// Check that a run may start now and open it. A job running is a refusal unless
    /// `force`; so is another benchmark run (one at a time).
    pub fn begin(&self, opts: &RunOptions) -> std::result::Result<i64, BeginError> {
        if opts.parallel == 0 || opts.parallel > MAX_PARALLEL {
            return Err(BeginError::Refused(format!("Run 1 to {MAX_PARALLEL} targets at once.")));
        }
        self.repo.seed_benchmark_targets().map_err(BeginError::Failed)?;
        if !opts.force {
            let running = self.repo.running_job_ids().map_err(BeginError::Failed)?.len();
            if running > 0 {
                return Err(BeginError::Refused(format!(
                    "{running} job(s) are running; a benchmark would slow them down. Run it when the queue is quiet, or force it."
                )));
            }
        }
        let params = serde_json::json!({ "targets": opts.targets, "parallel": opts.parallel, "force": opts.force });
        let started = self
            .repo
            .begin_benchmark_run(opts.mode, &params.to_string(), Utc::now(), ChronoDuration::minutes(STALE_AFTER_MINUTES), self.owner)
            .map_err(BeginError::Failed)?;
        match started {
            BenchStart::Busy(id) => Err(BeginError::Refused(format!("Benchmark run {id} is still going; one at a time."))),
            BenchStart::Started(id) => {
                // Only this run exists now: whatever is left under temp/benchmark is from a run that died.
                let _ = std::fs::remove_dir_all(self.paths.benchmark_temp());
                if let Err(e) = std::fs::create_dir_all(self.paths.benchmark_temp()) {
                    warn!("Benchmark run {id}: no scratch directory: {e}");
                }
                Ok(id)
            }
        }
    }

    /// Run the targets of run `run_id` (from [`Self::begin`]), record each result as it
    /// comes, then finish the run and write its report files.
    ///
    /// Any error ends the run as `failed`, so it never sits `running` until its heartbeat lapses.
    pub async fn execute(&self, run_id: i64, opts: &RunOptions, on_target: OnTarget) -> Result<Summary> {
        let result = self.execute_inner(run_id, opts, on_target).await;
        if let Err(e) = &result {
            warn!("Benchmark run {run_id} failed: {e:#}");
            let _ = self.repo.finish_benchmark_run(run_id, "failed", None, Utc::now());
        }
        result
    }

    async fn execute_inner(&self, run_id: i64, opts: &RunOptions, on_target: OnTarget) -> Result<Summary> {
        let chosen: Vec<TargetRow> =
            self.repo.benchmark_targets()?.into_iter().filter(|t| opts.targets.contains(&t.idx)).collect();
        let total = chosen.len();
        info!("Benchmark run {run_id}: {} mode, {total} target(s), {} at once", opts.mode.as_str(), opts.parallel);

        let courtesy = Arc::new(Courtesy {
            repo: self.repo.clone(),
            run_id,
            force: opts.force,
            poll: self.poll,
            waiting: AtomicUsize::new(0),
            encoders: self.encoders.clone(),
            gate: self.gate.clone(),
        });
        // Alive, whatever the targets are doing: a long download is not a dead run.
        let beat = {
            let courtesy = courtesy.clone();
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(HEARTBEAT_EVERY).await;
                    courtesy.heartbeat();
                }
            })
        };
        let gate = Arc::new(Semaphore::new(opts.parallel.clamp(1, MAX_PARALLEL)));
        let done = Arc::new(AtomicUsize::new(0));
        let mut set = JoinSet::new();
        for target in chosen {
            let (gate, done, on_target) = (gate.clone(), done.clone(), on_target.clone());
            let courtesy = courtesy.clone();
            let (stages, repo) = (self.stages.clone(), self.repo.clone());
            let dir = self.paths.benchmark_target_temp(run_id, target.idx);
            let mode = opts.mode;
            set.spawn(async move {
                let _slot = gate.acquire_owned().await;
                let mut row = run_target(stages.as_ref(), mode, &target, &dir, &courtesy).await;
                row.run_id = run_id;
                if let Err(e) = repo.record_benchmark_result(&row) {
                    warn!("Benchmark run {run_id}: result of #{} not stored: {e:#}", target.idx);
                }
                courtesy.heartbeat();
                let n = done.fetch_add(1, Ordering::SeqCst) + 1;
                on_target(&target, &row, n, total);
            });
        }
        let mut broken = false;
        while let Some(joined) = set.join_next().await {
            if let Err(e) = joined {
                warn!("Benchmark run {run_id}: a target's task ended abnormally: {e}");
                broken = true;
            }
        }
        beat.abort();
        // The per-target directories are gone already; this takes the run's own.
        let _ = std::fs::remove_dir(self.paths.benchmark_temp().join(run_id.to_string()));

        let report = benchmark::build_report(&self.repo, run_id)?.context("the run vanished")?;
        let summary = report.summary;
        let state = if broken { "failed" } else { "done" };
        self.repo.finish_benchmark_run(run_id, state, Some(&serde_json::to_string(&summary)?), Utc::now())?;
        if let Err(e) = self.write_report(run_id) {
            warn!("Benchmark run {run_id}: report files not written: {e:#}");
        }
        info!("Benchmark run {run_id} {state}");
        Ok(summary)
    }

    /// `report.json` and `report.md` in `data/benchmarks/{run}/`, from the stored rows.
    pub fn write_report(&self, run_id: i64) -> Result<PathBuf> {
        let report = benchmark::build_report(&self.repo, run_id)?.context("There is no such benchmark run.")?;
        let dir = self.paths.benchmark_report_dir(run_id);
        std::fs::create_dir_all(&dir).with_context(|| format!("Could not create {dir:?}"))?;
        std::fs::write(dir.join("report.json"), serde_json::to_string_pretty(&report)?)?;
        std::fs::write(dir.join("report.md"), benchmark::markdown(&report))?;
        Ok(dir)
    }
}

/// One target through the stages `mode` asks for. The scratch directory is removed
/// on every way out; nothing else is touched.
async fn run_target(stages: &dyn BenchStages, mode: Mode, target: &TargetRow, dir: &Path, courtesy: &Courtesy) -> ResultRow {
    let started = Instant::now();
    let mut row = ResultRow { target_id: target.id, ..Default::default() };

    let t = Instant::now();
    // yt-dlp may not be swapped under the probe.
    let pass = courtesy.pass().await;
    let found = match tokio::time::timeout(EXTRACT_BUDGET, stages.extract(target)).await {
        Ok(r) => r,
        Err(_) => Err(ErrorCode::ExtractTimeout.as_str().to_string()),
    };
    drop(pass);
    row.extract_ms = Some(t.elapsed().as_millis() as i64);
    let found = match found {
        Ok(f) => {
            row.extract_ok = Some(true);
            row.method = Some(f.method.clone());
            row.candidates = Some(f.candidates);
            row.stream_host = benchmark::stream_host(&f.stream);
            Some(f)
        }
        Err(code) => {
            row.extract_ok = Some(false);
            row.error_code = Some(code);
            None
        }
    };

    if let (Mode::Full, Some(found)) = (mode, found) {
        match tokio::fs::create_dir_all(dir).await {
            Ok(()) => full_stages(stages, target, &found, dir, &mut row, courtesy).await,
            Err(_) => {
                row.download_ok = Some(false);
                row.error_code = Some(ErrorCode::PipelineFailed.as_str().to_string());
            }
        }
        if let Err(e) = tokio::fs::remove_dir_all(dir).await {
            if e.kind() != std::io::ErrorKind::NotFound {
                warn!("Benchmark: scratch directory of #{} not removed: {e}", target.idx);
            }
        }
    }
    row.total_ms = Some(started.elapsed().as_millis() as i64);
    row
}

async fn full_stages(stages: &dyn BenchStages, target: &TargetRow, found: &Extracted, dir: &Path, row: &mut ResultRow, courtesy: &Courtesy) {
    // Already known to be too long: nothing is downloaded.
    if found.duration_secs.is_some_and(|d| d > MAX_SOURCE_SECS) {
        row.error_code = Some(ErrorCode::SourceTooLong.as_str().to_string());
        return;
    }
    // On-air jobs come first: nothing is downloaded while one is running.
    courtesy.wait_for_quiet(target.idx).await;
    // The pass is taken after the wait for quiet, and held for the download only.
    let pass = courtesy.pass().await;
    let t = Instant::now();
    let downloaded = stages.download(target, found, dir).await;
    drop(pass);
    let source = match downloaded {
        Ok(p) => {
            row.download_ok = Some(true);
            row.download_ms = Some(t.elapsed().as_millis() as i64);
            p
        }
        Err(code) => {
            row.download_ok = Some(false);
            row.download_ms = Some(t.elapsed().as_millis() as i64);
            row.error_code = Some(code);
            return;
        }
    };
    let duration = match stages.source_secs(target, &source).await {
        Ok(d) if d > MAX_SOURCE_SECS => {
            // Not a compliance failure: nothing was made to check.
            row.error_code = Some(ErrorCode::SourceTooLong.as_str().to_string());
            return;
        }
        Ok(d) => d,
        Err(code) => {
            row.error_code = Some(code);
            return;
        }
    };
    // An encoder slot first, then the on-air check: a target that waited for a slot must look again.
    let Ok(encoder) = courtesy.encoders.clone().acquire_owned().await else {
        row.error_code = Some(ErrorCode::PipelineFailed.as_str().to_string());
        return;
    };
    courtesy.wait_for_quiet(target.idx).await;
    let t = Instant::now();
    let transcoded = stages.transcode(target, &source, duration, dir).await;
    drop(encoder);
    row.transcode_ms = Some(t.elapsed().as_millis() as i64);
    let (mxf, duration) = match transcoded {
        Ok(v) => v,
        Err(code) => {
            row.error_code = Some(code);
            return;
        }
    };
    match stages.verify(target, &mxf, duration).await {
        Ok(true) => row.verify_ok = Some(true),
        Ok(false) => {
            row.verify_ok = Some(false);
            row.error_code = Some(ErrorCode::ComplianceFailed.as_str().to_string());
        }
        // A file that cannot be checked is not delivered by the pipeline either.
        Err(code) => {
            row.verify_ok = Some(false);
            row.error_code = Some(code);
        }
    }
}

/// At daemon start-up: a run whose heartbeat stopped is abandoned, and when no live run
/// remains, whatever it left under `temp/benchmark` goes (the whole directory).
pub fn startup_sweep(repo: &Repository, paths: &AppPaths) {
    match repo.abandon_stale_benchmark_runs(Utc::now(), ChronoDuration::minutes(STALE_AFTER_MINUTES)) {
        Ok(0) => {
            if let Err(e) = std::fs::remove_dir_all(paths.benchmark_temp()) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!("Benchmark scratch directory not removed: {e}");
                }
            }
        }
        Ok(_) => {}
        Err(e) => warn!("Benchmark runs not checked: {e:#}"),
    }
}

/// From the daemon's retention task: a run that was killed leaves cookie copies under
/// `temp/benchmark`. With no run `running`, the whole directory goes.
pub fn sweep_idle_scratch(repo: &Repository, paths: &AppPaths) {
    match repo.benchmark_running_count() {
        Ok(0) => {
            if let Err(e) = std::fs::remove_dir_all(paths.benchmark_temp()) {
                if e.kind() != std::io::ErrorKind::NotFound {
                    warn!("Benchmark scratch directory not removed: {e}");
                }
            }
        }
        Ok(_) => {}
        Err(e) => warn!("Benchmark runs not checked: {e:#}"),
    }
}

// ------------------------------------------------------------------ the admin's start button

/// Starts a run in the background for the admin's Benchmarks card.
pub struct Starter {
    bench: Arc<Benchmark>,
    /// A run this process spawned is still going.
    alive: Arc<std::sync::atomic::AtomicBool>,
}

impl Starter {
    pub fn new(bench: Arc<Benchmark>) -> Self {
        Self { bench, alive: Arc::new(std::sync::atomic::AtomicBool::new(false)) }
    }
}

impl BenchmarkStarter for Starter {
    fn start(&self, request: BenchRequest) -> std::result::Result<i64, BenchStartError> {
        let opts = RunOptions {
            mode: request.mode,
            targets: request.targets,
            parallel: request.parallel as usize,
            force: request.force,
        };
        if self.alive.swap(true, Ordering::SeqCst) {
            return Err(BenchStartError::Refused("A benchmark run is still going; one at a time.".into()));
        }
        let run_id = match self.bench.begin(&opts) {
            Ok(id) => id,
            Err(BeginError::Refused(why)) => {
                self.alive.store(false, Ordering::SeqCst);
                return Err(BenchStartError::Refused(why));
            }
            Err(BeginError::Failed(e)) => {
                self.alive.store(false, Ordering::SeqCst);
                warn!("Benchmark could not start: {e:#}");
                return Err(BenchStartError::Failed);
            }
        };
        let alive = self.alive.clone();
        let bench = self.bench.clone();
        let notify = request.notify;
        tokio::spawn(async move {
            let each = notify.clone();
            let on_target: OnTarget = Arc::new(move |_, _, _, _| each());
            // `execute` ends the run as failed on any error.
            let _ = bench.execute(run_id, &opts, on_target).await;
            alive.store(false, Ordering::SeqCst);
            notify();
        });
        Ok(run_id)
    }
}

// ------------------------------------------------------------------ the real stages

/// Where the tools are.
#[derive(Clone)]
pub struct Tools {
    pub ytdl: PathBuf,
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub bmxtranswrap: PathBuf,
}

pub struct RealStages {
    repo: Repository,
    tools: Tools,
    /// Where a jar's scratch copy goes while a link is probed.
    scratch: PathBuf,
}

impl RealStages {
    pub fn new(repo: Repository, tools: Tools, scratch: PathBuf) -> Self {
        Self { repo, tools, scratch }
    }

    fn slug(target: &TargetRow) -> String {
        format!("BENCH_{}", target.idx)
    }

    /// The number the tools are given where they want a job's: negative, so it can never be a
    /// queue job's, and only used for log lines and file names inside the target's own directory.
    fn tool_id(target: &TargetRow) -> i64 {
        -target.idx
    }
}

// ------------------------------------------------------------------ extraction, as a job does it

/// The four questions extraction asks, so the order they are asked in can be tested.
#[async_trait]
trait ExtractSteps: Send + Sync {
    /// The video the page's own API names (P3.8).
    async fn site_api(&self, url: &str) -> Option<String>;
    /// yt-dlp's page scan (`--flat-playlist -J`).
    async fn page_scan(&self, url: &str) -> PageScan;
    /// Where the page's address ends up after redirects, when that can be told.
    async fn final_url(&self, url: &str) -> Option<String>;
    /// yt-dlp's own extractors on the link: its length, or the reason it cannot.
    async fn ytdlp(&self, url: &str) -> std::result::Result<Option<f64>, String>;
    /// What the browser found on the page; `None` when it finds no stream.
    async fn sniff(&self, url: &str) -> Option<Sniff>;
    /// yt-dlp's say on one of the browser's candidates, with the browser's session.
    async fn probe(&self, candidate: &str, session: &Sniff) -> Probe;
    /// Which of `urls` the site shows on several of its articles (P3.16), video hash to the number of
    /// articles. Read only: the benchmark never records. `seen` is the browser's look at the page.
    async fn site_wide(&self, _page: &str, _urls: &[String], _seen: Option<&Sniff>) -> std::collections::HashMap<String, usize> {
        std::collections::HashMap::new()
    }
}

/// What a sniff found: its candidates in rank order (address, held), its own primary, the
/// session the download uses, and the address the browser ended on.
struct Sniff {
    streams: Vec<(String, bool)>,
    primary: String,
    referer: String,
    user_agent: String,
    cookies: Option<String>,
    final_url: Option<String>,
    /// The page's own canonical address, for its identity among the site's articles (P3.16).
    canonical: Option<String>,
}

/// How the browser's step ended.
enum SniffStep {
    Found(Extracted),
    /// No usable stream: yt-dlp may still be asked.
    Nothing,
    /// Every candidate is gone or live: the job ends with this code, yt-dlp is not asked.
    Ended(ErrorCode),
}

/// The browser's step as `run_job` takes it (P3.15): every candidate is asked before the
/// video is chosen, a gone or live one is skipped, and a page the browser ended on a video
/// platform's page counts as finding nothing when the browser was asked first.
async fn sniff_and_choose(steps: &dyn ExtractSteps, url: &str, sniff_first: bool, refused: &mut Vec<String>) -> SniffStep {
    let Some(s) = steps.sniff(url).await else { return SniffStep::Nothing };
    if sniff_first && omni_broadcast::downloader::landed_on_platform(s.final_url.as_deref()) {
        return SniffStep::Nothing;
    }
    let all: Vec<String> = s.streams.iter().map(|(u, _)| u.clone()).collect();
    // A video the site shows on its other articles is never the job's: read from the memory a job
    // fills, not added to it (P3.16).
    let wide = steps.site_wide(url, &all, Some(&s)).await;
    let (urls, furniture) = omni_core::site_videos::split_site_wide(&all, &wide);
    let held = |u: &str| s.streams.iter().any(|(k, h)| k == u && *h);
    // Furniture took the article's own video and no own one is left: none is taken for the article's.
    let (urls, dropped_held) = crate::site_memory::leave_nothing_held(furniture.iter().any(|f| !held(&f.url)), urls, held);
    // yt-dlp's own pick afterwards may not take the held ones this left out either.
    refused.extend(dropped_held);
    if urls.is_empty() && !furniture.is_empty() {
        // The browser took nothing: yt-dlp's own pick is asked next, and refused if it is furniture too, as in a job.
        return SniffStep::Nothing;
    }
    let cands = omni_broadcast::downloader::sniffed_cands(&urls, |u| s.streams.iter().any(|(k, held)| k == u && *held));
    let chosen = if cands.is_empty() {
        Some(s.primary.clone())
    } else {
        let session = &s;
        match omni_broadcast::downloader::choose_primary(&cands, omni_broadcast::downloader::MAX_LIVE_PROBES, |u| async move {
            steps.probe(&u, session).await
        })
        .await
        {
            Choice::Primary { url, .. } => Some(url),
            Choice::AllDead { code, .. } => return SniffStep::Ended(code),
            Choice::AllLive { .. } => return SniffStep::Ended(ErrorCode::LiveStream),
            Choice::Nothing => None,
        }
    };
    match chosen {
        Some(stream) => SniffStep::Found(Extracted {
            method: "sniffer".into(),
            candidates: s.streams.len() as i64,
            duration_secs: None,
            stream,
            referer: Some(s.referer.clone()),
            user_agent: Some(s.user_agent.clone()),
            cookies: s.cookies.clone(),
        }),
        None => SniffStep::Nothing,
    }
}

/// What a job asks first: the site's API; then yt-dlp's page scan, which says whether yt-dlp
/// reads the page only through its generic scan (P3.15). Then the browser chooses the video
/// ("sniffer") and yt-dlp is asked only when it finds none; otherwise yt-dlp first and the
/// browser for the pages it cannot read, as `run_job` does.
async fn extract_with(steps: &dyn ExtractSteps, url: &str) -> StageResult<Extracted> {
    if let Some(video) = steps.site_api(url).await {
        return Ok(Extracted {
            method: "site-api".into(),
            candidates: 1,
            duration_secs: None,
            stream: video,
            referer: Some(url.to_string()),
            user_agent: None,
            cookies: None,
        });
    }
    let mut refused: Vec<String> = Vec::new();
    let is_platform = omni_broadcast::downloader::is_video_platform(url);
    let scan = if is_platform { PageScan::default() } else { steps.page_scan(url).await };
    let route = first_attempt_resolved(false, is_platform, false, &scan, url, |u| async move { steps.final_url(&u).await }).await;
    if route != FirstAttempt::YtDlp {
        match sniff_and_choose(steps, url, route == FirstAttempt::SnifferFirst, &mut refused).await {
            SniffStep::Found(found) => return Ok(found),
            SniffStep::Ended(code) => return Err(code.as_str().to_string()),
            SniffStep::Nothing => {}
        }
        if route == FirstAttempt::SnifferOnly {
            return Err(ErrorCode::NoStreamFound.as_str().to_string());
        }
    }
    // yt-dlp's own pick is a video the site shows on its other articles: not taken, as in a job (P3.16).
    let refused_media = scan.media_urls.iter().any(|m| refused.iter().any(|r| omni_core::site_videos::video_key(r) == omni_core::site_videos::video_key(m)));
    let pick_is_furniture = !scan.media_urls.is_empty() && (refused_media || !steps.site_wide(url, &scan.media_urls, None).await.is_empty());
    let why = if pick_is_furniture {
        ErrorCode::NoStreamFound
    } else {
        match steps.ytdlp(url).await {
            Ok(duration_secs) => {
                return Ok(Extracted {
                    method: "yt-dlp".into(),
                    candidates: 1,
                    duration_secs,
                    stream: url.to_string(),
                    referer: None,
                    user_agent: None,
                    cookies: None,
                })
            }
            Err(reason) => classify_download_error(&reason),
        }
    };
    // The browser, for the pages yt-dlp cannot read, as a job would; it already looked
    // when it was asked first.
    if route == FirstAttempt::SnifferFirst || !why.should_sniff(url) {
        return Err(why.as_str().to_string());
    }
    match sniff_and_choose(steps, url, false, &mut refused).await {
        SniffStep::Found(found) => Ok(found),
        SniffStep::Ended(code) => Err(code.as_str().to_string()),
        SniffStep::Nothing => Err(ErrorCode::NoStreamFound.as_str().to_string()),
    }
}

#[async_trait]
impl ExtractSteps for RealStages {
    async fn site_api(&self, url: &str) -> Option<String> {
        // A failed lookup just takes the usual way.
        omni_browser::pages::resolve(url).await.ok().flatten()
    }

    async fn page_scan(&self, url: &str) -> PageScan {
        let jar = omni_broadcast::cookie_file::for_url(&self.repo, url, &self.scratch, true);
        let scan = Downloader::new(&self.tools.ytdl).with_cookie_jar(jar.as_ref()).page_videos(url, PROBE_TIMEOUT).await;
        drop(jar);
        scan
    }

    async fn final_url(&self, url: &str) -> Option<String> {
        crate::repair::final_page_url(url).await
    }

    async fn ytdlp(&self, url: &str) -> std::result::Result<Option<f64>, String> {
        // yt-dlp's own extractors, with the site's cookie jar when there is one.
        let jar = omni_broadcast::cookie_file::for_url(&self.repo, url, &self.scratch, true);
        let probed = Downloader::new(&self.tools.ytdl).with_cookie_jar(jar.as_ref()).probe_info(url, PROBE_TIMEOUT).await;
        drop(jar);
        probed.map(|info| info.duration_secs)
    }

    async fn sniff(&self, url: &str) -> Option<Sniff> {
        let sniffed = omni_browser::sniffer::StreamSniffer::extract_with_evidence(url, SNIFF_SECS).await;
        match sniffed.media {
            Ok(bundle) => Some(Sniff {
                streams: bundle
                    .all_streams
                    .iter()
                    .map(|u| (u.clone(), omni_browser::sniffer::is_demoted(&bundle.meta, u)))
                    .collect(),
                primary: bundle.primary_stream,
                referer: bundle.referer,
                user_agent: bundle.user_agent,
                cookies: bundle.cookies,
                final_url: sniffed.snapshot.final_url,
                canonical: sniffed.snapshot.canonical,
            }),
            Err(_) => None,
        }
    }

    async fn site_wide(&self, page: &str, urls: &[String], seen: Option<&Sniff>) -> std::collections::HashMap<String, usize> {
        let snapshot = seen.map(|s| {
            let mut snap = omni_browser::PageSnapshot::new(page);
            snap.final_url = s.final_url.clone();
            snap.canonical = s.canonical.clone();
            snap
        });
        crate::site_memory::recall(&self.repo, page, snapshot.as_ref(), urls)
    }

    async fn probe(&self, candidate: &str, session: &Sniff) -> Probe {
        let jar = omni_broadcast::cookie_file::for_url(&self.repo, candidate, &self.scratch, true);
        let probed = Downloader::new(&self.tools.ytdl)
            .with_cookie_jar(jar.as_ref())
            .probe_candidate(candidate, Some(&session.referer), Some(&session.user_agent), session.cookies.as_deref(), PROBE_TIMEOUT.min(Duration::from_secs(15)))
            .await;
        drop(jar);
        probed
    }
}

#[async_trait]
impl BenchStages for RealStages {
    async fn extract(&self, target: &TargetRow) -> StageResult<Extracted> {
        extract_with(self, target.url.as_str()).await
    }

    async fn download(&self, target: &TargetRow, found: &Extracted, dir: &Path) -> StageResult<PathBuf> {
        let jar = omni_broadcast::cookie_file::for_urls(&self.repo, &[found.stream.as_str()], dir, false);
        let result = Downloader::new(&self.tools.ytdl)
            .download(
                Self::tool_id(target),
                &found.stream,
                dir,
                DownloadOpts {
                    referer: found.referer.as_deref(),
                    user_agent: found.user_agent.as_deref(),
                    cookie_header: found.cookies.as_deref(),
                    cookie_jar: jar.as_ref().map(|f| f.path()),
                    cookie_values: jar.as_ref().map(|f| f.values()).unwrap_or(&[]),
                    timeout: DOWNLOAD_TIMEOUT,
                    ..Default::default()
                },
                |_| {},
            )
            .await;
        drop(jar);
        result.map_err(|e| {
            e.downcast_ref::<DownloadError>().map(|d| d.code).unwrap_or(ErrorCode::PipelineFailed).as_str().to_string()
        })
    }

    async fn source_secs(&self, _target: &TargetRow, source: &Path) -> StageResult<f64> {
        let probe = Transcoder::new(&self.tools.ffmpeg, &self.tools.ffprobe)
            .probe_source(source)
            .await
            .map_err(|_| ErrorCode::ProbeFailed.as_str().to_string())?;
        Ok(probe.duration_secs)
    }

    async fn transcode(&self, target: &TargetRow, source: &Path, duration_secs: f64, dir: &Path) -> StageResult<(PathBuf, f64)> {
        let transcoder = Transcoder::new(&self.tools.ffmpeg, &self.tools.ffprobe);
        omni_broadcast::pipeline::room_for(duration_secs, dir, dir).map_err(|_| ErrorCode::LowDisk.as_str().to_string())?;
        let (intermediate, duration) = transcoder
            .transcode(Self::tool_id(target), source, dir, |_| {})
            .await
            .map_err(|_| ErrorCode::TranscodeFailed.as_str().to_string())?;
        // The source has served its purpose; a long one is most of what the disk holds.
        let _ = tokio::fs::remove_file(source).await;
        let slug = Self::slug(target);
        let mxf = Rewrapper::new(&self.tools.bmxtranswrap)
            .rewrap_with_clip(Self::tool_id(target), &intermediate, dir, Some(&slug), None)
            .await
            .map_err(|_| ErrorCode::RewrapFailed.as_str().to_string())?;
        let _ = tokio::fs::remove_file(&intermediate).await;
        Ok((mxf, duration))
    }

    async fn verify(&self, target: &TargetRow, mxf: &Path, duration_secs: f64) -> StageResult<bool> {
        let slug = Self::slug(target);
        match omni_broadcast::verify::verify_mxf_with_clip(&self.tools.ffprobe, mxf, duration_secs, Some(&slug)).await {
            Ok(report) => Ok(report.pass),
            Err(_) => Err(ErrorCode::ComplianceFailed.as_str().to_string()),
        }
    }
}

// ------------------------------------------------------------------ the command line

#[derive(Subcommand)]
pub enum BenchAction {
    /// Run the corpus (or part of it) and write the report to data/benchmarks/{run}/
    Run {
        /// extract: find the video only. full: download, transcode, rewrap and verify, in a scratch directory.
        #[arg(long, default_value = "extract")]
        mode: String,
        /// Targets: a range, a list, or both (1-65, 1-5,9)
        #[arg(long, default_value = "1-65")]
        targets: String,
        /// Targets at once (1 to 4)
        #[arg(long, default_value_t = DEFAULT_PARALLEL)]
        parallel: usize,
        /// Run although a job is running
        #[arg(long)]
        force: bool,
    },
    /// List the runs so far
    List,
    /// Print the report of one run
    Report { id: i64 },
}

fn tool(configured: &Option<String>, name: &str, deps: &omni_core::dependencies::DependencyManager, fallback: &str) -> PathBuf {
    configured.as_ref().map(PathBuf::from).or_else(|| deps.find_binary(name)).unwrap_or_else(|| deps.unresolved_path(fallback))
}

/// `omni-ingest benchmark ...`. The result is whether the run's thresholds all passed
/// (`true` for `list` and `report`); the caller turns `false` into an exit code.
pub async fn cli(paths: &AppPaths, action: BenchAction) -> Result<bool> {
    let config = omni_core::config::AppConfig::load_from_file(&paths.config)
        .with_context(|| format!("Failed loading configuration from {:?}", paths.config))?;
    let repo = Repository::new(paths.resolve(&config.database_path))?;
    repo.seed_benchmark_targets()?;
    match action {
        BenchAction::List => {
            let runs = repo.benchmark_runs(50)?;
            if runs.is_empty() {
                println!("No benchmark run yet.");
            }
            for r in runs {
                let s: Option<Summary> = r.summary_json.as_deref().and_then(|j| serde_json::from_str(j).ok());
                println!(
                    "#{:<4} {:<8} {:<9} started {}  targets {}  thresholds {}",
                    r.id,
                    r.mode.as_str(),
                    r.state,
                    r.started_at.map(omni_core::timestamps::format).unwrap_or_default(),
                    s.as_ref().map(|s| s.targets_run.to_string()).unwrap_or_default(),
                    match s.and_then(|s| s.pass) {
                        Some(true) => "PASS",
                        Some(false) => "FAIL",
                        None => "",
                    }
                );
            }
            Ok(true)
        }
        BenchAction::Report { id } => {
            let report = benchmark::build_report(&repo, id)?.with_context(|| format!("There is no benchmark run {id}."))?;
            println!("{}", benchmark::markdown(&report));
            Ok(report.summary.pass != Some(false))
        }
        BenchAction::Run { mode, targets, parallel, force } => {
            let mode = Mode::parse(mode.trim()).context("The mode is extract or full.")?;
            let targets = benchmark::parse_targets(&targets, CORPUS.len() as i64).map_err(anyhow::Error::msg)?;
            let deps = omni_core::dependencies::DependencyManager::new(paths.resolve(&config.bin_dir));
            let tools = Tools {
                ytdl: tool(&config.ytdl_path, "yt-dlp", &deps, "yt-dlp.exe"),
                ffmpeg: tool(&config.ffmpeg_path, "ffmpeg", &deps, "ffmpeg.exe"),
                ffprobe: tool(&config.ffprobe_path, "ffprobe", &deps, "ffprobe.exe"),
                bmxtranswrap: tool(&config.bmxtranswrap_path, "bmxtranswrap", &deps, "bmxtranswrap.exe"),
            };
            let stages = Arc::new(RealStages::new(repo.clone(), tools, paths.benchmark_temp()));
            let bench = Benchmark::new(repo, paths.clone(), stages, "cli");
            let opts = RunOptions { mode, targets, parallel, force };
            let run_id = match bench.begin(&opts) {
                Ok(id) => id,
                Err(BeginError::Refused(why)) => anyhow::bail!("{why}"),
                Err(BeginError::Failed(e)) => return Err(e),
            };
            println!("Benchmark run {run_id}: {} mode, {} target(s), {} at once", mode.as_str(), opts.targets.len(), parallel);
            let on_target: OnTarget = Arc::new(|t, r, n, total| {
                println!(
                    "[{n:>2}/{total}] #{:<2} {:<18} extract {} {}{}",
                    t.idx,
                    t.domain,
                    match r.extract_ok {
                        Some(true) => "ok  ",
                        _ => "FAIL",
                    },
                    r.extract_ms.map(|m| format!("{:.1} s", m as f64 / 1000.0)).unwrap_or_default(),
                    r.error_code.as_deref().map(|c| format!("  {c}")).unwrap_or_default(),
                );
            });
            let summary = bench.execute(run_id, &opts, on_target).await?;
            println!("\nReport: {:?}", paths.benchmark_report_dir(run_id));
            for c in &summary.checks {
                println!(
                    "  {:<30} {} {:.1}{}  measured {}  {}",
                    c.name,
                    c.op,
                    c.limit,
                    c.unit,
                    c.value.map(|v| format!("{v:.1}{}", c.unit)).unwrap_or_else(|| "-".into()),
                    match c.pass {
                        Some(true) => "PASS",
                        Some(false) => "FAIL",
                        None => "n/a",
                    }
                );
            }
            Ok(summary.pass != Some(false))
        }
    }
}

#[cfg(test)]
#[path = "benchmark_tests.rs"]
mod tests;
