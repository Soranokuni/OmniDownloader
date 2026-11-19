//! The yt-dlp update, early, when a platform starts failing (plan P11.4d).
//!
//! The workers note each job that ended with an extraction failure on a
//! platform ([`EarlyYtdlUpdate::note_job_failure`]). Three on one platform in
//! 30 minutes, with `ytdl_update` last run more than 6 hours ago, send one
//! request to the scheduler, which runs the nightly task's own update
//! ([`EarlyYtdlUpdate::run_requested`]) and records it as a run of
//! `ytdl_update`. The 03:00 run that night is not moved.
//!
//! In the daytime the update waits only [`EARLY_DRAIN`] for running jobs
//! (intake is paused while it waits). A run that found them still busy
//! swapped nothing: it is not recorded, and the next request may come
//! 30 minutes later.

use chrono::{DateTime, Utc};
use omni_core::extraction_trouble::{self, ExtractionTrouble};
use omni_core::repository::Repository;
use omni_core::scheduler::TaskOutcome;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// The scheduler's task name the early run is recorded under.
pub(crate) const TASK: &str = "ytdl_update";

/// How long the 03:00 update waits for running jobs before it postpones.
pub(crate) const NIGHTLY_DRAIN: Duration = Duration::from_secs(600);

/// How long an early (daytime) update waits for running jobs: intake is
/// paused meanwhile, so only briefly.
pub(crate) const EARLY_DRAIN: Duration = Duration::from_secs(90);

/// How one yt-dlp update ended: the task outcome and a short reason.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct UpdateRun {
    pub outcome: TaskOutcome,
    /// A few words for the maintenance panel ("kept", "rolled back", ...).
    pub reason: &'static str,
    /// Nothing was swapped because jobs were still running after the drain.
    pub drain_postponed: bool,
}

impl UpdateRun {
    pub(crate) fn new(outcome: TaskOutcome, reason: &'static str) -> Self {
        Self { outcome, reason, drain_postponed: false }
    }

    /// The pool did not drain in time; the staged build waits.
    pub(crate) fn drain_postponed() -> Self {
        Self { outcome: TaskOutcome::Skipped, reason: "postponed: jobs still running", drain_postponed: true }
    }
}

/// Shared by the workers (through the canary env) and the scheduler.
pub(crate) struct EarlyYtdlUpdate {
    /// `ytdl_auto_update_nightly`: off, nothing is ever requested.
    enabled: bool,
    trouble: ExtractionTrouble,
    /// No early request before this time: 6 hours after the last one, or
    /// 30 minutes after a run that found the pool busy. In memory: a restart
    /// forgets it, and the 6 hours since the last recorded run still hold.
    not_before: Mutex<Option<DateTime<Utc>>>,
    requests: mpsc::Sender<&'static str>,
    /// Held by the nightly task and by an early run for their whole update:
    /// an early request that finds it taken is dropped, never queued.
    running: Arc<tokio::sync::Mutex<()>>,
}

impl EarlyYtdlUpdate {
    /// The shared value and the scheduler's end of the request channel.
    pub(crate) fn new(enabled: bool) -> (Arc<Self>, mpsc::Receiver<&'static str>) {
        // One slot: a second request while one waits is the same request.
        let (tx, rx) = mpsc::channel(1);
        let me = Self {
            enabled,
            trouble: ExtractionTrouble::new(),
            not_before: Mutex::new(None),
            requests: tx,
            running: Arc::new(tokio::sync::Mutex::new(())),
        };
        (Arc::new(me), rx)
    }

