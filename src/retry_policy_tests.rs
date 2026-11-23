//! The retry policy at the worker's decision sites (P11.3a), with a real
//! Repository. A failure is made, not waited for: an engine that is stopping
//! fails a delivery at once with DELIVERY_FAILED (the same four waits as
//! NETWORK), and a row that does not wait for the watchfolder sends that
//! failure to the policy instead of to the wait. The run_job site itself is
//! driven with a real attachment whose length no disk can hold (LOW_DISK);
//! that test needs ffmpeg and ffprobe in `bin/` and is skipped without them.

use super::*;
use std::path::{Path, PathBuf};

use omni_core::models::{Job, NewJob};

const OWNER: &str = "host:1:1";

fn tool(name: &str) -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("bin").join(format!("{name}.exe"));
    p.exists().then_some(p)
}

struct Rig {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: Repository,
    engine: Arc<BroadcastEngine>,
}

/// A rig whose engine uses `ffmpeg`/`ffprobe` (made-up paths when `None`).
fn rig_with(tools: Option<(PathBuf, PathBuf)>, stopping: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let repo = Repository::new(root.join("omni.db")).unwrap();
    let (ffmpeg, ffprobe) = tools.unwrap_or_else(|| (root.join("no-ffmpeg.exe"), root.join("no-ffprobe.exe")));
    let engine = BroadcastEngine::new(
        repo.clone(),
        root.join("no-yt-dlp.exe"),
        ffmpeg,
        ffprobe,
        root.join("no-bmx.exe"),
        root.join("temp"),
        root.join("watch"),
    );
    if stopping {
        engine.stop_new_deliveries();
    }
    Rig { _dir: dir, root, repo, engine: Arc::new(engine) }
}

/// A rig whose every delivery fails at once with DELIVERY_FAILED.
fn rig() -> Rig {
    rig_with(None, true)
}

impl Rig {
    fn queue(&self, n: u32) -> i64 {
        self.repo
            .enqueue(&NewJob::new(format!("https://news.example/story-{n}"), format!("{n}_ANNA_TEST"), "ANNA"), omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS)
            .unwrap()
            .job_id()
    }

    fn sql(&self, sql: &str, p: impl rusqlite::Params) {
        rusqlite::Connection::open(self.root.join("omni.db")).unwrap().execute(sql, p).unwrap();
    }

    /// Lease job `id` as the worker loop would, its back-off over.
    fn lease(&self, id: i64) -> Job {
        self.sql("UPDATE queue SET not_before = NULL WHERE id = ?1", rusqlite::params![id]);
        let job = self.repo.lease_job(OWNER, 600).unwrap().expect("a job to lease");
        assert_eq!(job.id, id);
        job
    }

    fn row(&self, id: i64) -> Job {
        self.repo.get_job(id).unwrap().unwrap()
    }

    /// Job `id` was queued `minutes` ago.
    fn queued_ago(&self, id: i64, minutes: i64) {
        let at = omni_core::timestamps::format(chrono::Utc::now() - chrono::Duration::minutes(minutes));
        self.sql("UPDATE queue SET created_at = ?1 WHERE id = ?2", rusqlite::params![at, id]);
    }

    fn ctx(&self) -> repair::RepairCtx {
        repair::RepairCtx {
            repo: self.repo.clone(),
            cfg: repair::RepairCfg {
                mode: RepairMode::Off,
                live: None,
                budget: Duration::from_secs(5),
                hold_hours: 24,
                held_max_bytes: 20 << 30,
                auto_deliver_after_approvals: 5,
                llm: repair::LlmCfg::default(),
                egress_allow: Default::default(),
            },
            env: Arc::new(repair::RealEnv::new(
                self.engine.clone(),
                self.root.join("no-ffprobe.exe"),
                self.root.join("no-yt-dlp.exe"),
                self.root.join("no-ffmpeg.exe"),
            )),
        }
    }

    /// `run_job` as the worker loop runs it, repair off.
    async fn run_job(&self, job: Job) {
        let health = HealthState::new();
        let canary = Arc::new(canary::CanaryEnv::new(self.repo.clone(), health.clone(), self.root.join("bin"), "stable".into()));
        run_job(&self.repo, &self.engine, OWNER, job, CancellationToken::new(), &self.root.join("evidence"), &self.ctx(), &health, &canary)
            .await
            .unwrap();
    }

    /// One failed delivery through `run_job` and its waiting-delivery site.
    async fn fail_waiting(&self, id: i64) {
        let job = self.lease(id);
        assert!(!job.delivery_wait, "the row must not wait, or the failure would park it");
        self.run_job(Job { delivery_wait: true, ..job }).await;
    }

    /// The policy's own call, for a code no stopping engine produces.
    fn decide(&self, id: i64, code: ErrorCode) -> bool {
        retry_by_policy(&self.repo, &self.engine, id, OWNER, code, "test failure").unwrap()
    }

