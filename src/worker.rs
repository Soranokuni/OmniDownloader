//! The job runner: one leased job through the pipeline (`run_job`), the approved-delivery
//! path, the yt-dlp and browser fallbacks, the failed-page evidence, and the error
//! classification; the lease holder sets every terminal status here.

use anyhow::Result;
use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use omni_broadcast::errors::ErrorCode;
use omni_broadcast::pipeline::BroadcastEngine;
use omni_browser::agent::{ComputerUseAgentPlaceholder, FileLockerResolver};
use omni_browser::evidence::{Evidence, EvidenceClass};
use omni_browser::sniffer::StreamSniffer;
use omni_core::config::RepairMode;
use omni_core::models::{JobStage, JobStatus};
use omni_core::repository::{DeliverySettled, Repository};
use omni_core::health::HealthState;
use omni_core::site_health::{SiteOutcome, SiteState};

use crate::{canary, egress, repair, site_memory};

/// How long a stop waits for jobs that have reached the watchfolder step.
/// With the runtime's 20 s shutdown it stays inside the service's stop wait
/// hint (`omni_service` STOP_WAIT_HINT, 60 s).
pub(crate) const SHUTDOWN_DELIVERY_BUDGET: Duration = Duration::from_secs(25);

/// A running job at DELIVER or later: stopping it now costs a stale staging
/// file or a second delivery, so a stop waits for it.
fn is_delivering(status: JobStatus, stage: JobStage) -> bool {
    status == JobStatus::Running && matches!(stage, JobStage::Deliver | JobStage::Archive | JobStage::Done)
}

/// Wait until no busy job is delivering, up to `budget`; how many still are.
/// Re-checked every poll, so a job that reaches DELIVER meanwhile is waited for too.
pub(crate) async fn wait_for_deliveries(
    busy: &omni_core::busy::BusyJobs,
    status_of: impl Fn(i64) -> Option<(JobStatus, JobStage)>,
    budget: Duration,
) -> usize {
    let deadline = tokio::time::Instant::now() + budget;
    let mut announced = false;
    loop {
        let delivering = busy
            .ids()
            .into_iter()
            .filter(|id| status_of(*id).is_some_and(|(status, stage)| is_delivering(status, stage)))
            .count();
        if delivering == 0 || tokio::time::Instant::now() >= deadline {
            return delivering;
        }
        if !announced {
            info!("Waiting up to {} s for {delivering} job(s) delivering to the watchfolder", budget.as_secs());
            announced = true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Lease renewal interval and lease length.
///
/// The lease outlives several missed heartbeats so a momentarily busy machine
/// does not lose a job mid-transcode, but is short enough that a crashed worker
/// is noticed within a couple of minutes rather than at the next restart.
pub(crate) const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
pub(crate) const LEASE_SECS: i64 = 180;
pub(crate) const LEASE_REAP_INTERVAL: Duration = Duration::from_secs(60);

/// Whether this worker still holds the job. A failed check counts as lost:
/// better to stop than to queue or deliver for a job MCR may have cancelled.
pub(crate) fn still_ours(repo: &Repository, job_id: i64, owner: &str) -> bool {
    repo.owns_lease(job_id, owner).unwrap_or(false)
}

/// How long a job run's internal error keeps the `workers` health line amber.
const PANIC_WINDOW: Duration = Duration::from_secs(24 * 60 * 60);

/// A job that hit an internal error on its first attempt is tried once more after this.
const PANIC_RETRY_SECS: i64 = 30;

/// What the worker loop shares with every job run (P11.8).
#[derive(Clone)]
pub(crate) struct JobRunCtx {
    pub(crate) repo: Repository,
    pub(crate) engine: Arc<BroadcastEngine>,
    pub(crate) health: HealthState,
    /// How often a run renews its lease ([`HEARTBEAT_INTERVAL`]; shorter in tests).
    pub(crate) heartbeat_interval: Duration,
    /// When job runs hit an internal error, oldest first.
    panics: Arc<Mutex<VecDeque<Instant>>>,
    /// The heartbeat of each run, so a test can see it has stopped.
    #[cfg(test)]
    heartbeats: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
}

impl JobRunCtx {
    pub(crate) fn new(repo: Repository, engine: Arc<BroadcastEngine>, health: HealthState) -> Self {
        Self {
            repo,
            engine,
            health,
            heartbeat_interval: HEARTBEAT_INTERVAL,
            panics: Arc::new(Mutex::new(VecDeque::new())),
            #[cfg(test)]
            heartbeats: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn panics(&self) -> std::sync::MutexGuard<'_, VecDeque<Instant>> {
        self.panics.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Bring the `workers` line up to date: amber while a run hit an internal
    /// error in the last 24 h. Called on every lease, so it turns green again.
    pub(crate) fn refresh_health(&self) {
        let check = workers_check(&mut self.panics(), Instant::now());
        self.health.set_if_changed(omni_core::health::checks::WORKERS, check);
    }

    fn note_panic(&self) {
        self.panics().push_back(Instant::now());
        self.refresh_health();
    }
}

/// The `workers` line for the internal errors in `panics`, dropping those older than 24 h.
fn workers_check(panics: &mut VecDeque<Instant>, now: Instant) -> omni_core::health::Check {
    while panics.front().is_some_and(|t| now.saturating_duration_since(*t) >= PANIC_WINDOW) {
        panics.pop_front();
    }
    match panics.len() {
        0 => omni_core::health::Check::ok("running"),
        n => omni_core::health::Check::degraded(format!("{n} job(s) hit an internal error in the last 24 h")),
    }
}

/// What one leased job holds until its run is over, a panic included.
pub(crate) struct LeasedJob {
    pub(crate) job_id: i64,
    pub(crate) owner: String,
    pub(crate) busy: omni_core::busy::BusyGuard,
    pub(crate) gate_pass: omni_core::update_gate::GatePass,
    pub(crate) job_slot: tokio::sync::OwnedSemaphorePermit,
    pub(crate) download_slot: Option<tokio::sync::OwnedSemaphorePermit>,
}

/// Ends a run's hold on its job when dropped, whichever way the run ends:
/// fires the run's token, stops the lease heartbeat, and stops the job body
/// if the run itself is dropped (a daemon stop) while the body still runs.
struct LeaseKeeper {
    cancel: CancellationToken,
    heartbeat: tokio::task::JoinHandle<()>,
    body: Option<tokio::task::AbortHandle>,
}

impl Drop for LeaseKeeper {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.heartbeat.abort();
        if let Some(body) = &self.body {
            body.abort();
        }
    }
}

/// Keep the lease alive while we work. If it stops succeeding we have lost the
/// job -- the reaper requeued it, or an operator cancelled it -- and must stop
/// rather than deliver a file for a job somebody else now owns.
fn spawn_heartbeat(
    repo: Repository,
    job_id: i64,
    owner: String,
    cancel: CancellationToken,
    every: Duration,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.tick().await; // fires immediately; skip it
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tick.tick() => {
                    match repo.heartbeat(job_id, &owner, LEASE_SECS) {
                        Ok(true) => {}
                        Ok(false) => {
                            warn!("Job #{job_id}: lease lost, abandoning work");
                            cancel.cancel();
                            break;
                        }
                        Err(e) => warn!("Job #{job_id}: heartbeat failed: {e:?}"),
                    }
                }
            }
        }
    })
}

/// Run one leased job's `body` (in the daemon, [`run_job`]) under its lease
/// heartbeat, and release everything the job holds when it ends (P11.8).
///
/// The body runs as its own task, so a panic in it is caught here: the
/// heartbeat stops, and a job still ours goes back to the queue once, then to
/// review, instead of staying RUNNING under a lease that never expires.
pub(crate) async fn run_leased_job<F, Fut>(ctx: &JobRunCtx, leased: LeasedJob, body: F)
where
    F: FnOnce(CancellationToken) -> Fut + Send,
    Fut: Future<Output = Result<()>> + Send + 'static,
{
    let LeasedJob { job_id, owner, busy, gate_pass, job_slot, download_slot } = leased;
    // Held for the whole job, including every error path: dropping it is what
    // tells a waiting updater the downloader is idle.
    let _gate_pass = gate_pass;
    // Until this run is done with the job, MCR cannot queue it again (a
    // cancelled job stops only at its next stage). Its token is the job run's:
    // MCR's cancel and a lost lease both fire it, and it kills the running
    // tool (plan P1.15).
    let cancel = busy.token();
    let _busy = busy;
    // Handed back on every exit, a panic in the body included.
    let _slot_guard = download_slot.map(|slot| {
        ctx.engine.hold_download_slot(job_id, &owner, slot);
        ctx.engine.download_slot_guard(job_id, &owner)
    });

    let mut keeper = LeaseKeeper {
        cancel: cancel.clone(),
        heartbeat: spawn_heartbeat(ctx.repo.clone(), job_id, owner.clone(), cancel.clone(), ctx.heartbeat_interval),
        body: None,
    };
    #[cfg(test)]
    ctx.heartbeats.lock().unwrap_or_else(|p| p.into_inner()).push(keeper.heartbeat.abort_handle());
    let task = tokio::spawn(body(cancel));
    keeper.body = Some(task.abort_handle());
    let joined = task.await;
    // The heartbeat stops before anything below releases the lease.
    drop(keeper);

    match joined {
        Ok(Ok(())) => {}
        Ok(Err(e)) => bookkeeping_failed(&ctx.repo, job_id, &owner, &e),
        Err(join) if join.is_panic() => after_panic(ctx, job_id, &owner, join.into_panic()).await,
        // Cancelled, which only a runtime shutdown does: the row is left RUNNING with
        // its delivery marker for start-up recovery to judge, as a crash would.
        Err(_) => info!("Job #{job_id}: the run was stopped before it finished; left to start-up recovery"),
    }

    drop(job_slot);
}

/// run_job handles its own failures; reaching here means the bookkeeping
/// itself failed, so release the lease rather than leaving the job RUNNING
/// until the reaper notices.
fn bookkeeping_failed(repo: &Repository, job_id: i64, owner: &str, e: &anyhow::Error) {
    error!("Job #{job_id}: worker bookkeeping failed: {e:?}");
    let _ = repo.finish(
        job_id,
        owner,
        JobStatus::RequiresReview,
        Some(ErrorCode::PipelineFailed.as_str()),
        Some(&e.to_string()),
        None,
    );
}

/// The job's body panicked. One that is no longer ours (cancelled, reaped,
/// finished before the panic) is left as it is. One at DELIVER or later is
/// never requeued: the video may already be on air, so the rule start-up
/// recovery applies decides (completed, or review to check Dalet). Any other
/// is tried once more after [`PANIC_RETRY_SECS`], then goes to review. The
/// job's event is written only after its row was.
async fn after_panic(ctx: &JobRunCtx, job_id: i64, owner: &str, payload: Box<dyn std::any::Any + Send>) {
    let text = panic_text(payload.as_ref());
    error!("Job #{job_id}: internal error while running the job: {text}");
    ctx.note_panic();
    let repo = &ctx.repo;
    // A row that cannot be read is treated as delivering: never a requeue on a guess.
    let (delivering, attempts) = match repo.get_job(job_id) {
        Ok(Some(j)) if j.lease_owner.as_deref() != Some(owner) || j.status != JobStatus::Running => {
            info!("Job #{job_id}: no longer ours after the internal error; nothing written");
            return;
        }
        Ok(Some(j)) => (is_delivering(j.status, j.stage), Some(j.attempts)),
        Ok(None) => return,
        Err(e) => {
            warn!("Job #{job_id}: the job could not be read after the internal error: {e:#}");
            (true, None)
        }
    };

    let event = if delivering {
        // Looks at the watchfolder, which may be a slow share: off the async threads.
        let settle = {
            let (repo, owner) = (repo.clone(), owner.to_string());
            tokio::task::spawn_blocking(move || repo.settle_after_delivery_panic(job_id, &owner)).await
        };
        match settle {
            Ok(Ok(DeliverySettled::Completed)) => {
                info!("Job #{job_id}: the video was already in the watchfolder; finished, not requeued");
                Some(format!("Internal error while running the job, after the video was delivered: {text}"))
            }
            Ok(Ok(DeliverySettled::Review)) => {
                warn!("Job #{job_id}: internal error while delivering; sent to review, not requeued");
                Some(format!("Internal error while running the job, during delivery: {text}"))
            }
            Ok(Ok(DeliverySettled::NotOurs)) => None,
            Ok(Err(e)) => {
                error!("Job #{job_id}: could not settle the delivery after the internal error: {e:#}");
                None
            }
            Err(e) => {
                error!("Job #{job_id}: could not settle the delivery after the internal error: {e}");
                None
            }
        }
    } else {
        let code = ErrorCode::PipelineFailed.as_str();
        let written = if attempts.is_some_and(|a| a <= 1) {
            repo.requeue_after(job_id, owner, chrono::Duration::seconds(PANIC_RETRY_SECS), code, "internal error (panic)")
        } else {
            repo.finish(job_id, owner, JobStatus::RequiresReview, Some(code), Some(&format!("internal error (panic): {text}")), None)
        };
        match written {
            Ok(true) => Some(format!("Internal error while running the job: {text}")),
            Ok(false) => None,
            Err(e) => {
                error!("Job #{job_id}: could not release the job after the internal error: {e:#}");
                None
            }
        }
    };
    match event {
        Some(line) => {
            if let Err(e) = repo.record_event(job_id, "ERROR", None, &line) {
                warn!("Job #{job_id}: the internal error could not be recorded: {e:#}");
            }
        }
        None => info!("Job #{job_id}: no longer ours after the internal error; nothing written"),
    }
}

/// A panic's message, for the log and the job: at most 300 characters, every
/// address cut to host and path (a slicing panic quotes its string; JSON
/// escapes `/` as `\/`), and the query, password and fragment a `url::Url`
/// prints with `{:?}` left out.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    let raw = if let Some(s) = payload.downcast_ref::<&str>() {
        s.to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".to_string()
    };
    let raw = without_debug_url_parts(raw);
    let scrubbed: Vec<String> = raw
        .split_whitespace()
        .map(|w| {
            if w.contains(r":\/\/") {
                omni_core::cookies::host_and_path(&w.replace(r"\/", "/"))
            } else if w.contains("://") {
                omni_core::cookies::host_and_path(w)
            } else {
                w.to_string()
            }
        })
        .collect();
    scrubbed.join(" ").chars().take(300).collect()
}

/// `query: Some("…")`, `password: Some("…")` and `fragment: Some("…")` of a
/// `url::Url` printed with `{:?}`, with the value left out.
fn without_debug_url_parts(mut s: String) -> String {
    for key in [r#"query: Some(""#, r#"password: Some(""#, r#"fragment: Some(""#] {
        let mut from = 0;
        while let Some(at) = s[from..].find(key) {
            let start = from + at + key.len();
            // The closing `")`, not an escaped quote inside the value.
            let mut end = s.len();
            let mut look = start;
            while let Some(i) = s[look..].find("\")") {
                let i = look + i;
                if !s[..i].ends_with('\\') {
                    end = i;
                    break;
                }
                look = i + 1;
            }
            s.replace_range(start..end, "…");
            from = start + "…".len();
        }
    }
    s
}

/// A repaired video a trusted recipe delivered without approval (P10.16a): the
/// pipeline has put it in the watchfolder; the worker holding the lease completes
/// the job with that file, as it does for any delivery.
fn finish_delivered(repo: &Repository, job_id: i64, owner: &str) -> Result<bool> {
    let delivered = repo.get_job(job_id)?.and_then(|j| j.file_path).filter(|p| !p.is_empty());
    repo.finish(job_id, owner, JobStatus::Completed, None, None, delivered.as_deref())
}

/// The waiting job's `error_message` when the engine has no line for it (P11.2a).
const DELIVERY_WAIT_MESSAGE: &str = "Watchfolder not reachable; the finished video waits and will be delivered when it answers";

/// P11.2a: a run whose finished video now waits for the watchfolder (the
/// engine tagged DELIVERY_FAILED and the row has `delivery_wait` = 1) puts
/// the job back in the queue to wait: no attempt spent, never review, however
/// often it repeats. `true` when the run was such a one, whatever the requeue
/// could write (a job that is no longer ours gets nothing); `false` leaves the
/// failure to the normal rules. Called only after the engine returned, so the
/// next lease never shares the waiting file with this run. The delay grows
/// with the job's waits in a row (30 s, 60 s, 2 min, then 5 min), and the
/// `error_message` is the engine's line for the last wait.
pub(crate) fn wait_for_watchfolder(
    repo: &Repository,
    engine: &BroadcastEngine,
    job_id: i64,
    owner: &str,
    code: ErrorCode,
) -> Result<bool> {
    if code != ErrorCode::DeliveryFailed {
        return Ok(false);
    }
    let row = repo.get_job(job_id)?;
    let waits = row
        .as_ref()
        .is_some_and(|j| j.delivery_wait && j.status == JobStatus::Running && j.lease_owner.as_deref() == Some(owner));
    if !waits {
        return Ok(false);
    }
    let backoff = engine.wait_backoff(job_id);
    // No line of the engine's (a stop, or the first try after a restart): the row's own stays.
    let line = engine
        .wait_line(job_id)
        .or_else(|| row.and_then(|j| j.error_message))
        .unwrap_or_else(|| DELIVERY_WAIT_MESSAGE.to_string());
    if repo.requeue_waiting_delivery(job_id, owner, backoff, &line)? {
        info!("Job #{job_id}: the finished video waits for the watchfolder (next try in {} s)", backoff.num_seconds());
    } else {
        warn!("Job #{job_id}: no longer ours; its waiting video is left to whoever holds it");
    }
    Ok(true)
}

