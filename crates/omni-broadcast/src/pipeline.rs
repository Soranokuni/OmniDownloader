use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use omni_core::models::{Job, JobStage};
use omni_core::repository::{HeldOutput, Repository};

use crate::delivery::{DeliveryError, LeaseLostBeforeRename, WatchfolderDelivery};
use omni_core::repository::MarkerCheck;
use omni_core::selftest::WatchfolderState;
use crate::downloader::{DownloadError, DownloadOpts, Downloader, FormatRoute};
use crate::errors::ErrorCode;
use crate::preview::{self, PreviewEncoder};
use crate::rewrapper::Rewrapper;
use crate::transcoder::Transcoder;

/// `Err(why)` when the temp or watchfolder volume lacks room for a clip of
/// `duration_secs` (see [`omni_core::selftest::space_needed`]). A volume
/// whose free space cannot be read is not held against the job.
pub fn room_for(duration_secs: f64, temp: &std::path::Path, watchfolder: &std::path::Path) -> std::result::Result<(), String> {
    let (need_temp, need_watch) = omni_core::selftest::space_needed(duration_secs);
    let gb = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
    for (label, path, need) in [("temp", temp, need_temp), ("watchfolder", watchfolder, need_watch)] {
        if let Some((free, _)) = omni_core::selftest::disk_space(path) {
            if free < need {
                return Err(format!(
                    "Not enough disk space for a {:.0} min clip: {label} has {:.1} GB free, needs {:.1} GB",
                    duration_secs / 60.0,
                    gb(free),
                    gb(need)
                ));
            }
        }
    }
    Ok(())
}

/// Encoders running at once unless the daemon says otherwise. One MPEG-2
/// 50 Mbps 1080i encode keeps about six cores busy; two fill the reference
/// MCR machine (12 threads).
pub const DEFAULT_ENCODER_SLOTS: usize = 2;

/// The worker no longer holds the job's lease (discarded, requeued, or taken
/// by another worker). Carried inside the pipeline's error so the worker
/// can recognise it by type, not by text.
#[derive(Debug, thiserror::Error)]
#[error("job #{job_id} is no longer ours at {}", .stage.as_str())]
pub struct LeaseLost {
    pub job_id: i64,
    pub stage: JobStage,
}

/// What a finished, verified video does next (plan P10.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMode {
    /// Into the watchfolder, as always.
    Deliver,
    /// Keep the file under `temp/held/{id}` for MCR to approve; the
    /// watchfolder is never touched. `held_max_bytes` caps everything held
    /// at once.
    Hold { hold_hours: i64, held_max_bytes: u64 },
}

/// How a run that returned `Ok` ended (P10.5). The worker parks or
/// completes the job from this, not from what the database says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// The file is in the watchfolder.
    Delivered,
    /// The file is held for MCR; nothing was delivered.
    Held,
}

/// How [`BroadcastEngine::deliver_ready`] ended (P11.2a) when it returned `Ok`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyOutcome {
    /// The video that waited is in the watchfolder (copied now, or found
    /// there from the interrupted delivery); the worker completes the job.
    Delivered,
    /// The video that waited is gone or changed and was not delivered; its
    /// workspace is removed. The worker queues the job to run from its link.
    Rerun,
}

/// Free space a waiting video needs on the watchfolder volume beyond its own size.
const WATCHFOLDER_MARGIN: u64 = 64 * 1024 * 1024;

/// `Err(cause)` when the watchfolder volume's free space is known (`free`)
/// and smaller than `bytes` plus 64 MiB (P11.2a): a waiting video is not
/// copied into a share that is full, it waits again. Unknown space is not
/// held against it.
pub fn watchfolder_room(free: Option<u64>, bytes: u64) -> std::result::Result<(), String> {
    let need = bytes.saturating_add(WATCHFOLDER_MARGIN);
    match free {
        Some(free) if free < need => {
            let gb = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
            Err(format!("the watchfolder has {:.1} GB free, the video needs {:.1} GB", gb(free), gb(need)))
        }
        _ => Ok(()),
    }
}

/// The requeue delay after a job's `n`th wait in a row (P11.2a): 30 s, 60 s,
/// 2 min, then 5 min for every later one.
pub fn wait_backoff_for(n: u32) -> chrono::Duration {
    chrono::Duration::seconds(match n {
        0 | 1 => 30,
        2 => 60,
        3 => 120,
        _ => 300,
    })
}

/// A video that waited for the watchfolder and that no run may deliver on its
/// own (P11.2a): its earlier rename may have landed and been taken into Dalet
/// by hand, or the watchfolder changed while it waited (`code`
/// DELIVERY_UNCONFIRMED, P11.2b); or its file in the workspace could not be
/// read for [`UNREADABLE_WAIT_LIMIT`] (PIPELINE_FAILED). The worker holding the
/// lease sends the job to review with `code` and `message`; nothing is copied,
/// and the workspace is kept.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct DeliveryNeedsCheck {
    pub code: ErrorCode,
    pub message: String,
}

/// How long a waiting video whose file in the workspace cannot be read (held
/// open, no permission, a disk error) waits, counted from its first park,
/// before a person is asked (P11.2b). A watchfolder that is down is waited for
/// whatever the age; only this local read failure is capped.
pub const UNREADABLE_WAIT_LIMIT: chrono::Duration = chrono::Duration::hours(2);

/// Whether a waiting video parked at `ready_at` and unreadable now has waited
/// past [`UNREADABLE_WAIT_LIMIT`] at `now`. An unknown park time (a row parked
/// before the time was kept) is never past it.
pub fn unreadable_too_long(ready_at: Option<chrono::DateTime<chrono::Utc>>, now: chrono::DateTime<chrono::Utc>) -> bool {
    ready_at.is_some_and(|at| now - at > UNREADABLE_WAIT_LIMIT)
}

/// Whether `a` and `b` name the same folder, as written: separators and a
/// trailing one ignored, and on Windows case too. Used to tell a delivery
/// marker of the current watchfolder from one of an older setting (P11.2a).
pub fn same_folder(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let s = p.to_string_lossy().replace('/', "\\");
        let s = s.trim_end_matches('\\').to_string();
        if cfg!(windows) {
            s.to_lowercase()
        } else {
            s
        }
    };
    norm(a) == norm(b)
}

/// The cause when `e` is a watchfolder that could not be written (P11.2a).
fn unreachable_cause(e: &anyhow::Error) -> Option<String> {
    match e.downcast_ref::<DeliveryError>() {
        Some(DeliveryError::Unreachable(cause)) => Some(cause.clone()),
        _ => None,
    }
}

/// What a job's download is sent with besides its link: the sniffed page's
/// referer, user agent and cookie header, when there are any.
#[derive(Clone, Copy, Default)]
struct SourceContext<'a> {
    referer: Option<&'a str>,
    user_agent: Option<&'a str>,
    cookies: Option<&'a str>,
}

fn is_cancelled(cancel: &Option<CancellationToken>) -> bool {
    cancel.as_ref().is_some_and(|t| t.is_cancelled())
}

/// The code and the timeline line for a failed download. A [`DownloadError`]
/// already shows its code, so its line is "CODE: message", never
/// "CODE: CODE: message" (seen on the desk 2026-10-19).
fn download_failure_text(e: &anyhow::Error) -> (ErrorCode, String) {
    match e.downcast_ref::<DownloadError>() {
        Some(d) => (d.code, format!("{}: {}", d.code, d.message)),
        None => (ErrorCode::PipelineFailed, format!("{}: {e}", ErrorCode::PipelineFailed)),
    }
}

/// Lets a stage's progress reach the queue row at most once a second (plan
/// P1.20). Each write is a SQLite commit, an fsync under synchronous=FULL, and
/// yt-dlp and ffmpeg report many times a second. The first report passes, and
/// the first 100 % passes at once (only once: yt-dlp's estimate for HLS/DASH
/// can undershoot, so every later line may read 100); `last` moves only when
/// a report is let through.
struct ProgressGate {
    last: Option<Instant>,
    sent_100: bool,
}

impl ProgressGate {
    const MIN_GAP: Duration = Duration::from_secs(1);

    fn new() -> Self {
        Self { last: None, sent_100: false }
    }

    fn allow(&mut self, now: Instant, percent: f64) -> bool {
        let first_100 = percent >= 100.0 && !self.sent_100;
        let due = match self.last {
            None => true,
            Some(last) => first_100 || now.saturating_duration_since(last) >= Self::MIN_GAP,
        };
        if due {
            self.last = Some(now);
            self.sent_100 |= percent >= 100.0;
        }
        due
    }
}

/// A progress callback for one stage of one job: owns its own gate, so the
/// first line of the next stage is written at once. Errors are ignored, as
/// progress is display only.
fn progress_writer(repo: Repository, job_id: i64) -> impl FnMut(f64, &str, &str) + Send + 'static {
    let mut gate = ProgressGate::new();
    move |percent, speed, eta| {
        if gate.allow(Instant::now(), percent) {
            let _ = repo.update_job_progress(job_id, percent, speed, eta);
        }
    }
}

#[cfg(test)]
mod progress_gate_tests {
    use super::*;
    use omni_core::models::NewJob;
    use omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS;

    #[test]
    fn a_burst_in_one_instant_lets_one_write_through() {
        let t0 = Instant::now();
        let mut gate = ProgressGate::new();
        let allowed = (0..1000).filter(|i| gate.allow(t0, (*i as f64) / 20.0)).count();
        assert_eq!(allowed, 1);
    }

    #[test]
    fn the_gap_is_one_second_from_the_last_allowed_write_and_100_always_passes() {
        let t0 = Instant::now();
        let mut gate = ProgressGate::new();
        assert!(gate.allow(t0, 1.0));
        assert!(!gate.allow(t0 + Duration::from_millis(999), 2.0));
        // A refused call does not move the window.
        assert!(gate.allow(t0 + Duration::from_millis(1000), 3.0));
        assert!(!gate.allow(t0 + Duration::from_millis(1500), 50.0));
        assert!(gate.allow(t0 + Duration::from_millis(1001), 100.0));
        // Only the first 100 skips the wait: an estimate stuck at 100 is
        // throttled like any other line.
        assert!(!gate.allow(t0 + Duration::from_millis(1002), 100.0));
        assert!(gate.allow(t0 + Duration::from_millis(2001), 100.0));
    }

    fn row_progress(repo: &Repository, id: i64) -> f64 {
        repo.get_job(id).unwrap().unwrap().progress
    }

    #[test]
    fn the_writer_commits_the_first_and_the_100_and_skips_the_burst_between() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let id = repo
            .enqueue(&NewJob::new("https://news.example/story-1", "1_ANNA_TEST", "ANNA"), DEFAULT_DEDUP_WINDOW_HOURS)
            .unwrap()
            .job_id();

        let mut write = progress_writer(repo.clone(), id);
        let started = Instant::now();
        write(10.0, "1MiB/s", "00:10");
        assert_eq!(row_progress(&repo, id), 10.0);
        for p in 20..=90 {
            write(p as f64, "1MiB/s", "00:05");
        }
        // The real clock runs here: a machine stalled past the gap (parallel
        // gate) legitimately lets one line through, so only judge a fast run.
        if started.elapsed() < Duration::from_millis(900) {
            assert_eq!(row_progress(&repo, id), 10.0, "the burst inside one second must not reach the row");
        } else {
            eprintln!("warning: burst took over 900 ms; throttle assertion skipped");
        }
        write(100.0, "1MiB/s", "00:00");
        assert_eq!(row_progress(&repo, id), 100.0);

        // The next stage has its own gate: its first value is written at once.
        let mut next = progress_writer(repo.clone(), id);
        next(5.0, "2.0x", "01:00");
        assert_eq!(row_progress(&repo, id), 5.0);
    }
}

#[cfg(test)]
mod waiting_rule_tests {
    use super::*;

    /// B1 of P11.2a: a waiting video is not copied into a share without room
    /// for it and 64 MiB; unknown space is not held against it.
    #[test]
    fn a_waiting_video_needs_its_size_and_64_mib_free_in_the_watchfolder() {
        const MIB: u64 = 1024 * 1024;
        let video = 500 * MIB;
        assert!(watchfolder_room(None, video).is_ok(), "space that cannot be read is not held against it");
        assert!(watchfolder_room(Some(video + 64 * MIB), video).is_ok());
        let why = watchfolder_room(Some(video + 64 * MIB - 1), video).unwrap_err();
        assert_eq!(why, "the watchfolder has 0.6 GB free, the video needs 0.6 GB");
        assert_eq!(watchfolder_room(Some(0), 3 * 1024 * MIB).unwrap_err(), "the watchfolder has 0.0 GB free, the video needs 3.1 GB");
    }