    /// Job `id`, leased, fails with `e` the way `run_job`'s failure site
    /// handles it (P11.7): the DEBUG chain, the policy, and on review the
    /// hint and the end. `run_job` cannot reach a live stream without the
    /// network; `every_failure_site_uses_the_retry_policy` and
    /// `every_review_site_writes_the_review_hint` keep the sites on these calls.
    fn fail_like_run_job(&self, id: i64, e: &anyhow::Error) {
        let code = classify_pipeline_error(e);
        let message = readable_cause(e);
        self.repo.record_event(id, "DEBUG", None, &diagnostic_chain(e)).unwrap();
        if !retry_by_policy(&self.repo, &self.engine, id, OWNER, code, &message).unwrap() {
            let url = self.row(id).url;
            self.repo.record_event(id, "ERROR", None, &review_hint(&self.repo, id, code, &url)).unwrap();
            assert!(self.repo.finish(id, OWNER, JobStatus::RequiresReview, Some(code.as_str()), Some(&message), None).unwrap());
        }
    }

    /// The job's events at `level`.
    fn events(&self, id: i64, level: &str) -> Vec<String> {
        self.repo.get_job_events(id, 500).unwrap().into_iter().filter(|e| e.level == level).map(|e| e.message).collect()
    }
}

/// A yt-dlp failure as the pipeline returns it: the classified `DownloadError`
/// under the pipeline's code layer.
fn download_failure(stderr_line: &str) -> anyhow::Error {
    let code = omni_broadcast::errors::classify_download_error(stderr_line);
    anyhow::Error::from(omni_broadcast::downloader::DownloadError { code, message: stderr_line.to_string() }).context(code.as_str())
}

/// The waits of a live stream, in seconds: checks 10, 30, 60 and 120 minutes after the first run.
const LIVE_WAITS: [i64; 4] = [600, 1200, 1800, 3600];

/// P11.7: a stream still live on its first run goes back to the queue for
/// 10 minutes as LIVE_STREAM, not to review, untouched by anyone; it is late
/// at once. Still live at every check, it goes to review after the fourth
/// with the live text. The page's own line is INFO: a checked-again failure
/// writes no ERROR (its requeue writes one WARN), and the last one writes one
/// about the failure, the review hint (plus the repository's «Finished as»
/// status line every reviewed job gets).
#[test]
fn a_page_whose_videos_are_all_live_is_checked_four_times_then_reviewed() {
    let r = rig();
    let id = r.queue(1);
    for (n, secs) in LIVE_WAITS.into_iter().enumerate() {
        r.lease(id);
        r.queued_ago(id, [0, 10, 30, 60][n]);
        let e = every_video_is_live(&r.repo, id).unwrap();
        r.fail_like_run_job(id, &e);
        let job = r.row(id);
        assert_eq!(job.status, JobStatus::Pending, "failure {} went to review: {:?}", n + 1, job.error_message);
        assert_eq!((job.attempts, job.max_attempts), (n as i32 + 1, 5), "failure {}", n + 1);
        assert_eq!((job.error_code.as_deref(), job.first_error_code.as_deref()), (Some("LIVE_STREAM"), Some("LIVE_STREAM")));
        assert_eq!(job.error_message.as_deref(), Some("every video on the page is live"));
        assert!(!job.human_touched && job.touch_reason.is_none(), "failure {}: the requeue counts as a person", n + 1);
        let left = waits_secs(&job);
        assert!(left > secs - 10 && left <= secs, "failure {}: {left} s, expected {secs}", n + 1);
        assert!(job.is_late(chrono::Utc::now(), omni_core::models::LateAfter::default()), "failure {}: not late", n + 1);
        assert!(r.events(id, "ERROR").is_empty(), "failure {}: {:?}", n + 1, r.events(id, "ERROR"));
        assert_eq!(r.events(id, "WARN").len(), n + 1, "one requeue line per failure");
    }
    r.lease(id);
    r.queued_ago(id, 120);
    let e = every_video_is_live(&r.repo, id).unwrap();
    r.fail_like_run_job(id, &e);
    let job = r.row(id);
    assert_eq!((job.status, job.attempts, job.error_code.as_deref()), (JobStatus::RequiresReview, 5, Some("LIVE_STREAM")));
    assert!(!job.is_late(chrono::Utc::now(), omni_core::models::LateAfter::default()), "a reviewed job is late");
    assert_eq!(failure_errors(&r, id), vec![omni_broadcast::errors::LIVE_STREAM_EXHAUSTED_EL.to_string()]);
    assert_eq!(r.events(id, "ERROR").len(), 2, "the hint and the repository's «Finished as» line: {:?}", r.events(id, "ERROR"));
    assert_eq!(r.events(id, "INFO").iter().filter(|m| m.contains("live stream")).count(), 5, "the page's line, once per run");
    for ev in r.repo.get_job_events(id, 500).unwrap() {
        assert!(ev.message.matches("LIVE_STREAM").count() <= 1, "{}: {}", ev.level, ev.message);
    }
}