/// The retry policy for a run that failed with `code` (P11.3a), as every
/// failed run's lease holder applies it: `true` when the job goes back to the
/// queue for the code's next wait (whatever the requeue could write: a job
/// that is no longer ours gets nothing), `false` when it goes to review.
///
/// Bounded on every path: a retried code has at most four waits, each
/// spends the attempt its lease took, and a job older than the engine's
/// `retry_max_age` (from `retry_from`, or `created_at`) is not retried at
/// all. LOW_DISK included: a clip too big for the volume fails the same way
/// on every run, so it gets three waits like any other code; a disk that is
/// really full stops the worker loop from leasing instead.
pub(crate) fn retry_by_policy(
    repo: &Repository,
    engine: &BroadcastEngine,
    job_id: i64,
    owner: &str,
    code: ErrorCode,
    message: &str,
) -> Result<bool> {
    let row = repo.get_job(job_id)?;
    let attempts = row.as_ref().map(|j| j.attempts).unwrap_or(1);
    // An unreadable or legacy timestamp is no reason to stop: the waits still bound the job.
    let age = row
        .as_ref()
        .and_then(|j| j.retry_from.or(j.created_at))
        .map_or_else(chrono::Duration::zero, |from| chrono::Utc::now() - from);
    let (Some(delay), Some(max)) =
        (omni_broadcast::errors::retry_after(code, attempts, age, engine.retry_max_age()), code.retry_max_attempts())
    else {
        return Ok(false);
    };
    info!("Job #{job_id}: {code}, retrying in {} s (attempt {attempts}/{max})", delay.num_seconds());
    repo.requeue_retry(job_id, owner, delay, code.as_str(), message, max)?;
    Ok(true)
}

/// The history's hint for job `job_id` going to review with `code`, as the
/// desk's card shows it: a code whose own hint says a retry is under way
/// gets the exhausted text with the job's attempts instead (P11.3a), since
/// no retry follows, and a rate limit on a platform the site's cookie advice
/// after it ([`omni_broadcast::errors::stopped_hint_el`], the card's helper).
/// Every other code: [`omni_broadcast::errors::hint_for_job`] for `url`.
pub(crate) fn review_hint(repo: &Repository, job_id: i64, code: ErrorCode, url: &str) -> String {
    if code.uses_exhausted_hint() {
        let attempts = repo.get_job(job_id).ok().flatten().map_or(1, |j| j.attempts);
        return omni_broadcast::errors::stopped_hint_el(code, attempts, url, repo);
    }
    omni_broadcast::errors::hint_for_job(code, url, repo)
}

/// A waiting video the engine would not deliver on its own (P11.2a): its
/// earlier rename may have landed, or the watchfolder changed
/// (DELIVERY_UNCONFIRMED), or its file could not be read for too long
/// (PIPELINE_FAILED, P11.2b). The lease holder sends the job to review with
/// the engine's code, keeping its marker, wait and file. `true` when `e` is
/// such a case (whatever could be written).
fn review_if_needs_check(repo: &Repository, job_id: i64, owner: &str, e: &anyhow::Error) -> Result<bool> {
    let Some(check) = e.downcast_ref::<omni_broadcast::pipeline::DeliveryNeedsCheck>() else {
        return Ok(false);
    };
    if !repo.review_waiting_delivery(job_id, owner, check.code.as_str(), &check.message)? {
        warn!("Job #{job_id}: no longer ours; its delivery is left to whoever holds it");
    }
    Ok(true)
}

/// A job leased with `delivery_wait` = 1 (P11.2a): deliver the finished video
/// that waited for the watchfolder, and nothing else. The terminal status is
/// set here, by the worker holding the lease, as for every other job.
pub(crate) async fn run_waiting_delivery(
    repo: &Repository,
    engine: &Arc<BroadcastEngine>,
    owner: &str,
    job: &omni_core::models::Job,
    cancel: &CancellationToken,
    repair: &repair::RepairCtx,
) -> Result<()> {
    let job_id = job.id;
    let result = engine.deliver_ready(job, owner, Some(cancel.clone())).await;
    if lost_lease(&result) {
        warn!("Job #{job_id}: no longer ours; nothing more to do");
        return Ok(());
    }
    match result {
        Ok(omni_broadcast::pipeline::ReadyOutcome::Delivered) => {
            // A trusted recipe's video that waited: counted and told as delivered without approval, once.
            if finish_delivered(repo, job_id, owner)? {
                repair::after_waiting_delivery(repair, job_id);
            }
        }
        Ok(omni_broadcast::pipeline::ReadyOutcome::Rerun) => {
            // The workspace is gone already; the next lease is a normal one.
            if !repo.requeue_for_rerun(job_id, owner, "The parked video is missing or changed; running the job again")? {
                warn!("Job #{job_id}: no longer ours; not queued to run again");
            }
        }
        Err(e) => {
            if review_if_needs_check(repo, job_id, owner, &e)? {
                return Ok(());
            }
            let code = classify_pipeline_error(&e);
            if wait_for_watchfolder(repo, engine, job_id, owner, code)? {
                return Ok(());
            }
            // Not waiting any more: the next run starts the back-off afresh.
            engine.forget_waits(job_id);
            let message = readable_cause(&e);
            let diagnostic: String = diagnostic_chain(&e).chars().take(8192).collect();
            let _ = repo.record_event(job_id, "DEBUG", None, &diagnostic);
            // A full run next time: the requeue ends the wait.
            if !retry_by_policy(repo, engine, job_id, owner, code, &message)? {
                if still_ours(repo, job_id, owner) {
                    repo.record_event(job_id, "ERROR", None, &review_hint(repo, job_id, code, &job.url))?;
                }
                repo.finish(job_id, owner, JobStatus::RequiresReview, Some(code.as_str()), Some(&message), None)?;
            }
        }
    }
    Ok(())
}

/// The run holding the lease parks the job for MCR's approval (P10.5): not
/// terminal, no lease, never leased again until approved. `false` when the
/// job is no longer this worker's.
pub(crate) fn finish_held(repo: &Repository, job_id: i64, owner: &str) -> Result<bool> {
    repo.finish(job_id, owner, JobStatus::AwaitingApproval, None, None, None)
}

/// An approved held video: deliver exactly that file (P10.5). No download,
/// no encode, no site-health or evidence bookkeeping. The terminal status is
/// set here, by the worker holding the lease, as for every other job.
pub(crate) async fn run_approved_delivery(
    repo: &Repository,
    engine: &Arc<BroadcastEngine>,
    owner: &str,
    job: &omni_core::models::Job,
    held: &omni_core::repository::HeldOutput,
    cancel: &CancellationToken,
) -> Result<()> {
    let job_id = job.id;
    let result = engine.deliver_held(job, owner, held, Some(cancel.clone())).await;
    if lost_lease(&result) {
        warn!("Job #{job_id}: no longer ours; nothing more to do");
        return Ok(());
    }
    match result {
        Ok(()) => {
            // The job and the held row finish together; a lost lease marks
            // nothing and keeps the held file.
            if repo.finish_held_delivery(job_id, owner)? {
                engine.discard_held_dir(job_id);
            } else {
                warn!("Job #{job_id}: no longer ours after delivering the approved video; the held file is kept");
            }
        }
        Err(e) => {
            // The approved file's earlier rename needs a person (P11.2a).
            if review_if_needs_check(repo, job_id, owner, &e)? {
                return Ok(());
            }
            let code = classify_pipeline_error(&e);
            // The watchfolder did not answer: the approved file waits for it (P11.2a).
            if wait_for_watchfolder(repo, engine, job_id, owner, code)? {
                return Ok(());
            }
            engine.forget_waits(job_id);
            let message = readable_cause(&e);
            let diagnostic: String = diagnostic_chain(&e).chars().take(8192).collect();
            let _ = repo.record_event(job_id, "DEBUG", None, &diagnostic);
            // A retry keeps the approval: the next lease delivers the same file.
            if !retry_by_policy(repo, engine, job_id, owner, code, &message)? {
                if still_ours(repo, job_id, owner) {
                    repo.record_event(job_id, "ERROR", None, &review_hint(repo, job_id, code, &job.url))?;
                }
                repo.finish(job_id, owner, JobStatus::RequiresReview, Some(code.as_str()), Some(&message), None)?;
            }
        }
    }
    Ok(())
}