    /// Finding 7: a marker's folder is compared with the watchfolder as written,
    /// separators, a trailing one and (Windows) case aside.
    #[test]
    fn the_same_folder_is_recognised_however_it_is_written() {
        assert!(same_folder(Path::new(r"\\fileserver\Dalet\Watch"), Path::new(r"\\fileserver\Dalet\Watch\")));
        assert!(same_folder(Path::new("D:/omni/watch"), Path::new(r"D:\omni\watch")));
        #[cfg(windows)]
        assert!(same_folder(Path::new(r"\\FILESERVER\dalet\watch"), Path::new(r"\\fileserver\Dalet\Watch")));
        assert!(!same_folder(Path::new(r"\\fileserver\dalet\watch-old"), Path::new(r"\\fileserver\dalet\watch")));
        assert!(!same_folder(Path::new(r"\\fileserver\dalet\watch\sub"), Path::new(r"\\fileserver\dalet\watch")));
    }

    /// B2 of P11.2a: 30 s, 60 s, 2 min, then 5 min for every later wait.
    #[test]
    fn repeated_waits_back_off_to_five_minutes() {
        let secs: Vec<i64> = (1..=7).map(|n| wait_backoff_for(n).num_seconds()).collect();
        assert_eq!(secs, vec![30, 60, 120, 300, 300, 300, 300]);
        assert_eq!(wait_backoff_for(0).num_seconds(), 30);
    }

    #[test]
    fn the_engine_counts_waits_per_job_and_forgets_them() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let p = |n: &str| dir.path().join(n);
        let engine = BroadcastEngine::new(repo, p("y"), p("f"), p("p"), p("b"), p("temp"), p("watch"));
        let shared = engine.clone();
        assert_eq!(engine.wait_backoff(7).num_seconds(), 30);
        engine.note_wait(7, true, "first");
        engine.note_wait(7, false, "again");
        engine.note_wait(8, true, "other job");
        assert_eq!(shared.wait_backoff(7).num_seconds(), 60, "clones share the count (the ladder's engine is a clone)");
        assert_eq!(shared.wait_line(7).as_deref(), Some("again"));
        engine.note_wait(7, true, "a new series");
        assert_eq!(engine.wait_backoff(7).num_seconds(), 30, "a first park starts again");
        engine.forget_waits(7);
        assert_eq!((engine.wait_backoff(7).num_seconds(), engine.wait_line(7)), (30, None));
        assert_eq!(engine.wait_line(8).as_deref(), Some("other job"));
    }

    /// P11.3a: the age limit on automatic retries is two hours unless set,
    /// and a set value is read as 30..=360 minutes.
    #[test]
    fn the_retry_age_limit_is_two_hours_and_clamped() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let p = |n: &str| dir.path().join(n);
        let engine = BroadcastEngine::new(repo, p("y"), p("f"), p("p"), p("b"), p("temp"), p("watch"));
        assert_eq!(engine.retry_max_age().num_minutes(), 120);
        let mins = |m: i64| engine.clone().with_retry_max_age(m).retry_max_age().num_minutes();
        assert_eq!((mins(90), mins(5), mins(-1), mins(10_000)), (90, 30, 30, 360));
    }

    /// P11.4: one run per platform at a time and an 8 s gap unless set; set
    /// values are read as 1..=4 and 0..=120 s; clones share one pacer, so a
    /// turn held through one is seen through the other.
    #[tokio::test]
    async fn the_engine_paces_with_one_shared_pacer_and_clamps_its_settings() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let p = |n: &str| dir.path().join(n);
        let engine = BroadcastEngine::new(repo, p("y"), p("f"), p("p"), p("b"), p("temp"), p("watch"));
        assert_eq!((engine.pacer().per_key(), engine.pacer().min_gap()), (1, Duration::from_secs(8)));
        let set = |c: i64, g: i64| {
            let e = engine.clone().with_pacing(c, g);
            (e.pacer().per_key(), e.pacer().min_gap().as_secs())
        };
        assert_eq!((set(2, 15), set(0, -5), set(9, 1_000)), ((2, 15), (1, 0), (4, 120)));
        let engine = engine.with_pacing(1, 8);
        let clone = engine.clone();
        let _turn = engine.pacer().acquire("youtube", &CancellationToken::new()).await.unwrap();
        assert_eq!(clone.pacer().running("youtube"), 1);
    }

    /// A wait over two seconds is one INFO line in the job's history, naming
    /// only the platform.
    #[test]
    fn a_wait_for_a_turn_is_written_to_the_jobs_history() {
        use omni_core::models::NewJob;
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let id = repo
            .enqueue(&NewJob::new("https://youtu.be/abcdefghijk", "1_ANNA_TEST", "ANNA"), omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS)
            .unwrap()
            .job_id();
        let p = |n: &str| dir.path().join(n);
        let engine = BroadcastEngine::new(repo.clone(), p("y"), p("f"), p("p"), p("b"), p("temp"), p("watch"));
        (engine.pace_note(id, JobStage::Download))("youtube", Duration::from_millis(12_300));
        let lines: Vec<_> = repo.get_job_events(id, 100).unwrap().into_iter().filter(|e| e.message.starts_with("Σε σειρά")).collect();
        assert_eq!(lines.len(), 1);
        assert_eq!((lines[0].level.as_str(), lines[0].message.as_str()), ("INFO", "Σε σειρά για youtube: 12″ αναμονή"));
    }
}

#[cfg(test)]
mod failure_text_tests {
    use super::*;

    #[test]
    fn a_classified_download_failure_names_its_code_once() {
        let e = anyhow::Error::new(DownloadError {
            code: ErrorCode::LiveStream,
            message: "the video is live or not yet started; yt-dlp skipped it".into(),
        });
        let (code, text) = download_failure_text(&e);
        assert_eq!(code, ErrorCode::LiveStream);
        assert_eq!(text, "LIVE_STREAM: the video is live or not yet started; yt-dlp skipped it");

        let plain = anyhow::anyhow!("yt-dlp could not start");
        let (code, text) = download_failure_text(&plain);
        assert_eq!(code, ErrorCode::PipelineFailed);
        assert_eq!(text, format!("{}: yt-dlp could not start", ErrorCode::PipelineFailed));
    }
}

/// Forgets a job's download size limit when dropped (see [`BroadcastEngine::set_max_filesize`]).
pub struct MaxFilesizeGuard {
    sizes: Arc<Mutex<HashMap<i64, u64>>>,
    job_id: i64,
}

impl Drop for MaxFilesizeGuard {
    fn drop(&mut self) {
        self.sizes.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.job_id);
    }
}

/// Forgets a job's extra login site when dropped (see [`BroadcastEngine::set_login_site`]).
pub struct LoginSiteGuard {
    sites: Arc<Mutex<HashMap<i64, String>>>,
    job_id: i64,
}

impl Drop for LoginSiteGuard {
    fn drop(&mut self) {
        self.sites.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.job_id);
    }
}

/// Hands back a job's download slot when dropped, panic unwinding included
/// (see [`BroadcastEngine::download_slot_guard`]).
pub struct DownloadSlotGuard {
    slots: Arc<Mutex<HashMap<(i64, String), OwnedSemaphorePermit>>>,
    job_id: i64,
    owner: String,
}

impl Drop for DownloadSlotGuard {
    fn drop(&mut self) {
        self.slots.lock().unwrap_or_else(|p| p.into_inner()).remove(&(self.job_id, self.owner.clone()));
    }
}

#[derive(Clone)]
pub struct BroadcastEngine {
    repo: Repository,
    ytdl_path: PathBuf,
    ffmpeg_path: PathBuf,
    ffprobe_path: PathBuf,
    bmxtranswrap_path: PathBuf,
    temp_dir: PathBuf,
    watchfolder_dir: PathBuf,
    /// Encodes in progress (plan P1.13). Downloading is network-bound and
    /// encoding CPU-bound, so they are counted apart: a job finished
    /// downloading hands back its download slot and waits here, and the
    /// next download starts instead of sitting behind someone's encode.
    encoder_slots: Arc<Semaphore>,
    /// The download slot each job holds until it reaches the encoder, keyed by
    /// job and lease owner: a worker whose lease was reaped must not hand
    /// back the slot of the worker that leased the job again.
    download_slots: Arc<Mutex<HashMap<(i64, String), OwnedSemaphorePermit>>>,
    /// The address of a video the page of job N embeds on another site, so the
    /// job's yt-dlp runs also carry that site's cookie jar (P3.5). The job's own
    /// address, and every other argument of its download, are unchanged.
    login_sites: Arc<Mutex<HashMap<i64, String>>>,
    /// The largest file a job's download may bring, for the jobs that have one (P11.15a2).
    max_filesizes: Arc<Mutex<HashMap<i64, u64>>>,
    /// `locker.max_file_gb` in bytes: the limit of a file-locker share's download (P11.15a2).
    locker_max_file_bytes: u64,
    /// The H.264 encoder previews use, found on first use (P10.6).
    preview_encoder: Arc<tokio::sync::OnceCell<PreviewEncoder>>,
    /// Where a delivered job's source goes instead of the bin (P1.7); `None`
    /// when the archive is switched off.
    archive: Option<omni_core::archive::ArchiveSettings>,
    /// Set once the daemon is stopping (P2.15): no job enters DELIVER after
    /// it, so none starts a watchfolder copy the stop would cut.
    stopping: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the watchfolder answers (P11.2a): a delivery that finds it
    /// unreachable clears it at once, a delivery that lands sets it.
    watchfolder_state: WatchfolderState,
    /// Each waiting job's consecutive waits and the line of the last one
    /// (P11.2a): the requeue backs off, and a repeated cause is not written
    /// again. Lost on restart, which only restarts the back-off.
    waits: Arc<Mutex<HashMap<i64, (u32, String)>>>,
    /// No automatic retry for a job older than this (P11.3a, `retry.max_age_mins`).
    retry_max_age: chrono::Duration,
    /// Every yt-dlp run on a video platform waits here for its turn (P11.4);
    /// shared by clones, the repair ladder, the canaries and the self-check.
    pacer: crate::pacing::HostPacer,
    /// Whether a source that failed probe, encode, wrap or the gate takes the
    /// second route before review (P11.6, [`crate::second_route`]). Off unless
    /// the daemon turns it on for its workers.
    second_route: bool,
}

/// `retry.max_age_mins` when the daemon sets none: the owner's two hours.
pub const DEFAULT_RETRY_MAX_AGE_MINS: i64 = 120;

impl BroadcastEngine {
    /// No automatic retry for a job older than `minutes` (P11.3a), read as 30..=360.
    pub fn with_retry_max_age(mut self, minutes: i64) -> Self {
        self.retry_max_age = chrono::Duration::minutes(minutes.clamp(30, 360));
        self
    }

    /// How old a job may be and still be retried automatically; the worker's
    /// retry decision reads it.
    pub fn retry_max_age(&self) -> chrono::Duration {
        self.retry_max_age
    }

    /// Pace yt-dlp per platform (P11.4): at most `per_host_concurrency` runs at
    /// a time (read as 1..=4), starts `min_gap_secs` plus jitter apart (read as
    /// 0..=120; 0 switches the gap off). A new pacer: call it before the engine
    /// is cloned or its pacer handed out.
    pub fn with_pacing(mut self, per_host_concurrency: i64, min_gap_secs: i64) -> Self {
        self.pacer = crate::pacing::HostPacer::new(
            per_host_concurrency.clamp(1, 4) as usize,
            std::time::Duration::from_secs(min_gap_secs.clamp(0, 120) as u64),
        );
        self
    }

    /// The pacer every yt-dlp run of this daemon shares.
    pub fn pacer(&self) -> crate::pacing::HostPacer {
        self.pacer.clone()
    }

    /// Writes a wait for a turn into job `job_id`'s history (P11.4).
    fn pace_note(&self, job_id: i64, stage: JobStage) -> crate::downloader::WaitNote {
        let repo = self.repo.clone();
        Arc::new(move |key, waited| {
            let _ = repo.record_event(job_id, "INFO", Some(stage), &crate::pacing::wait_line(key, waited));
        })
    }

    /// A downloader on this engine's pacer, its waits written to job `job_id`'s history.
    fn downloader_for(&self, job_id: i64, stage: JobStage) -> Downloader {
        Downloader::new(&self.ytdl_path, Some(&self.ffmpeg_path), &self.pacer).for_a_job().on_pace_wait(self.pace_note(job_id, stage))
    }

    /// Count one more wait of `job_id` (the first of a series when `first`).
    fn note_wait(&self, job_id: i64, first: bool, line: &str) {
        let mut waits = self.waits.lock().unwrap_or_else(|p| p.into_inner());
        let n = if first { 1 } else { waits.get(&job_id).map_or(1, |(n, _)| n.saturating_add(1)) };
        waits.insert(job_id, (n, line.to_string()));
    }

    /// How long job `job_id` waits before its next try ([`wait_backoff_for`]).
    pub fn wait_backoff(&self, job_id: i64) -> chrono::Duration {
        let n = self.waits.lock().unwrap_or_else(|p| p.into_inner()).get(&job_id).map_or(1, |(n, _)| *n);
        wait_backoff_for(n)
    }