/// P11.7: the recording exists at the second check: the job completes, and
/// nobody touched it. A premiere ("will begin in 3 hours") takes the same way
/// as a stream still live, and so does yt-dlp's live filter.
#[test]
fn a_live_download_that_has_its_recording_at_the_second_check_completes_untouched() {
    for line in [
        "ERROR: [youtube] abcdefghijk: This live event will begin in 3 hours",
        "the video is live or not yet started; yt-dlp skipped it",
    ] {
        let r = rig();
        let id = r.queue(1);
        let e = if line.starts_with("ERROR:") {
            download_failure(line)
        } else {
            anyhow::Error::from(omni_broadcast::downloader::DownloadError { code: ErrorCode::LiveStream, message: line.into() })
                .context(ErrorCode::LiveStream.as_str())
        };
        assert_eq!(classify_pipeline_error(&e), ErrorCode::LiveStream, "{line}");
        for (n, secs) in LIVE_WAITS[..2].iter().enumerate() {
            r.lease(id);
            r.fail_like_run_job(id, &e);
            let job = r.row(id);
            assert_eq!((job.status, job.error_code.as_deref()), (JobStatus::Pending, Some("LIVE_STREAM")), "{line}: failure {}", n + 1);
            let left = waits_secs(&job);
            assert!(left > secs - 10 && left <= *secs, "{line}: failure {}: {left} s", n + 1);
            assert!(job.error_message.as_deref().is_some_and(|m| !m.contains("LIVE_STREAM")), "{:?}", job.error_message);
        }
        // The second check: the recording is there, and the lease holder completes the job.
        r.lease(id);
        assert!(r.repo.finish(id, OWNER, JobStatus::Completed, None, None, Some("watch/1_ANNA_TEST.mxf")).unwrap());
        let job = r.row(id);
        assert_eq!(job.status, JobStatus::Completed);
        assert!(!job.human_touched && job.touch_reason.is_none(), "{line}: counted as touched");
        assert_eq!(job.first_error_code.as_deref(), Some("LIVE_STREAM"));
        assert!(!job.is_late(chrono::Utc::now(), omni_core::models::LateAfter::default()));
    }
}

/// P11.7: a live stream queued more than two hours ago is not checked again:
/// its first failure goes to review with the plain text (it never waited).
#[test]
fn a_live_stream_past_the_age_limit_goes_to_review_at_once() {
    let r = rig();
    let id = r.queue(1);
    r.lease(id);
    r.queued_ago(id, 121);
    r.fail_like_run_job(id, &every_video_is_live(&r.repo, id).unwrap());
    let job = r.row(id);
    assert_eq!((job.status, job.error_code.as_deref()), (JobStatus::RequiresReview, Some("LIVE_STREAM")));
    // Never checked again as live: the exhausted text, never the «wait» hint (P11.7b2).
    assert_eq!(failure_errors(&r, id), vec![omni_broadcast::errors::exhausted_hint_el(1)]);
}

/// The job's ERROR lines about its failure: every one but the repository's
/// own status line («Finished as …»), which every job sent to review gets.
fn failure_errors(r: &Rig, id: i64) -> Vec<String> {
    r.events(id, "ERROR").into_iter().filter(|m| !m.starts_with("Finished as ")).collect()
}

/// P11.7: the page's live line is written once per failure, as INFO, by both
/// sites through one helper; neither writes it as ERROR or names the code in its text.
#[test]
fn both_live_page_sites_use_the_one_helper() {
    let src = include_str!("worker.rs");
    assert_eq!(src.matches("every_video_is_live(repo, job_id)?").count(), 2, "the two page sites");
    assert!(!src.contains("\"ERROR\", Some(JobStage::Extract), \"Every video found on the page is a live stream\""));
    assert!(!src.contains("\"{}: every video on the page is live\""), "the page's text names the code again");
}

/// How long job `id` waits now, in whole seconds (rounded up).
fn waits_secs(job: &Job) -> i64 {
    let left = job.not_before.expect("a back-off") - chrono::Utc::now();
    (left.num_milliseconds() + 999) / 1000
}

/// A NETWORK failure on a fresh job: back in the queue for 30 s, and the desk's
/// «έγιναν 1 από 5». The next three wait 2, 5 and 10 minutes; the fifth does
/// not retry.
#[test]
fn a_network_failure_on_a_fresh_job_waits_30_s_and_counts_five_attempts() {
    let r = rig();
    let id = r.queue(1);
    for (n, secs) in [30, 120, 300, 600].into_iter().enumerate() {
        r.lease(id);
        assert!(r.decide(id, ErrorCode::Network), "failure {} was not retried", n + 1);
        let job = r.row(id);
        assert_eq!((job.status, job.attempts, job.max_attempts), (JobStatus::Pending, n as i32 + 1, 5));
        assert_eq!(job.error_code.as_deref(), Some("NETWORK"));
        let left = waits_secs(&job);
        assert!(left > secs - 10 && left <= secs, "failure {}: {left} s, expected {secs}", n + 1);
    }
    r.lease(id);
    assert!(!r.decide(id, ErrorCode::Network), "a fifth try");
    assert_eq!(r.row(id).status, JobStatus::Running, "the policy alone never ends a job");
}

