//! P11.4d: the early yt-dlp update, with a fake updater that counts its calls.

use super::*;
use omni_core::health::HealthState;
use omni_core::scheduler::{default_tasks, TaskStatus};
use std::sync::atomic::{AtomicUsize, Ordering};

const YT: &str = "https://www.youtube.com/watch?v=abc123";
const X: &str = "https://x.com/someone/status/1";

struct Rig {
    _db: crate::site_memory::tests::TestDb,
    repo: Repository,
    early: Arc<EarlyYtdlUpdate>,
    rx: mpsc::Receiver<&'static str>,
    calls: Arc<AtomicUsize>,
    /// The drain each fake update was given.
    drains: Arc<Mutex<Vec<Duration>>>,
}

fn rig() -> Rig {
    let (db, repo) = crate::site_memory::tests::repo();
    repo.ensure_scheduled_tasks(&default_tasks()).unwrap();
    let (early, rx) = EarlyYtdlUpdate::new(true);
    Rig { _db: db, repo, early, rx, calls: Arc::new(AtomicUsize::new(0)), drains: Arc::new(Mutex::new(Vec::new())) }
}

impl Rig {
    fn fail(&self, url: &str, code: &str) -> bool {
        self.fail_at(url, code, Utc::now())
    }

    fn fail_at(&self, url: &str, code: &str, now: DateTime<Utc>) -> bool {
        self.early.note_job_failure(&self.repo, url, code, now)
    }

    fn task(&self) -> TaskStatus {
        self.repo.list_scheduled_tasks(&default_tasks()).unwrap().into_iter().find(|t| t.name == TASK).unwrap()
    }

    /// The scheduler's side: take every waiting request and run the fake updater for it.
    async fn serve(&mut self) -> usize {
        self.serve_with(UpdateRun::new(TaskOutcome::Ok, "kept"), Utc::now()).await
    }

    /// As [`Self::serve`], the fake update ending with `run`, the scheduler's clock at `now`.
    async fn serve_with(&mut self, run: UpdateRun, now: DateTime<Utc>) -> usize {
        let mut ran = 0;
        while let Ok(platform) = self.rx.try_recv() {
            let (calls, drains) = (self.calls.clone(), self.drains.clone());
            let update = move |drain: Duration| async move {
                calls.fetch_add(1, Ordering::SeqCst);
                drains.lock().unwrap().push(drain);
                run
            };
            if self.early.run_requested(&self.repo, platform, now, update).await {
                ran += 1;
            }
        }
        ran
    }

    fn audit_rows(&self) -> usize {
        let line = extraction_trouble::audit_line("youtube");
        self.repo.get_recent_audit_logs(50).unwrap().iter().filter(|a| a.message == line).count()
    }
}

#[tokio::test]
async fn three_extraction_failures_on_one_platform_run_the_update_once_as_a_ytdl_update_run() {
    let mut r = rig();
    let next_before = r.task().next_run;
    assert!(r.task().last_run.is_none());

    assert!(!r.fail(YT, "NO_STREAM_FOUND"));
    assert!(!r.fail(YT, "HTTP_403"));
    assert!(r.fail(YT, "VIDEO_UNRESOLVED"), "the third failure asks");
    assert_eq!(r.serve().await, 1);
    assert_eq!(r.calls.load(Ordering::SeqCst), 1);

    let task = r.task();
    assert!(task.last_run.is_some(), "recorded as a run of ytdl_update");
    assert_eq!(task.last_outcome.as_deref(), Some(TaskOutcome::Ok.as_str()));
    assert_eq!(task.last_error.as_deref(), Some("early: kept"), "the panel tells it from the 03:00 run");
    assert_eq!(task.next_run, next_before, "the 03:00 run is not moved");
    assert_eq!(*r.drains.lock().unwrap(), vec![Duration::from_secs(90)], "the early run waits 90 s for jobs");
    assert_eq!(r.audit_rows(), 1);

    // A fourth failure in the same minute: the tracker is still tripped, but nothing more runs.
    assert!(!r.fail(YT, "NO_STREAM_FOUND"));
    assert!(!r.fail(YT, "UNSUPPORTED_URL"));
    assert_eq!(r.serve().await, 0);
    assert_eq!(r.calls.load(Ordering::SeqCst), 1);
    assert_eq!(r.audit_rows(), 1);
}