/// Run one leased job through the pipeline.
///
/// Extracted from the worker loop so the lease, the heartbeat and the permit are
/// all managed in one place and cannot be forgotten on an early return.
pub(crate) async fn run_job(
    repo: &Repository,
    engine: &Arc<BroadcastEngine>,
    owner: &str,
    job: omni_core::models::Job,
    cancel: CancellationToken,
    evidence_dir: &std::path::Path,
    repair: &repair::RepairCtx,
    health: &HealthState,
    canary: &Arc<canary::CanaryEnv>,
) -> Result<()> {
    let repair_mode = repair.cfg.mode;
    let job_id = job.id;
    let orig_url = job.url.clone();

    // File lockers (WeTransfer and friends) are never handed to yt-dlp: the
    // link is a landing page, not a media URL. Plan P8 may resolve them
    // automatically; until then an operator fetches the file.
    let locker_resolver = ComputerUseAgentPlaceholder::new(None, None);
    if locker_resolver.can_handle(&orig_url) {
        warn!("Job #{job_id} is a file-locker link; routing to the MCR desk.");
        repo.record_event(
            job_id,
            "WARN",
            Some(JobStage::Extract),
            "File-locker link (WeTransfer/AirBridge/…); needs a manual download",
        )?;
        repo.finish(
            job_id,
            owner,
            JobStatus::ManualDownload,
            Some(ErrorCode::ManualDownload.as_str()),
            Some("File-locker link: download the file and use 'Upload file' on this job."),
            None,
        )?;
        return Ok(());
    }

    // MCR approved the video this job held back: deliver that file, and do
    // nothing of what follows (no download, no page scan, no sniffing).
    if let Some(held) = repo.approved_held(job_id)? {
        return run_approved_delivery(repo, engine, owner, &job, &held, &cancel).await;
    }

    // The finished video waited for the watchfolder (P11.2a): deliver that file,
    // and nothing else (no download, no encode, no page scan).
    if job.delivery_wait {
        return run_waiting_delivery(repo, engine, owner, &job, &cancel, repair).await;
    }

    // MCR asked for the next verified candidate of this job (P10.13): hold that
    // one, from the list the earlier run already verified. No download of the
    // link, no page scan, no sniffing, no recipes, no model.
    if let Some(attempt) = repair::open_next(repo, job_id)? {
        if !repo.set_stage(job_id, owner, JobStage::Download)? {
            warn!("Job #{job_id}: no longer ours; stopping without the next choice");
            return Ok(());
        }
        // Only `hold` lets MCR walk the verified list; with repair off or in shadow
        // nothing is downloaded and the job goes to review.
        if repair_mode != omni_core::config::RepairMode::Hold {
            let end = omni_core::repository::AttemptEnd {
                rung: Some("next".into()),
                outcome: Some("repair_off".into()),
                domain: attempt.domain.clone(),
                candidates_json: attempt.candidates_json.clone(),
                next_index: attempt.next_index,
                ..Default::default()
            };
            if let Err(e) = repo.finish_attempt(attempt.id, &end, chrono::Utc::now()) {
                warn!("Job #{job_id}: the next-choice attempt could not be closed: {e:#}");
            }
            repo.finish(
                job_id,
                owner,
                JobStatus::RequiresReview,
                Some(ErrorCode::VideoUnresolved.as_str()),
                Some(repair::REPAIR_OFF_EL),
                None,
            )?;
            return Ok(());
        }
        match repair::run_next(repair, &job, owner, evidence_dir, &attempt, &cancel).await {
            repair::NextResult::Held => {
                info!("Job #{job_id}: MCR's next choice is held for approval");
                finish_held(repo, job_id, owner)?;
            }
            repair::NextResult::Lost => warn!("Job #{job_id}: no longer ours; nothing more to do"),
            repair::NextResult::Failed => {
                if !cancel.is_cancelled() && still_ours(repo, job_id, owner) {
                    repo.record_event(job_id, "ERROR", None, ErrorCode::VideoUnresolved.hint_el())?;
                }
                repo.finish(
                    job_id,
                    owner,
                    JobStatus::RequiresReview,
                    Some(ErrorCode::VideoUnresolved.as_str()),
                    Some(repair::NEXT_FAILED_EL),
                    None,
                )?;
            }
        }
        return Ok(());
    }

    if !repo.set_stage(job_id, owner, JobStage::Download)? {
        warn!("Job #{job_id}: no longer ours; stopping without delivering");
        return Ok(());
    }

    // A word typed onto the end of the link that the parser did not know
    // ("…/arthro/-ΑΠΟΚΛΕΙΣΤΙΚΟ", plan P3.10): used only when the site says
    // the address as given does not exist and confirms the one without it.
    //
    // First the words the parser knows (P3.12): a job queued before P3.9
    // still carries "…-ΒΙΝΤΕΟ", and some sites (amna.gr) answer 200 for the
    // decorated address, so a 404 never says so. The same rule as the
    // parser: no guess, no request.
    let mut job = job;
    // The first request of the job path is the first thing checked (P2.12): a link to a
    // loopback, private, link-local or reserved address ends the job here, before any
    // request of any kind (the dead-link check, the site API, yt-dlp, the browser).
    let egress_allow = &repair.cfg.egress_allow;
    // Every URL but an attachment's is judged by its PARSED form: `HTTP://10.0.0.5/` and
    // `ftp://...` are not skipped for the way they are written. What passes is used from here
    // on as written, trimmed and with the scheme lowercased (not stored: a retry vets it again).
    let orig_url = if orig_url.starts_with("attachment://") {
        orig_url
    } else {
        match egress::vet_url(&orig_url, egress_allow).await {
            Ok(normal) => {
                job.url = normal.clone();
                normal
            }
            Err(why) => return egress::end_job_internal_address(repo, job_id, owner, &why),
        }
    };
    let orig_url = if orig_url.starts_with("http://") || orig_url.starts_with("https://") {
        let known = omni_email::parser::without_annotation(&orig_url);
        let (fixed, why) = if known != orig_url {
            (Some(known.to_string()), "the word the sender glued to it removed")
        } else {
            (omni_browser::pages::repair_dead_link(&orig_url).await, "it does not exist as sent (404); the site has it without the ending the sender added")
        };
        match fixed {
            Some(fixed) => {
                // A replacement address is judged by the same rule before it is used.
                let fixed = match egress::vet_url(&fixed, egress_allow).await {
                    Ok(normal) => normal,
                    Err(why) => return egress::end_job_internal_address(repo, job_id, owner, &why),
                };
                info!("Job #{job_id}: {} -> {}", omni_core::cookies::host_and_path(&orig_url), omni_core::cookies::host_and_path(&fixed));
                repo.record_event(job_id, "WARN", Some(JobStage::Extract), &format!("Link corrected, {why}: {fixed}"))?;
                if let Err(e) = repo.set_leased_job_url(job_id, owner, &fixed) {
                    warn!("Job #{job_id}: could not store the corrected link: {e:#}");
                }
                job.url = fixed.clone();
                fixed
            }
            None => orig_url,
        }
    } else {
        orig_url
    };

    // A news page yt-dlp reads as several videos (an article with four
    // Streamable embeds): this job takes the first, the others become
    // sibling jobs or offers, as when the sniffer finds them. Before, all
    // were downloaded into this job and one was delivered.
    let is_web = orig_url.starts_with("http://") || orig_url.starts_with("https://");
    let mut page_referer: Option<String> = None;
    // How many videos yt-dlp's page scan named (P10.2).
    let mut scanned_videos = 0usize;
    // The ones queued as this job's siblings, which a later sniff must not take as its own.
    // A retry starts from what an earlier attempt queued: those videos are neither taken nor
    // queued again, whichever of yt-dlp or the browser ranks one first now.
    let mut scan_siblings: Vec<String> = match repo.jobs_with_children(job_id) {
        Ok(jobs) => omni_broadcast::article::queued_children(&jobs, job_id),
        Err(e) => {
            warn!("Job #{job_id}: could not read the siblings already queued: {e:#}");
            Vec::new()
        }
    };
    // Every video the page offers is already queued as this job's sibling.
    let mut siblings_hold_the_rest = false;
    // What the page showed was only the site's furniture (P3.16): the pick yt-dlp made first was refused,
    // the one it made after the browser was refused (`Some`), and the addresses taken out, for the repair ladder.
    let mut pick_refused = false;
    let mut fallback_refused: Option<bool> = None;
    let mut furniture_urls: Vec<String> = Vec::new();
    // Held videos left out when furniture took the article's own: yt-dlp's pick after the browser may not take them.
    let mut refused_held: Vec<String> = Vec::new();
    // An embedded video on another site, whose cookie jar this job also carries (P3.5).
    let mut login_site: Option<String> = None;
    let mut _login_guard: Option<omni_broadcast::pipeline::LoginSiteGuard> = None;

    // A portal whose video one API request names (plan P3.8): no yt-dlp
    // page scan, no browser. A failed request just takes the usual way.
    if is_web {
        match omni_browser::pages::resolve(&orig_url).await {
            Ok(Some(video)) => {
                // The API's answer can name any address: it is judged like the job's own.
                let video = match egress::vet_url(&video, egress_allow).await {
                    Ok(normal) => normal,
                    Err(why) => return egress::end_job_internal_address(repo, job_id, owner, &why),
                };
                info!("Job #{job_id}: the page's own API names {}", omni_core::cookies::host_and_path(&video));
                repo.record_event(job_id, "INFO", Some(JobStage::Extract), &format!("Video named by the site's API: {video}"))?;
                job.url = video;
                page_referer = Some(orig_url.clone());
            }
            Ok(None) => {}
            Err(e) => warn!("Job #{job_id}: site API lookup failed, trying the usual way: {e:#}"),
        }
    }

    if cancel.is_cancelled() {
        warn!("Job #{job_id}: cancelled; not scanning the page");
        return Ok(());
    }
    // The page scan, for a page neither the site's API nor a video platform named.
    let is_platform = omni_broadcast::downloader::is_video_platform(&orig_url);
    let mut scan = omni_broadcast::downloader::PageScan::default();
    if is_web && page_referer.is_none() && !is_platform {
        scan = engine.page_videos(job_id, &orig_url).await;
        scanned_videos = scan.videos.len();
    }
    // yt-dlp reads this page only through its generic scan, which does not know
    // where on the page each video sits: the browser chooses (P3.15), and yt-dlp's
    // own pick waits as the fallback.
    // Where the page lands is judged by the egress rule too; an internal landing ends the job.
    let landed_refusal = std::sync::Mutex::new(None);
    let route = omni_broadcast::downloader::first_attempt_resolved(
        page_referer.is_some(),
        is_platform,
        job.parent_job_id.is_some(),
        &scan,
        &orig_url,
        |u| {
            let slot = &landed_refusal;
            async move {
                match egress::vet_landing_with(repair::final_page_landing(&u).await, egress_allow, omni_core::net::system_lookup).await {
                    Ok(landed) => landed,
                    Err(why) => {
                        *slot.lock().unwrap_or_else(|p| p.into_inner()) = Some(why);
                        None
                    }
                }
            }
        },
    )
    .await;
    if let Some(why) = landed_refusal.into_inner().unwrap_or_else(|p| p.into_inner()) {
        return egress::end_job_internal_address(repo, job_id, owner, &why);
    }
    let sniff_first = route == omni_broadcast::downloader::FirstAttempt::SnifferFirst;
    let mut deferred_scan: Option<omni_broadcast::downloader::PageScan> = None;
    let mut process_result = if sniff_first {
        repo.record_event(
            job_id,
            "INFO",
            Some(JobStage::Extract),
            "yt-dlp reaches this page only through its generic scan; the browser chooses the video",
        )?;
        // The page's one video on another site: its cookie jar joins the job's for the
        // sniffed download too, as in `ytdlp_attempt` (P3.5).
        if let [only] = scan.videos.as_slice() {
            if other_site(&orig_url, only) {
                login_site = Some(only.clone());
                _login_guard = login_site.as_deref().map(|v| engine.set_login_site(job_id, v));
            }
        }
        deferred_scan = Some(scan);
        Err(anyhow::anyhow!("{}: yt-dlp reads the page only generically", ErrorCode::UnsupportedUrl.as_str())
            .context(ErrorCode::UnsupportedUrl.as_str()))
    } else {
        match ytdlp_attempt(repo, engine, owner, &mut job, &cancel, &orig_url, &scan_siblings, scan, page_referer.clone(), None, &[], egress_allow).await? {
            None => return Ok(()),
            Some(a) => {
                page_referer = a.page_referer;
                scan_siblings.extend(a.scan_siblings);
                siblings_hold_the_rest = a.siblings_hold_the_rest;
                pick_refused = a.only_furniture;
                furniture_urls.extend(a.furniture_urls);
                login_site = a.login_site;
                _login_guard = a.login_guard;
                a.result
            }
        }
    };

    if lost_lease(&process_result) {
        warn!("Job #{job_id}: no longer ours; nothing more to do");
        return Ok(());
    }
    // The video was made and verified, and waits for the watchfolder (P11.2a): no
    // sniffing, no second download into the workspace that holds it.
    if let Err(e) = &process_result {
        if wait_for_watchfolder(repo, engine, job_id, owner, classify_pipeline_error(e))? {
            return Ok(());
        }
    }

    // yt-dlp could not resolve the page. For a news portal that is expected:
    // the video is behind an embedded player, so sniff the actual stream and
    // retry with the session context (referer/UA/cookies) it needs to avoid 403.
    // A platform post is sniffed only where that can help (`should_sniff`):
    // its page also plays other posts' videos.
    // What the browser saw, kept for a job that ends without a video (P10.1).
    let mut snapshot: Option<omni_browser::PageSnapshot> = None;
    let mut sniffed_streams: Vec<String> = Vec::new();
    // Every video the sniffer found is already queued as this job's sibling: the
    // job ends as NO_STREAM_FOUND for that reason, without the page assessment or
    // the repair ladder (they would look for, and could repair into, a second copy).
    // The referer the sniffed stream was tried with (the repair ladder does not retry it).
    let mut sniff_referer: Option<String> = None;
    let sniff = sniff_first
        || match &process_result {
            Err(e) => classify_pipeline_error(e).should_sniff(&orig_url),
            Ok(_) => false,
        };
    // The sniffer, taken first, found no usable video: yt-dlp's own pick is tried (P3.15).
    let mut sniff_end = SniffEnd::NotRun;
    if sniff && (orig_url.starts_with("http://") || orig_url.starts_with("https://")) {
        if sniff_first {
            info!("Job #{job_id}: yt-dlp reads the page only generically; the stream sniffer chooses.");
        } else {
            info!("Job #{job_id}: direct download failed; trying the stream sniffer.");
        }
        if !repo.set_stage(job_id, owner, JobStage::Extract)? {
            warn!("Job #{job_id}: no longer ours; not sniffing");
            return Ok(());
        }
        repo.record_event(
            job_id,
            "INFO",
            Some(JobStage::Extract),
            if sniff_first { "Sniffing the page for its video" } else { "Direct download failed; sniffing the page for a stream" },
        )?;

        if cancel.is_cancelled() {
            return Ok(());
        }
        let sniffed = StreamSniffer::extract_with_evidence(&orig_url, 25).await;
        snapshot = Some(sniffed.snapshot);
        let early_end = sniff_result_end(
            sniff_first,
            sniffed.media.is_ok(),
            snapshot.as_ref().and_then(|s| s.final_url.as_deref()),
        );
        match sniffed.media {
            Ok(_) if early_end == Some(SniffEnd::LandedOnPlatform) => {
                sniff_end = SniffEnd::LandedOnPlatform;
                repo.record_event(
                    job_id,
                    "INFO",
                    Some(JobStage::Extract),
                    "The browser ended on a video platform's page; leaving the choice to yt-dlp",
                )?;
            }
            Ok(bundle) => {
                // An article's embedded posts are not all videos: the first
                // X post in a news247 article was a photo, became the job's
                // video, and sent the job to review while its two real
                // videos were delivered as siblings (P3.13). Ask yt-dlp
                // about each post first; keep the ones it cannot rule out.
                // Every video the browser saw is remembered for this article of the site,
                // then the ones the site shows on its other articles too (a promo, a show, a
                // recommendation) leave the candidates: never the job's video, never queued,
                // only offered (P3.16). An article's sibling job holds a stream or post, not
                // a page of the site.
                let site_memory::SniffScreen { pool, furniture, removed_own } = site_memory::screen_sniff(
                    repo,
                    &orig_url,
                    snapshot.as_ref(),
                    &bundle.all_streams,
                    job.parent_job_id.is_some(),
                    |u| omni_browser::sniffer::is_demoted(&bundle.meta, u),
                );
                furniture_urls.extend(furniture.iter().map(|f| f.url.clone()));
                // No stream on a loopback, private or reserved address is probed, chosen or
                // downloaded (P2.12). Screened before the first probe of any of them.
                // Nothing public left after dropping internal ones: the job ends here, and yt-dlp is
                // not sent to fetch the same address as its fallback.
                let pool = match egress::screen_candidates(pool, egress_allow).await {
                    egress::Screened::Kept { urls, dropped, unreadable } => {
                        egress::record_dropped_hosts(repo, job_id, &dropped, unreadable)?;
                        urls
                    }
                    egress::Screened::AllInternal { dropped } => return egress::end_job_internal_hosts(repo, job_id, owner, &dropped),
                };
                let mut streams = without_videoless_posts(repo, engine, job_id, &pool, &cancel).await;
                if cancel.is_cancelled() {
                    return Ok(());
                }
                // Not one already queued as this job's sibling: it would air twice.
                let (left, queued_emptied) = omni_broadcast::article::remaining_after_queued(&streams, &scan_siblings);
                streams = left;
                // Furniture took the article's own video and no own one is left: a rail or header video is not
                // the article's either (P3.16).
                let (left, dropped_held) =
                    site_memory::leave_nothing_held(removed_own, streams, |u| omni_browser::sniffer::is_demoted(&bundle.meta, u));
                streams = left;
                furniture_urls.extend(dropped_held.iter().cloned());
                refused_held.extend(dropped_held);
                // Only what is left after the photo-only posts are dropped
                // says the page has a video.
                sniffed_streams = streams.clone();
                let primary = if streams.contains(&bundle.primary_stream) {
                    Some(bundle.primary_stream.clone())
                } else {
                    streams.first().cloned()
                };
                // The job's video among the candidates, in the sniffer's rank
                // order: the page's own first, a held one (outside the article,
                // a link outside its body, a contextual list) only when no
                // other exists, and a live platform video never (P1.18). With
                // nothing live this is the first candidate, as the ranking
                // already made it.
                let mut all_live = false;
                let mut all_dead: Option<ErrorCode> = None;
                let mut primary = primary;
                if primary.is_some() {
                    // Every kind of candidate is asked before the primary is decided (P3.15):
                    // a player page or stream that is gone is skipped for the next one, and
                    // no sibling or offer is planned until the primary is confirmed. With the
                    // sniffer's own session, as the download would have it.
                    let cands = omni_broadcast::downloader::sniffed_cands(&streams, |u| omni_browser::sniffer::is_demoted(&bundle.meta, u));
                    // The candidates were asked with the sniffer's referer: the repair ladder
                    // does not retry them with it again.
                    sniff_referer = Some(bundle.referer.clone());
                    let session = &bundle;
                    let choice = omni_broadcast::downloader::choose_primary(&cands, omni_broadcast::downloader::MAX_LIVE_PROBES, |u| {
                        let cancel = &cancel;
                        async move {
                            tokio::select! {
                                p = engine.probe_candidate(job_id, &u, Some(&session.referer), Some(&session.user_agent), session.cookies.as_deref()) => p,
                                _ = cancel.cancelled() => omni_broadcast::downloader::Probe::Unknown,
                            }
                        }
                    })
                    .await;
                    use omni_broadcast::downloader::Choice;
                    let (live, dead) = match choice {
                        Choice::Primary { url, live, dead } => {
                            primary = Some(url);
                            (live, dead)
                        }
                        Choice::AllLive { live } => {
                            all_live = true;
                            primary = None;
                            (live, Vec::new())
                        }
                        Choice::AllDead { code, live } => {
                            all_dead = Some(code);
                            primary = None;
                            (live, Vec::new())
                        }
                        Choice::Nothing => (Vec::new(), Vec::new()),
                    };
                    for l in &live {
                        info!("Job #{job_id}: {} is live; not taken", omni_core::cookies::host_and_path(l));
                        repo.record_event(job_id, "INFO", Some(JobStage::Extract), &format!("Live stream skipped: {l}"))?;
                    }
                    for d in &dead {
                        info!("Job #{job_id}: {} is gone; not taken", omni_core::cookies::host_and_path(d));
                        // A warning on the desk, so a video wrongly read as gone is seen.
                        repo.record_event(job_id, "WARN", Some(JobStage::Extract), &format!("Video that is gone skipped: {d}"))?;
                    }
                    streams.retain(|u| !live.contains(u) && !dead.contains(u));
                    if cancel.is_cancelled() {
                        return Ok(());
                    }
                }
                // The site's furniture is offered to MCR whatever the job takes; when the job
                // queues the article's siblings they write the offered list first, so then
                // it is done after them.
                if primary.is_none() {
                    omni_broadcast::article::offer_site_wide(repo, &job, &furniture);
                }
                match primary {
                    Some(primary) => {
                        // Found: no fallback even when this download fails. yt-dlp's own pick is
                        // the promo clip the browser was asked to avoid; a retry or review beats it.
                        sniff_end = SniffEnd::Found;
                        // The yt-dlp attempt may have found every scanned video already
                        // queued; the browser found one more, so a failure from here is
                        // that video's own and gets the assessment and the ladder.
                        siblings_hold_the_rest = false;
                        info!("Job #{job_id}: sniffer found {}", omni_core::cookies::host_and_path(&primary));
                        repo.record_event(job_id, "INFO", Some(JobStage::Extract), &format!("Sniffed stream: {primary}"))?;
                        let mut retry_job = job.clone();
                        if !still_ours(repo, job_id, owner) {
                            warn!("Job #{job_id}: no longer ours; not queuing the article's other videos");
                            return Ok(());
                        }
                        let facts = article_facts(engine, &retry_job, &primary, &streams, &bundle.meta, &cancel).await;
                        if !still_ours(repo, job_id, owner) {
                            warn!("Job #{job_id}: no longer ours; not queuing the article's other videos");
                            return Ok(());
                        }
                        omni_broadcast::article::queue_article_siblings(repo, owner, &mut retry_job, &primary, &streams, &facts);
                        omni_broadcast::article::offer_site_wide(repo, &retry_job, &furniture);
                        retry_job.url = primary;
                        sniff_referer = Some(bundle.referer.clone());
                        if !repo.set_stage(job_id, owner, JobStage::Download)? {
                            warn!("Job #{job_id}: no longer ours; not downloading the sniffed stream");
                            return Ok(());
                        }
                        process_result = engine
                            .process_job_mode(
                                retry_job,
                                owner,
                                Some(&bundle.referer),
                                Some(&bundle.user_agent),
                                bundle.cookies.as_deref(),
                                Some(cancel.clone()),
                                omni_broadcast::pipeline::DeliveryMode::Deliver,
                            )
                            .await;
                    }
                    None if all_dead.is_some() => {
                        // Every video the browser found is gone: the job ends as a failed
                        // download with that code would, so the page assessment and the repair
                        // ladder see it as before. yt-dlp's own pick is not tried.
                        let code = all_dead.unwrap_or(ErrorCode::NoStreamFound);
                        sniff_end = SniffEnd::AllDead;
                        repo.record_event(
                            job_id,
                            "ERROR",
                            Some(JobStage::Extract),
                            &format!("Every video the browser found is gone ({})", code.as_str()),
                        )?;
                        process_result = Err(anyhow::anyhow!("{}: every video the browser found is gone", code.as_str())
                            .context(code.as_str()));
                    }
                    None if all_live => {
                        sniff_end = SniffEnd::AllLive;
                        repo.record_event(job_id, "ERROR", Some(JobStage::Extract), "Every video found on the page is a live stream")?;
                        process_result = Err(anyhow::anyhow!("{}: every video on the page is live", ErrorCode::LiveStream.as_str())
                            .context(ErrorCode::LiveStream.as_str()));
                    }
                    None if queued_emptied => {
                        sniff_end = SniffEnd::SiblingsHoldTheRest;
                        siblings_hold_the_rest = true;
                        repo.record_event(
                            job_id,
                            "ERROR",
                            Some(JobStage::Extract),
                            "The page's remaining videos are already queued as this job's siblings",
                        )?;
                        process_result = Err(anyhow::anyhow!(
                            "{}: the page's remaining videos are already queued as siblings",
                            ErrorCode::NoStreamFound.as_str()
                        )
                        .context(ErrorCode::NoStreamFound.as_str()));
                    }
                    None if site_memory::ends_on_furniture(&furniture, &streams) => {
                        // What the page showed is the site's own furniture (P3.16): no video for
                        // this job. yt-dlp's own pick is asked next (it is refused if it is the same
                        // promo), and a failure after that is assessed and repaired as any other.
                        sniff_end = SniffEnd::SiteWide;
                        repo.record_event(
                            job_id,
                            "ERROR",
                            Some(JobStage::Extract),
                            "The only videos on this page are ones the site shows on its other articles",
                        )?;
                        process_result = Err(anyhow::anyhow!(
                            "{}: the only videos on the page are ones the site shows on its other articles",
                            ErrorCode::NoStreamFound.as_str()
                        )
                        .context(ErrorCode::NoStreamFound.as_str()));
                    }
                    None => {
                        sniff_end = SniffEnd::PostsHaveNoVideo;
                        repo.record_event(
                            job_id,
                            "ERROR",
                            Some(JobStage::Extract),
                            "The page's embedded posts have no video (photos or text only)",
                        )?;
                        process_result = Err(anyhow::anyhow!(
                            "{}: the page's embedded posts have no video",
                            ErrorCode::NoStreamFound.as_str()
                        )
                        .context(ErrorCode::NoStreamFound.as_str()));
                    }
                }
            }
            Err(sniff_err) => {
                warn!("Job #{job_id}: sniffer found nothing: {sniff_err}");
                sniff_end = SniffEnd::Error;
                repo.record_event(
                    job_id,
                    "ERROR",
                    Some(JobStage::Extract),
                    &format!("Sniffer found no stream: {sniff_err}"),
                )?;
                if let Err(previous) = std::mem::replace(&mut process_result, Ok(omni_broadcast::pipeline::RunOutcome::Delivered)) {
                    process_result = Err(after_failed_sniff(previous, sniff_err));
                }
            }
        }
    }

    // The browser, asked first, found no video: yt-dlp's own pick, as it would have
    // been taken before. Its result takes the error handling below; the sniff's
    // snapshot stays as evidence. Not after a live stream, siblings that hold the
    // rest, a cancel or a lost lease.
    let end = if lost_lease(&process_result) {
        SniffEnd::LeaseLost
    } else if cancel.is_cancelled() {
        SniffEnd::Cancelled
    } else {
        sniff_end
    };
    if falls_back_to_ytdlp(sniff_first, end) {
        repo.record_event(job_id, "INFO", Some(JobStage::Extract), "The browser found no video; trying yt-dlp's own pick")?;
        if !repo.set_stage(job_id, owner, JobStage::Download)? {
            warn!("Job #{job_id}: no longer ours; not trying yt-dlp's own pick");
            return Ok(());
        }
        // The login guard is the attempt's to make again; two for one job would drop the new one.
        _login_guard = None;
        let scan = deferred_scan.take().unwrap_or_default();
        match ytdlp_attempt(repo, engine, owner, &mut job, &cancel, &orig_url, &scan_siblings, scan, page_referer.clone(), snapshot.as_ref().and_then(|s| s.final_url.as_deref()), &refused_held, egress_allow).await? {
            None => return Ok(()),
            Some(a) => {
                page_referer = a.page_referer;
                siblings_hold_the_rest = a.siblings_hold_the_rest;
                fallback_refused = Some(a.only_furniture);
                furniture_urls.extend(a.furniture_urls);
                login_site = a.login_site;
                _login_guard = a.login_guard;
                process_result = a.result;
            }
        }
    }

    if lost_lease(&process_result) {
        warn!("Job #{job_id}: no longer ours; nothing more to do");
        return Ok(());
    }

    match process_result {
        Ok(omni_broadcast::pipeline::RunOutcome::Held) => {
            // Made and checked, but held for MCR (P10.5): nothing was delivered.
            finish_held(repo, job_id, owner)?;
            Ok(())
        }
        Ok(omni_broadcast::pipeline::RunOutcome::Delivered) => {
            let delivered = repo
                .get_job(job_id)?
                .and_then(|j| j.file_path)
                .filter(|p| !p.is_empty());
            let done = repo.finish(
                job_id,
                owner,
                JobStatus::Completed,
                None,
                None,
                delivered.as_deref(),
            )?;
            if done && !cancel.is_cancelled() {
                // An article's sibling job holds a sniffed stream or post,
                // not a page of the site: it says nothing about the site.
                if job.parent_job_id.is_none() {
                    note_site_outcome(repo, health, job_id, &orig_url, SiteOutcome::Success);
                }
            }
            Ok(())
        }
        Err(e) => {
            // The classified code decides whether waiting could possibly help
            // (plan P1.9). Retrying a deleted video burns worker slots and
            // delays telling the journalist their link is dead; retrying a
            // dropped connection usually just works.
            let mut code = classify_pipeline_error(&e);
            // The video was made and verified, and waits for the watchfolder (P11.2a).
            if wait_for_watchfolder(repo, engine, job_id, owner, code)? {
                return Ok(());
            }
            let mut message = readable_cause(&e);

            // The full context chain, which carries each stage's stderr tail,
            // into the job's own timeline (plan P6.1). `error_message` is the
            // one line the MCR card shows; this is what an engineer needs an
            // hour later, and putting it on the job means it survives log
            // rotation and does not require finding the right file.
            let diagnostic: String = diagnostic_chain(&e).chars().take(8192).collect();
            let _ = repo.record_event(job_id, "DEBUG", None, &diagnostic);

            if !retry_by_policy(repo, engine, job_id, owner, code, &message)? {
                // MANUAL_DOWNLOAD is not a failure, it is a different workflow.
                let status = match code {
                    ErrorCode::ManualDownload => JobStatus::ManualDownload,
                    _ => JobStatus::RequiresReview,
                };
                // A page that gave no video: what did it show (P10.2)? May
                // name a more exact code, and may look at the page again.
                let original = code;
                if is_web
                    && matches!(code, ErrorCode::NoStreamFound | ErrorCode::UnsupportedUrl | ErrorCode::Http403)
                    && !omni_broadcast::downloader::is_video_platform(&orig_url)
                    && !siblings_hold_the_rest
                {
                    let page = FailedPage {
                        url: &orig_url,
                        mode: repair_mode,
                        snapshot: snapshot.take(),
                        sniffed_streams: &sniffed_streams,
                        original: code,
                        named_by_site: site_named_video(page_referer.is_some(), scanned_videos),
                    };
                    let (exact, line, seen) = assess_failed_page(repo, owner, job_id, &cancel, page).await;
                    code = exact;
                    if let Some(line) = line {
                        message = line;
                    }
                    snapshot = seen;
                }
                let wants_evidence = |code: ErrorCode| {
                    matches!(original, ErrorCode::NoStreamFound | ErrorCode::UnsupportedUrl | ErrorCode::Http403)
                        || matches!(
                            code,
                            ErrorCode::DrmProtected
                                | ErrorCode::NoVideoOnPage
                                | ErrorCode::VideoUnresolved
                                | ErrorCode::RepairExhausted
                        )
                };

                // The page showed a video the normal path could not resolve:
                // the repair ladder (P10.11a). Only here, only for a web page,
                // only with repair on; a held video waits for MCR.
                let tried = repair::Tried::of(&job, &orig_url, page_referer.as_deref(), &sniffed_streams, sniff_referer.as_deref());
                let ladder = if siblings_hold_the_rest {
                    None
                } else {
                    repair::after_failure_with(repair, &job, owner, &orig_url, code, &snapshot, tried, furniture_urls.clone(), &cancel).await
                };
                if let Some(ladder) = ladder {
                    match &ladder {
                        repair::LadderResult::ShadowFound { verified } => {
                            info!("Job #{job_id}: repair (shadow) verified {} video(s); the job ends as before", verified.len())
                        }
                        repair::LadderResult::Held { recipe_hash } => {
                            info!("Job #{job_id}: a repaired video is held for MCR (recipe {})", &recipe_hash[..recipe_hash.len().min(8)])
                        }
                        repair::LadderResult::Delivered { recipe_hash } => {
                            info!("Job #{job_id}: a repaired video was delivered without approval (trusted recipe {})", &recipe_hash[..recipe_hash.len().min(8)])
                        }
                        repair::LadderResult::Parked { recipe_hash } => {
                            info!("Job #{job_id}: a repaired video waits for the watchfolder (trusted recipe {})", &recipe_hash[..recipe_hash.len().min(8)])
                        }
                        _ => {}
                    }
                    match repair::disposition(&ladder, repair_mode, repair.cfg.budget) {
                        repair::Disposition::Held => {
                            if let Some(snap) = &snapshot {
                                if wants_evidence(code) && still_ours(repo, job_id, owner) {
                                    save_page_evidence(repo, evidence_dir, job_id, snap).await;
                                }
                            }
                            // Waiting, not failed: no site-health record.
                            finish_held(repo, job_id, owner)?;
                            return Ok(());
                        }
                        repair::Disposition::Delivered => {
                            // Delivered by the pipeline; the worker completes the job. No failure is recorded.
                            finish_delivered(repo, job_id, owner)?;
                            return Ok(());
                        }
                        repair::Disposition::Parked => {
                            // A trusted recipe's video waits for the watchfolder (P11.2a); its
                            // automatic-delivery marker stays for the delivery after the wait.
                            if !wait_for_watchfolder(repo, engine, job_id, owner, ErrorCode::DeliveryFailed)? {
                                warn!("Job #{job_id}: the repaired video was to wait for the watchfolder, but the job is no longer ours");
                            }
                            return Ok(());
                        }
                        repair::Disposition::Lost => {
                            warn!("Job #{job_id}: no longer ours; nothing more to do");
                            return Ok(());
                        }
                        repair::Disposition::Finish { code: c, message: m } => {
                            code = c;
                            message = m;
                        }
                        // The model said why nothing can be resolved: MCR reads it on the card.
                        repair::Disposition::Annotate(reason) => message = repair::annotate(&message, &reason),
                        repair::Disposition::Unchanged => {}
                    }
                }

                // Nothing more is written for a job that is no longer ours.
                if !cancel.is_cancelled() && still_ours(repo, job_id, owner) {
                    repo.record_event(job_id, "ERROR", None, &review_hint(repo, job_id, code, login_site.as_deref().unwrap_or(&job.url)))?;
                }
                // Only the job's owner writes evidence: a job MCR cancelled
                // or the reaper gave away has no business leaving a file.
                if let Some(snap) = &snapshot {
                    if wants_evidence(code) && !cancel.is_cancelled() && still_ours(repo, job_id, owner) {
                        save_page_evidence(repo, evidence_dir, job_id, snap).await;
                    }
                }
                let done = repo.finish(
                    job_id,
                    owner,
                    status,
                    Some(code.as_str()),
                    Some(&message),
                    None,
                )?;
                // Site health (P10.3): only a finished job, and only the
                // codes that say something about the site, not this page.
                // A repair that ran out of time counts as the unresolved video it was.
                let site_code = repair::site_code(code);
                if site_failure_counts(done, cancel.is_cancelled(), job.parent_job_id.is_some(), furniture_only_end(pick_refused, end, fallback_refused), site_code.as_str())
                {
                    let noted = note_site_outcome(
                        repo,
                        health,
                        job_id,
                        &orig_url,
                        SiteOutcome::SiteFailure(site_code.as_str().to_string()),
                    );
                    // A site that just started failing: look at its recent
                    // articles in the background (P10.4). Never awaited here.
                    if let Some((domain, before, after)) = noted {
                        if canary::should_trigger(repair_mode, before, after) {
                            canary.trigger(&domain);
                        }
                    }
                }
            }
            Ok(())
        }
    }
}