/// HTTP_403 keeps its old budget: two retries (30 s, 2 min) showing three
/// attempts, and the third failure is not retried, so the evidence and repair
/// route starts after about 2.5 minutes rather than 17.
#[test]
fn an_http_403_is_retried_twice_then_left_to_the_review_route() {
    let r = rig();
    let id = r.queue(1);
    for (n, secs) in [30, 120].into_iter().enumerate() {
        r.lease(id);
        assert!(r.decide(id, ErrorCode::Http403), "failure {} was not retried", n + 1);
        let job = r.row(id);
        assert_eq!((job.status, job.attempts, job.max_attempts), (JobStatus::Pending, n as i32 + 1, 3));
        assert_eq!(job.error_code.as_deref(), Some("HTTP_403"));
        let left = waits_secs(&job);
        assert!(left > secs - 10 && left <= secs, "failure {}: {left} s, expected {secs}", n + 1);
    }
    r.lease(id);
    assert!(!r.decide(id, ErrorCode::Http403), "a third try of a 403");
    let job = r.row(id);
    assert_eq!((job.status, job.attempts, job.max_attempts), (JobStatus::Running, 3, 3), "the policy alone never ends a job");
}

/// The fifth failure goes to review, through `run_job` and its delivery site:
/// four requeues of 30 s, 2, 5 and 10 min first, each showing 5 attempts.
#[tokio::test]
async fn the_fifth_failure_goes_to_review() {
    let r = rig();
    let id = r.queue(1);
    for (n, secs) in [30, 120, 300, 600].into_iter().enumerate() {
        r.fail_waiting(id).await;
        let job = r.row(id);
        assert_eq!(job.status, JobStatus::Pending, "failure {} went to review: {:?}", n + 1, job.error_message);
        assert_eq!((job.attempts, job.max_attempts, job.error_code.as_deref()), (n as i32 + 1, 5, Some("DELIVERY_FAILED")));
        let left = waits_secs(&job);
        assert!(left > secs - 10 && left <= secs, "failure {}: {left} s, expected {secs}", n + 1);
    }
    r.fail_waiting(id).await;
    let job = r.row(id);
    assert_eq!((job.status, job.attempts, job.error_code.as_deref()), (JobStatus::RequiresReview, 5, Some("DELIVERY_FAILED")));
    assert_eq!(job.lease_owner, None);
}

/// An approved held video whose delivery fails: the same rule at the approved
/// delivery's site (reached through `run_job`), the approval kept on every retry.
#[tokio::test]
async fn the_approved_delivery_site_applies_the_same_rule() {
    let r = rig();
    let id = r.queue(1);
    r.lease(id);
    let sha = "ab".repeat(32);
    assert!(r.repo.hold_output(id, OWNER, "temp/held/1/1_ANNA_TEST.mxf", 10, &sha, Some(9.0), chrono::Utc::now() + chrono::Duration::hours(24)).unwrap());
    assert!(finish_held(&r.repo, id, OWNER).unwrap());
    assert!(r.repo.approve_held(id, &sha, "desk").unwrap());
    for n in 1..=5 {
        let job = r.lease(id);
        assert!(r.repo.approved_held(id).unwrap().is_some(), "try {n}: the approval was lost");
        r.run_job(Job { delivery_wait: true, ..job }).await;
        let job = r.row(id);
        if n < 5 {
            assert_eq!((job.status, job.attempts, job.max_attempts), (JobStatus::Pending, n, 5), "try {n}");
        } else {
            assert_eq!((job.status, job.attempts, job.error_code.as_deref()), (JobStatus::RequiresReview, 5, Some("DELIVERY_FAILED")));
        }
    }
}