    /// The line of job `job_id`'s last wait: what its `error_message` says.
    pub fn wait_line(&self, job_id: i64) -> Option<String> {
        self.waits.lock().unwrap_or_else(|p| p.into_inner()).get(&job_id).map(|(_, l)| l.clone())
    }

    /// Job `job_id` no longer waits: delivered, run again, or failed otherwise.
    pub fn forget_waits(&self, job_id: i64) {
        self.waits.lock().unwrap_or_else(|p| p.into_inner()).remove(&job_id);
    }

    /// Share the daemon's watchfolder flag (P11.2a), which the worker loop
    /// reads before it leases a job whose video waits for the watchfolder.
    pub fn with_watchfolder_state(mut self, state: WatchfolderState) -> Self {
        self.watchfolder_state = state;
        self
    }

    /// The watchfolder flag this engine updates.
    pub fn watchfolder_state(&self) -> WatchfolderState {
        self.watchfolder_state.clone()
    }

    /// Keep delivered sources as `settings` says (plan P1.7).
    pub fn with_archive(mut self, settings: Option<omni_core::archive::ArchiveSettings>) -> Self {
        // Only a usable archive is kept: off or overlapping the watchfolder means none.
        self.archive = settings.filter(|s| s.writes());
        self
    }

    /// At most `n` encodes at once (at least one).
    pub fn with_encoder_slots(mut self, n: usize) -> Self {
        self.encoder_slots = Arc::new(Semaphore::new(n.max(1)));
        self
    }

    /// Give the engine the download slot job `job_id` was leased under; it
    /// is released when the job starts waiting for an encoder, or by
    /// [`Self::release_download_slot`] when the job ends before that.
    pub fn hold_download_slot(&self, job_id: i64, owner: &str, permit: OwnedSemaphorePermit) {
        self.download_slots.lock().unwrap_or_else(|p| p.into_inner()).insert((job_id, owner.to_string()), permit);
    }

    /// Job `job_id` also needs the cookie jar of `video_url`'s site; forgotten when the guard drops.
    /// Limit job `job_id`'s downloads to `bytes` while the guard lives (P11.15a2):
    /// a file-locker share's direct download.
    pub fn set_max_filesize(&self, job_id: i64, bytes: u64) -> MaxFilesizeGuard {
        self.max_filesizes.lock().unwrap_or_else(|p| p.into_inner()).insert(job_id, bytes);
        MaxFilesizeGuard { sizes: self.max_filesizes.clone(), job_id }
    }

    /// `locker.max_file_gb` in bytes (P11.15a2).
    pub fn with_locker_max_file_bytes(mut self, bytes: u64) -> Self {
        self.locker_max_file_bytes = bytes.max(1);
        self
    }

    /// The limit of a file-locker share's download, in bytes (P11.15a2).
    pub fn locker_max_file_bytes(&self) -> u64 {
        self.locker_max_file_bytes
    }

    pub fn set_login_site(&self, job_id: i64, video_url: &str) -> LoginSiteGuard {
        self.login_sites.lock().unwrap_or_else(|p| p.into_inner()).insert(job_id, video_url.to_string());
        LoginSiteGuard { sites: self.login_sites.clone(), job_id }
    }

    /// Hands back the slot of (`job_id`, `owner`) when dropped, even if the worker panics.
    pub fn download_slot_guard(&self, job_id: i64, owner: &str) -> DownloadSlotGuard {
        DownloadSlotGuard { slots: self.download_slots.clone(), job_id, owner: owner.to_string() }
    }

    /// Hand back job `job_id`'s download slot, if it still holds one.
    pub fn release_download_slot(&self, job_id: i64, owner: &str) {
        self.download_slots.lock().unwrap_or_else(|p| p.into_inner()).remove(&(job_id, owner.to_string()));
    }

    /// Whether job `job_id` still holds its download slot.
    pub fn holds_download_slot(&self, job_id: i64, owner: &str) -> bool {
        self.download_slots.lock().unwrap_or_else(|p| p.into_inner()).contains_key(&(job_id, owner.to_string()))
    }

    /// The encoder held-video previews use; asked of ffmpeg once, then kept.
    pub async fn preview_encoder(&self) -> PreviewEncoder {
        // A failed question is not remembered: the next hold asks again.
        let found = self
            .preview_encoder
            .get_or_try_init(|| async { preview::discover(&self.ffmpeg_path).await.ok_or(()) })
            .await;
        found.copied().unwrap_or(PreviewEncoder::None)
    }

    pub fn new(
        repo: Repository,
        ytdl_path: PathBuf,
        ffmpeg_path: PathBuf,
        ffprobe_path: PathBuf,
        bmxtranswrap_path: PathBuf,
        temp_dir: PathBuf,
        watchfolder_dir: PathBuf,
    ) -> Self {
        Self {
            repo,
            ytdl_path,
            ffmpeg_path,
            ffprobe_path,
            bmxtranswrap_path,
            temp_dir,
            watchfolder_dir,
            encoder_slots: Arc::new(Semaphore::new(DEFAULT_ENCODER_SLOTS)),
            download_slots: Arc::new(Mutex::new(HashMap::new())),
            login_sites: Arc::new(Mutex::new(HashMap::new())),
            max_filesizes: Arc::new(Mutex::new(HashMap::new())),
            locker_max_file_bytes: omni_core::config::LockerConfig::default().max_file_bytes(),
            preview_encoder: Arc::new(tokio::sync::OnceCell::new()),
            archive: None,
            stopping: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            watchfolder_state: WatchfolderState::default(),
            waits: Arc::new(Mutex::new(HashMap::new())),
            retry_max_age: chrono::Duration::minutes(DEFAULT_RETRY_MAX_AGE_MINS),
            pacer: crate::pacing::HostPacer::default(),
            second_route: false,
        }
    }

    /// Give a source that failed probe, encode, wrap or the gate its second
    /// route before review (P11.6), once per job.
    pub fn with_second_route(mut self, on: bool) -> Self {
        self.second_route = on;
        self
    }

    /// The daemon is stopping: a job reaching DELIVER from now on waits just before it
    /// (P2.15). It is still at its earlier stage under this host's lease, so
    /// the next start requeues it cleanly, with no staging file in the
    /// watchfolder. Deliveries already copying are not affected.
    pub fn stop_new_deliveries(&self) {
        self.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// The videos yt-dlp sees on a news page that holds several, in page
    /// order, and whether yt-dlp has no extractor for it. See
    /// [`crate::downloader::PageScan`].
    ///
    /// This and the three probes below run for job `job_id`: a wait for a turn
    /// on a video platform (P11.4) is written to its history.
    pub async fn page_videos(&self, job_id: i64, url: &str) -> crate::downloader::PageScan {
        // The site's cookie jar, for the length of the call (P3.5).
        let jar = crate::cookie_file::for_url(&self.repo, url, &self.temp_dir, true);
        self.downloader_for(job_id, JobStage::Extract)
            .with_cookie_jar(jar.as_ref())
            .page_videos(url, std::time::Duration::from_secs(90))
            .await
    }

    /// Whether the post at `url` has a video, as yt-dlp's own extractor
    /// sees it without downloading (P3.13). `Some(false)` only when it says
    /// so ("No video could be found in this tweet"); a failure that proves
    /// nothing (network, login, timeout) is `None`.
    pub async fn post_has_video(&self, job_id: i64, url: &str) -> Option<bool> {
        let jar = crate::cookie_file::for_url(&self.repo, url, &self.temp_dir, true);
        let probed = self
            .downloader_for(job_id, JobStage::Extract)
            .with_cookie_jar(jar.as_ref())
            .probe(url, std::time::Duration::from_secs(60))
            .await;
        drop(jar);
        match probed {
            Ok(_) => Some(true),
            Err(line) if crate::errors::classify_download_error(&line) == ErrorCode::NoStreamFound => Some(false),
            Err(_) => None,
        }
    }

    /// Length and live flag of the video at `url`, as yt-dlp's own extractor
    /// sees them without downloading (P3.14). `None` when it could not say
    /// (network, login, timeout): the caller treats the length as unknown.
    pub async fn video_length(&self, job_id: i64, url: &str) -> Option<crate::downloader::PlatformInfo> {
        let jar = crate::cookie_file::for_url(&self.repo, url, &self.temp_dir, true);
        let probed = self
            .downloader_for(job_id, JobStage::Extract)
            .with_cookie_jar(jar.as_ref())
            .probe_media(url, std::time::Duration::from_secs(15))
            .await;
        drop(jar);
        probed.ok()
    }

    /// What yt-dlp says about one of the sniffer's candidates without downloading it (P3.15):
    /// live, gone, readable or unknown, asked with the sniffer's referer, user agent and cookies.
    pub async fn probe_candidate(
        &self,
        job_id: i64,
        url: &str,
        referer: Option<&str>,
        user_agent: Option<&str>,
        cookies: Option<&str>,
    ) -> crate::downloader::Probe {
        let jar = crate::cookie_file::for_url(&self.repo, url, &self.temp_dir, true);
        let probed = self
            .downloader_for(job_id, JobStage::Extract)
            .with_cookie_jar(jar.as_ref())
            .probe_candidate(url, referer, user_agent, cookies, std::time::Duration::from_secs(15))
            .await;
        drop(jar);
        probed
    }

    pub async fn process_job(&self, job: Job, owner: &str) -> Result<()> {
        self.process_job_with_context(job, owner, None, None, None).await
    }

    /// Move the job to `stage`, or stop for good if this worker no longer
    /// holds it (P1.14). A job MCR discarded, or one the reaper requeued and
    /// another worker took, must not reach the watchfolder from here. Writes
    /// no event and no audit row: the job is not ours to write to.
    async fn enter_stage(&self, job_id: i64, owner: &str, stage: JobStage, job_temp: &Path) -> Result<()> {
        if stage == JobStage::Deliver && self.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            info!("Job #{job_id}: the daemon is stopping; not starting its delivery");
            std::future::pending::<()>().await;
        }
        match self.repo.set_stage(job_id, owner, stage) {
            Ok(true) => Ok(()),
            Ok(false) => Err(self.lost(job_id, stage, job_temp).await),
            // At Deliver an unknown lease state must not let a file go to air
            // for a job MCR may just have cancelled: fail, and let the worker's
            // normal retry path run.
            Err(e) if stage == JobStage::Deliver => Err(e.context(ErrorCode::DeliveryFailed.as_str())),
            Err(e) => {
                warn!("Job #{job_id}: could not record stage {}: {e:#}", stage.as_str());
                Ok(())
            }
        }
    }

    /// Copy `source` into the watchfolder, recording the destination on the job right
    /// before the rename (P1.22). A job that is no longer ours at that moment delivers
    /// nothing: no `.mxf`, no `.tmp`, and the error is [`LeaseLost`].
    async fn deliver_to_watchfolder(
        &self,
        job_id: i64,
        owner: &str,
        source: &Path,
        slug: &str,
        job_temp: &Path,
    ) -> Result<crate::delivery::Delivered> {
        match WatchfolderDelivery::deliver_with(source, &self.watchfolder_dir, slug, |to, bytes| {
            self.record_delivering(job_id, owner, to, bytes)
        })
        .await
        {
            Err(e) if e.downcast_ref::<LeaseLostBeforeRename>().is_some() => {
                Err(self.lost(job_id, JobStage::Deliver, job_temp).await)
            }
            other => other,
        }
    }

    /// Write the delivery marker (P1.22) for the rename that follows. `Ok(false)`:
    /// the lease is gone, so the rename must not happen. An error also stops the
    /// delivery: without the marker a crash after the rename would be delivered
    /// a second time.
    fn record_delivering(&self, job_id: i64, owner: &str, to: &Path, bytes: u64) -> Result<bool> {
        self.repo
            .mark_delivering(job_id, owner, &to.to_string_lossy(), bytes)
            .context("Could not record the delivery before the rename")
            .context(ErrorCode::DeliveryFailed.as_str())
    }

    /// The error for a job that is no longer ours. Its workspace is removed
    /// when the row is gone (discarded) or CANCELLED: otherwise another
    /// worker may hold the very same `temp/jobs/{id}` and be reading its
    /// source.
    ///
    /// A cancelled job's directory goes too (P1.15): a killed download
    /// leaves a `.part`, and a retry or override of the same job would
    /// resume it into a different link. That is safe because the retry,
    /// override and redownload routes answer STILL_STOPPING while this
    /// worker's BusyJobs guard is alive, and this runs inside its task.
    async fn lost(&self, job_id: i64, stage: JobStage, job_temp: &Path) -> anyhow::Error {
        warn!("Job #{job_id}: no longer ours at {}; stopping without delivering", stage.as_str());
        match self.repo.get_job(job_id) {
            Ok(None) => WatchfolderDelivery::cleanup_job_temp_files(job_temp).await,
            Ok(Some(row)) if row.status == omni_core::models::JobStatus::Cancelled => {
                WatchfolderDelivery::cleanup_job_temp_files(job_temp).await
            }
            Ok(Some(_)) => {}
            Err(e) => warn!("Job #{job_id}: could not tell whether the job still exists, leaving its workspace: {e:#}"),
        }
        anyhow::Error::new(LeaseLost { job_id, stage }).context(ErrorCode::LeaseExpired.as_str())
    }