/// Fold a finished job into its site's health (plan P10.3). Bookkeeping only:
/// a database error is a warning and never changes the job's outcome.
fn note_site_outcome(
    repo: &Repository,
    health: &HealthState,
    job_id: i64,
    url: &str,
    outcome: SiteOutcome,
) -> Option<(String, SiteState, SiteState)> {
    let is_platform = omni_broadcast::downloader::is_video_platform(url);
    let domain = omni_core::site_health::site_for_job(url, is_platform)?;
    match repo.record_site_outcome(&domain, outcome) {
        Ok((before, after)) => {
            if before != after {
                let line = match (after, repo.get_site(&domain)) {
                    (SiteState::Healthy, _) => format!("Site {domain} is healthy again"),
                    (state, Ok(Some(row))) => format!(
                        "Site {domain} is now {} ({} failures in a row)",
                        state.as_str(),
                        row.consecutive_fail
                    ),
                    (state, _) => format!("Site {domain} is now {}", state.as_str()),
                };
                info!("Job #{job_id}: {line}");
                let _ = repo.record_event(job_id, "INFO", None, &line);
            }
            omni_core::site_health::refresh_health(repo, health);
            Some((domain, before, after))
        }
        Err(e) => {
            warn!("Job #{job_id}: site health for {domain} not recorded: {e:#}");
            None
        }
    }
}

/// Whether to open the page again with the diagnostic recorders: only when
/// repair is on, for a page (not a platform post), and when the passive look
/// proved nothing (no snapshot at all, weak or none).
fn should_diagnose(mode: RepairMode, passive: Option<EvidenceClass>, is_platform: bool) -> bool {
    mode != RepairMode::Off
        && !is_platform
        && passive.map_or(true, |c| matches!(c, EvidenceClass::Weak | EvidenceClass::None))
}

/// The code and the one-line reason (Greek, for the MCR card) a page's
/// evidence gives a job that failed with `original`. `None` for the line
/// keeps the existing cause text. A "no video" verdict needs the diagnostic
/// run; without it the original code stands, as it does for a weak page.
fn evidence_code(original: ErrorCode, ev: &Evidence) -> (ErrorCode, Option<String>) {
    match ev.class {
        EvidenceClass::Protected => {
            let system = ev.signals.iter().find_map(|s| s.strip_prefix("eme:")).unwrap_or("DRM");
            (ErrorCode::DrmProtected, Some(format!("Το βίντεο έχει DRM ({system})")))
        }
        EvidenceClass::LoginWall => (ErrorCode::LoginRequired, None),
        EvidenceClass::GeoBlocked => (ErrorCode::GeoBlocked, None),
        EvidenceClass::Strong => (
            ErrorCode::VideoUnresolved,
            Some(format!("Βρέθηκε βίντεο στη σελίδα ({}) αλλά δεν κατέβηκε", ev.signals.join(", "))),
        ),
        EvidenceClass::None if ev.diagnosed => {
            (ErrorCode::NoVideoOnPage, Some("Δεν βρέθηκε βίντεο ούτε στον δεύτερο έλεγχο".to_string()))
        }
        EvidenceClass::None | EvidenceClass::Weak => (original, None),
    }
}

/// What a failed web job's page left behind, for [`assess_failed_page`].
struct FailedPage<'a> {
    url: &'a str,
    mode: RepairMode,
    snapshot: Option<omni_browser::PageSnapshot>,
    sniffed_streams: &'a [String],
    original: ErrorCode,
    /// The site's API or yt-dlp's page scan named a video for this job.
    named_by_site: bool,
}

/// How the sniffer's run ended for a job whose page it was asked about first (P3.15).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SniffEnd {
    /// The sniffer did not run.
    NotRun,
    /// It chose a video; whatever the download does, yt-dlp is not asked.
    Found,
    /// It failed or found no stream.
    Error,
    /// Its streams were embedded posts with no video in them.
    PostsHaveNoVideo,
    /// Every video it found is live.
    AllLive,
    /// Every video it found is gone (probed): the job ends with that code, yt-dlp's pick is not tried.
    AllDead,
    /// What it found is already queued as this job's siblings.
    SiblingsHoldTheRest,
    /// Every video it found is one the site shows on its other articles (P3.16), or only held ones
    /// were left once those were out: none is the job's. yt-dlp's own pick is asked next, and refused
    /// if it is one of them (`guard_scan`).
    SiteWide,
    Cancelled,
    LeaseLost,
    /// The browser ended on a video platform's page (a meta refresh or script redirect): yt-dlp
    /// follows it and takes the post itself, as before.
    LandedOnPlatform,
}

/// How a sniff ends before its streams are looked at: it failed, or (for a page the browser was
/// asked about first) it ended on a video platform's page, which yt-dlp follows instead.
/// `None`: the streams it found are looked at.
fn sniff_result_end(sniff_first: bool, media_ok: bool, final_url: Option<&str>) -> Option<SniffEnd> {
    if !media_ok {
        return Some(SniffEnd::Error);
    }
    (sniff_first && omni_broadcast::downloader::landed_on_platform(final_url)).then_some(SniffEnd::LandedOnPlatform)
}

/// Whether yt-dlp's own pick is tried after the sniffer, asked first, ended as `end`: only when
/// it found no usable video (its only videos were the site's furniture: yt-dlp's pick is asked, and
/// refused if it is one of them). Never after a live stream, siblings that hold the rest, a video
/// it did choose, a cancel or a lost lease.
fn falls_back_to_ytdlp(sniff_first: bool, end: SniffEnd) -> bool {
    sniff_first && matches!(end, SniffEnd::Error | SniffEnd::PostsHaveNoVideo | SniffEnd::LandedOnPlatform | SniffEnd::SiteWide)
}

/// What yt-dlp's attempt at a web page left: its result, and what the page scan
/// set up for it (the first video's referer, the siblings queued, the other site's login).
struct YtdlpAttempt {
    result: Result<omni_broadcast::pipeline::RunOutcome>,
    page_referer: Option<String>,
    scan_siblings: Vec<String>,
    /// Every video of the page's scan is already queued as a sibling: nothing was downloaded.
    siblings_hold_the_rest: bool,
    /// The page's only videos are the site's furniture (P3.16): not a failure of the site.
    only_furniture: bool,
    /// What was taken out of the scan as the site's furniture.
    furniture_urls: Vec<String>,
    login_site: Option<String>,
    login_guard: Option<omni_broadcast::pipeline::LoginSiteGuard>,
}