    fn not_before(&self) -> std::sync::MutexGuard<'_, Option<DateTime<Utc>>> {
        self.not_before.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// A job on `url` just ended for good with `code`. True when this asked
    /// the scheduler for an early update. Never waits, never fails the job.
    pub(crate) fn note_job_failure(&self, repo: &Repository, url: &str, code: &str, now: DateTime<Utc>) -> bool {
        if !self.enabled || !extraction_trouble::counts(code) {
            return false;
        }
        let Some(platform) = omni_broadcast::pacing::platform_key(url) else { return false };
        if !self.trouble.note_failure(platform, now) {
            return false;
        }
        // Check and set under one lock: many workers failing at once send one request.
        let mut not_before = self.not_before();
        if !ytdl_run_is_old(repo, *not_before, now) {
            return false;
        }
        *not_before = Some(extraction_trouble::not_before_after_request(now));
        match self.requests.try_send(platform) {
            Ok(()) => {
                info!("Early yt-dlp update requested: {} extraction failures on {platform} in 30 minutes", extraction_trouble::THRESHOLD);
                true
            }
            Err(e) => {
                warn!("Early yt-dlp update for {platform} not requested: {e}");
                false
            }
        }
    }

    /// Hold while the nightly `ytdl_update` runs.
    pub(crate) async fn lock_running(&self) -> tokio::sync::OwnedMutexGuard<()> {
        self.running.clone().lock_owned().await
    }

    /// The scheduler's side of a request for `platform`: run `update` (the
    /// nightly task's, given [`EARLY_DRAIN`]) and record it as a run of
    /// `ytdl_update` with an `early:` note. Dropped, with a log line, while an
    /// update runs or when `ytdl_update` ran within the last 6 hours (the
    /// nightly run may have finished while the request waited). A run the
    /// busy pool postponed is not recorded. True when `update` ran.
    pub(crate) async fn run_requested<F, Fut>(&self, repo: &Repository, platform: &str, now: DateTime<Utc>, update: F) -> bool
    where
        F: FnOnce(Duration) -> Fut,
        Fut: Future<Output = UpdateRun>,
    {
        if !self.enabled {
            return false;
        }
        let Ok(_running) = self.running.try_lock() else {
            info!("Early yt-dlp update for {platform} dropped: an update is already running");
            return false;
        };
        if !ytdl_run_is_old(repo, None, now) {
            info!("Early yt-dlp update for {platform} dropped: yt-dlp was checked less than 6 hours ago");
            return false;
        }
        let line = extraction_trouble::audit_line(platform);
        info!("{line}");
        let _ = repo.log_audit("INFO", "SYSTEM", &line);
        let started = std::time::Instant::now();
        let run = update(EARLY_DRAIN).await;
        let elapsed_ms = started.elapsed().as_millis() as i64;
        if run.drain_postponed {
            // Nothing was swapped: not a run, and another early request may come soon.
            *self.not_before() = Some(extraction_trouble::not_before_after_busy(now));
            info!(
                "Early yt-dlp update postponed: jobs were still running after {} s; another early update may run in 30 minutes",
                EARLY_DRAIN.as_secs()
            );
            return true;
        }
        info!("Early yt-dlp update: {} ({})", run.outcome.as_str(), run.reason);
        let note = format!("early: {}", run.reason);
        if let Err(e) = repo.record_task_run(TASK, run.outcome, Some(&note), elapsed_ms) {
            warn!("The early yt-dlp update was not recorded: {e:#}");
        }
        true
    }
}

/// Whether `ytdl_update` last ran (nightly or early) more than 6 hours ago,
/// or never, and `now` is not before `not_before`. A schedule that cannot be
/// read says no: nothing runs on a guess.
fn ytdl_run_is_old(repo: &Repository, not_before: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    match repo.list_scheduled_tasks(&omni_core::scheduler::default_tasks()) {
        Ok(tasks) => {
            let last_run = tasks.into_iter().find(|t| t.name == TASK).and_then(|t| t.last_run);
            extraction_trouble::early_run_due(last_run, not_before, now)
        }
        Err(e) => {
            warn!("The last yt-dlp update run could not be read: {e:#}");
            false
        }
    }
}

#[cfg(test)]
#[path = "early_update_tests.rs"]
mod tests;