/// A job queued 121 minutes ago is not retried: its first failure goes to
/// review. MCR's «Δοκιμή ξανά» starts the two hours again, and the next
/// failure is retried. 119 minutes still retries.
#[tokio::test]
async fn an_old_job_goes_to_review_and_a_retry_gives_it_the_full_budget() {
    let r = rig();
    let old = r.queue(1);
    r.queued_ago(old, 121);
    r.lease(old);
    assert!(!r.decide(old, ErrorCode::Network), "a NETWORK failure of a 2-hour-old job was retried");
    assert!(r.repo.requeue_after(old, OWNER, chrono::Duration::zero(), "PIPELINE_FAILED", "x").unwrap());
    r.fail_waiting(old).await;
    let job = r.row(old);
    assert_eq!((job.status, job.error_code.as_deref()), (JobStatus::RequiresReview, Some("DELIVERY_FAILED")));

    assert!(r.repo.retry_job(old, None).unwrap());
    assert!(r.row(old).retry_from.is_some());
    r.lease(old);
    assert!(r.decide(old, ErrorCode::Network), "the retry did not give the job its budget back");
    let job = r.row(old);
    assert_eq!((job.status, job.attempts, job.max_attempts), (JobStatus::Pending, 1, 5));
    assert!(waits_secs(&job) <= 30);

    let young = r.queue(2);
    r.queued_ago(young, 119);
    r.fail_waiting(young).await;
    assert_eq!(r.row(young).status, JobStatus::Pending, "119 minutes is inside the limit");
}

/// LOW_DISK is an ordinary retried code (review round 1): three waits of
/// 10 minutes, each spending its attempt, `max_attempts` 4, and the fourth
/// failure is not retried. A LOW_DISK job past the age limit is not retried.
#[test]
fn a_full_disk_retries_three_times_then_stops() {
    let r = rig();
    let id = r.queue(1);
    for n in 1..=3 {
        r.lease(id);
        assert!(r.decide(id, ErrorCode::LowDisk), "failure {n} was not retried");
        let job = r.row(id);
        assert_eq!((job.status, job.attempts, job.max_attempts), (JobStatus::Pending, n, 4), "failure {n}");
        let left = waits_secs(&job);
        assert!(left > 590 && left <= 600, "{left} s");
    }
    r.lease(id);
    assert!(!r.decide(id, ErrorCode::LowDisk), "a fourth LOW_DISK try");
    assert_eq!(r.row(id).attempts, 4);

    let old = r.queue(2);
    r.queued_ago(old, 121);
    r.lease(old);
    assert!(!r.decide(old, ErrorCode::LowDisk), "a LOW_DISK job past the age limit was retried");
}