    /// Run the pipeline for a job the caller holds the lease on.
    ///
    /// `owner` is the lease owner string. Stage transitions go through
    /// `set_stage`, which refuses to advance a job this worker no longer owns --
    /// so a worker whose lease was reaped mid-transcode cannot keep driving a
    /// job another worker has already picked up. The engine never sets a
    /// terminal status; the worker that holds the lease decides that, so the two
    /// cannot race each other into an inconsistent row.
    pub async fn process_job_with_context(
        &self,
        job: Job,
        owner: &str,
        referer: Option<&str>,
        user_agent: Option<&str>,
        cookies: Option<&str>,
    ) -> Result<()> {
        self.process_job_cancellable(job, owner, referer, user_agent, cookies, None).await
    }

    /// [`Self::process_job_with_context`] with the run's token (plan P1.15).
    /// When it is cancelled the running tool is killed, and whatever error
    /// the stage then produces comes out as [`LeaseLost`]: the worker writes
    /// nothing for a job somebody else (MCR) has taken.
    pub async fn process_job_cancellable(
        &self,
        job: Job,
        owner: &str,
        referer: Option<&str>,
        user_agent: Option<&str>,
        cookies: Option<&str>,
        cancel: Option<CancellationToken>,
    ) -> Result<()> {
        self.process_job_mode(job, owner, referer, user_agent, cookies, cancel, DeliveryMode::Deliver)
            .await
            .map(|_| ())
    }

    /// [`Self::process_job_cancellable`] with what to do with the finished
    /// file (P10.5). `Hold` stops after the compliance gate, keeps the file
    /// and returns `Ok`; the worker then parks the job as AWAITING_APPROVAL
    /// (it, not the engine, sets the status).
    pub async fn process_job_mode(
        &self,
        job: Job,
        owner: &str,
        referer: Option<&str>,
        user_agent: Option<&str>,
        cookies: Option<&str>,
        cancel: Option<CancellationToken>,
        delivery: DeliveryMode,
    ) -> Result<RunOutcome> {
        let job_id = job.id;
        let slug = job.slug.clone();
        let url = job.url.clone();

        // Every stage works inside this job's own directory. Cleanup is a
        // remove_dir_all of exactly this path -- never a filename-prefix match
        // in a shared directory, which is how job 1 used to delete the working
        // files of jobs 10-19 and 100-199 mid-transcode (defect D-04).
        let job_temp = self.temp_dir.join("jobs").join(job_id.to_string());
        tokio::fs::create_dir_all(&job_temp).await.with_context(|| {
            format!("Failed creating the workspace for job #{job_id} at {job_temp:?}")
        })?;

        info!("BroadcastEngine: Processing Job #{} ({})", job_id, slug);

        // 1a. A video attached to an email (plan P4.6) is already on disk:
        // the mail watcher saved it and recorded `source_path`. It lives
        // outside the job workspace, so a failed attempt's cleanup below does
        // not take the only copy with it and a retry still has its source.
        if url.starts_with("attachment://") {
            let source = job.source_path.as_deref().map(PathBuf::from).filter(|p| p.is_file());
            let Some(source) = source else {
                let err_msg = "The attachment is not on disk; download it from the email by hand";
                warn!("Job #{job_id}: {err_msg}");
                let _ = self.repo.record_event(job_id, "ERROR", Some(JobStage::Download), err_msg);
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(anyhow::anyhow!(err_msg).context(ErrorCode::ManualDownload.as_str()));
            };
            let _ = self.repo.record_event(
                job_id,
                "INFO",
                Some(JobStage::Download),
                "Source is the file attached to the email; nothing to download",
            );
            let first = self.finish_from_source(job.clone(), owner, source, job_temp.clone(), cancel.clone(), delivery).await;
            let context = SourceContext { referer, user_agent, cookies };
            return self.or_second_route(first, job, owner, context, cancel, delivery, job_temp).await;
        }

        // 1. Download stage
        let raw_download_res =
            self.download_source(job_id, &url, &job_temp, SourceContext { referer, user_agent, cookies }, &cancel, FormatRoute::Usual).await;

        let downloaded_file = match raw_download_res {
            Ok(path) => path,
            Err(e) => {
                if is_cancelled(&cancel) {
                    return Err(self.lost(job_id, JobStage::Download, &job_temp).await);
                }
                // The downloader already classified the failure, so the pipeline
                // does not have to re-read stderr to decide whether a retry could
                // possibly help (plan P1.9).
                let (code, err_msg) = download_failure_text(&e);
                warn!("Job #{job_id}: {err_msg}");
                let _ = self.repo.record_event(job_id, "ERROR", Some(JobStage::Download), &err_msg);
                let _ = self.repo.log_audit(
                    "WARN",
                    "DOWNLOAD",
                    &format!("Job #{job_id} ({slug}): {err_msg}"),
                );
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(e.context(code.as_str()));
            }
        };

        let first = self.finish_from_source(job.clone(), owner, downloaded_file, job_temp.clone(), cancel.clone(), delivery).await;
        self.or_second_route(first, job, owner, SourceContext { referer, user_agent, cookies }, cancel, delivery, job_temp).await
    }

    /// Download job `job_id`'s link `url` into its workspace with `formats`.
    /// The site's cookie jar (P3.5) is a file in the workspace for the length
    /// of the download only: the guard removes it on every way out.
    async fn download_source(
        &self,
        job_id: i64,
        url: &str,
        job_temp: &Path,
        context: SourceContext<'_>,
        cancel: &Option<CancellationToken>,
        formats: FormatRoute,
    ) -> Result<PathBuf> {
        let downloader = self.downloader_for(job_id, JobStage::Download);
        let login_site = self.login_sites.lock().unwrap_or_else(|p| p.into_inner()).get(&job_id).cloned();
        let jar_urls: Vec<&str> = std::iter::once(url).chain(login_site.as_deref()).collect();
        let cookie_file = crate::cookie_file::for_urls(&self.repo, &jar_urls, job_temp, false);
        let max_filesize = self.max_filesizes.lock().unwrap_or_else(|p| p.into_inner()).get(&job_id).copied();
        let downloaded = downloader
            .download(
                job_id,
                url,
                job_temp,
                DownloadOpts {
                    referer: context.referer,
                    user_agent: context.user_agent,
                    cookie_header: context.cookies,
                    cookie_jar: cookie_file.as_ref().map(|f| f.path()),
                    cookie_values: cookie_file.as_ref().map(|f| f.values()).unwrap_or(&[]),
                    cancel: cancel.clone(),
                    formats,
                    max_filesize,
                    ..Default::default()
                },
                {
                    let mut write = progress_writer(self.repo.clone(), job_id);
                    move |prog| write(prog.percent, &prog.speed, &prog.eta)
                },
            )
            .await;
        drop(cookie_file);
        downloaded
    }

    /// The first route's result, or, for a source that could not be made into
    /// a compliant file (PROBE_FAILED, TRANSCODE_FAILED, REWRAP_FAILED,
    /// COMPLIANCE_FAILED), the second route's (P11.6, [`crate::second_route`]):
    /// the link downloaded again with other formats (an attachment: its file
    /// as it is), copied into a clean container, then the usual encode, wrap,
    /// gate and delivery. Once per job, recorded on the row before anything
    /// runs, so a restart or a retry does not take it again; with the engine's
    /// second route off, or the job's taken, the first failure stands.
    ///
    /// When the second route fails at the download, the remux, or the same
    /// kind of stage (its own source failure), the job ends with its first
    /// failure; any other ending of the second run (delivered, held, waiting
    /// for the watchfolder, a full disk, a lost lease) is the job's ending.
    async fn or_second_route(
        &self,
        first: Result<RunOutcome>,
        job: Job,
        owner: &str,
        context: SourceContext<'_>,
        cancel: Option<CancellationToken>,
        delivery: DeliveryMode,
        job_temp: PathBuf,
    ) -> Result<RunOutcome> {
        let first_err = match first {
            Ok(outcome) => return Ok(outcome),
            Err(e) => e,
        };
        // A stopping daemon starts no new download or remux and does not use up the job's one
        // second route; the first failure stands, as before the second route existed.
        if !self.second_route || first_err.downcast_ref::<LeaseLost>().is_some() || is_cancelled(&cancel) || self.is_stopping() {
            return Err(first_err);
        }
        let Some(code) = crate::second_route::applies(&first_err) else {
            return Err(first_err);
        };
        let job_id = job.id;
        match self.repo.claim_second_route(job_id, owner) {
            Ok(true) => {}
            Ok(false) if !self.repo.owns_lease(job_id, owner).unwrap_or(true) => {
                return Err(self.lost(job_id, JobStage::Download, &job_temp).await);
            }
            // Taken already, by an earlier run of this job.
            Ok(false) => return Err(first_err),
            Err(e) => {
                warn!("Job #{job_id}: could not record the second route, so it is not taken: {e:#}");
                return Err(first_err);
            }
        }

        let attachment = job.url.starts_with("attachment://");
        let line = if attachment {
            format!("{code}: second route: the attached file is copied into a clean container and made again")
        } else {
            format!("{code}: second route: downloading again with other formats, copied into a clean container and made again")
        };
        info!("Job #{job_id}: {line}");
        let _ = self.repo.record_event(job_id, "INFO", Some(JobStage::Download), &line);

        let source = match self.second_source(&job, owner, context, &cancel, &job_temp, attachment).await {
            Ok(source) => source,
            Err(e) if e.downcast_ref::<LeaseLost>().is_some() => return Err(e),
            Err(_) if is_cancelled(&cancel) => return Err(self.lost(job_id, JobStage::Download, &job_temp).await),
            Err(e) => {
                let why = format!("The second route did not help: {e:#}");
                warn!("Job #{job_id}: {why}");
                let _ = self.repo.record_event(job_id, "WARN", Some(JobStage::Download), &why);
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(first_err);
            }
        };

        match self.finish_from_source(job, owner, source, job_temp, cancel, delivery).await {
            Ok(outcome) => {
                let _ = self.repo.record_event(job_id, "INFO", None, "The second route made a compliant file");
                Ok(outcome)
            }
            Err(e) if e.downcast_ref::<LeaseLost>().is_none() && crate::second_route::applies(&e).is_some() => {
                let why = format!("The second route did not help either ({}); the first failure stands", crate::second_route::applies(&e).map_or("", |c| c.as_str()));
                warn!("Job #{job_id}: {why}");
                let _ = self.repo.record_event(job_id, "WARN", None, &why);
                Err(first_err)
            }
            Err(e) => Err(e),
        }
    }

    /// The second route's source in `job_temp` (P11.6): the link downloaded
    /// again with the second route's formats (an attachment's file as it is),
    /// copied into a clean container. A downloaded file is removed once
    /// copied; an attachment is never touched.
    async fn second_source(
        &self,
        job: &Job,
        owner: &str,
        context: SourceContext<'_>,
        cancel: &Option<CancellationToken>,
        job_temp: &Path,
        attachment: bool,
    ) -> Result<PathBuf> {
        let job_id = job.id;
        tokio::fs::create_dir_all(job_temp)
            .await
            .with_context(|| format!("Failed creating the workspace for job #{job_id} at {job_temp:?}"))
            .context(ErrorCode::PipelineFailed.as_str())?;
        let (source, downloaded) = if attachment {
            let source = job.source_path.as_deref().map(PathBuf::from).filter(|p| p.is_file());
            let source = source.context("the attached file is not on disk any more").context(ErrorCode::ManualDownload.as_str())?;
            (source, false)
        } else {
            self.enter_stage(job_id, owner, JobStage::Download, job_temp).await?;
            let path = self.download_source(job_id, &job.url, job_temp, context, cancel, FormatRoute::Second).await?;
            (path, true)
        };
        let _ = self.repo.update_job_progress(job_id, 0.0, "Remuxing", "--:--");
        let remuxed = crate::second_route::remux(&self.ffmpeg_path, &self.ffprobe_path, &source, job_temp, cancel.clone()).await?;
        if downloaded {
            let _ = tokio::fs::remove_file(&source).await;
        }
        let v = &remuxed.source.video;
        let _ = self.repo.record_event(
            job_id,
            "INFO",
            Some(JobStage::Download),
            &format!("Second route source: {}x{} {}, {:.1} s, in a clean container", v.width, v.height, v.codec, remuxed.source.duration_secs),
        );
        Ok(remuxed.path)
    }