#[tokio::test]
async fn rate_limits_and_logins_are_not_extractor_breakage() {
    let mut r = rig();
    for _ in 0..5 {
        assert!(!r.fail(YT, "RATE_LIMITED"));
        assert!(!r.fail(YT, "LOGIN_REQUIRED"));
    }
    assert_eq!(r.serve().await, 0);
    assert_eq!(r.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn platforms_do_not_add_up_and_a_news_site_is_no_platform() {
    let mut r = rig();
    assert!(!r.fail(YT, "NO_STREAM_FOUND"));
    assert!(!r.fail(YT, "NO_STREAM_FOUND"));
    assert!(!r.fail(X, "NO_STREAM_FOUND"));
    for _ in 0..3 {
        assert!(!r.fail("https://www.news.example/a/story-1", "NO_STREAM_FOUND"));
    }
    assert_eq!(r.serve().await, 0);
}

#[tokio::test]
async fn a_recent_ytdl_update_run_means_no_early_one() {
    let mut r = rig();
    r.repo.record_task_run(TASK, TaskOutcome::Ok, None, 10).unwrap();
    for _ in 0..3 {
        r.fail(YT, "NO_STREAM_FOUND");
    }
    assert_eq!(r.serve().await, 0);
    assert_eq!(r.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_request_that_finds_an_update_running_is_dropped() {
    let mut r = rig();
    for _ in 0..3 {
        r.fail(YT, "NO_STREAM_FOUND");
    }
    let nightly = r.early.lock_running().await;
    assert_eq!(r.serve().await, 0, "ran beside the nightly update");
    drop(nightly);
    assert_eq!(r.calls.load(Ordering::SeqCst), 0);
    assert!(r.task().last_run.is_none());
    // Dropped, not queued: nothing runs once the nightly one is done either.
    assert_eq!(r.serve().await, 0);
    assert_eq!(r.calls.load(Ordering::SeqCst), 0);
    // One request per 6 hours, even one that was dropped.
    for _ in 0..3 {
        assert!(!r.fail(YT, "NO_STREAM_FOUND"));
    }
    assert_eq!(r.serve().await, 0);
}

#[tokio::test]
async fn a_request_that_waited_for_the_nightly_run_is_dropped() {
    let mut r = rig();
    for _ in 0..3 {
        r.fail(YT, "NO_STREAM_FOUND");
    }
    // The nightly run finished while the request sat in the channel.
    r.repo.record_task_run(TASK, TaskOutcome::Ok, None, 10).unwrap();
    assert_eq!(r.serve().await, 0);
    assert_eq!(r.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn many_workers_failing_at_once_send_one_request() {
    let mut r = rig();
    let sent = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..12 {
            s.spawn(|| {
                if r.early.note_job_failure(&r.repo, YT, "NO_STREAM_FOUND", Utc::now()) {
                    sent.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });
    assert_eq!(sent.load(Ordering::SeqCst), 1);
    assert_eq!(r.serve().await, 1);
    assert_eq!(r.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn off_when_the_nightly_update_is_off() {
    let (db, repo) = crate::site_memory::tests::repo();
    repo.ensure_scheduled_tasks(&default_tasks()).unwrap();
    let (early, mut rx) = EarlyYtdlUpdate::new(false);
    for _ in 0..3 {
        assert!(!early.note_job_failure(&repo, YT, "NO_STREAM_FOUND", Utc::now()));
    }
    assert!(rx.try_recv().is_err());
    assert!(!early.run_requested(&repo, "youtube", Utc::now(), |_| async { UpdateRun::new(TaskOutcome::Ok, "kept") }).await);
    drop(db);
}

#[tokio::test]
async fn the_worker_counts_only_a_job_it_finished_and_nobody_cancelled() {
    let mut r = rig();
    let canary = crate::canary::CanaryEnv::new(r.repo.clone(), HealthState::new(), std::path::PathBuf::from("bin"), "stable".into());
    // Without the daemon's early update (tests, a bare env): nothing.
    for _ in 0..3 {
        assert!(!crate::worker::note_extraction_trouble(&r.repo, &canary, true, false, YT, "NO_STREAM_FOUND"));
    }
    canary.set_early_ytdl(r.early.clone());
    for _ in 0..3 {
        assert!(!crate::worker::note_extraction_trouble(&r.repo, &canary, false, false, YT, "NO_STREAM_FOUND"), "not ours");
        assert!(!crate::worker::note_extraction_trouble(&r.repo, &canary, true, true, YT, "NO_STREAM_FOUND"), "cancelled");
    }
    assert_eq!(r.serve().await, 0);
    assert!(!crate::worker::note_extraction_trouble(&r.repo, &canary, true, false, YT, "NO_STREAM_FOUND"));
    assert!(!crate::worker::note_extraction_trouble(&r.repo, &canary, true, false, YT, "NO_STREAM_FOUND"));
    assert!(crate::worker::note_extraction_trouble(&r.repo, &canary, true, false, YT, "NO_STREAM_FOUND"));
    assert_eq!(r.serve().await, 1);
}

#[tokio::test]
async fn an_early_run_the_busy_pool_postponed_is_no_run_and_may_come_again_in_thirty_minutes() {
    let mut r = rig();
    let t0 = Utc::now();
    for _ in 0..3 {
        r.fail_at(YT, "NO_STREAM_FOUND", t0);
    }
    assert_eq!(r.serve_with(UpdateRun::drain_postponed(), t0).await, 1);
    assert_eq!(r.calls.load(Ordering::SeqCst), 1);
    let task = r.task();
    assert!(task.last_run.is_none(), "a postponed early run was recorded as a ytdl_update run");
    assert!(task.last_outcome.is_none() && task.last_error.is_none());

    // 29 minutes later: still failing, but not yet.
    let soon = t0 + chrono::Duration::minutes(29);
    for _ in 0..3 {
        assert!(!r.fail_at(YT, "NO_STREAM_FOUND", soon));
    }
    assert_eq!(r.serve_with(UpdateRun::new(TaskOutcome::Ok, "kept"), soon).await, 0);
    // 31 minutes later: the next early request goes, not 6 hours later.
    let later = t0 + chrono::Duration::minutes(31);
    assert!(r.fail_at(YT, "NO_STREAM_FOUND", later));
    assert_eq!(r.serve_with(UpdateRun::new(TaskOutcome::Ok, "kept"), later).await, 1);
    assert_eq!(r.calls.load(Ordering::SeqCst), 2);
    assert!(r.task().last_run.is_some());
}

#[tokio::test]
async fn every_other_outcome_is_recorded_with_its_reason_and_holds_six_hours() {
    for (outcome, reason) in [
        (TaskOutcome::Failed, "rolled back"),
        (TaskOutcome::Failed, "unverified"),
        (TaskOutcome::Skipped, "postponed by the self-check"),
        (TaskOutcome::Ok, "already the latest"),
        (TaskOutcome::Failed, "install failed"),
    ] {
        let mut r = rig();
        let t0 = Utc::now();
        for _ in 0..3 {
            r.fail_at(YT, "NO_STREAM_FOUND", t0);
        }
        assert_eq!(r.serve_with(UpdateRun::new(outcome, reason), t0).await, 1, "{reason}");
        let task = r.task();
        assert_eq!(task.last_outcome.as_deref(), Some(outcome.as_str()), "{reason}");
        let note = format!("early: {reason}");
        assert_eq!(task.last_error.as_deref(), Some(note.as_str()));
        // 31 minutes later: nothing, the 6 hours hold.
        let later = t0 + chrono::Duration::minutes(31);
        for _ in 0..3 {
            assert!(!r.fail_at(YT, "NO_STREAM_FOUND", later), "{reason}");
        }
    }
}

/// The nightly task gives the update 10 minutes for running jobs, the early run 90 s.
#[test]
fn the_nightly_drain_stays_ten_minutes_and_the_early_one_is_ninety_seconds() {
    assert_eq!(NIGHTLY_DRAIN, Duration::from_secs(600));
    assert_eq!(EARLY_DRAIN, Duration::from_secs(90));
    let main = include_str!("main.rs");
    let task = &main[main.find("\"ytdl_update\" => {").expect("the nightly task")..];
    let task = &task[..task.find("\"selfcheck\" =>").unwrap()];
    assert!(task.contains("ffmpeg.as_deref(), early_update::NIGHTLY_DRAIN)"), "the nightly task's drain");
    let update = &main[main.find("async fn nightly_ytdl_update(").unwrap()..];
    let update = &update[..update.find("\n}").unwrap()];
    assert!(update.contains("if !swap.drain(drain).await {"), "the update waits the drain it is given");
}

/// P11.7b: the update records the pending verdict before it swaps the build in
/// and clears it only once the verdict is in, under the swap lock; start-up
/// recovers an unjudged swap before any yt-dlp is used.
#[test]
fn an_update_marks_its_pending_verdict_around_the_swap_and_start_up_recovers_it() {
    let main = include_str!("main.rs");
    let update = &main[main.find("async fn nightly_ytdl_update(").unwrap()..];
    let update = &update[..update.find("\n}").unwrap()];
    // Every needle at a line's start, so a commented-out call does not count (P11.7b2).
    let at = |s: &str| update.find(s).unwrap_or_else(|| panic!("{s} missing"));
    let mark = at("\n        if let Err(e) = dep_mgr.mark_ytdl_verifying(&staged.sha256) {");
    let apply = at("\n        match dep_mgr.apply_staged_ytdl() {");
    let verdict = at("\n    let verdict = verify_ytdl_update(");
    let clear = verdict + update[verdict..].find("\n    if let Err(e) = dep_mgr.clear_ytdl_verifying() {").expect("cleared after the verdict");
    let unlock = at("\n    drop(swap);");
    assert!(mark < apply, "the marker is written before the swap");
    assert!(verdict < clear && clear < unlock, "cleared after the verdict, before the lock is released");
    // A marker that could not be cleared is audited, not only logged (P11.7b2).
    assert!(update[clear..unlock].contains("\n        let _ = repo.log_audit(\n            \"ERROR\","), "the failed clear is audited");
    let start = main.find("\n    report_unverified_ytdl(&repo, dep_mgr.recover_unverified_ytdl());").expect("start-up recovery");
    assert!(start < main.find("ensure_ytdl_folder_build(&dep_mgr,").unwrap(), "before the tools are resolved");
}

/// P11.7b2: a staged release that is already live is left alone, unless the
/// live build still waits for its verdict: then it is verified as it stands.
#[test]
fn a_live_release_still_waiting_for_its_verdict_is_verified_not_skipped() {
    use crate::{staged_release_action as act, StagedRelease::*};
    assert_eq!(act(Some("bbb"), "bbb", None), AlreadyLatest);
    assert_eq!(act(Some("bbb"), "bbb", Some("aaa")), AlreadyLatest, "a stale marker for another build");
    assert_eq!(act(Some("bbb"), "bbb", Some("bbb")), Reverify);
    assert_eq!(act(Some("aaa"), "bbb", None), Install);
    assert_eq!(act(Some("aaa"), "bbb", Some("bbb")), Install, "not live yet: swapped in and verified");
    assert_eq!(act(None, "bbb", None), Install);
}

/// The worker notes an extraction failure only for a job that ended for good
/// (inside the no-retry branch, after it was finished), with `done` and the cancel state.
#[test]
fn the_worker_notes_extraction_trouble_after_finishing_a_job_it_will_not_retry() {
    let src = include_str!("worker.rs");
    let call = "note_extraction_trouble(repo, canary, done, cancel.is_cancelled(), &orig_url, site_code.as_str());";
    assert_eq!(src.matches(call).count(), 1, "the call in run_job");
    let at = src.find(call).unwrap();
    let before = &src[..at];
    let branch = before.rfind("if !retry_by_policy(repo, engine, job_id, owner, code, &message)? {").expect("the no-retry branch");
    let finish = before.rfind("let done = repo.finish(").expect("the job is finished first");
    assert!(branch < finish, "the finish belongs to the branch");
    // Still inside the branch: its brace never closes before the call.
    let mut depth = 0i32;
    for c in src[branch..at].chars() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                assert!(depth > 0, "the call is outside the no-retry branch");
            }
            _ => {}
        }
    }
}