/// The run_job site itself: an emailed video whose length no disk can hold
/// fails with LOW_DISK before the encode on every run. It is retried three
/// times, `attempts` counting up, and the fourth failure sends it to review
/// as LOW_DISK: it no longer loops for ever.
#[tokio::test]
async fn low_disk_through_run_job_reaches_review_after_its_budget() {
    let (Some(ffmpeg), Some(ffprobe)) = (tool("ffmpeg"), tool("ffprobe")) else {
        eprintln!("WARNING: skipping the run_job LOW_DISK test -- ffmpeg/ffprobe not in bin/");
        return;
    };
    let r = rig_with(Some((ffmpeg.clone(), ffprobe)), false);
    // Two frames 100,000,000 s apart: the probe reads a clip of about three years.
    let source = r.root.join("attachments").join("1").join("source.mkv");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    let args: Vec<String> = [
        "-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=25:duration=0.08", "-vf",
        "setpts=N*100000000/TB", "-fps_mode", "passthrough", "-c:v", "mjpeg",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([source.to_string_lossy().into_owned()])
    .collect();
    let made = omni_core::process::run(&ffmpeg, &args, omni_core::process::RunOpts::new(Duration::from_secs(60))).await.unwrap();
    assert!(made.success, "could not build the long attachment");
    let mut new = NewJob::new("attachment://m1@example.test/1", "1_ANNA_TEST", "ANNA");
    new.status = JobStatus::ManualDownload;
    new.extraction_method = Some("attachment".into());
    let id = r.repo.enqueue(&new, omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();
    assert!(r.repo.attach_source(id, &source.to_string_lossy()).unwrap());

    for n in 1..=3 {
        let job = r.lease(id);
        r.run_job(job).await;
        let job = r.row(id);
        assert_eq!(job.status, JobStatus::Pending, "failure {n}: {:?} {:?}", job.error_code, job.error_message);
        assert_eq!(job.error_code.as_deref(), Some("LOW_DISK"), "failure {n}: {:?}", job.error_message);
        assert_eq!((job.attempts, job.max_attempts), (n, 4), "failure {n}");
        let left = waits_secs(&job);
        assert!(left > 590 && left <= 600, "{left} s");
    }
    let job = r.lease(id);
    r.run_job(job).await;
    let job = r.row(id);
    assert_eq!((job.status, job.error_code.as_deref(), job.attempts), (JobStatus::RequiresReview, Some("LOW_DISK"), 4));
    assert_eq!(job.lease_owner, None);
}

/// `run_job`'s own site cannot be driven to every code without the network,
/// so the decision is `retry_by_policy`; this keeps all three sites on it.
#[test]
fn every_failure_site_uses_the_retry_policy() {
    let src = include_str!("worker.rs");
    let call = "retry_by_policy(repo, engine, job_id, owner, code, &message)?";
    assert_eq!(src.matches(call).count(), 3, "`{call}` must be the decision at the three failure sites");
    assert_eq!(src.matches("attempts < max_attempts").count(), 0, "an old attempts rule is back");
}

/// P11.4: a platform's rate limit or bot check goes back to the queue for 2,
/// 5, 10 and 15 minutes, showing five attempts; the fifth failure is not
/// retried, and neither is a job past the age limit.
#[test]
fn a_rate_limited_failure_waits_two_minutes_and_counts_five_attempts() {
    let r = rig();
    let id = r.queue(1);
    for (n, secs) in [120, 300, 600, 900].into_iter().enumerate() {
        r.lease(id);
        assert!(r.decide(id, ErrorCode::RateLimited), "failure {} was not retried", n + 1);
        let job = r.row(id);
        assert_eq!((job.status, job.attempts, job.max_attempts), (JobStatus::Pending, n as i32 + 1, 5));
        assert_eq!(job.error_code.as_deref(), Some("RATE_LIMITED"));
        let left = waits_secs(&job);
        assert!(left > secs - 10 && left <= secs, "failure {}: {left} s, expected {secs}", n + 1);
    }
    r.lease(id);
    assert!(!r.decide(id, ErrorCode::RateLimited), "a fifth try");

    let old = r.queue(2);
    r.queued_ago(old, 121);
    r.lease(old);
    assert!(!r.decide(old, ErrorCode::RateLimited), "a RATE_LIMITED job past the age limit was retried");
}

/// P11.4 follow-up: a job that stopped retrying a passing condition gets the
/// exhausted hint in its history, as on its card, never «Γίνεται αυτόματη
/// επανάληψη»; any other code keeps its own hint.
#[test]
fn the_review_hint_of_an_exhausted_retry_is_the_exhausted_text() {
    let r = rig();
    let id = r.queue(1);
    for _ in 0..4 {
        r.lease(id);
        assert!(r.decide(id, ErrorCode::Network));
    }
    r.lease(id);
    assert!(!r.decide(id, ErrorCode::Network));
    assert_eq!(r.row(id).attempts, 5);
    let url = "https://news.example/story-1";
    let hint = review_hint(&r.repo, id, ErrorCode::Network, url);
    assert_eq!(hint, omni_broadcast::errors::exhausted_hint_el(5));
    assert!(!hint.contains("Γίνεται αυτόματη επανάληψη"), "{hint}");
    assert_eq!(review_hint(&r.repo, id, ErrorCode::RateLimited, url), omni_broadcast::errors::exhausted_hint_el(5));
    // A rate limit that outlasted its retries on a platform: the site's cookie advice follows.
    assert_eq!(
        review_hint(&r.repo, id, ErrorCode::RateLimited, "https://x.com/a/status/1"),
        format!("{} {}", omni_broadcast::errors::exhausted_hint_el(5), omni_broadcast::errors::login_hint_el("x.com", false))
    );
    // Its own advice stays: the share, a 403, a full disk, a login wall.
    for code in [ErrorCode::DeliveryFailed, ErrorCode::Http403, ErrorCode::LowDisk, ErrorCode::PrivateOrRemoved] {
        assert_eq!(review_hint(&r.repo, id, code, url), code.hint_el(), "{code}");
    }
    assert_eq!(
        review_hint(&r.repo, id, ErrorCode::LoginRequired, "https://x.com/a/status/1"),
        omni_broadcast::errors::hint_for_job(ErrorCode::LoginRequired, "https://x.com/a/status/1", &r.repo)
    );
    // An old job that ran once: the age limit stopped it.
    let old = r.queue(2);
    r.queued_ago(old, 121);
    r.lease(old);
    assert!(!r.decide(old, ErrorCode::Network));
    assert_eq!(review_hint(&r.repo, old, ErrorCode::Network, url), omni_broadcast::errors::exhausted_hint_el(1));
}

/// The three sites that send a job to review write its hint through `review_hint`.
#[test]
fn every_review_site_writes_the_review_hint() {
    let src = include_str!("worker.rs");
    assert_eq!(src.matches("&review_hint(repo, job_id, code, ").count(), 3, "the three review sites");
    assert_eq!(src.matches("None, &omni_broadcast::errors::hint_for_job(").count(), 0, "a site writes the plain hint again");
}

/// P11.15a: a locker link a person must fetch (WeTransfer, a shared folder)
/// goes to the MCR desk before anything is requested, with the vendor named;
/// a share of one file fetched directly does not.
#[tokio::test]
async fn a_locker_a_person_must_fetch_goes_to_the_desk_and_a_direct_share_does_not() {
    use omni_core::locker::{classify, Locker};
    // A broken guard here would reach the egress lookup: refused, so nothing leaves the machine (P11.15a2).
    omni_core::net::refuse_lookups_for_tests();
    let r = rig();
    for (n, url, vendor) in [(1, "https://we.tl/t-abc123", "WeTransfer"), (2, "https://www.dropbox.com/sh/folder1/AAB?dl=0", "Dropbox")] {
        let id = r.repo.enqueue(&NewJob::new(url, &format!("{n}_ANNA_TEST"), "ANNA"), omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();
        let job = r.lease(id);
        r.run_job(job).await;
        let row = r.row(id);
        assert_eq!((row.status, row.error_code.as_deref()), (JobStatus::ManualDownload, Some("MANUAL_DOWNLOAD")), "{url}");
        assert!(row.error_message.as_deref().unwrap_or("").contains(vendor), "{url}: {:?}", row.error_message);
    }
    assert!(locker_manual_message(&classify("https://www.dropbox.com/s/abc/clip.mp4")).is_none(), "a direct share is not sent to the desk");
    assert!(matches!(classify("https://www.dropbox.com/s/abc/clip.mp4"), Locker::Direct { .. }));
    assert!(locker_manual_message(&classify("https://news.example/story")).is_none());
}

/// P11.15a: run_job fetches a direct share from its direct address, judged by
/// the egress rule, before yt-dlp or the browser see the link.
#[test]
fn run_job_turns_a_direct_share_into_its_address_after_the_egress_check() {
    let src = include_str!("worker.rs");
    let run = &src[src.find("pub(crate) async fn run_job(").unwrap()..];
    let at = |s: &str| run.find(s).unwrap_or_else(|| panic!("{s} missing"));
    let vet = at("\n        match egress::vet_url(&orig_url, egress_allow).await {");
    let direct = at("\n    let orig_url = match omni_core::locker::direct_download(&orig_url) {");
    let direct_vet = direct + run[direct..].find("egress::vet_url(&direct, egress_allow)").expect("the direct address is vetted");
    let page_scan = at("\n    let is_web = (orig_url.starts_with(\"http://\") || orig_url.starts_with(\"https://\")) && !locker_direct;");
    assert!(vet < direct && direct < direct_vet && direct_vet < page_scan);
    // P11.15a2: a direct share is a download only. Its address is never opened as a
    // page, no sniffer, no repair ladder, no site health, and its error is scrubbed.
    for gate in [
        "\n    let route = if locker_direct {\n        omni_broadcast::downloader::FirstAttempt::YtDlp",
        "\n            Err(e) => !locker_direct && classify_pipeline_error(e).should_sniff(&orig_url),",
        "\n                let ladder = if siblings_hold_the_rest || locker_direct {",
        "\n        process_result = process_result.map_err(|e| scrub_locker_error(e, &locker_label));",
        "\n                let done_for_site = done && !locker_direct;",
        "\n            _size_guard = Some(engine.set_max_filesize(job_id, engine.locker_max_file_bytes()));",
        "\n            _redaction = repo.redact_in_logs(omni_core::locker::secret_parts(&direct));",
        "\n            _link_guard = Some(engine.set_locker_link(job_id, &locker_label));",
        "\n    let locker_label = omni_core::urlnorm::site_for_log(&sent_link);",
    ] {
        at(gate);
    }
}

/// P11.15a2: a share of one file is a download only. After a run whose download
/// fails, the row keeps the link that was sent, and neither its error nor any
/// line of its history holds the direct address or the share's token. Nothing
/// leaves the machine: lookups are refused and the rig has no yt-dlp.
#[tokio::test]
async fn a_direct_share_run_keeps_the_sent_link_and_never_shows_the_direct_address() {
    omni_core::net::refuse_lookups_for_tests();
    let r = rig();
    let sent = "https://1drv.ms/v/s!AkExampleShare123";
    let direct = omni_core::locker::direct_download(sent).unwrap();
    let token = direct.split("shares/").nth(1).unwrap().split('/').next().unwrap().to_string();
    let id = r.repo.enqueue(&NewJob::new(sent, "1_ANNA_TEST", "ANNA"), omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();
    let job = r.lease(id);
    r.run_job(job).await;
    let row = r.row(id);
    assert_eq!(row.url, sent, "the row keeps the sent link");
    assert_ne!(row.status, JobStatus::ManualDownload, "a direct share is downloaded, not sent to the desk");
    let mut texts: Vec<String> = r.repo.get_job_events(id, 500).unwrap().into_iter().map(|e| e.message).collect();
    texts.extend(row.error_message.clone());
    assert!(texts.iter().any(|t| t.contains("OneDrive share: the file is downloaded directly")), "{texts:?}");
    for t in &texts {
        assert!(!t.contains(&direct) && !t.contains(&token) && !t.contains("api.onedrive.com"), "the direct address leaked: {t}");
    }
    // No page scan, no browser: nothing of the web path ran.
    assert!(!texts.iter().any(|t| t.contains("Sniffing") || t.contains("sniff")), "{texts:?}");
}

/// P11.15a2: a tool's error about a share's direct address names the sent link,
/// keeps its code, and a lost lease is passed on untouched.
#[test]
fn a_direct_shares_error_is_scrubbed_and_keeps_its_code() {
    let sent = "https://www.dropbox.com/scl/fi/x1y2/clip.mov?rlkey=k9k9&dl=0";
    let direct = omni_core::locker::direct_download(sent).unwrap();
    let raw = anyhow::anyhow!("ERROR: [generic] Unable to download {direct}: HTTP Error 403: Forbidden")
        .context(format!("yt-dlp failed for {direct}"))
        .context(ErrorCode::Http403.as_str());
    let code = classify_pipeline_error(&raw);
    let clean = scrub_locker_error(raw, sent);
    assert_eq!(classify_pipeline_error(&clean), code);
    let all = format!("{clean:#}");
    assert!(!all.contains("dl=1") && all.contains(sent), "{all}");
    assert!(!readable_cause(&clean).contains("dl=1"), "{}", readable_cause(&clean));
    let lost = scrub_locker_error(anyhow::Error::new(omni_broadcast::pipeline::LeaseLost { job_id: 1, stage: JobStage::Download }), sent);
    assert!(lost.downcast_ref::<omni_broadcast::pipeline::LeaseLost>().is_some());
}

/// P11.15a3: a fake yt-dlp whose error names the share's direct address and the
/// signed address it was sent on to. After the run, no line of the job's history,
/// no audit row and not its `error_message` holds either: the engine scrubs its
/// own lines, run_job the rest. Lookups are refused; nothing leaves the machine.
#[tokio::test]
async fn a_tools_error_about_a_direct_share_never_shows_its_addresses() {
    omni_core::net::refuse_lookups_for_tests();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let repo = Repository::new(root.join("omni.db")).unwrap();
    let sent = "https://www.dropbox.com/scl/fi/x1y2z3w4v5/clip.mov?rlkey=abcdef123456&dl=0";
    let direct = omni_core::locker::direct_download(sent).unwrap();
    let signed = "https://uc12.dl.dropboxusercontent.com/cd/0/get/SIGNEDTOKEN123/file?dl=1";
    let err = root.join("stderr.txt");
    std::fs::write(&err, format!("ERROR: [generic] Unsupported URL: {direct}\r\nERROR: unable to download video data: HTTP Error 403: Forbidden (redirected to {signed})\r\n")).unwrap();
    let shim = root.join("fake-yt-dlp.cmd");
    std::fs::write(&shim, format!("@echo off\r\ntype \"{}\" 1>&2\r\nexit /b 1\r\n", err.display())).unwrap();
    let engine = BroadcastEngine::new(
        repo.clone(),
        shim,
        root.join("no-ffmpeg.exe"),
        root.join("no-ffprobe.exe"),
        root.join("no-bmx.exe"),
        root.join("temp"),
        root.join("watch"),
    );
    let r = Rig { _dir: dir, root, repo, engine: Arc::new(engine) };
    let id = r.repo.enqueue(&NewJob::new(sent, "1_ANNA_TEST", "ANNA"), omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();
    let job = r.lease(id);
    r.run_job(job).await;

    // The engine logs exactly the text it writes as the job's ERROR event and its
    // audit row (P11.15a4), so these cover its log lines too.
    let row = r.row(id);
    let mut texts: Vec<String> = r.repo.get_job_events(id, 500).unwrap().into_iter().map(|e| e.message).collect();
    assert!(texts.iter().any(|t| t.contains("Unsupported URL: dropbox.com")), "the fake's error, its address named by the site: {texts:?}");
    texts.extend(r.repo.get_recent_audit_logs(200).unwrap().into_iter().map(|a| a.message));
    texts.extend(row.error_message.clone());
    for t in &texts {
        assert!(!t.contains(direct.as_str()) && !t.contains("SIGNEDTOKEN123") && !t.contains("dropboxusercontent"), "an address leaked: {t}");
        // P11.15a4: not the sent link's key either: a share is named by its site.
        assert!(!t.contains("rlkey") && !t.contains("abcdef123456") && !t.contains("x1y2z3w4v5"), "the share's key leaked: {t}");
    }
    assert_eq!(row.url, sent);
}

/// P11.15a4: two runs of one job overlap (a reaped lease and its new run): the
/// first run's guards ending do not take the second run's values away.
#[test]
fn an_earlier_runs_guard_does_not_remove_a_later_runs_values() {
    let r = rig();
    let first_size = r.engine.set_max_filesize(7, 1 << 30);
    let first_link = r.engine.set_locker_link(7, "first.example");
    let second_size = r.engine.set_max_filesize(7, 2 << 30);
    let second_link = r.engine.set_locker_link(7, "second.example");
    drop((first_size, first_link));
    assert_eq!(r.engine.job_locker_state(7), (Some(2 << 30), Some("second.example".to_string())), "the later run's values stay");
    drop((second_size, second_link));
    assert_eq!(r.engine.job_locker_state(7), (None, None));
}