    /// Stages 2–6, from a source file on disk to the watchfolder.
    async fn finish_from_source(
        &self,
        job: Job,
        owner: &str,
        downloaded_file: PathBuf,
        job_temp: PathBuf,
        cancel: Option<CancellationToken>,
        delivery: DeliveryMode,
    ) -> Result<RunOutcome> {
        let job_id = job.id;
        let slug = job.slug.clone();

        // 2. Transcode stage (Sony XDCAM HD422 PAL 1080i50)
        if let Err(e) = self.enter_stage(job_id, owner, JobStage::Transcode, &job_temp).await {
            // Not ours any more: the next download must not wait on this slot.
            self.release_download_slot(job_id, owner);
            return Err(e);
        }

        // The source is on disk: let the next download start, then wait for
        // an encoder (plan P1.13). Held until the encode ends; rewrap and
        // delivery are disk-bound and do not need it.
        self.release_download_slot(job_id, owner);
        let encoder = match self.encoder_slots.clone().try_acquire_owned() {
            Ok(p) => p,
            Err(_) => {
                let _ = self.repo.update_job_progress(job_id, 0.0, "Waiting for encoder", "--:--");
                let _ = self.repo.record_event(
                    job_id,
                    "INFO",
                    Some(JobStage::Transcode),
                    "Downloaded; waiting for a free encoder",
                );
                let wait = self.encoder_slots.clone().acquire_owned();
                match &cancel {
                    Some(t) => tokio::select! {
                        _ = t.cancelled() => return Err(self.lost(job_id, JobStage::Transcode, &job_temp).await),
                        p = wait => p.context("encoder slots closed")?,
                    },
                    None => wait.await.context("encoder slots closed")?,
                }
            }
        };
        // Discarded or taken while it waited for an encoder: do not burn
        // minutes of encode on it.
        if !self.repo.owns_lease(job_id, owner).unwrap_or(true) {
            drop(encoder);
            return Err(self.lost(job_id, JobStage::Transcode, &job_temp).await);
        }
        let _ = self.repo.update_job_progress(job_id, 0.0, "Transcoding", "--:--");

        let mut transcoder = Transcoder::new(&self.ffmpeg_path, &self.ffprobe_path);
        if let Some(t) = &cancel {
            transcoder = transcoder.with_cancel(t.clone());
        }

        // Room for this file before it is made (plan P1.4): a full disk
        // mid-encode fails late, after minutes of work, and can take the
        // database down with it. Waiting may fix it: LOW_DISK goes back to
        // the queue for 10 min, three times, and then to review (a clip that
        // needs more room than the volume has fails the same way every run).
        if let Ok(probe) = transcoder.probe_source(&downloaded_file).await {
            if let Err(why) = room_for(probe.duration_secs, &job_temp, &self.watchfolder_dir) {
                warn!("Job #{job_id}: {why}");
                let _ = self.repo.record_event(job_id, "WARN", Some(JobStage::Transcode), &why);
                drop(encoder);
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(anyhow::anyhow!("{}: {why}", ErrorCode::LowDisk.as_str()).context(ErrorCode::LowDisk.as_str()));
            }
        }
        let mut write_progress = progress_writer(self.repo.clone(), job_id);
        let transcode_res = transcoder
            .transcode(job_id, &downloaded_file, &job_temp, move |prog| {
                write_progress(prog.percent, &prog.speed, &prog.eta)
            })
            .await;
        drop(encoder);

        let (intermediate_mxf, duration) = match transcode_res {
            Ok(res) => res,
            Err(e) => {
                if is_cancelled(&cancel) {
                    return Err(self.lost(job_id, JobStage::Transcode, &job_temp).await);
                }
                let err_msg = format!("FFmpeg Transcode failed: {e:#}");
                error!("Job #{}: {}", job_id, err_msg);
                let _ = self.repo.record_event(job_id, "ERROR", None, &err_msg);
                let _ = self.repo.log_audit("ERROR", "TRANSCODE", &format!("Job #{} ({}): {}", job_id, slug, err_msg));
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(e);
            }
        };

        // 3. Rewrap stage (SMPTE RDD9 OP1a MXF)
        self.enter_stage(job_id, owner, JobStage::Rewrap, &job_temp).await?;
        // A rename by MCR up to now names the file (P7.12). Read after the
        // stage is REWRAP, which is when a rename starts being refused, so
        // one either lands here or is turned down; never half-applied.
        let slug = self.repo.get_job(job_id).ok().flatten().map(|j| j.slug).unwrap_or(slug);
        let _ = self.repo.update_job_progress(job_id, 99.0, "Rewrapping", "--:--");

        let rewrapper = Rewrapper::new(&self.bmxtranswrap_path);
        let final_temp_mxf = match rewrapper
            .rewrap_with_clip(job_id, &intermediate_mxf, &job_temp, Some(&slug), cancel.clone())
            .await {
            Ok(path) => path,
            Err(e) => {
                if is_cancelled(&cancel) {
                    return Err(self.lost(job_id, JobStage::Rewrap, &job_temp).await);
                }
                let err_msg = format!("bmxtranswrap RDD9 failed: {e:#}");
                error!("Job #{}: {}", job_id, err_msg);
                let _ = self.repo.record_event(job_id, "ERROR", None, &err_msg);
                let _ = self.repo.log_audit("ERROR", "REWRAP", &format!("Job #{} ({}): {}", job_id, slug, err_msg));
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(e);
            }
        };
        // The intermediate has served its purpose: dropping it now halves
        // what a long clip holds on the temp disk until delivery (P1.4).
        let _ = tokio::fs::remove_file(&intermediate_mxf).await;

        // 4. Compliance gate (plan P1.6, defect D-06).
        //
        // The last thing between the transcoder and air. ffmpeg exits zero for
        // plenty of files Dalet will reject or mis-play -- a chain that quietly
        // fell back to 4:2:0, an audio map that produced one stereo stream
        // instead of eight mono ones, a transcode truncated by a source that
        // ended early. A failed report is never delivered: a job in review is a
        // minor annoyance, a wrong file in the running order is not.
        self.enter_stage(job_id, owner, JobStage::Verify, &job_temp).await?;
        let report = match crate::verify::verify_mxf_with_clip(&self.ffprobe_path, &final_temp_mxf, duration, Some(&slug))
            .await {
            Ok(report) => report,
            Err(e) => {
                // Could not run the check at all. Treated as a failure: an
                // unverifiable file must not reach the watchfolder.
                let err_msg = format!("Compliance check could not run: {e}");
                error!("Job #{job_id}: {err_msg}");
                let _ = self.repo.record_event(job_id, "ERROR", Some(JobStage::Verify), &err_msg);
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(e.context("COMPLIANCE_FAILED"));
            }
        };

        if let Ok(json) = serde_json::to_string(&report) {
            let _ = self.repo.set_compliance_report(job_id, &json);
        }

        if !report.pass {
            let summary = report.summary();
            error!("Job #{job_id}: {summary}");
            let _ = self
                .repo
                .record_event(job_id, "ERROR", Some(JobStage::Verify), &summary);
            let _ = self.repo.log_audit(
                "ERROR",
                "COMPLIANCE",
                &format!("Job #{job_id} ({slug}) failed the compliance gate: {summary}"),
            );
            WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
            return Err(anyhow::anyhow!("COMPLIANCE_FAILED: {summary}"));
        }

        let _ = self.repo.record_event(
            job_id,
            "INFO",
            Some(JobStage::Verify),
            &report.summary(),
        );

        // 4b. Held for MCR: the file stays here, bound to its SHA-256, and
        // nothing reaches the watchfolder until it is approved (P10.5).
        if let DeliveryMode::Hold { hold_hours, held_max_bytes } = delivery {
            self.hold_final(job_id, owner, &final_temp_mxf, &slug, duration, hold_hours, held_max_bytes, &job_temp, cancel.as_ref())
                .await?;
            return Ok(RunOutcome::Held);
        }

        // 5. Atomic Delivery to Dalet Watchfolder
        self.enter_stage(job_id, owner, JobStage::Deliver, &job_temp).await?;
        let delivered = match self.deliver_to_watchfolder(job_id, owner, &final_temp_mxf, &slug, &job_temp).await {
            Ok(dest) => dest,
            Err(e) if e.downcast_ref::<LeaseLost>().is_some() => return Err(e),
            // The share does not answer (P11.2a): the verified file waits in its
            // workspace and is delivered when it does; nothing is made again.
            Err(e) if unreachable_cause(&e).is_some() => {
                return Err(self.park(job_id, owner, &final_temp_mxf, None, duration, &slug, &job_temp, e, true).await);
            }
            Err(e) => {
                let err_msg = format!("Watchfolder delivery failed: {e:#}");
                error!("Job #{}: {}", job_id, err_msg);
                let _ = self.repo.record_event(job_id, "ERROR", None, &err_msg);
                let _ = self.repo.log_audit("ERROR", "DELIVERY", &format!("Job #{} ({}): {}", job_id, slug, err_msg));
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(e);
            }
        };

        self.after_delivery(job_id, owner, &slug, &delivered, duration, Some(&downloaded_file), &job_temp).await;
        info!("Job #{} ({}) broadcast ingest successfully finished!", job_id, slug);
        Ok(RunOutcome::Delivered)
    }

    /// Stages 6 and 7 after the file is in the watchfolder: the collision
    /// warning, the recorded file, the archive and the workspace's removal.
    /// The status is not set here: the worker that holds the lease finishes the
    /// job as Completed with this file_path. Never fails the job.
    async fn after_delivery(
        &self,
        job_id: i64,
        owner: &str,
        slug: &str,
        delivered: &crate::delivery::Delivered,
        duration: f64,
        source: Option<&Path>,
        job_temp: &Path,
    ) {
        self.watchfolder_state.set(true);
        self.forget_waits(job_id);
        if delivered.suffixed {
            // Two assets now share a slug in Dalet. Not a failure -- the file is
            // on air -- but an operator has to know which one is which.
            let _ = self.repo.record_event(
                job_id,
                "WARN",
                Some(JobStage::Deliver),
                &format!(
                    "Slug collision: delivered as {} because {slug}.mxf already existed",
                    delivered.filename
                ),
            );
        }

        // 6. Record the file.
        let dest_str = delivered.path.to_string_lossy().to_string();
        let _ = self.repo.update_job_progress(job_id, 100.0, "Completed", "00:00");
        match self.repo.set_delivered_file(job_id, owner, &dest_str, duration) {
            Ok(true) => {}
            Ok(false) => warn!("Job #{job_id}: lease lost after delivery; the file {dest_str} is in the watchfolder"),
            Err(e) => warn!("Job #{job_id}: could not record the delivered file {dest_str}: {e:#}"),
        }

        // 7. The file is on air and recorded: keep the source for the archive
        // (P1.7), then remove the workspace. Never fails the job.
        self.archive_then_clean(job_id, owner, source, job_temp, &delivered.filename).await;

        let _ = self.repo.log_audit(
            "SUCCESS",
            "INGEST",
            &format!("Job #{} ({}) ready in watchfolder at {:?}", job_id, slug, dest_str),
        );
    }