/// yt-dlp's own attempt at a job's link: the page scan's pick among several videos
/// (the rest become siblings), or the download of the link itself. Taken first
/// for most links, and after a sniff that found nothing for a page yt-dlp reads
/// only generically (P3.15). `Ok(None)`: the job is no longer ours or was
/// cancelled; nothing more to do.
async fn ytdlp_attempt(
    repo: &Repository,
    engine: &Arc<BroadcastEngine>,
    owner: &str,
    job: &mut omni_core::models::Job,
    cancel: &CancellationToken,
    orig_url: &str,
    queued: &[String],
    mut scan: omni_broadcast::downloader::PageScan,
    page_referer: Option<String>,
    // Where the browser ended, when it looked at the page first (its site is looked up too).
    browser_final: Option<&str>,
    // Held videos the browser's look took out (furniture removed the article's own): not yt-dlp's pick either.
    refused: &[String],
    egress_allow: &omni_core::net::CidrSet,
) -> Result<Option<YtdlpAttempt>> {
    let job_id = job.id;
    // yt-dlp's page scan names videos by any http(s) address: none on an internal address is probed,
    // queued or downloaded (P2.12). Nothing public left and something refused: the job ends here.
    // yt-dlp's own pick on a single-video page is among `fetch_urls` (every address its result names,
    // uncapped): screened with the videos, and an internal or refused one ends the job before yt-dlp
    // reads the page. Nothing kept and something refused ends it as well: yt-dlp is not sent to pick
    // the same address. No guard around it: with nothing to screen it is `Continue` at once.
    let scan_dropped = match egress::screen_page_scan(std::mem::take(&mut scan.videos), &scan.fetch_urls, egress_allow).await {
        egress::PageScanVerdict::Continue { videos, dropped, unreadable } => {
            egress::record_dropped_hosts(repo, job_id, &dropped, unreadable)?;
            scan.videos = videos;
            !dropped.is_empty() || unreadable > 0
        }
        egress::PageScanVerdict::EndInternal { hosts } => {
            egress::end_job_internal_hosts(repo, job_id, owner, &hosts)?;
            return Ok(None);
        }
        egress::PageScanVerdict::EndUnreadable => {
            egress::end_job_unreadable(repo, job_id, owner)?;
            return Ok(None);
        }
    };
    let mut page_referer = page_referer;
    let mut scan_siblings: Vec<String> = Vec::new();
    // An embedded video on another site, whose cookie jar this job also carries (P3.5).
    let mut login_site: Option<String> = None;
    // Every video of the page's scan was live (P1.18).
    let mut all_live_scan = false;
    let known_unsupported = scan.unsupported;
    let scan_count = scan.videos.len();
    // Videos an earlier attempt queued as siblings are neither taken nor queued again.
    let (mut videos, held_by_siblings) = omni_broadcast::article::remaining_after_queued(&scan.videos, queued);
    // A video the site shows on its other articles too is never yt-dlp's pick (P3.16): out of the
    // multi-video choice, and a single result whose media is one is not downloaded. Read only: the
    // browser's sightings are what is remembered.
    let mut furniture: Vec<omni_core::site_videos::Furniture> = Vec::new();
    let mut only_furniture = false;
    if job.parent_job_id.is_none() && page_referer.is_none() {
        let guard = site_memory::guard_scan(repo, orig_url, scan.webpage_url.as_deref(), browser_final, std::mem::take(&mut videos), &scan.media_urls, refused);
        (videos, furniture, only_furniture) = (guard.videos, guard.furniture, guard.only_furniture);
    }
    // One left after the screen took the others out is still a choice among the page's videos: yt-dlp
    // is not sent to read the page itself, where it could pick the refused one.
    if (scan_count > 1 || scan_dropped) && !videos.is_empty() {
        // yt-dlp's page scan says nothing about where on the page a video
        // sat, so the placement rule (R2) is the sniffer's alone; here only
        // a glomex contextual list is known (held), and live videos are
        // skipped: one chosen or queued as a sibling would air twice once
        // the sniffer ran for the failed job (P1.18).
        let (cands, scan_meta) = page_scan_cands(&videos);
        let choice = omni_broadcast::downloader::choose_primary(&cands, omni_broadcast::downloader::MAX_LIVE_PROBES, |u| {
            let cancel = &cancel;
            async move {
                tokio::select! {
                    i = engine.video_length(job_id, &u) => omni_broadcast::downloader::Probe::from_length(i),
                    _ = cancel.cancelled() => omni_broadcast::downloader::Probe::Unknown,
                }
            }
        })
        .await;
        if cancel.is_cancelled() {
            return Ok(None);
        }
        use omni_broadcast::downloader::Choice;
        let (first, live) = match choice {
            Choice::Primary { url, live, .. } => (Some(url), live),
            Choice::AllLive { live } => (None, live),
            // A length probe never says a video is gone, so this cannot happen here.
            Choice::AllDead { live, .. } => (None, live),
            Choice::Nothing => (None, Vec::new()),
        };
        for l in &live {
            info!("Job #{job_id}: {} is live; not taken", omni_core::cookies::host_and_path(l));
            repo.record_event(job_id, "INFO", Some(JobStage::Extract), &format!("Live stream skipped: {l}"))?;
        }
        videos.retain(|u| !live.contains(u));
        match first {
            None => {
                all_live_scan = true;
                videos.clear();
            }
            Some(first) => {
                info!("Job #{job_id}: the page holds {} videos; this job takes the first", videos.len());
                repo.record_event(
                    job_id,
                    "INFO",
                    Some(JobStage::Extract),
                    &format!("The page holds {} videos; this job downloads the first: {}", videos.len(), first),
                )?;
                if !still_ours(repo, job_id, owner) {
                    warn!("Job #{job_id}: no longer ours; not queuing the article's other videos");
                    return Ok(None);
                }
                let facts = article_facts(engine, &job, &first, &videos, &scan_meta, &cancel).await;
                let count = job.max_videos.filter(|n| *n > 0).map(|n| n as usize);
                scan_siblings =
                    omni_broadcast::article::plan_article_videos_facts(&job.url, &first, &videos, &job.index_str, count, &facts)
                        .siblings
                        .into_iter()
                        .map(|(u, _)| u)
                        .collect();
                omni_broadcast::article::queue_article_siblings(repo, owner, job, &first, &videos, &facts);
                job.url = first;
                page_referer = Some(orig_url.to_string());
            }
        }
    } else if scan_count == 1 && videos.len() == 1 && other_site(&orig_url, &videos[0]) {
        // The article's one video is another site's (an X post, say): that site's
        // cookie jar joins the job's, and its name is in the login hint. The
        // download itself is unchanged.
        login_site = Some(videos[0].clone());
    }
    if !still_ours(repo, job_id, owner) {
        warn!("Job #{job_id}: no longer ours; not downloading");
        return Ok(None);
    }
    // After the article's siblings were queued, which write the offered list first.
    omni_broadcast::article::offer_site_wide(repo, job, &furniture);
    let login_guard = login_site.as_deref().map(|v| engine.set_login_site(job_id, v));
    let result = if only_furniture && !all_live_scan && !held_by_siblings {
        repo.record_event(
            job_id,
            "ERROR",
            Some(JobStage::Extract),
            "The only videos on this page are ones the site shows on its other articles",
        )?;
        Err(anyhow::anyhow!(
            "{}: the only videos on the page are ones the site shows on its other articles",
            ErrorCode::NoStreamFound.as_str()
        )
        .context(ErrorCode::NoStreamFound.as_str()))
    } else if all_live_scan {
        repo.record_event(job_id, "ERROR", Some(JobStage::Extract), "Every video found on the page is a live stream")?;
        Err(anyhow::anyhow!("{}: every video on the page is live", ErrorCode::LiveStream.as_str())
            .context(ErrorCode::LiveStream.as_str()))
    } else if held_by_siblings {
        repo.record_event(
            job_id,
            "ERROR",
            Some(JobStage::Extract),
            "The page's remaining videos are already queued as this job's siblings",
        )?;
        Err(anyhow::anyhow!("{}: the page's remaining videos are already queued as siblings", ErrorCode::NoStreamFound.as_str())
            .context(ErrorCode::NoStreamFound.as_str()))
    } else if known_unsupported {
        repo.record_event(
            job_id,
            "INFO",
            Some(JobStage::Extract),
            "yt-dlp has no extractor for this page; going straight to the browser",
        )?;
        Err(anyhow::anyhow!("{}: yt-dlp has no extractor for the page", ErrorCode::UnsupportedUrl.as_str())
            .context(ErrorCode::UnsupportedUrl.as_str()))
    } else {
        engine
            .process_job_mode(
                job.clone(),
                owner,
                page_referer.as_deref(),
                None,
                None,
                Some(cancel.clone()),
                omni_broadcast::pipeline::DeliveryMode::Deliver,
            )
            .await
    };
    Ok(Some(YtdlpAttempt {
        result,
        page_referer,
        scan_siblings,
        // Nothing is left to assess or repair into: the page's videos are queued, or are the site's furniture.
        siblings_hold_the_rest: held_by_siblings,
        only_furniture,
        furniture_urls: furniture.iter().map(|f| f.url.clone()).collect(),
        login_site,
        login_guard,
    }))
}

/// The site or yt-dlp's page scan named a video for this job: the API
/// resolved it (a referer is kept), or the scan found at least one.
/// Whether `video` is on another registrable domain than `article`.
fn other_site(article: &str, video: &str) -> bool {
    match (omni_core::urlnorm::registrable_domain(article), omni_core::urlnorm::registrable_domain(video)) {
        (Some(a), Some(v)) => a != v,
        _ => false,
    }
}

fn site_named_video(api_or_sibling: bool, scanned_videos: usize) -> bool {
    api_or_sibling || scanned_videos >= 1
}

/// The passive verdict with "the site itself named a video" added as a strong
/// signal: such a job is never reported as a page without video.
fn with_site_hint(passive: Option<Evidence>, named_by_site: bool) -> Option<Evidence> {
    if !named_by_site {
        return passive;
    }
    let hint = Evidence { class: EvidenceClass::Strong, signals: vec!["named-by-site".into()], diagnosed: false };
    Some(match passive {
        Some(p) => omni_browser::evidence::combine(&hint, &p),
        None => hint,
    })
}

/// Judge a web page that ended without a video from what the browser saw, run
/// the diagnostic second look when that is allowed and needed, and note the
/// verdict on the job. Returns the code to finish with, a replacement cause
/// line, and the snapshot to keep as evidence (the diagnostic one when there
/// is one). Never fails: whatever goes wrong leaves today's outcome.
async fn assess_failed_page(
    repo: &Repository,
    owner: &str,
    job_id: i64,
    cancel: &CancellationToken,
    page: FailedPage<'_>,
) -> (ErrorCode, Option<String>, Option<omni_browser::PageSnapshot>) {
    let FailedPage { url, mode, snapshot, sniffed_streams, original, named_by_site } = page;
    let is_ad = |u: &str| StreamSniffer::is_ad_or_tracking(u);
    let mut snapshot = snapshot;
    let passive = with_site_hint(
        snapshot.as_ref().map(|snap| omni_browser::evidence::classify(snap, sniffed_streams, None, is_ad)),
        named_by_site,
    );
    let mut verdict = passive.clone();

    if should_diagnose(mode, passive.as_ref().map(|e| e.class), false)
        && !cancel.is_cancelled()
        && still_ours(repo, job_id, owner)
    {
        info!("Job #{job_id}: the page showed no video; looking at it once more with the diagnostic run");
        // Dropping the run closes its browser.
        let ran = tokio::select! {
            r = StreamSniffer::diagnose(url, 30) => Some(r),
            _ = cancel.cancelled() => None,
        };
        let Some((diag_snap, diag)) = ran else {
            return (original, None, snapshot);
        };
        if cancel.is_cancelled() || !still_ours(repo, job_id, owner) {
            return (original, None, snapshot);
        }
        let looked = omni_browser::evidence::classify(&diag_snap, sniffed_streams, Some(&diag), is_ad);
        verdict = Some(match &passive {
            Some(p) => omni_browser::evidence::combine(p, &looked),
            None => looked,
        });
        // A run that was blocked or cut short leaves the first look as the evidence.
        if diag.complete || snapshot.is_none() {
            snapshot = Some(diag_snap);
        }
    }

    let Some(ev) = verdict else {
        return (original, None, snapshot);
    };
    if cancel.is_cancelled() || !still_ours(repo, job_id, owner) {
        return (original, None, snapshot);
    }
    let line = format!("Evidence: {} — {}", ev.class.as_str(), ev.signals.join(", "));
    if let Err(e) = repo.record_event(job_id, "INFO", Some(JobStage::Extract), &line) {
        warn!("Job #{job_id}: could not note the page evidence verdict: {e:#}");
    }
    let (code, message) = evidence_code(original, &ev);
    (code, message, snapshot)
}

/// Largest evidence file written for one job.
const EVIDENCE_MAX_BYTES: usize = 512 * 1024;

/// Write the page evidence of a failed job to `{dir}/{job_id}.json` and note
/// it in the database. A failure here is a warning only: it never changes the
/// job's outcome.
async fn save_page_evidence(
    repo: &Repository,
    dir: &std::path::Path,
    job_id: i64,
    snapshot: &omni_browser::PageSnapshot,
) {
    let path = dir.join(format!("{job_id}.json"));
    let json = snapshot.to_capped_json(EVIDENCE_MAX_BYTES);
    let bytes = json.len() as i64;
    let (dir, file) = (dir.to_path_buf(), path.clone());
    let written = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        std::fs::create_dir_all(&dir)?;
        std::fs::write(&file, json)
    })
    .await;
    match written {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return warn!("Job #{job_id}: page evidence not written: {e}"),
        Err(e) => return warn!("Job #{job_id}: page evidence not written: {e}"),
    }
    if let Err(e) = repo.record_evidence(job_id, &path.to_string_lossy(), bytes) {
        warn!("Job #{job_id}: page evidence not recorded: {e:#}");
    }
}

/// What is known about the article's other videos before they are queued
/// (P3.14): where on the page each sat and whether it was a contextual
/// playlist (from the sniffer; nothing for yt-dlp's own page scan), and the
/// length of each one that would be queued, asked of yt-dlp without
/// downloading. At most `MAX_AUTO_SIBLINGS` probes plus one for each sibling
/// held back (its replacement is asked too), 15 s each, one after another. A length that cannot be had stays unknown and plans as before
/// (queued). Nothing is probed when the journalist asked for a count: they
/// chose those videos.
async fn article_facts(
    engine: &BroadcastEngine,
    job: &omni_core::models::Job,
    primary: &str,
    streams: &[String],
    meta: &std::collections::HashMap<String, omni_browser::sniffer::StreamMeta>,
    cancel: &CancellationToken,
) -> omni_broadcast::article::Facts {
    use omni_broadcast::article::{facts_from_placement, probe_siblings, Probed};
    let mut facts = facts_from_placement(
        streams.iter().filter_map(|u| meta.get(u).map(|m| (u.as_str(), m.in_article, m.contextual))),
    );
    for u in streams {
        if meta.get(u).is_some_and(|m| m.link) {
            if let Some(f) = facts.get_mut(&omni_broadcast::article::facts_key(u)) {
                f.link = true;
            }
        }
    }
    let count = job.max_videos.filter(|n| *n > 0).map(|n| n as usize);
    probe_siblings(&job.url, primary, streams, &job.index_str, count, &mut facts, |url| async move {
        let info = tokio::select! {
            i = engine.video_length(job.id, &url) => i,
            _ = cancel.cancelled() => None,
        };
        info.map(|i| Probed { duration_secs: i.duration_secs, live: i.live })
    })
    .await;
    facts
}

/// The candidates of yt-dlp's page scan for [`omni_broadcast::downloader::choose_primary`],
/// in page order, and the meta that marks the glomex contextual lists (`cl-…`)
/// among them. A contextual list is held: never the job's video while another
/// is on the page. Nothing else is known to be held here (P3.14).
fn page_scan_cands(
    videos: &[String],
) -> (Vec<omni_broadcast::downloader::Cand>, std::collections::HashMap<String, omni_browser::sniffer::StreamMeta>) {
    let contextual = |u: &str| omni_browser::glomex::parse_embed(u).is_some_and(|e| e.is_contextual_list());
    let cands = videos
        .iter()
        .map(|u| omni_broadcast::downloader::Cand {
            url: u.clone(),
            held: contextual(u),
            probe_able: omni_broadcast::downloader::is_video_platform(u),
        })
        .collect();
    let meta = videos
        .iter()
        .filter(|u| contextual(u))
        .map(|u| (u.clone(), omni_browser::sniffer::StreamMeta { in_article: None, contextual: true, link: false }))
        .collect();
    (cands, meta)
}

/// `streams` without the social posts yt-dlp says have no video, in page
/// order (P3.13). YouTube is not asked (it has no photo posts) and raw
/// streams cannot be; a post that cannot be checked (network, login) stays.
/// About a second per post with the folder build of yt-dlp.
async fn without_videoless_posts(
    repo: &Repository,
    engine: &BroadcastEngine,
    job_id: i64,
    streams: &[String],
    cancel: &CancellationToken,
) -> Vec<String> {
    use omni_broadcast::downloader::{is_video_platform, is_youtube};
    let mut kept = Vec::with_capacity(streams.len());
    let mut seen = std::collections::HashSet::new();
    for (i, url) in streams.iter().enumerate() {
        // One post under two addresses (".../visegrad24/status/N" and
        // ".../i/status/N") is asked about, and kept, once.
        if !seen.insert(omni_core::urlnorm::normalize(url)) {
            continue;
        }
        let ask = i < 12 && is_video_platform(url) && !is_youtube(url);
        let has_video = if ask {
            // Dropping the future kills yt-dlp (run_capture sets kill_on_drop).
            tokio::select! {
                r = engine.post_has_video(job_id, url) => r,
                _ = cancel.cancelled() => break,
            }
        } else {
            None
        };
        if ask && has_video == Some(false) {
            info!("Job #{job_id}: {url} has no video; not a candidate");
            let _ = repo.record_event(job_id, "INFO", Some(JobStage::Extract), &format!("Embedded post without a video, skipped: {url}"));
            continue;
        }
        kept.push(url.clone());
    }
    kept
}

/// The job's failure once the sniffer has also come back empty-handed.
///
/// A page the browser opened and found no video on is reported as that
/// (NO_STREAM_FOUND): yt-dlp's "Unsupported URL" for a news article only
/// means it has no extractor for the portal, which is why the sniffer ran,
/// and told MCR to "try a direct video link" for an article that has none.
/// A browser that never got that far leaves yt-dlp's failure standing.
fn after_failed_sniff(previous: anyhow::Error, sniff_err: anyhow::Error) -> anyhow::Error {
    match sniff_err.downcast_ref::<omni_browser::BrowserError>() {
        Some(omni_browser::BrowserError::NoStreamFound(_)) => {
            anyhow::anyhow!("{}: no video found in the page", ErrorCode::NoStreamFound.as_str())
                .context(ErrorCode::NoStreamFound.as_str())
        }
        _ => previous,
    }
}

/// Whether the engine stopped because this worker no longer holds the job
/// (P1.14). Nothing more may be written for it: `finish` and `requeue_after`
/// would refuse, but the events and sniffing would not.
fn lost_lease<T>(result: &anyhow::Result<T>) -> bool {
    matches!(result, Err(e) if e.downcast_ref::<omni_broadcast::pipeline::LeaseLost>().is_some())
}

/// The one readable line for a failure's `error_message` (P7.15): the root
/// cause, without the code strings the pipeline wraps around it.
/// The error chain for the job's DEBUG diagnostic, as `{e:#}` would print it,
/// minus a bare code layer whose next layer already starts with that code: the
/// pipeline adds `.context(code)` to a download error that names its code itself,
/// which printed "LIVE_STREAM: LIVE_STREAM: …" (2026-10-19).
fn diagnostic_chain(e: &anyhow::Error) -> String {
    let parts: Vec<String> = e.chain().map(|c| c.to_string()).collect();
    let mut kept: Vec<&str> = Vec::new();
    for (i, part) in parts.iter().enumerate() {
        let repeated = ErrorCode::from_code(part).is_some()
            && parts.get(i + 1).is_some_and(|next| next.starts_with(part.as_str()));
        if !repeated {
            kept.push(part);
        }
    }
    kept.join(": ")
}

fn readable_cause(e: &anyhow::Error) -> String {
    const CAP: usize = 300;
    const TAIL: &str = ". stderr tail: ";
    let outer = format!("{e}");
    let chain: Vec<String> = e.chain().map(|c| c.to_string()).collect();
    let Some(ri) = chain.iter().rposition(|s| ErrorCode::from_code(s).is_none()) else { return outer };
    let root = &chain[ri];

    let strip = |mut text: &str| -> String {
        loop {
            let before = text;
            if let Some(rest) = text.strip_prefix("ERROR:") {
                text = rest.trim_start();
            }
            for code in ErrorCode::ALL {
                if let Some(rest) = text.strip_prefix(code.as_str()).and_then(|r| r.strip_prefix(':')) {
                    text = rest.trim_start();
                }
            }
            if text == before {
                return text.trim().to_string();
            }
        }
    };

    let text = if let Some((header, tail)) = root.split_once(TAIL) {
        // A tool that failed (omni_core::process): keep "exited / timed out",
        // add the tail's FIRST line that names an error. FFmpeg 7+ follows
        // the cause ("Error while opening encoder - maybe incorrect
        // parameters…") with generic thread-teardown lines that also say
        // "error"; those name nothing (seen with the installed FFmpeg 9).
        const GENERIC: [&str; 5] = [
            "Conversion failed!",
            "Task finished with error code",
            "Terminating thread with return code",
            "Error sending frames to consumers",
            "Nothing was written into output file",
        ];
        let header = strip(header);
        let hit = tail
            .lines()
            .map(|l| without_log_prefixes(l.trim()))
            .find(|l| l.to_ascii_lowercase().contains("error") && !GENERIC.iter().any(|g| l.contains(g)));
        match hit {
            Some(l) => format!("{header}: {}", strip(l)),
            None => header,
        }
    } else {
        // A stderr tail: the ERROR line if there is one, else the last line with text.
        let lines: Vec<&str> = root.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        let line = lines.iter().rev().find(|l| l.contains("ERROR")).or(lines.last()).copied().unwrap_or("");
        let line = strip(line);
        // A bare OS or serde error says little alone; keep what was being done.
        // The OS error goes first: a parent quoting two long paths
        // ("Failed copying … to …") would otherwise push it past the cap.
        let parent = ri.checked_sub(1).map(|i| chain[i].as_str());
        match parent {
            Some(p) if lines.len() <= 1 && ErrorCode::from_code(p).is_none() && !p.trim().is_empty() && !line.is_empty() => {
                format!("{line} ({})", p.trim())
            }
            _ => line,
        }
    };
    if text.is_empty() {
        return outer;
    }
    if text.chars().count() > CAP {
        let cut: String = text.chars().take(CAP).collect();
        return format!("{}…", cut.trim_end());
    }
    text
}

/// "[vost#0:0/mpeg2video @ 000001] [enc:mpeg2video @ 000002] Error …" ->
/// "Error …": FFmpeg's per-component prefixes carry only addresses.
fn without_log_prefixes(mut line: &str) -> &str {
    while line.starts_with('[') {
        match line.find("] ") {
            Some(end) => line = line[end + 2..].trim_start(),
            None => break,
        }
    }
    line
}

/// Recover the error code the pipeline attached to a failure.
///
/// The pipeline wraps failures with `.context(code.as_str())`, so the code is in
/// the anyhow chain: the first code of `errors::CLASSIFIED` the chain's text
/// names as a whole token (`errors::code_in_text`). Anything unrecognised goes
/// to a human rather than being retried on a guess.
pub(crate) fn classify_pipeline_error(e: &anyhow::Error) -> ErrorCode {
    omni_broadcast::errors::code_in_text(&format!("{e:#}")).unwrap_or(ErrorCode::PipelineFailed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P2.15: a stop lets a job that reached the watchfolder step finish, so it
    /// leaves neither a stale `.tmp` holding its slug nor a requeue that
    /// delivers it twice; earlier stages are left to recovery at once.
    #[tokio::test]
    async fn a_stop_waits_for_a_delivering_job_and_for_nothing_else() {
        use omni_core::models::{JobStage, JobStatus};
        use std::time::Duration;
        let busy = omni_core::busy::BusyJobs::new();
        let budget = Duration::from_secs(5);

        // Nothing busy, and a job still transcoding: no wait.
        let t = std::time::Instant::now();
        assert_eq!(super::wait_for_deliveries(&busy, |_| None, budget).await, 0);
        let transcoding = busy.guard(1, "w1");
        assert_eq!(super::wait_for_deliveries(&busy, |_| Some((JobStatus::Running, JobStage::Transcode)), budget).await, 0);
        assert!(t.elapsed() < Duration::from_secs(1));

        // A delivering job is waited for until its worker is done with it.
        let delivering = busy.guard(2, "w2");
        let stage = |id: i64| Some((JobStatus::Running, if id == 2 { JobStage::Deliver } else { JobStage::Transcode }));
        let t = std::time::Instant::now();
        let done = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(600)).await;
            drop(delivering);
        });
        assert_eq!(super::wait_for_deliveries(&busy, stage, budget).await, 0);
        assert!(t.elapsed() >= Duration::from_millis(500), "returned before the delivery ended");
        done.await.unwrap();

        // One that never ends is given up on at the budget, and counted.
        let _stuck = busy.guard(3, "w3");
        let t = std::time::Instant::now();
        let left = super::wait_for_deliveries(&busy, |id| Some((JobStatus::Running, if id == 3 { JobStage::Archive } else { JobStage::Transcode })), Duration::from_millis(400)).await;
        assert_eq!(left, 1);
        assert!(t.elapsed() < Duration::from_secs(2));
        drop(transcoding);

        // A job already finished in the database is not waited for.
        assert!(!super::is_delivering(JobStatus::Completed, JobStage::Done));
        assert!(super::is_delivering(JobStatus::Running, JobStage::Deliver));
        assert!(!super::is_delivering(JobStatus::Running, JobStage::Verify));
    }

    #[tokio::test]
    async fn the_page_scan_never_picks_a_contextual_list_or_a_live_video() {
        use omni_broadcast::downloader::{choose_primary, Choice, Probe};
        let cl = "https://player.glomex.com/integration/1/iframe-player.html?integrationId=i1&playlistId=cl-abc".to_string();
        let live = "https://www.youtube.com/watch?v=liveliveli1".to_string();
        let x = "https://x.com/i/status/2".to_string();
        let run = |videos: Vec<String>, live: String| async move {
            let (cands, meta) = page_scan_cands(&videos);
            let l = live.clone();
            (choose_primary(&cands, 3, move |u| {
                let is = u == l;
                async move { if is { Probe::Live } else { Probe::Ok } }
            })
            .await, meta)
        };
        // [cl-, LIVE youtube, x post] -> the x post.
        let (c, meta) = run(vec![cl.clone(), live.clone(), x.clone()], live.clone()).await;
        assert!(matches!(c, Choice::Primary { ref url, .. } if *url == x), "{c:?}");
        assert!(meta[&cl].contextual && meta.len() == 1);
        // [LIVE youtube, cl-] -> all live, not the cl- list.
        let (c, _) = run(vec![live.clone(), cl.clone()], live.clone()).await;
        assert!(matches!(c, Choice::AllLive { .. }), "{c:?}");
        // Nothing live: the first non-contextual, as before; a concrete clip keeps its place.
        let clip = "https://player.glomex.com/integration/1/iframe-player.html?integrationId=i1&playlistId=v-abc".to_string();
        let (c, meta) = run(vec![clip.clone(), x.clone()], String::new()).await;
        assert!(matches!(c, Choice::Primary { ref url, .. } if *url == clip), "{c:?}");
        assert!(meta.is_empty());
        let (c, _) = run(vec![cl.clone(), x.clone()], String::new()).await;
        assert!(matches!(c, Choice::Primary { ref url, .. } if *url == x), "{c:?}");
    }

    #[test]
    fn a_single_video_on_another_site_names_that_site_and_leaves_the_job_alone() {
        assert!(other_site("https://www.news.example/story/1", "https://x.com/a/status/1"));
        assert!(!other_site("https://www.news.example/story/1", "https://cdn.news.example/v/1"));
        // The hint is keyed on the embed's site while the job's own address stays the article.
        let dir = std::env::temp_dir().join(format!("omni-f6-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let repo = Repository::new(dir.join("omni.db")).unwrap();
        let article = "https://www.news.example/story/1";
        let embed = Some("https://x.com/a/status/1".to_string());
        assert_eq!(
            omni_broadcast::errors::hint_for_job(ErrorCode::LoginRequired, embed.as_deref().unwrap_or(article), &repo),
            omni_broadcast::errors::login_hint_el("x.com", false)
        );
        assert_eq!(
            omni_broadcast::errors::hint_for_job(ErrorCode::LoginRequired, None::<String>.as_deref().unwrap_or(article), &repo),
            ErrorCode::LoginRequired.hint_el()
        );
        drop(repo);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A scratch database that removes itself.
    struct ScratchDb(std::path::PathBuf);
    impl ScratchDb {
        fn new() -> (Self, Repository) {
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let dir = std::env::temp_dir().join(format!("omni-main-test-{}-{nanos}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            let repo = Repository::new(dir.join("omni.db")).unwrap();
            (Self(dir), repo)
        }
    }
    impl Drop for ScratchDb {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// One leased job's run on a scratch database, with what the worker loop
    /// would hand it (P11.8).
    struct LeasedRun {
        db: ScratchDb,
        ctx: JobRunCtx,
        busy: omni_core::busy::BusyJobs,
        gate: omni_core::update_gate::UpdateGate,
        jobs: Arc<tokio::sync::Semaphore>,
        downloads: Arc<tokio::sync::Semaphore>,
    }

    impl LeasedRun {
        fn new(heartbeat: Duration) -> Self {
            let (db, repo) = ScratchDb::new();
            let engine = Arc::new(BroadcastEngine::new(
                repo.clone(),
                db.0.join("bin").join("yt-dlp.exe"),
                db.0.join("bin").join("ffmpeg.exe"),
                db.0.join("bin").join("ffprobe.exe"),
                db.0.join("bin").join("bmxtranswrap.exe"),
                db.0.join("temp"),
                db.0.join("watch"),
            ));
            let mut ctx = JobRunCtx::new(repo, engine, HealthState::new());
            ctx.heartbeat_interval = heartbeat;
            Self {
                db,
                ctx,
                busy: omni_core::busy::BusyJobs::new(),
                gate: omni_core::update_gate::UpdateGate::new(),
                jobs: Arc::new(tokio::sync::Semaphore::new(1)),
                downloads: Arc::new(tokio::sync::Semaphore::new(1)),
            }
        }

        fn repo(&self) -> &Repository {
            &self.ctx.repo
        }

        /// Queue job `n` and lease it as `owner` for `lease_secs`.
        fn queue_and_lease(&self, n: u32, owner: &str, lease_secs: i64) -> i64 {
            let id = self
                .repo()
                .enqueue(
                    &omni_core::models::NewJob::new(&format!("https://example.gr/p118/{n}"), &format!("{n}_ELENI_P118"), "ELENI"),
                    omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS,
                )
                .unwrap()
                .job_id();
            assert_eq!(self.repo().lease_job(owner, lease_secs).unwrap().map(|j| j.id), Some(id));
            id
        }

        /// What the worker loop hands the run of `id`, and the run's token.
        fn leased(&self, id: i64, owner: &str) -> (LeasedJob, CancellationToken) {
            let busy = self.busy.guard(id, owner);
            let token = busy.token();
            let leased = LeasedJob {
                job_id: id,
                owner: owner.to_string(),
                busy,
                gate_pass: self.gate.try_enter().expect("no update pending"),
                job_slot: self.jobs.clone().try_acquire_owned().expect("a job slot"),
                download_slot: Some(self.downloads.clone().try_acquire_owned().expect("a download slot")),
            };
            (leased, token)
        }

        fn job(&self, id: i64) -> omni_core::models::Job {
            self.repo().get_job(id).unwrap().unwrap()
        }

        fn internal_error_lines(&self, id: i64) -> Vec<omni_core::models::JobEvent> {
            self.repo()
                .get_job_events(id, 100)
                .unwrap()
                .into_iter()
                .filter(|e| e.message.starts_with("Internal error while running the job"))
                .collect()
        }

        /// Whether every run's heartbeat task has ended (within a second).
        async fn heartbeats_stopped(&self) -> bool {
            for _ in 0..100 {
                if self.ctx.heartbeats.lock().unwrap().iter().all(|h| h.is_finished()) {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            false
        }

        /// The 30 s back-off is over.
        fn end_backoff(&self, id: i64) {
            rusqlite::Connection::open(self.db.0.join("omni.db"))
                .unwrap()
                .execute("UPDATE queue SET not_before = NULL WHERE id = ?1", rusqlite::params![id])
                .unwrap();
        }
    }

    /// P11.8: a panic in the job's body left the heartbeat renewing the lease
    /// for ever, so the job stayed RUNNING until the daemon restarted.
    #[tokio::test]
    async fn a_panicking_job_goes_back_to_the_queue_and_its_heartbeat_stops() {
        let r = LeasedRun::new(HEARTBEAT_INTERVAL);
        let id = r.queue_and_lease(1, "w1", LEASE_SECS);
        let (leased, token) = r.leased(id, "w1");
        run_leased_job(&r.ctx, leased, |_| async { panic!("the stderr of https://cdn.example.gr/v/1.mp4?sig=SECRET1 ended") }).await;

        assert!(token.is_cancelled(), "the run's token must fire when the run ends");
        assert!(r.heartbeats_stopped().await, "the heartbeat still runs after the job ended");
        let job = r.job(id);
        assert_eq!(job.status, JobStatus::Pending);
        assert_eq!(job.lease_owner, None);
        assert_eq!(job.lease_expires_at, None);
        assert!(job.not_before.is_some_and(|t| t > chrono::Utc::now()), "requeued without a back-off: {:?}", job.not_before);
        assert_eq!(job.attempts, 1);
        assert_eq!(job.error_code.as_deref(), Some(ErrorCode::PipelineFailed.as_str()));
        let lines = r.internal_error_lines(id);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0].level, "ERROR");
        assert!(lines[0].message.contains("cdn.example.gr/v/1.mp4"), "{}", lines[0].message);
        assert!(!lines[0].message.contains("SECRET1"), "an address's query reached the job: {}", lines[0].message);
    }

    #[tokio::test]
    async fn a_second_panic_sends_the_job_to_review() {
        let r = LeasedRun::new(HEARTBEAT_INTERVAL);
        let id = r.queue_and_lease(1, "w1", LEASE_SECS);
        let (leased, _) = r.leased(id, "w1");
        run_leased_job(&r.ctx, leased, |_| async { panic!("first run") }).await;
        assert_eq!(r.job(id).status, JobStatus::Pending);

        r.end_backoff(id);
        assert_eq!(r.repo().lease_job("w2", LEASE_SECS).unwrap().map(|j| j.id), Some(id));
        let (leased, _) = r.leased(id, "w2");
        let index = 7;
        run_leased_job(&r.ctx, leased, move |_| async move { panic!("index {index} out of range in the second run") }).await;

        let job = r.job(id);
        assert_eq!(job.status, JobStatus::RequiresReview);
        assert_eq!(job.attempts, 2);
        assert_eq!(job.lease_owner, None);
        assert_eq!(job.error_code.as_deref(), Some(ErrorCode::PipelineFailed.as_str()));
        assert_eq!(job.error_message.as_deref(), Some("internal error (panic): index 7 out of range in the second run"));
        assert!(r.heartbeats_stopped().await);
    }

    #[tokio::test]
    async fn a_normal_job_body_behaves_as_before() {
        let r = LeasedRun::new(Duration::from_millis(20));

        // Ok: the job is left exactly as the body left it (here: untouched).
        let id = r.queue_and_lease(1, "w1", LEASE_SECS);
        let (leased, token) = r.leased(id, "w1");
        run_leased_job(&r.ctx, leased, |_| async { Ok(()) }).await;
        let job = r.job(id);
        assert_eq!(job.status, JobStatus::Running);
        assert_eq!(job.lease_owner.as_deref(), Some("w1"));
        assert_eq!(job.error_code, None);
        assert!(token.is_cancelled());
        // Ten heartbeat intervals later the lease has not been renewed.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(r.job(id).lease_expires_at, job.lease_expires_at, "the heartbeat renewed a lease after the run ended");
        assert!(r.internal_error_lines(id).is_empty());
        r.repo().finish(id, "w1", JobStatus::Completed, None, None, None).unwrap();

        // Err: review, exactly as the old bookkeeping path.
        let id = r.queue_and_lease(2, "w1", LEASE_SECS);
        let (leased, _) = r.leased(id, "w1");
        run_leased_job(&r.ctx, leased, |_| async { Err(anyhow::anyhow!("the stage table could not be written")) }).await;
        let job = r.job(id);
        assert_eq!(job.status, JobStatus::RequiresReview);
        assert_eq!(job.lease_owner, None);
        assert_eq!(job.error_code.as_deref(), Some(ErrorCode::PipelineFailed.as_str()));
        assert_eq!(job.error_message.as_deref(), Some("the stage table could not be written"));
        assert!(r.internal_error_lines(id).is_empty());
        assert!(r.heartbeats_stopped().await);
        assert!(r.ctx.health.get(omni_core::health::checks::WORKERS).is_none(), "no panic, no amber");
    }

    #[tokio::test]
    async fn a_panic_releases_the_slot_and_the_busy_entry() {
        let r = LeasedRun::new(HEARTBEAT_INTERVAL);
        let id = r.queue_and_lease(1, "w1", LEASE_SECS);
        let (leased, _) = r.leased(id, "w1");
        // What the body saw: download slot held, busy entry, update-gate pass, permits taken.
        let seen = Arc::new(Mutex::new(None));
        let (engine, busy, gate, jobs, downloads, sink) =
            (r.ctx.engine.clone(), r.busy.clone(), r.gate.clone(), r.jobs.clone(), r.downloads.clone(), seen.clone());
        run_leased_job(&r.ctx, leased, move |_| async move {
            *sink.lock().unwrap() = Some((
                engine.holds_download_slot(id, "w1"),
                busy.contains(id),
                gate.active(),
                jobs.available_permits(),
                downloads.available_permits(),
            ));
            panic!("after everything was taken")
        })
        .await;

        assert_eq!(*seen.lock().unwrap(), Some((true, true, 1, 0, 0)), "the body did not run holding everything");
        assert!(!r.ctx.engine.holds_download_slot(id, "w1"), "the download slot was kept");
        assert!(!r.busy.contains(id), "the busy entry was kept: MCR could never queue the job again");
        assert_eq!(r.gate.active(), 0, "the update-gate pass was kept: a yt-dlp update would wait for ever");
        assert_eq!(r.jobs.available_permits(), 1, "the in-flight permit was kept");
        assert_eq!(r.downloads.available_permits(), 1, "the download permit was kept");
    }

    #[tokio::test]
    async fn a_panic_after_the_lease_was_lost_writes_nothing() {
        let r = LeasedRun::new(HEARTBEAT_INTERVAL);

        // The reaper took the job back and another worker leased it.
        let id = r.queue_and_lease(1, "w1", -5);
        let (leased, _) = r.leased(id, "w1");
        let repo = r.repo().clone();
        run_leased_job(&r.ctx, leased, move |_| async move {
            assert_eq!(repo.reap_expired_leases().unwrap(), (1, 0));
            assert!(repo.lease_job("w2", LEASE_SECS).unwrap().is_some());
            panic!("after the lease was lost")
        })
        .await;
        let job = r.job(id);
        assert_eq!(job.status, JobStatus::Running);
        assert_eq!(job.lease_owner.as_deref(), Some("w2"));
        assert_eq!(job.attempts, 2);
        assert_eq!(job.not_before, None);
        assert_eq!(job.error_code, None);
        assert!(r.internal_error_lines(id).is_empty());

        // MCR cancelled the job.
        let id = r.queue_and_lease(2, "w1", LEASE_SECS);
        let (leased, _) = r.leased(id, "w1");
        let repo = r.repo().clone();
        run_leased_job(&r.ctx, leased, move |_| async move {
            repo.cancel_job(id).unwrap();
            panic!("after MCR cancelled")
        })
        .await;
        let job = r.job(id);
        assert_eq!(job.status, JobStatus::Cancelled);
        assert_eq!(job.lease_owner, None);
        assert_eq!(job.error_code, None);
        assert_eq!(job.error_message, None);
        assert!(r.internal_error_lines(id).is_empty());
        // Still counted: the code panicked whoever owns the job now.
        assert_eq!(r.ctx.health.get(omni_core::health::checks::WORKERS).unwrap().state, omni_core::health::Health::Degraded);
    }

    #[tokio::test]
    async fn the_workers_line_goes_amber_after_a_panic_and_green_a_day_later() {
        use omni_core::health::{checks, Health};
        let r = LeasedRun::new(HEARTBEAT_INTERVAL);
        r.ctx.refresh_health();
        let line = r.ctx.health.get(checks::WORKERS).expect("a workers line");
        assert_eq!((line.state, line.detail.as_deref()), (Health::Ok, Some("running")));

        let id = r.queue_and_lease(1, "w1", LEASE_SECS);
        let (leased, _) = r.leased(id, "w1");
        run_leased_job(&r.ctx, leased, |_| async { panic!("boom") }).await;
        let line = r.ctx.health.get(checks::WORKERS).unwrap();
        assert_eq!(line.state, Health::Degraded, "amber, never red: other jobs still run");
        assert_eq!(line.detail.as_deref(), Some("1 job(s) hit an internal error in the last 24 h"));
        r.ctx.refresh_health();
        assert_eq!(r.ctx.health.get(checks::WORKERS).unwrap().state, Health::Degraded, "the next lease keeps it amber");

        // The window: two panics an hour ago count; a day later none does.
        let t = Instant::now();
        let mut panics = VecDeque::from([t, t]);
        let c = workers_check(&mut panics, t + Duration::from_secs(3600));
        assert_eq!((c.state, c.detail.as_deref()), (Health::Degraded, Some("2 job(s) hit an internal error in the last 24 h")));
        let c = workers_check(&mut panics, t + PANIC_WINDOW);
        assert_eq!((c.state, c.detail.as_deref()), (Health::Ok, Some("running")));
        assert!(panics.is_empty());
    }

    /// A body that reaches DELIVER, records the marker, optionally puts a file of
    /// that size at the marker's path, and panics (P11.8 review).
    async fn panic_while_delivering(r: &LeasedRun, n: u32, file_there: bool) -> (i64, String) {
        let id = r.queue_and_lease(n, "w1", LEASE_SECS);
        let watch = r.db.0.join("watch");
        std::fs::create_dir_all(&watch).unwrap();
        let path = watch.join(format!("{n}_ELENI_P118.mxf")).to_string_lossy().into_owned();
        let (leased, _) = r.leased(id, "w1");
        let (repo, at) = (r.repo().clone(), path.clone());
        run_leased_job(&r.ctx, leased, move |_| async move {
            assert!(repo.set_stage(id, "w1", JobStage::Deliver).unwrap());
            assert!(repo.mark_delivering(id, "w1", &at, 4096).unwrap());
            if file_there {
                std::fs::write(&at, vec![0u8; 4096]).unwrap();
            }
            panic!("after the rename into the watchfolder")
        })
        .await;
        (id, path)
    }

    /// The review's blocker: a panic after the rename requeued the job, and the
    /// re-run put a second copy (`_1`) on air.
    #[tokio::test]
    async fn a_panic_after_the_rename_finishes_the_job_and_never_requeues_it() {
        let r = LeasedRun::new(HEARTBEAT_INTERVAL);
        let (id, path) = panic_while_delivering(&r, 1, true).await;
        let job = r.job(id);
        assert_eq!(job.status, JobStatus::Completed, "a delivered video was not finished: {:?}", job.status);
        assert_eq!(job.stage, JobStage::Done);
        assert_eq!(job.lease_owner, None);
        assert_eq!(job.attempts, 1);
        assert_eq!(job.not_before, None);
        assert_eq!(job.error_code, None);
        assert_eq!(job.file_path.as_deref(), Some(path.as_str()));
        let lines = r.internal_error_lines(id);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].message.contains("after the video was delivered"), "{}", lines[0].message);
        let events = r.repo().get_job_events(id, 100).unwrap();
        assert!(
            events.iter().any(|e| e.message == "Recovered after an internal error: already delivered to the watchfolder as 1_ELENI_P118.mxf; archive skipped"),
            "{events:?}"
        );
        assert!(r.repo().lease_job("w2", LEASE_SECS).unwrap().is_none(), "the job was leased again");
        assert!(std::path::Path::new(&path).exists(), "the delivered file is never touched");
    }

    #[tokio::test]
    async fn a_panic_at_deliver_with_no_file_goes_to_review_never_back_to_the_queue() {
        let r = LeasedRun::new(HEARTBEAT_INTERVAL);
        let (id, _) = panic_while_delivering(&r, 1, false).await;
        let job = r.job(id);
        assert_eq!(job.status, JobStatus::RequiresReview, "{:?}", job.status);
        assert_eq!(job.lease_owner, None);
        assert_eq!(job.not_before, None);
        assert_eq!(job.attempts, 1);
        assert_eq!(job.error_code.as_deref(), Some(ErrorCode::DeliveryUnconfirmed.as_str()));
        assert_eq!(
            job.error_message.as_deref(),
            Some("An internal error happened while delivering 1_ELENI_P118.mxf; check Dalet before retrying")
        );
        let lines = r.internal_error_lines(id);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].message.contains("during delivery"), "{}", lines[0].message);
        let events = r.repo().get_job_events(id, 100).unwrap();
        assert!(
            events.iter().any(|e| e.message == "Internal error during delivery of 1_ELENI_P118.mxf; the watchfolder does not have it. Not requeued"),
            "{events:?}"
        );
        assert!(r.repo().lease_job("w2", LEASE_SECS).unwrap().is_none(), "the job was leased again");
    }

    #[tokio::test]
    async fn a_panic_at_archive_after_the_recorded_delivery_finishes_the_job() {
        let r = LeasedRun::new(HEARTBEAT_INTERVAL);
        let id = r.queue_and_lease(1, "w1", LEASE_SECS);
        let watch = r.db.0.join("watch");
        std::fs::create_dir_all(&watch).unwrap();
        let path = watch.join("1_ELENI_P118.mxf").to_string_lossy().into_owned();
        let (leased, _) = r.leased(id, "w1");
        let (repo, at) = (r.repo().clone(), path.clone());
        run_leased_job(&r.ctx, leased, move |_| async move {
            assert!(repo.set_stage(id, "w1", JobStage::Deliver).unwrap());
            assert!(repo.mark_delivering(id, "w1", &at, 4096).unwrap());
            std::fs::write(&at, vec![0u8; 4096]).unwrap();
            assert!(repo.set_delivered_file(id, "w1", &at, 12.0).unwrap());
            assert!(repo.set_stage(id, "w1", JobStage::Archive).unwrap());
            // Dalet has already taken the file: the recorded delivery is the proof.
            std::fs::remove_file(&at).unwrap();
            panic!("while archiving the source")
        })
        .await;
        let job = r.job(id);
        assert_eq!(job.status, JobStatus::Completed, "{:?}", job.status);
        assert_eq!(job.attempts, 1);
        assert_eq!(job.file_path.as_deref(), Some(path.as_str()));
        assert_eq!(r.internal_error_lines(id).len(), 1);
        assert!(r.repo().lease_job("w2", LEASE_SECS).unwrap().is_none());
    }

    #[tokio::test]
    async fn a_panic_at_transcode_is_still_requeued_once() {
        let r = LeasedRun::new(HEARTBEAT_INTERVAL);
        let id = r.queue_and_lease(1, "w1", LEASE_SECS);
        let (leased, _) = r.leased(id, "w1");
        let repo = r.repo().clone();
        run_leased_job(&r.ctx, leased, move |_| async move {
            assert!(repo.set_stage(id, "w1", JobStage::Transcode).unwrap());
            panic!("mid-encode")
        })
        .await;
        let job = r.job(id);
        assert_eq!(job.status, JobStatus::Pending);
        assert!(job.not_before.is_some_and(|t| t > chrono::Utc::now()));
        assert_eq!(job.error_code.as_deref(), Some(ErrorCode::PipelineFailed.as_str()));
        assert_eq!(r.internal_error_lines(id).len(), 1);
    }

    /// A daemon stop drops the run: its job body must stop with it, not run on
    /// detached holding the job; the job is left RUNNING for start-up recovery.
    #[tokio::test]
    async fn a_dropped_run_stops_its_job_body() {
        let r = LeasedRun::new(HEARTBEAT_INTERVAL);
        let id = r.queue_and_lease(1, "w1", LEASE_SECS);
        let (leased, token) = r.leased(id, "w1");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let (alive_tx, alive_rx) = tokio::sync::oneshot::channel::<()>();
        let ctx = r.ctx.clone();
        let run = tokio::spawn(async move {
            run_leased_job(&ctx, leased, move |_| async move {
                let _alive = alive_tx;
                let _ = started_tx.send(());
                std::future::pending::<()>().await;
                Ok(())
            })
            .await
        });
        started_rx.await.expect("the body started");
        run.abort();
        let _ = run.await;
        assert!(
            tokio::time::timeout(Duration::from_secs(2), alive_rx).await.is_ok_and(|r| r.is_err()),
            "the job body still runs after its run was dropped"
        );
        assert!(token.is_cancelled());
        assert!(r.heartbeats_stopped().await);
        assert!(!r.busy.contains(id));
        assert_eq!(r.jobs.available_permits(), 1);
        assert_eq!(r.job(id).status, JobStatus::Running, "left to start-up recovery, as before");
    }

    #[test]
    fn a_panic_text_names_no_full_address_and_stays_short() {
        assert_eq!(panic_text(&"plain"), "plain");
        assert_eq!(panic_text(&String::from("owned")), "owned");
        assert_eq!(panic_text(&42_u32), "non-string panic payload");
        let t = panic_text(&String::from("of `https://user:pw@news.example.gr/a/1?token=T0K#x` here"));
        assert_eq!(t, "of news.example.gr/a/1 here");
        assert_eq!(panic_text(&"ά".repeat(500)).chars().count(), 300);
        // A JSON-escaped address.
        let t = panic_text(&String::from(r#"bad json {"url":"https:\/\/news.example.gr\/a\/2?token=T1"} end"#));
        assert_eq!(t, "bad json news.example.gr/a/2 end");
        // A `url::Url` printed with `{:?}`.
        let t = panic_text(&String::from(
            r#"Url { scheme: "https", username: "", password: Some("PW1"), host: Some(Domain("news.example.gr")), port: None, path: "/a", query: Some("sig=S1&x=\")y"), fragment: Some("F1") }"#,
        ));
        for secret in ["PW1", "sig=", "S1", ")y", "F1"] {
            assert!(!t.contains(secret), "{secret} in {t}");
        }
        assert!(t.contains(r#"password: Some("…")"#) && t.contains(r#"query: Some("…")"#) && t.contains(r#"fragment: Some("…")"#), "{t}");
        assert!(t.contains("news.example.gr") && t.contains(r#"path: "/a""#), "{t}");
    }

    #[test]
    fn finish_held_parks_the_job_for_its_owner_and_nobody_else() {
        let (_db, repo) = ScratchDb::new();
        let id = repo
            .enqueue(
                &omni_core::models::NewJob::new("https://example.gr/h", "1_ANNA_HELD", "ANNA"),
                omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS,
            )
            .unwrap()
            .job_id();
        repo.lease_job("w1", 180).unwrap().expect("leased");

        assert!(!finish_held(&repo, id, "w2").unwrap(), "another worker must not park the job");
        assert_eq!(repo.get_job(id).unwrap().unwrap().status, JobStatus::Running);

        assert!(finish_held(&repo, id, "w1").unwrap());
        let job = repo.get_job(id).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::AwaitingApproval);
        assert_eq!(job.lease_owner, None);
        assert!(!finish_held(&repo, id, "w1").unwrap(), "the lease is gone once parked");
        assert!(repo.lease_job("w3", 180).unwrap().is_none(), "a parked job is not leased");
    }

    /// P10.5: a changed held file is a terminal, named failure, never "unexpected".
    #[test]
    fn a_changed_held_file_classifies_as_its_own_terminal_code() {
        let e = anyhow::anyhow!("{}: the video changed", ErrorCode::HeldFileChanged.as_str())
            .context(ErrorCode::HeldFileChanged.as_str());
        let code = classify_pipeline_error(&e);
        assert_eq!(code, ErrorCode::HeldFileChanged);
        assert!(!code.is_retryable(), "a file that is not the approved one is never retried into delivery");
    }

    /// P11.1: a failed encode or rewrap keeps its code; it used to read
    /// PIPELINE_FAILED, "unexpected error", as did a full disk in rewrap (job 68).
    #[test]
    fn a_stage_failure_keeps_its_code_and_an_uncoded_one_is_unexpected() {
        let rewrap = anyhow::anyhow!("bmxtranswrap exited with code Some(1). stderr tail: ERROR: BMX exception caught")
            .context("REWRAP_FAILED")
            .context("bmxtranswrap RDD9 failed");
        assert_eq!(classify_pipeline_error(&rewrap), ErrorCode::RewrapFailed);
        let encode = anyhow::anyhow!("ffmpeg exited with code Some(1). stderr tail: Conversion failed!")
            .context("TRANSCODE_FAILED")
            .context("FFmpeg Transcode failed");
        assert_eq!(classify_pipeline_error(&encode), ErrorCode::TranscodeFailed);
        let full = anyhow::anyhow!("bmxtranswrap exited with code Some(1). stderr tail: ERROR: 'mxf_file->write(data, size) == size' check failed")
            .context(ErrorCode::LowDisk.as_str());
        assert_eq!(classify_pipeline_error(&full), ErrorCode::LowDisk);
        assert!(classify_pipeline_error(&full).is_retryable());
        let plain = anyhow::anyhow!("x").context("something went wrong");
        assert_eq!(classify_pipeline_error(&plain), ErrorCode::PipelineFailed);
        // A code inside a longer identifier (a slug in a path) is not that code.
        let slug = anyhow::anyhow!("could not open C:\\temp\\jobs\\9\\3_NETWORK_OUTAGE.mxf");
        assert_eq!(classify_pipeline_error(&slug), ErrorCode::PipelineFailed);
    }

    /// 2026-10-19: a live download's DEBUG line read "LIVE_STREAM: LIVE_STREAM: …".
    #[test]
    fn the_diagnostic_chain_names_a_code_once_and_keeps_every_other_layer() {
        let e = anyhow::anyhow!("LIVE_STREAM: the video is live or not yet started; yt-dlp skipped it").context("LIVE_STREAM");
        assert_eq!(format!("{e:#}"), "LIVE_STREAM: LIVE_STREAM: the video is live or not yet started; yt-dlp skipped it");
        assert_eq!(diagnostic_chain(&e), "LIVE_STREAM: the video is live or not yet started; yt-dlp skipped it");
        // A code layer over a different message, and plain layers, all stay.
        let e = anyhow::anyhow!("exited with 1. stderr tail: boom").context("transcode failed").context("TRANSCODE_FAILED");
        assert_eq!(diagnostic_chain(&e), format!("{e:#}"));
    }

    /// Job #20 (2026-10-08): the card said PIPELINE_FAILED; the reason was only
    /// in the timeline.
    #[test]
    fn readable_cause_is_the_root_of_the_chain() {
        let e = anyhow::anyhow!("ERROR: [twitter] 2106778297484939750: No video could be found in this tweet")
            .context("PIPELINE_FAILED");
        assert_eq!(readable_cause(&e), "[twitter] 2106778297484939750: No video could be found in this tweet");
    }

    #[test]
    fn readable_cause_prefers_the_error_line_of_a_stderr_tail() {
        let e = anyhow::anyhow!("WARNING: something minor\nERROR: [generic] Unable to download webpage: HTTP Error 404\n\n")
            .context("PAGE_NOT_FOUND");
        assert_eq!(readable_cause(&e), "[generic] Unable to download webpage: HTTP Error 404");
        let plain = anyhow::anyhow!("first\nlast line\n\n").context("NETWORK");
        assert_eq!(readable_cause(&plain), "last line");
    }

    #[test]
    fn readable_cause_of_a_bare_code_is_the_code() {
        let e = anyhow::anyhow!("PIPELINE_FAILED");
        assert_eq!(readable_cause(&e), "PIPELINE_FAILED");
        let repeated = anyhow::anyhow!("PIPELINE_FAILED: PIPELINE_FAILED: boom").context("PIPELINE_FAILED");
        assert_eq!(readable_cause(&repeated), "boom");
    }

    #[test]
    fn readable_cause_keeps_the_header_of_a_tool_failure() {
        let ff = anyhow::anyhow!(
            "ffmpeg exited with code Some(1). stderr tail: frame=0 fps=0.0\n[mpeg2video @ 000001] Error initializing output stream\nError while opening encoder for output stream #0:0 - maybe incorrect parameters\nConversion failed!\n"
        )
        .context("PIPELINE_FAILED");
        let s = readable_cause(&ff);
        assert!(s.starts_with("ffmpeg exited with code Some(1)"), "{s}");
        assert!(s.ends_with(": Error initializing output stream"), "the first error line, without its [… @ addr] prefix: {s}");
        assert!(!s.contains("Conversion failed"), "{s}");

        // The tail the installed FFmpeg 9 writes for an encoder that will not
        // open (VBV buffer too small): the cause comes first, then lines
        // that also say "error" but name nothing.
        let ff9 = anyhow::anyhow!(concat!(
            "ffmpeg exited with code Some(1). stderr tail: ",
            "[mpeg2video @ 0000029b555468c0] VBV buffer too small for bitrate\n",
            "[vost#0:0/mpeg2video @ 0000029b55546680] [enc:mpeg2video @ 0000029b4afd5540] Error while opening encoder - maybe incorrect parameters such as bit_rate, rate, width or height.\n",
            "[vf#0:0 @ 0000029b55549840] Error sending frames to consumers: Invalid argument\n",
            "[vf#0:0 @ 0000029b55549840] Task finished with error code: -22 (Invalid argument)\n",
            "[vost#0:0/mpeg2video @ 0000029b55546680] [enc:mpeg2video @ 0000029b4afd5540] Could not open encoder before EOF\n",
            "[vf#0:0 @ 0000029b55549840] Terminating thread with return code -22 (Invalid argument)\n",
            "[vost#0:0/mpeg2video @ 0000029b55546680] Task finished with error code: -22 (Invalid argument)\n",
            "[vost#0:0/mpeg2video @ 0000029b55546680] Terminating thread with return code -22 (Invalid argument)\n",
            "[out#0/null @ 0000029b55544780] Nothing was written into output file, because at least one of its streams received no packets.\n",
            "frame=    0 fps=0.0 q=0.0 Lsize=       0KiB time=N/A bitrate=N/A speed=N/A elapsed=0:00:00.02\n",
            "Conversion failed!\n",
        ))
        .context("TRANSCODE_FAILED");
        assert_eq!(
            readable_cause(&ff9),
            "ffmpeg exited with code Some(1): Error while opening encoder - maybe incorrect parameters such as bit_rate, rate, width or height."
        );

        let timeout = anyhow::anyhow!("ffmpeg timed out after 3600s; process tree killed. stderr tail: frame=10\nspeed=0.1x\n")
            .context("TRANSCODE_TIMEOUT");
        assert_eq!(readable_cause(&timeout), "ffmpeg timed out after 3600s; process tree killed");

        let ytdlp = anyhow::anyhow!("yt-dlp exited with code Some(1). stderr tail: WARNING: x\nERROR: [twitter] 1: No video could be found\n")
            .context("PIPELINE_FAILED");
        let s = readable_cause(&ytdlp);
        assert!(s.starts_with("yt-dlp exited with code Some(1)"), "{s}");
        assert!(s.ends_with("[twitter] 1: No video could be found"), "{s}");
    }

    #[test]
    fn readable_cause_keeps_the_parent_of_a_bare_os_error() {
        let e = anyhow::anyhow!("The network path was not found. (os error 53)")
            .context("Failed creating the watchfolder")
            .context("DELIVERY_FAILED");
        let s = readable_cause(&e);
        assert!(s.contains("Failed creating the watchfolder") && s.contains("os error 53"), "{s}");
        // Two long quoted paths in the parent must not push the OS error
        // itself past the cap.
        let long = format!("Failed copying \"{}\" to \"{}\"", "C:\\OmniIngest\\temp\\jobs\\42\\x".repeat(6), "\\\\dalet\\watch\\y".repeat(8));
        let e = anyhow::anyhow!("Access is denied. (os error 5)").context(long).context("DELIVERY_FAILED");
        assert!(readable_cause(&e).starts_with("Access is denied. (os error 5)"));
        // A code above the root is not a parent worth quoting.
        let coded = anyhow::anyhow!("boom").context("PIPELINE_FAILED");
        assert_eq!(readable_cause(&coded), "boom");
    }

    #[test]
    fn readable_cause_is_capped_on_a_char_boundary() {
        let e = anyhow::anyhow!("Σφάλμα λήψης βίντεο ".repeat(50)).context("PIPELINE_FAILED");
        let s = readable_cause(&e);
        assert!(s.chars().count() <= 301, "{}", s.chars().count());
        assert!(s.ends_with('…'));
    }

    /// The engine's lost-lease error is recognised by type; a failure that
    /// merely quotes the code in its text (a URL ending in it) is not.
    #[test]
    fn a_lost_lease_is_recognised_by_type_not_by_text() {
        let lost: anyhow::Result<()> = Err(anyhow::Error::new(omni_broadcast::pipeline::LeaseLost { job_id: 7, stage: JobStage::Rewrap })
            .context(ErrorCode::LeaseExpired.as_str()));
        assert!(lost_lease(&lost));

        let quoting: anyhow::Result<()> = Err(anyhow::anyhow!("ERROR: unable to download https://example.gr/LEASE_EXPIRED")
            .context("LEASE_EXPIRED"));
        assert!(!lost_lease(&quoting));
        assert!(!lost_lease(&Ok(())));
    }

    /// neakriti.gr article without a video (2026-10-05): the card said
    /// UNSUPPORTED_URL, "try a direct video link".
    #[test]
    fn a_page_without_a_video_is_reported_as_no_stream_not_unsupported() {
        let yt_dlp = || anyhow::anyhow!("ERROR: Unsupported URL: https://www.neakriti.gr/x").context(ErrorCode::UnsupportedUrl.as_str());

        let empty = anyhow::Error::from(omni_browser::BrowserError::NoStreamFound("https://www.neakriti.gr/x".into()));
        assert_eq!(classify_pipeline_error(&after_failed_sniff(yt_dlp(), empty)), ErrorCode::NoStreamFound);

        let no_browser = anyhow::Error::from(omni_browser::BrowserError::LaunchFailed("no Chrome".into()));
        assert_eq!(classify_pipeline_error(&after_failed_sniff(yt_dlp(), no_browser)), ErrorCode::UnsupportedUrl);
    }

    /// A mistyped YouTube id (2026-10-07): the card said «Απρόβλεπτο
    /// σφάλμα». The downloader's code must come back out of the chain the
    /// pipeline wraps around it.
    #[test]
    fn an_unavailable_video_keeps_its_code_through_the_pipeline() {
        let line = "ERROR: [youtube] zzzzzzzzzz0: This video is unavailable";
        let code = omni_broadcast::errors::classify_download_error(line);
        let e = anyhow::Error::from(omni_broadcast::downloader::DownloadError { code, message: line.into() })
            .context(code.as_str());
        assert_eq!(classify_pipeline_error(&e), ErrorCode::PrivateOrRemoved);
    }

    fn verdict(class: EvidenceClass, signals: &[&str], diagnosed: bool) -> Evidence {
        Evidence { class, signals: signals.iter().map(|s| s.to_string()).collect(), diagnosed }
    }

    #[test]
    fn evidence_names_the_exact_failure() {
        let orig = ErrorCode::NoStreamFound;
        let (c, m) = evidence_code(orig, &verdict(EvidenceClass::None, &[], false));
        assert_eq!((c, m), (orig, None), "not diagnosed: no claim that the page has no video");
        let (c, m) = evidence_code(orig, &verdict(EvidenceClass::None, &[], true));
        assert_eq!(c, ErrorCode::NoVideoOnPage);
        assert_eq!(m.as_deref(), Some("Δεν βρέθηκε βίντεο ούτε στον δεύτερο έλεγχο"));
        assert_eq!(evidence_code(ErrorCode::Http403, &verdict(EvidenceClass::Weak, &["player-container"], true)), (ErrorCode::Http403, None));
        let (c, m) = evidence_code(orig, &verdict(EvidenceClass::Strong, &["og:video", "embed:jwplayer"], false));
        assert_eq!(c, ErrorCode::VideoUnresolved);
        assert_eq!(m.as_deref(), Some("Βρέθηκε βίντεο στη σελίδα (og:video, embed:jwplayer) αλλά δεν κατέβηκε"));
        let (c, m) = evidence_code(orig, &verdict(EvidenceClass::Protected, &["eme:com.widevine.alpha", "mse:video/mp4"], true));
        assert_eq!(c, ErrorCode::DrmProtected);
        assert_eq!(m.as_deref(), Some("Το βίντεο έχει DRM (com.widevine.alpha)"));
        assert_eq!(evidence_code(orig, &verdict(EvidenceClass::LoginWall, &["http-401 cdn.example"], true)), (ErrorCode::LoginRequired, None));
        assert_eq!(evidence_code(orig, &verdict(EvidenceClass::GeoBlocked, &["http-451 cdn.example"], false)), (ErrorCode::GeoBlocked, None));
        // every result is a final code: nothing here sends a job round again
        for class in [EvidenceClass::Protected, EvidenceClass::LoginWall, EvidenceClass::GeoBlocked, EvidenceClass::Strong] {
            let (c, _) = evidence_code(ErrorCode::UnsupportedUrl, &verdict(class, &[], true));
            assert!(!c.is_retryable(), "{c}");
        }
    }

    #[test]
    fn the_second_look_needs_repair_on_a_page_and_nothing_proven() {
        use EvidenceClass as C;
        for passive in [None, Some(C::Weak), Some(C::None)] {
            assert!(!should_diagnose(RepairMode::Off, passive, false), "off never diagnoses");
            assert!(should_diagnose(RepairMode::Shadow, passive, false));
            assert!(should_diagnose(RepairMode::Hold, passive, false));
            assert!(!should_diagnose(RepairMode::Hold, passive, true), "a platform post is never re-sniffed");
        }
        for passive in [C::Protected, C::LoginWall, C::GeoBlocked, C::Strong] {
            assert!(!should_diagnose(RepairMode::Hold, Some(passive), false), "{passive:?} is already decided");
        }
    }

    /// A job the site itself named a video for can never be "no video on the
    /// page", and needs no second look.
    #[test]
    fn one_scanned_video_counts_as_named_by_the_site() {
        assert!(!site_named_video(false, 0));
        assert!(site_named_video(false, 1), "a single video is enough, not only several");
        assert!(site_named_video(false, 3));
        assert!(site_named_video(true, 0));
    }

    #[test]
    fn a_video_the_site_named_is_never_reported_missing() {
        assert!(with_site_hint(None, false).is_none());
        let named = with_site_hint(None, true).unwrap();
        assert_eq!(named.class, EvidenceClass::Strong);
        assert_eq!(named.signals, vec!["named-by-site"]);
        assert_eq!(evidence_code(ErrorCode::NoStreamFound, &named).0, ErrorCode::VideoUnresolved);
        assert!(!should_diagnose(RepairMode::Hold, Some(named.class), false));
        // over a weak or empty look it wins; over a refusal it does not
        let weak = verdict(EvidenceClass::Weak, &["player-container"], false);
        let both = with_site_hint(Some(weak), true).unwrap();
        assert_eq!(both.class, EvidenceClass::Strong);
        assert!(both.signals.contains(&"player-container".to_string()));
        let drm = with_site_hint(Some(verdict(EvidenceClass::Protected, &["drm:setkeys"], false)), true).unwrap();
        assert_eq!(drm.class, EvidenceClass::Protected);
    }

    /// A job MCR cancelled while the page was judged gets no verdict event and
    /// keeps today's code; the same page for a job still ours gets both.
    #[tokio::test]
    async fn a_cancelled_job_gets_no_evidence_verdict() {
        let db = std::env::temp_dir().join(format!("omni_p102_cancel_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&db);
        let repo = Repository::new(&db).unwrap();
        let id = repo
            .enqueue(&omni_core::models::NewJob::new("https://news.example/a", "slug-a", "Test Person"), 24)
            .unwrap()
            .job_id();
        let job = repo.lease_job("w1", 60).unwrap().expect("leased");
        assert_eq!(job.id, id);
        let page = || {
            let mut snap = omni_browser::PageSnapshot::new("https://news.example/a");
            snap.meta = vec![("og:video".into(), "https://cdn.example/v.mp4".into())];
            FailedPage {
                url: "https://news.example/a",
                mode: RepairMode::Off,
                snapshot: Some(snap),
                sniffed_streams: &[],
                original: ErrorCode::NoStreamFound,
                named_by_site: false,
            }
        };
        let notes = |repo: &Repository| {
            repo.get_job_events(id, 50).unwrap().iter().filter(|e| e.message.starts_with("Evidence:")).count()
        };

        let cancel = CancellationToken::new();
        cancel.cancel();
        let (code, line, _) = assess_failed_page(&repo, "w1", id, &cancel, page()).await;
        assert_eq!((code, line), (ErrorCode::NoStreamFound, None));
        assert_eq!(notes(&repo), 0, "no verdict for a cancelled job");

        let live = CancellationToken::new();
        let (code, line, _) = assess_failed_page(&repo, "w1", id, &live, page()).await;
        assert_eq!(code, ErrorCode::VideoUnresolved);
        assert!(line.is_some());
        assert_eq!(notes(&repo), 1);

        let (code, _, _) = assess_failed_page(&repo, "someone-else", id, &live, page()).await;
        assert_eq!(code, ErrorCode::NoStreamFound, "a lost lease gets no verdict either");
        assert_eq!(notes(&repo), 1);
        drop(repo);
        let _ = std::fs::remove_file(&db);
    }
}

#[cfg(test)]
mod sniff_fallback_tests {
    use super::{falls_back_to_ytdlp, sniff_result_end, SniffEnd};


    #[test]
    fn a_sniff_that_ended_on_a_platform_page_is_not_taken_but_only_when_the_browser_was_first() {
        let post = Some("https://www.youtube.com/watch?v=abcdefghijk");
        let page = Some("https://www.example-news.gr/article");
        assert_eq!(sniff_result_end(true, true, post), Some(SniffEnd::LandedOnPlatform));
        assert_eq!(sniff_result_end(true, true, page), None);
        assert_eq!(sniff_result_end(true, true, None), None);
        assert_eq!(sniff_result_end(false, true, post), None, "yt-dlp went first for this page");
        assert_eq!(sniff_result_end(true, false, page), Some(SniffEnd::Error));
        assert!(falls_back_to_ytdlp(true, sniff_result_end(true, true, post).unwrap()));
    }

    #[test]
    fn yt_dlp_is_tried_after_the_browser_only_when_it_found_no_usable_video() {
        assert!(falls_back_to_ytdlp(true, SniffEnd::Error));
        assert!(falls_back_to_ytdlp(true, SniffEnd::PostsHaveNoVideo));
        assert!(falls_back_to_ytdlp(true, SniffEnd::LandedOnPlatform));
        assert!(falls_back_to_ytdlp(true, SniffEnd::SiteWide), "yt-dlp's pick may be another video; guard_scan blocks a furniture one");
        for end in [
            SniffEnd::Found,
            SniffEnd::AllLive,
            SniffEnd::AllDead,
            SniffEnd::SiblingsHoldTheRest,
            SniffEnd::Cancelled,
            SniffEnd::LeaseLost,
            SniffEnd::NotRun,
        ] {
            assert!(!falls_back_to_ytdlp(true, end), "{end:?}");
        }
    }

    #[test]
    fn a_page_the_browser_was_not_asked_first_never_falls_back() {
        assert!(!falls_back_to_ytdlp(false, SniffEnd::Error));
        assert!(!falls_back_to_ytdlp(false, SniffEnd::PostsHaveNoVideo));
    }
}

/// Whether a failed job's end is written to its site's health (P10.3): only a finished,
/// uncancelled job of its own page, with a code that says something about the site. A page whose
/// only videos are the site's furniture (P3.16) fails the job but says nothing about the site.
fn site_failure_counts(done: bool, cancelled: bool, is_child: bool, furniture_only: bool, code: &str) -> bool {
    done && !cancelled && !is_child && !furniture_only && omni_core::site_health::counts_against_site(code)
}

#[cfg(test)]
mod site_failure_tests {
    use super::*;

    #[test]
    fn a_furniture_only_ending_is_not_a_site_failure_but_a_real_no_stream_found_is() {
        assert!(site_failure_counts(true, false, false, false, "NO_STREAM_FOUND"), "control: counts as before");
        assert!(!site_failure_counts(true, false, false, true, "NO_STREAM_FOUND"));
    }

    #[test]
    fn the_furniture_ending_leaves_the_stored_site_health_unchanged() {
        let (_d, repo) = crate::site_memory::tests::repo();
        let health = HealthState::new();
        let url = "https://www.news.example/a/story-1";
        // As the end of run_job does it: only what site_failure_counts allows is recorded.
        for furniture_only in [true, false] {
            if site_failure_counts(true, false, false, furniture_only, "NO_STREAM_FOUND") {
                note_site_outcome(&repo, &health, 1, url, SiteOutcome::SiteFailure("NO_STREAM_FOUND".into()));
            }
            let row = repo.get_site("news.example").unwrap();
            if furniture_only {
                assert!(row.is_none(), "nothing recorded for the site");
            } else {
                assert_eq!(row.unwrap().consecutive_fail, 1, "a real NO_STREAM_FOUND still counts");
            }
        }
    }
}

/// Whether a failed job ended for lack of a video only because the page's videos are the site's
/// furniture (P3.16), which says nothing about the site: `pick_refused` is yt-dlp's first pick refused
/// as furniture (the route where yt-dlp goes first), `sniff` how the browser's look ended, and
/// `fallback_refused` whether yt-dlp's pick after the browser was refused (`None`: not made).
fn furniture_only_end(pick_refused: bool, sniff: SniffEnd, fallback_refused: Option<bool>) -> bool {
    match sniff {
        SniffEnd::SiteWide => fallback_refused.unwrap_or(true),
        SniffEnd::Error | SniffEnd::PostsHaveNoVideo | SniffEnd::LandedOnPlatform => fallback_refused.unwrap_or(pick_refused),
        SniffEnd::NotRun => pick_refused,
        SniffEnd::Found
        | SniffEnd::AllLive
        | SniffEnd::AllDead
        | SniffEnd::SiblingsHoldTheRest
        | SniffEnd::Cancelled
        | SniffEnd::LeaseLost => false,
    }
}

#[cfg(test)]
mod furniture_end_tests {
    use super::*;

    #[test]
    fn the_browser_first_route_ends_on_furniture_unless_yt_dlps_own_pick_is_another_video() {
        assert!(furniture_only_end(false, SniffEnd::SiteWide, None), "no pick was made after the browser");
        assert!(furniture_only_end(false, SniffEnd::SiteWide, Some(true)), "the pick was the same promo");
        assert!(!furniture_only_end(false, SniffEnd::SiteWide, Some(false)), "another video that then failed is a real failure");
        assert!(furniture_only_end(false, SniffEnd::Error, Some(true)), "browser found nothing, pick refused");
        assert!(!furniture_only_end(false, SniffEnd::Error, Some(false)));
    }

    #[test]
    fn the_yt_dlp_first_route_keeps_a_refused_pick_through_a_sniff_that_finds_nothing() {
        for end in [SniffEnd::Error, SniffEnd::PostsHaveNoVideo, SniffEnd::SiteWide, SniffEnd::NotRun] {
            assert!(furniture_only_end(true, end, None), "{end:?}");
            assert!(!furniture_only_end(false, end, None) || end == SniffEnd::SiteWide, "{end:?}: no refused pick, a real failure");
        }
    }

    #[test]
    fn a_sniff_that_chose_a_video_or_ended_for_another_reason_is_a_real_ending() {
        for end in [SniffEnd::Found, SniffEnd::AllLive, SniffEnd::AllDead, SniffEnd::SiblingsHoldTheRest, SniffEnd::Cancelled, SniffEnd::LeaseLost] {
            assert!(!furniture_only_end(true, end, Some(true)), "{end:?}");
        }
    }
}

#[cfg(test)]
#[path = "parked_delivery_tests.rs"]
mod parked_delivery_tests;

#[cfg(test)]
#[path = "retry_policy_tests.rs"]
mod retry_policy_tests;