    /// The watchfolder did not answer (P11.2a): record that the verified file
    /// at `ready` waits for it, with its size and SHA-256 (`known`, or worked
    /// out now), and return the error tagged DELIVERY_FAILED; the worker queues
    /// the job to wait. The workspace stays. When the wait cannot be recorded,
    /// the job is handled as before: lost lease → [`LeaseLost`]; anything else
    /// → the workspace is removed and the error, tagged DELIVERY_FAILED, takes
    /// the normal retry rule.
    async fn park(
        &self,
        job_id: i64,
        owner: &str,
        ready: &Path,
        known: Option<(u64, String)>,
        duration: f64,
        slug: &str,
        job_temp: &Path,
        delivery_err: anyhow::Error,
        first: bool,
    ) -> anyhow::Error {
        self.watchfolder_state.set(false);
        let fingerprint = match known {
            Some(k) => Ok(k),
            None => {
                let path = ready.to_path_buf();
                tokio::task::spawn_blocking(move || -> std::io::Result<(u64, String)> {
                    let bytes = std::fs::metadata(&path)?.len();
                    Ok((bytes, omni_core::held::sha256_file(&path)?))
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r.map_err(anyhow::Error::from))
            }
        };
        let parked = fingerprint.and_then(|(bytes, sha)| {
            self.repo.park_for_delivery(job_id, owner, &ready.to_string_lossy(), bytes, &sha, duration)
        });
        match parked {
            Ok(true) => self.waiting(job_id, slug, delivery_err, first, true),
            Ok(false) => {
                self.forget_waits(job_id);
                self.lost(job_id, JobStage::Deliver, job_temp).await
            }
            Err(e) => {
                self.forget_waits(job_id);
                let err_msg = format!("Watchfolder delivery failed and the finished video could not be kept for later: {delivery_err:#}");
                error!("Job #{job_id}: {err_msg} ({e:#})");
                let _ = self.repo.record_event(job_id, "ERROR", None, &err_msg);
                let _ = self.repo.log_audit("ERROR", "DELIVERY", &format!("Job #{job_id} ({slug}): {err_msg}"));
                WatchfolderDelivery::cleanup_job_temp_files(job_temp).await;
                delivery_err.context(ErrorCode::DeliveryFailed.as_str())
            }
        }
    }

    /// The error of a video that waits for the watchfolder (P11.2a); the row
    /// already says it waits. One more wait is counted (the requeue backs off,
    /// see [`Self::wait_backoff`]). The WARN event and the audit row are
    /// written for the first park of a job (`first`) and afterwards only when
    /// the cause is not the one the job's `error_message` already carries: a
    /// share that stays down writes them once, not every try. `share_down`
    /// closes the lease gate until the share watch sees the share again.
    fn waiting(&self, job_id: i64, slug: &str, delivery_err: anyhow::Error, first: bool, share_down: bool) -> anyhow::Error {
        if share_down {
            self.watchfolder_state.set(false);
        }
        let line = format!("Watchfolder not reachable; the finished video waits and will be delivered when it answers: {delivery_err}");
        let current = self.repo.get_job(job_id).ok().flatten().and_then(|j| j.error_message);
        self.note_wait(job_id, first, &line);
        if first || current.as_deref() != Some(line.as_str()) {
            warn!("Job #{job_id}: {line}");
            let _ = self.repo.record_event(job_id, "WARN", Some(JobStage::Deliver), &line);
            let _ = self.repo.log_audit("WARN", "DELIVERY", &format!("Job #{job_id} ({slug}): {line}"));
        } else {
            info!("Job #{job_id}: still waiting for the watchfolder");
        }
        delivery_err.context(ErrorCode::DeliveryFailed.as_str())
    }

    /// B1 of P11.2a: `Err` (the video waits again, nothing is copied) when the
    /// watchfolder volume's free space can be read and is short of `bytes`
    /// plus 64 MiB. Asked off the async threads, for at most 15 s; a volume
    /// that does not answer is not held against the delivery.
    async fn room_in_watchfolder(&self, job_id: i64, slug: &str, bytes: u64) -> Result<()> {
        let wf = self.watchfolder_dir.clone();
        let asked = tokio::time::timeout(
            omni_core::selftest::WATCHFOLDER_PROBE_TIMEOUT,
            tokio::task::spawn_blocking(move || omni_core::selftest::disk_space(&wf).map(|(free, _)| free)),
        )
        .await;
        let free = match asked {
            Ok(Ok(free)) => free,
            _ => None,
        };
        watchfolder_room(free, bytes)
            .map_err(|cause| self.waiting(job_id, slug, anyhow::Error::new(DeliveryError::Unreachable(cause)), false, false))
    }

    /// What the watchfolder says about the job's delivery marker; blocking
    /// file I/O (up to 15 s on a share), so off the async threads.
    async fn ask_marker(&self, job_id: i64) -> Result<Option<MarkerCheck>> {
        let repo = self.repo.clone();
        tokio::task::spawn_blocking(move || repo.check_delivery_marker(job_id))
            .await
            .context("delivery marker check task")?
    }

    /// The event for a marker whose file is in the watchfolder with another
    /// size: not this job's video, so the delivery goes on (with `_N`), as
    /// start-up recovery decides for it.
    fn note_wrong_size(&self, job_id: i64, path: &str, found: u64, expected: u64) {
        let name = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.to_string());
        let _ = self.repo.record_event(
            job_id,
            "WARN",
            Some(JobStage::Deliver),
            &format!("{name} is in the watchfolder with {found} bytes, not the {expected} this job was delivering; it is not treated as this job's video, so it is delivered again. Check Dalet for a duplicate"),
        );
    }

    fn is_stopping(&self) -> bool {
        self.stopping.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The daemon is stopping: a waiting video goes back to waiting at once,
    /// with no event, no count and no change to the lease gate, so the stop
    /// does not wait for it (P11.2a).
    fn stopping_wait(&self) -> anyhow::Error {
        anyhow::Error::new(DeliveryError::Unreachable("the daemon is stopping".into())).context(ErrorCode::DeliveryFailed.as_str())
    }

    /// The error for a waiting video a person must settle (P11.2a); the worker
    /// sends the job to review with `code` and `message` and keeps everything.
    fn needs_check(&self, job_id: i64, code: ErrorCode, message: String) -> anyhow::Error {
        warn!("Job #{job_id}: {message}");
        self.forget_waits(job_id);
        anyhow::Error::new(DeliveryNeedsCheck { code, message }).context(code.as_str())
    }

    /// What became of the rename an earlier attempt of this waiting video
    /// tried, if any (its delivery marker, P11.2a):
    /// - none recorded: `Ok(None)`, deliver;
    /// - its file is in the watchfolder at that size, or its delivery was
    ///   recorded: `Ok(Some(path))`, complete without copying;
    /// - a file of another size is there: noted, `Ok(None)`, deliver next to it;
    /// - the share cannot say: `Err`, wait again (the lease gate closes);
    /// - not there although a rename was attempted (it may have landed and been
    ///   moved into Dalet by hand), or the watchfolder changed since: `Err`
    ///   [`DeliveryNeedsCheck`], a person decides; nothing is copied.
    async fn earlier_rename(&self, job_id: i64, slug: &str) -> Result<Option<String>> {
        let marker = self.repo.delivery_marker(job_id).context(ErrorCode::DeliveryFailed.as_str())?;
        let Some((path, _, recorded)) = marker else {
            return Ok(None);
        };
        let name = Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.clone());
        let folder = Path::new(&path).parent().map(Path::to_path_buf).unwrap_or_default();
        if !recorded && !same_folder(&folder, &self.watchfolder_dir) {
            // An old folder may be a share that is gone for good: not asked, not waited for.
            return Err(self.needs_check(
                job_id,
                ErrorCode::DeliveryUnconfirmed,
                format!("The watchfolder changed while {name} was waiting; check {} and Dalet before retrying", folder.display()),
            ));
        }
        match self.ask_marker(job_id).await.context(ErrorCode::DeliveryFailed.as_str())? {
            Some(MarkerCheck::Delivered { path } | MarkerCheck::Recorded { path }) => Ok(Some(path)),
            Some(MarkerCheck::Unknown { reason, .. }) => {
                Err(self.waiting(job_id, slug, anyhow::Error::new(DeliveryError::Unreachable(reason)), false, true))
            }
            Some(MarkerCheck::WrongSize { path, found, expected }) => {
                self.note_wrong_size(job_id, &path, found, expected);
                Ok(None)
            }
            Some(MarkerCheck::Missing) => Err(self.needs_check(
                job_id,
                ErrorCode::DeliveryUnconfirmed,
                format!("The delivery of {name} was interrupted and the file is not in the watchfolder now. Check Dalet for {name} before retrying"),
            )),
            None => Ok(None),
        }
    }

    /// Before a waiting video picks its name: its own leftover staging file
    /// from an attempt the share cut goes, if it was there before this run
    /// began (`run_started`) and is not in use
    /// ([`crate::delivery::reclaim_own_staging`]). Off the async threads.
    async fn reclaim_own_staging(&self, job_id: i64, slug: &str, run_started: std::time::SystemTime) {
        let (dir, name) = (self.watchfolder_dir.clone(), format!("{slug}.mxf"));
        let ran = tokio::time::timeout(
            omni_core::selftest::WATCHFOLDER_PROBE_TIMEOUT,
            tokio::task::spawn_blocking(move || crate::delivery::reclaim_own_staging(&dir, &name, run_started)),
        )
        .await;
        if !matches!(ran, Ok(Ok(_))) {
            warn!("Job #{job_id}: could not look for its leftover staging file");
        }
    }

    /// Deliver the finished, verified video that waited for the watchfolder
    /// (P11.2a), for a job leased with `delivery_wait` = 1: no download, no
    /// encode, no compliance run (the bytes are proven to be the verified ones).
    ///
    /// First the watchfolder is asked about the delivery marker the earlier
    /// attempt left: a rename that landed although it looked failed completes
    /// the job without a second copy; a share that cannot be read waits again.
    /// Then the file must still be the one recorded (inside this job's
    /// workspace, same size, same SHA-256, hashed through a handle nobody can
    /// write to and kept open for the copy); if not, the workspace is removed
    /// and the worker runs the job again ([`ReadyOutcome::Rerun`]). Otherwise
    /// it is delivered exactly as the normal path delivers, and on success the
    /// job ends as the normal path ends it. The engine sets no status.
    pub async fn deliver_ready(&self, job: &Job, owner: &str, cancel: Option<CancellationToken>) -> Result<ReadyOutcome> {
        // Before anything is copied: a staging file of this slug older than this is a cut copy.
        let run_started = std::time::SystemTime::now();
        let job_id = job.id;
        let job_temp = self.temp_dir.join("jobs").join(job_id.to_string());
        // Nothing is downloaded: the slot it was leased under goes back.
        self.release_download_slot(job_id, owner);
        let duration = job.duration_secs;
        let source = if job.url.starts_with("attachment://") {
            job.source_path.as_deref().map(PathBuf::from).filter(|p| p.is_file())
        } else {
            crate::downloader::find_downloaded_file(&job_temp).await
        };

        // A stop is under way: back to waiting at once, as it is (no event).
        if self.is_stopping() {
            return Err(self.stopping_wait());
        }
        // The name the file was wrapped with (`--clip`): a waiting job cannot be
        // renamed, so the row's slug is it; never re-read for a later name.
        let slug = job.slug.clone();

        // a. Did the interrupted delivery land after all?
        match self.earlier_rename(job_id, &slug).await? {
            Some(path) => {
                if !self.repo.owns_lease(job_id, owner).unwrap_or(false) {
                    return Err(self.lost(job_id, JobStage::Deliver, &job_temp).await);
                }
                let filename = Path::new(&path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                info!("Job #{job_id}: the waiting video is already in the watchfolder as {filename}; not copied again");
                let _ = self.repo.record_event(
                    job_id,
                    "INFO",
                    Some(JobStage::Deliver),
                    "Already in the watchfolder from the interrupted delivery; not copied again",
                );
                let bytes = job.ready_bytes.unwrap_or(0).max(0) as u64;
                let delivered = crate::delivery::Delivered { path: PathBuf::from(&path), filename, suffixed: false, bytes };
                self.after_delivery(job_id, owner, &slug, &delivered, duration, source.as_deref(), &job_temp).await;
                return Ok(ReadyOutcome::Delivered);
            }
            None => {}
        }

        // b. Still exactly the file that was verified?
        let ready = job.ready_path.as_deref().map(PathBuf::from).filter(|p| p.starts_with(&job_temp));
        let (Some(ready), Some(bytes), Some(sha)) = (ready, job.ready_bytes, job.ready_sha256.clone()) else {
            return self.rerun(job_id, owner, "nothing is recorded for it in this job's workspace").await;
        };
        let lock_path = ready.clone();
        let hashed = tokio::task::spawn_blocking(move || {
            let file = omni_core::held::open_locked(&lock_path)?;
            let len = file.metadata()?.len();
            if len as i64 != bytes {
                return Ok((None, file));
            }
            let found = omni_core::held::sha256_reader(&file)?;
            Ok::<_, std::io::Error>((Some(found), file))
        })
        .await;
        let guard = match hashed {
            Ok(Ok((Some(found), guard))) if found.eq_ignore_ascii_case(&sha) => guard,
            Ok(Ok((Some(_), guard))) => {
                drop(guard);
                return self.rerun(job_id, owner, "its SHA-256 is not the one that was verified").await;
            }
            Ok(Ok((None, guard))) => {
                drop(guard);
                return self.rerun(job_id, owner, "its size is not the one that was verified").await;
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                return self.rerun(job_id, owner, &format!("it cannot be read: {e}")).await
            }
            // Held open, no permission, a disk error: the file may be fine. Wait, never delete it;
            // past the limit a person is asked, the workspace kept (P11.2b).
            Ok(Err(e)) if unreadable_too_long(job.ready_at, chrono::Utc::now()) => {
                return Err(self.needs_check(
                    job_id,
                    ErrorCode::PipelineFailed,
                    format!(
                        "The finished video in the workspace could not be read for {} hours: {e}",
                        UNREADABLE_WAIT_LIMIT.num_hours()
                    ),
                ));
            }
            Ok(Err(e)) => {
                let cause = format!("the waiting video cannot be read now: {e}");
                return Err(self.waiting(job_id, &slug, anyhow::Error::new(DeliveryError::Unreachable(cause)), false, false));
            }
            Err(e) => return Err(anyhow::Error::new(e).context(ErrorCode::PipelineFailed.as_str())),
        };
        if is_cancelled(&cancel) {
            drop(guard);
            return Err(self.lost(job_id, JobStage::Deliver, &job_temp).await);
        }

        // c. The same delivery as every other video, under the name it was wrapped with.
        if let Err(e) = self.enter_stage(job_id, owner, JobStage::Deliver, &job_temp).await {
            drop(guard);
            return Err(e);
        }
        // A share that answers but has no room for it: not copied, it waits again.
        if let Err(e) = self.room_in_watchfolder(job_id, &slug, bytes.max(0) as u64).await {
            drop(guard);
            return Err(e);
        }
        self.reclaim_own_staging(job_id, &slug, run_started).await;
        let delivered = match self.deliver_to_watchfolder(job_id, owner, &ready, &slug, &job_temp).await {
            Ok(dest) => dest,
            Err(e) if e.downcast_ref::<LeaseLost>().is_some() => {
                drop(guard);
                return Err(e);
            }
            // d. Still not answering: it waits again, as it is.
            Err(e) if unreachable_cause(&e).is_some() => {
                drop(guard);
                return Err(self.waiting(job_id, &slug, e, false, true));
            }
            Err(e) => {
                drop(guard);
                let err_msg = format!("Watchfolder delivery failed: {e:#}");
                error!("Job #{job_id}: {err_msg}");
                let _ = self.repo.record_event(job_id, "ERROR", None, &err_msg);
                let _ = self.repo.log_audit("ERROR", "DELIVERY", &format!("Job #{job_id} ({slug}): {err_msg}"));
                WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
                return Err(e);
            }
        };
        drop(guard);
        self.after_delivery(job_id, owner, &slug, &delivered, duration, source.as_deref(), &job_temp).await;
        info!("Job #{job_id} ({slug}) delivered after waiting for the watchfolder");
        Ok(ReadyOutcome::Delivered)
    }

    /// The waiting video is not the verified one any more: deliver nothing,
    /// remove the workspace, and say why (P11.2a).
    async fn rerun(&self, job_id: i64, owner: &str, why: &str) -> Result<ReadyOutcome> {
        let job_temp = self.temp_dir.join("jobs").join(job_id.to_string());
        // Another worker may hold the very same workspace now.
        if !self.repo.owns_lease(job_id, owner).unwrap_or(false) {
            return Err(self.lost(job_id, JobStage::Deliver, &job_temp).await);
        }
        self.forget_waits(job_id);
        let err_msg = format!("The parked video is missing or changed; running the job again: {why}");
        error!("Job #{job_id}: {err_msg}");
        let _ = self.repo.record_event(job_id, "ERROR", Some(JobStage::Deliver), &err_msg);
        let _ = self.repo.log_audit("ERROR", "DELIVERY", &format!("Job #{job_id}: {err_msg}"));
        WatchfolderDelivery::cleanup_job_temp_files(&job_temp).await;
        Ok(ReadyOutcome::Rerun)
    }

    /// The encoder slots (for tests and diagnostics): a deliver-only job
    /// must never wait on them.
    pub fn encoder_slots(&self) -> Arc<Semaphore> {
        self.encoder_slots.clone()
    }

    /// Remove job `job_id`'s held directory (best effort; the sweep retries).
    pub fn discard_held_dir(&self, job_id: i64) {
        omni_core::held::remove_held_dir(&self.temp_dir, job_id);
    }

    /// `temp/held/{id}` for this engine's temp directory.
    fn held_dir(&self, job_id: i64) -> PathBuf {
        omni_core::held::held_dir(&self.temp_dir, job_id)
    }

    /// The `Hold` end of the pipeline: the verified file leaves the job's
    /// workspace for `temp/held/{id}/{slug}.mxf`, its SHA-256 is recorded,
    /// and the job's workspace is removed. Never touches the watchfolder.
    async fn hold_final(
        &self,
        job_id: i64,
        owner: &str,
        final_mxf: &Path,
        slug: &str,
        duration: f64,
        hold_hours: i64,
        held_max_bytes: u64,
        job_temp: &Path,
        cancel: Option<&CancellationToken>,
    ) -> Result<()> {
        if !self.repo.owns_lease(job_id, owner).unwrap_or(false) {
            return Err(self.lost(job_id, JobStage::Verify, job_temp).await);
        }
        let bytes = tokio::fs::metadata(final_mxf)
            .await
            .with_context(|| format!("Could not read the size of the file to hold: {final_mxf:?}"))
            .context(ErrorCode::PipelineFailed.as_str())?
            .len();
        let in_use = self
            .repo
            .held_bytes_in_use(job_id)
            .context("Could not read what is already held for approval")
            .context(ErrorCode::PipelineFailed.as_str())?;
        if in_use.saturating_add(bytes) > held_max_bytes {
            let gb = |b: u64| b as f64 / (1024.0 * 1024.0 * 1024.0);
            let why = format!(
                "Not enough room to hold the video for approval: {:.1} GB already held, this one is {:.1} GB, the limit is {:.1} GB",
                gb(in_use),
                gb(bytes),
                gb(held_max_bytes)
            );
            warn!("Job #{job_id}: {why}");
            let _ = self.repo.record_event(job_id, "WARN", Some(JobStage::Verify), &why);
            WatchfolderDelivery::cleanup_job_temp_files(job_temp).await;
            return Err(anyhow::anyhow!("{}: {why}", ErrorCode::LowDisk.as_str()).context(ErrorCode::LowDisk.as_str()));
        }

        // A fresh directory: an older hold of this job (retried since) goes.
        let dir = self.held_dir(job_id);
        if dir.exists() {
            tokio::fs::remove_dir_all(&dir)
                .await
                .with_context(|| format!("Could not clear the old held directory {dir:?}"))
                .context(ErrorCode::PipelineFailed.as_str())?;
        }
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("Could not create the held directory {dir:?}"))
            .context(ErrorCode::PipelineFailed.as_str())?;
        let held_path = dir.join(format!("{slug}.mxf"));
        if tokio::fs::rename(final_mxf, &held_path).await.is_err() {
            // Another volume: copy, then remove the original.
            let copied = tokio::fs::copy(final_mxf, &held_path).await;
            if let Err(e) = copied {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                return Err(anyhow::Error::new(e)
                    .context(format!("Could not move the video to {held_path:?}"))
                    .context(ErrorCode::PipelineFailed.as_str()));
            }
            let _ = tokio::fs::remove_file(final_mxf).await;
        }

        let hash_path = held_path.clone();
        let hashed = tokio::task::spawn_blocking(move || omni_core::held::sha256_file(&hash_path)).await;
        let sha256 = match hashed {
            Ok(Ok(h)) => h,
            Ok(Err(e)) => {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                return Err(anyhow::Error::new(e)
                    .context("Could not fingerprint the held video")
                    .context(ErrorCode::PipelineFailed.as_str()));
            }
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                return Err(anyhow::Error::new(e)
                    .context("Could not fingerprint the held video")
                    .context(ErrorCode::PipelineFailed.as_str()));
            }
        };

        let expires_at = chrono::Utc::now() + chrono::Duration::hours(hold_hours.max(1));
        let recorded = self.repo.hold_output(
            job_id,
            owner,
            &held_path.to_string_lossy(),
            bytes as i64,
            &sha256,
            Some(duration),
            expires_at,
        );
        match recorded {
            Ok(true) => {}
            Ok(false) => {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                return Err(self.lost(job_id, JobStage::Verify, job_temp).await);
            }
            Err(e) => {
                let _ = tokio::fs::remove_dir_all(&dir).await;
                return Err(e.context("Could not record the held video").context(ErrorCode::PipelineFailed.as_str()));
            }
        }

        // A look at exactly the held file for MCR; best effort, never fails the hold.
        let token = cancel.cloned().unwrap_or_default();
        let encoder = self.preview_encoder().await;
        let made = preview::make(&self.ffmpeg_path, &held_path, &dir, encoder, duration, &token).await;
        if !made.has_preview() {
            let why = made.why.as_deref().unwrap_or("άγνωστος λόγος");
            let _ = self.repo.record_event(
                job_id,
                "WARN",
                Some(JobStage::Verify),
                &format!("Δεν έγινε προεπισκόπηση: {why}"),
            );
        } else {
            let _ = self.repo.record_event(job_id, "INFO", Some(JobStage::Verify), "Προεπισκόπηση έτοιμη");
        }

        WatchfolderDelivery::cleanup_job_temp_files(job_temp).await;
        let _ = self.repo.record_event(
            job_id,
            "INFO",
            Some(JobStage::Verify),
            "Το βίντεο περιμένει έγκριση από το MCR",
        );
        info!("Job #{job_id} ({slug}) is held for approval ({bytes} bytes, sha256 {sha256})");
        Ok(())
    }

    /// Deliver the held file MCR approved, and nothing else (P10.5). The
    /// file passes the compliance gate again; then, at the last moment, it
    /// is opened so that nobody can change or delete it, hashed through that
    /// very handle against the SHA-256 recorded when it was held, and copied
    /// by the same [`WatchfolderDelivery::deliver`] as any other video while
    /// the handle is still open. Takes no download slot and no encoder slot:
    /// nothing is downloaded or encoded.
    ///
    /// The file's path in the watchfolder is recorded on the job and on the
    /// held row together right after the copy; a run that finds both equal
    /// (a crash before the worker finished the job) copies nothing again.
    /// The held row stays `approved` and the held directory stays: the
    /// worker holding the lease finishes the job and the row together
    /// (`Repository::finish_held_delivery`) and then calls
    /// [`Self::discard_held_dir`]. The engine sets no terminal status.
    pub async fn deliver_held(
        &self,
        job: &Job,
        owner: &str,
        held: &HeldOutput,
        cancel: Option<CancellationToken>,
    ) -> Result<()> {
        // Before anything is copied: a staging file of this slug older than this is a cut copy.
        let run_started = std::time::SystemTime::now();
        let job_id = job.id;
        let job_temp = self.temp_dir.join("jobs").join(job_id.to_string());
        // A deliver-only job has no use for the slot it was leased under.
        self.release_download_slot(job_id, owner);

        // Delivered by an earlier run of this approval, then the worker died.
        if let (Some(done), Some(at)) = (held.delivered_path.as_deref(), job.file_path.as_deref()) {
            if !done.is_empty() && done == at {
                info!("Job #{job_id}: the approved video is already at {at}; nothing to copy");
                let _ = self.repo.record_event(
                    job_id,
                    "INFO",
                    Some(JobStage::Deliver),
                    &format!("Already in the watchfolder at {at}; not copied again"),
                );
                return Ok(());
            }
        }

        // The approved video waited for the watchfolder (P11.2a): did the
        // interrupted delivery land after all? A share that cannot say waits again.
        if job.delivery_wait {
            if self.is_stopping() {
                return Err(self.stopping_wait());
            }
            if let Some(path) = self.earlier_rename(job_id, &job.slug).await? {
                match self.repo.record_held_delivery(job_id, owner, &path, held.duration.unwrap_or(0.0)) {
                    Ok(true) => {}
                    Ok(false) => return Err(self.lost(job_id, JobStage::Deliver, &job_temp).await),
                    Err(e) => return Err(e.context("Could not record the delivery").context(ErrorCode::PipelineFailed.as_str())),
                }
                self.watchfolder_state.set(true);
                self.forget_waits(job_id);
                info!("Job #{job_id}: the approved video is already in the watchfolder; not copied again");
                let _ = self.repo.record_event(
                    job_id,
                    "INFO",
                    Some(JobStage::Deliver),
                    "Already in the watchfolder from the interrupted delivery; not copied again",
                );
                return Ok(());
            }
        }

        self.enter_stage(job_id, owner, JobStage::Verify, &job_temp).await?;
        let path = PathBuf::from(&held.path);
        // The file is the one recorded for this job, in its own directory.
        let slug = match path.file_stem().and_then(|s| s.to_str()) {
            Some(stem) if path.starts_with(self.held_dir(job_id)) => stem.to_string(),
            _ => return Err(self.held_changed(job_id, &format!("the held path {path:?} is not in this job's held directory"))),
        };
        // Gone or a different size: no need to run the checks below.
        match tokio::fs::metadata(&path).await {
            Ok(m) if m.is_file() && m.len() as i64 == held.bytes => {}
            Ok(_) => return Err(self.held_changed(job_id, "its size is not the one that was approved")),
            Err(e) => return Err(self.held_changed(job_id, &format!("it cannot be read: {e}"))),
        }
        if is_cancelled(&cancel) {
            return Err(self.lost(job_id, JobStage::Verify, &job_temp).await);
        }

        let duration = held.duration.unwrap_or(0.0);
        let report = match crate::verify::verify_mxf_with_clip(&self.ffprobe_path, &path, duration, Some(&slug)).await {
            Ok(report) => report,
            Err(e) => {
                let err_msg = format!("Compliance check could not run: {e}");
                error!("Job #{job_id}: {err_msg}");
                let _ = self.repo.record_event(job_id, "ERROR", Some(JobStage::Verify), &err_msg);
                return Err(e.context("COMPLIANCE_FAILED"));
            }
        };
        if let Ok(json) = serde_json::to_string(&report) {
            let _ = self.repo.set_compliance_report(job_id, &json);
        }
        if !report.pass {
            let summary = report.summary();
            error!("Job #{job_id}: {summary}");
            let _ = self.repo.record_event(job_id, "ERROR", Some(JobStage::Verify), &summary);
            let _ = self.repo.log_audit(
                "ERROR",
                "COMPLIANCE",
                &format!("Job #{job_id} ({slug}) failed the compliance gate before delivery: {summary}"),
            );
            return Err(anyhow::anyhow!("COMPLIANCE_FAILED: {summary}"));
        }
        let _ = self.repo.record_event(job_id, "INFO", Some(JobStage::Verify), &report.summary());

        self.enter_stage(job_id, owner, JobStage::Deliver, &job_temp).await?;

        // Hash right before the copy, through a handle nobody can write to
        // or delete from, and keep it open until the copy is done.
        let lock_path = path.clone();
        let hashed = tokio::task::spawn_blocking(move || {
            let file = omni_core::held::open_locked(&lock_path)?;
            let sha = omni_core::held::sha256_reader(&file)?;
            // Hashing moved the position; the handle only guards the file.
            Ok::<_, std::io::Error>((sha, file))
        })
        .await;
        let (sha, guard) = match hashed {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => return Err(self.held_changed(job_id, &format!("it cannot be read: {e}"))),
            Err(e) => return Err(anyhow::Error::new(e).context(ErrorCode::PipelineFailed.as_str())),
        };
        if !sha.eq_ignore_ascii_case(&held.sha256) {
            return Err(self.held_changed(job_id, "its SHA-256 is not the one that was approved"));
        }
        // An approved video that waited, and a share that answers but has no room for it (P11.2a).
        if job.delivery_wait {
            if let Err(e) = self.room_in_watchfolder(job_id, &slug, held.bytes.max(0) as u64).await {
                drop(guard);
                return Err(e);
            }
            self.reclaim_own_staging(job_id, &slug, run_started).await;
        }

        let delivered = match self.deliver_to_watchfolder(job_id, owner, &path, &slug, &job_temp).await {
            Ok(dest) => dest,
            Err(e) if e.downcast_ref::<LeaseLost>().is_some() => {
                drop(guard);
                return Err(e);
            }
            // The share does not answer (P11.2a): the approved file waits where it
            // is, bound to the hash it was approved with.
            Err(e) if unreachable_cause(&e).is_some() => {
                drop(guard);
                let known = Some((held.bytes.max(0) as u64, held.sha256.clone()));
                return Err(self.park(job_id, owner, &path, known, duration, &slug, &job_temp, e, !job.delivery_wait).await);
            }
            Err(e) => {
                drop(guard);
                let err_msg = format!("Watchfolder delivery failed: {e:#}");
                error!("Job #{job_id}: {err_msg}");
                let _ = self.repo.record_event(job_id, "ERROR", None, &err_msg);
                let _ = self.repo.log_audit("ERROR", "DELIVERY", &format!("Job #{job_id} ({slug}): {err_msg}"));
                return Err(e);
            }
        };
        drop(guard);
        self.watchfolder_state.set(true);
        self.forget_waits(job_id);
        if delivered.suffixed {
            let _ = self.repo.record_event(
                job_id,
                "WARN",
                Some(JobStage::Deliver),
                &format!("Slug collision: delivered as {} because {slug}.mxf already existed", delivered.filename),
            );
        }

        let dest_str = delivered.path.to_string_lossy().to_string();
        let _ = self.repo.update_job_progress(job_id, 100.0, "Completed", "00:00");
        match self.repo.record_held_delivery(job_id, owner, &dest_str, duration) {
            Ok(true) => {}
            Ok(false) => {
                // On air but not ours to record. The row stays approved and
                // the held file stays; nothing is marked delivered.
                warn!("Job #{job_id}: lease lost after delivery; the file {dest_str} is in the watchfolder");
                return Err(self.lost(job_id, JobStage::Deliver, &job_temp).await);
            }
            Err(e) => {
                warn!("Job #{job_id}: could not record the delivered file {dest_str}: {e:#}");
                return Err(e.context("Could not record the delivery").context(ErrorCode::PipelineFailed.as_str()));
            }
        }
        let _ = self.repo.log_audit(
            "SUCCESS",
            "INGEST",
            &format!("Job #{job_id} ({slug}) approved by MCR and ready in watchfolder at {dest_str:?}"),
        );
        info!("Job #{job_id} ({slug}) delivered after approval");
        Ok(())
    }

    /// After delivery: move the job's source into the archive with its
    /// sidecar (when the archive is on and this worker still holds the lease),
    /// then remove the job's workspace. Nothing here can fail or undo the
    /// delivery: a failed archive is a warning and an `ARCHIVE_FAILED` event,
    /// and the cleanup runs regardless. On a lost lease nothing is written.
    pub async fn archive_then_clean(&self, job_id: i64, owner: &str, source: Option<&Path>, job_temp: &Path, delivered_filename: &str) {
        if let (Some(settings), Some(source)) = (self.archive.clone(), source) {
            match self.repo.set_stage(job_id, owner, JobStage::Archive) {
                Ok(true) => {
                    let outcome = match self.repo.get_job(job_id) {
                        Ok(Some(row)) => {
                            let (source, job_temp, name) = (source.to_path_buf(), job_temp.to_path_buf(), delivered_filename.to_string());
                            let day = chrono::Local::now().date_naive();
                            tokio::task::spawn_blocking(move || {
                                omni_core::archive::archive_source(&settings.root, &row, &source, &job_temp, &name, day)
                            })
                            .await
                            .map_err(anyhow::Error::from)
                            .and_then(|r| r)
                        }
                        Ok(None) => Err(anyhow::anyhow!("the job row is gone")),
                        Err(e) => Err(e),
                    };
                    match outcome {
                        Ok(dest) => {
                            info!("Job #{job_id}: source archived at {dest:?}");
                            let _ = self.repo.record_event(job_id, "INFO", Some(JobStage::Archive), "Source archived");
                        }
                        Err(e) => {
                            warn!("Job #{job_id}: could not archive the source: {e:#}");
                            let _ = self.repo.record_event(
                                job_id,
                                "WARN",
                                Some(JobStage::Archive),
                                &format!("ARCHIVE_FAILED: {e:#}"),
                            );
                        }
                    }
                }
                Ok(false) => warn!("Job #{job_id}: lease lost after delivery; the source is not archived"),
                Err(e) => warn!("Job #{job_id}: could not enter the archive stage, the source is not archived: {e:#}"),
            }
        }
        WatchfolderDelivery::cleanup_job_temp_files(job_temp).await;
    }

    /// The held file is not what was approved: stop, deliver nothing.
    fn held_changed(&self, job_id: i64, why: &str) -> anyhow::Error {
        let err_msg = format!("The video that waited for approval changed or is gone, and was not delivered: {why}");
        error!("Job #{job_id}: {err_msg}");
        let _ = self.repo.record_event(job_id, "ERROR", Some(JobStage::Verify), &err_msg);
        let _ = self.repo.log_audit("ERROR", "DELIVERY", &format!("Job #{job_id}: {err_msg}"));
        anyhow::anyhow!("{}: {err_msg}", ErrorCode::HeldFileChanged.as_str()).context(ErrorCode::HeldFileChanged.as_str())
    }
}

#[cfg(test)]
mod stopping_tests {
    use super::*;
    use omni_core::models::NewJob;
    use omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS;

    /// P2.15: once the daemon is stopping, a job that finishes encoding does
    /// not start its watchfolder copy (a stop would cut it and leave a stale
    /// `.tmp` that pushes the re-run to `_2`); earlier stages go on as ever.
    #[tokio::test]
    async fn a_stopping_engine_starts_no_new_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let engine = BroadcastEngine::new(
            repo.clone(),
            dir.path().join("yt-dlp.exe"),
            dir.path().join("ffmpeg.exe"),
            dir.path().join("ffprobe.exe"),
            dir.path().join("bmxtranswrap.exe"),
            dir.path().join("temp"),
            dir.path().join("watchfolder"),
        );
        let id = repo
            .enqueue(&NewJob::new("https://example.org/v/1", "1_PAPADAKI_LIMANI", "PAPADAKI"), DEFAULT_DEDUP_WINDOW_HOURS)
            .unwrap()
            .job_id();
        repo.lease_job("w1", 180).unwrap().expect("leasable");
        let job_temp = dir.path().join("temp").join(id.to_string());

        // Earlier stages go on whether or not the engine is stopping.
        let shared = engine.clone();
        engine.enter_stage(id, "w1", JobStage::Verify, &job_temp).await.unwrap();
        shared.stop_new_deliveries(); // a clone shares the flag, as the workers' Arc does
        engine.enter_stage(id, "w1", JobStage::Verify, &job_temp).await.unwrap();
        let blocked = tokio::time::timeout(
            Duration::from_millis(300),
            engine.enter_stage(id, "w1", JobStage::Deliver, &job_temp),
        )
        .await;
        assert!(blocked.is_err(), "a stopping engine must not enter DELIVER");
        assert_eq!(repo.get_job(id).unwrap().unwrap().stage, JobStage::Verify, "recovery sees the job before delivery");

        // Not stopping: DELIVER is entered at once.
        let fresh = BroadcastEngine::new(
            repo.clone(),
            dir.path().join("yt-dlp.exe"),
            dir.path().join("ffmpeg.exe"),
            dir.path().join("ffprobe.exe"),
            dir.path().join("bmxtranswrap.exe"),
            dir.path().join("temp"),
            dir.path().join("watchfolder"),
        );
        fresh.enter_stage(id, "w1", JobStage::Deliver, &job_temp).await.unwrap();
        assert_eq!(repo.get_job(id).unwrap().unwrap().stage, JobStage::Deliver);
    }

    /// P1.22: the engine writes the delivery marker right before the rename, and a job
    /// that is no longer ours at that moment delivers nothing.
    fn marker_rig() -> (tempfile::TempDir, Repository, BroadcastEngine, i64, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::new(dir.path().join("omni.db")).unwrap();
        let watchfolder = dir.path().join("watchfolder");
        let engine = BroadcastEngine::new(
            repo.clone(),
            dir.path().join("yt-dlp.exe"),
            dir.path().join("ffmpeg.exe"),
            dir.path().join("ffprobe.exe"),
            dir.path().join("bmxtranswrap.exe"),
            dir.path().join("temp"),
            watchfolder.clone(),
        );
        let id = repo
            .enqueue(&NewJob::new("https://example.org/v/2", "2_PAPADAKI_LIMANI", "PAPADAKI"), DEFAULT_DEDUP_WINDOW_HOURS)
            .unwrap()
            .job_id();
        repo.lease_job("host:1:0", 180).unwrap().expect("leasable");
        let source = dir.path().join("out.mxf");
        std::fs::write(&source, b"A_FINISHED_MXF").unwrap();
        (dir, repo, engine, id, source, watchfolder)
    }

    fn listing(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default()
    }

    #[tokio::test]
    async fn a_job_that_lost_its_lease_before_the_rename_leaves_nothing_in_the_watchfolder() {
        let (dir, repo, engine, id, source, watchfolder) = marker_rig();
        let job_temp = dir.path().join("temp").join("jobs").join(id.to_string());
        // The lease is gone by the time the copy is done (a worker whose lease was
        // reaped and taken over); here finish() just ends this one.
        assert!(repo.finish(id, "host:1:0", omni_core::models::JobStatus::Failed, Some("X"), Some("gone"), None).unwrap());
        let before = repo.get_job(id).unwrap().unwrap();

        let err = engine
            .deliver_to_watchfolder(id, "host:1:0", &source, "2_PAPADAKI_LIMANI", &job_temp)
            .await
            .expect_err("a job that is not ours delivers nothing");

        assert!(err.downcast_ref::<LeaseLost>().is_some(), "{err:#}");
        assert!(listing(&watchfolder).is_empty(), "left in the watchfolder: {:?}", listing(&watchfolder));
        let after = repo.get_job(id).unwrap().unwrap();
        assert_eq!(
            (after.status, after.stage, after.file_path, after.lease_owner),
            (before.status, before.stage, before.file_path, before.lease_owner),
            "the row changed"
        );
    }

    #[tokio::test]
    async fn a_delivery_records_where_it_is_going_and_a_crash_before_finish_is_recovered_as_delivered() {
        let (dir, repo, engine, id, source, watchfolder) = marker_rig();
        let job_temp = dir.path().join("temp").join("jobs").join(id.to_string());

        let delivered = engine
            .deliver_to_watchfolder(id, "host:1:0", &source, "2_PAPADAKI_LIMANI", &job_temp)
            .await
            .expect("delivers");
        assert_eq!(listing(&watchfolder), vec!["2_PAPADAKI_LIMANI.mxf".to_string()]);

        // The process dies here: no set_delivered_file, no finish. Only the marker
        // the engine wrote (path and size) can tell recovery the video is on air.
        assert_eq!(repo.recover_on_startup("host").unwrap(), 0, "the delivered video was requeued");
        let job = repo.get_job(id).unwrap().unwrap();
        assert_eq!(job.status, omni_core::models::JobStatus::Completed);
        assert_eq!(job.file_path.as_deref(), Some(delivered.path.to_string_lossy().as_ref()));
        assert_eq!(listing(&watchfolder).len(), 1);
    }
}
