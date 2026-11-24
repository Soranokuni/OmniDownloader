//! A finished video whose watchfolder cannot be written waits for it (P11.2a),
//! through `run_job` and the real engine. Needs ffmpeg, ffprobe and
//! bmxtranswrap in `bin/`; skipped with a visible warning without them.

use super::*;
use std::path::{Path, PathBuf};

use omni_core::models::{Job, NewJob};

const OWNER: &str = "host:1:1";
const SLUG: &str = "1_ELENI_LIMANI";

fn tool(name: &str) -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("bin").join(format!("{name}.exe"));
    p.exists().then_some(p)
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("omni-parked-test-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Rig {
    _dir: Scratch,
    root: PathBuf,
    repo: Repository,
    engine: Arc<BroadcastEngine>,
    ffmpeg: PathBuf,
    /// The engine's watchfolder.
    watch: PathBuf,
}

/// The real tools; the watchfolder is `root/share/watch`. `blocked`: `root/share`
/// is a file, so the watchfolder cannot be created (a share that is down).
fn rig(blocked: bool, archive: bool) -> Option<Rig> {
    let (Some(ffmpeg), Some(ffprobe), Some(bmx)) = (tool("ffmpeg"), tool("ffprobe"), tool("bmxtranswrap")) else {
        eprintln!("WARNING: skipping the parked-delivery test -- ffmpeg/ffprobe/bmxtranswrap not in bin/");
        return None;
    };
    let dir = Scratch::new();
    let root = dir.0.clone();
    let repo = Repository::new(root.join("omni.db")).unwrap();
    if blocked {
        std::fs::write(root.join("share"), b"not a directory").unwrap();
    }
    let watch = root.join("share").join("watch");
    let engine = Arc::new(
        BroadcastEngine::new(repo.clone(), root.join("no-yt-dlp.exe"), ffmpeg.clone(), ffprobe, bmx, root.join("temp"), watch.clone())
            .with_archive(archive.then(|| omni_core::archive::ArchiveSettings::new(root.join("archive"), 14))),
    );
    Some(Rig { _dir: dir, root, repo, engine, ffmpeg, watch })
}

impl Rig {
    /// The share answers again.
    fn unblock(&self) {
        let share = self.root.join("share");
        if share.is_file() {
            std::fs::remove_file(share).unwrap();
        }
    }

    /// The 30 s wait is over.
    fn end_backoff(&self, id: i64) {
        rusqlite::Connection::open(self.root.join("omni.db"))
            .unwrap()
            .execute("UPDATE queue SET not_before = NULL WHERE id = ?1", rusqlite::params![id])
            .unwrap();
    }

    fn events(&self, id: i64) -> Vec<String> {
        self.repo.get_job_events(id, 500).unwrap().into_iter().map(|e| e.message).collect()
    }

    fn workspace(&self, id: i64) -> PathBuf {
        self.root.join("temp").join("jobs").join(id.to_string())
    }

    fn job(&self, id: i64) -> Job {
        self.repo.get_job(id).unwrap().unwrap()
    }

    fn watch_files(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.watch)
            .map(|d| d.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    /// An attachment job with a real 2 s source, queued (not leased).
    async fn queue(&self) -> i64 {
        let source = self.root.join("attachments").join("1").join("source.mp4");
        std::fs::create_dir_all(source.parent().unwrap()).unwrap();
        let args: Vec<String> = [
            "-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=25:duration=2", "-f", "lavfi", "-i",
            "sine=frequency=1000:sample_rate=48000:duration=2", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac",
            "-shortest",
        ]
        .iter()
        .map(|s| s.to_string())
        .chain([source.to_string_lossy().into_owned()])
        .collect();
        let made = omni_core::process::run(&self.ffmpeg, &args, omni_core::process::RunOpts::new(Duration::from_secs(120)))
            .await
            .unwrap();
        assert!(made.success, "could not build the synthetic attachment");
        let mut new = NewJob::new("attachment://m1@example.gr/1", SLUG, "ELENI");
        new.keyword = "LIMANI".into();
        new.status = JobStatus::ManualDownload;
        new.extraction_method = Some("attachment".into());
        let id = self.repo.enqueue(&new, omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();
        assert!(self.repo.attach_source(id, &source.to_string_lossy()).unwrap());
        id
    }

    /// `run_job` as the worker loop runs it, repair off.
    async fn work(&self, job: Job) {
        let ctx = crate::repair::RepairCtx {
            repo: self.repo.clone(),
            cfg: crate::repair::RepairCfg {
                mode: RepairMode::Off,
                live: None,
                budget: Duration::from_secs(5),
                hold_hours: 24,
                held_max_bytes: 20 << 30,
                auto_deliver_after_approvals: 5,
                llm: crate::repair::LlmCfg::default(),
                egress_allow: Default::default(),
            },
            env: Arc::new(crate::repair::RealEnv::new(
                self.engine.clone(),
                self.root.join("no-ffprobe.exe"),
                self.root.join("no-yt-dlp.exe"),
                self.ffmpeg.clone(),
            )),
        };
        let health = HealthState::new();
        let canary = Arc::new(crate::canary::CanaryEnv::new(self.repo.clone(), health.clone(), self.root.join("bin"), "stable".into()));
        run_job(&self.repo, &self.engine, OWNER, job, CancellationToken::new(), &self.root.join("evidence"), &ctx, &health, &canary)
            .await
            .unwrap();
    }
}

/// Test 7 (pinned before P11.2a changed anything): a delivery the watchfolder
/// code refuses by itself (999 files with the slug) is not a share that is
/// down. It goes to review with PIPELINE_FAILED and its workspace removed,
/// exactly as before, and never waits.
#[tokio::test]
async fn a_refused_delivery_still_goes_to_review_and_never_waits() {
    let Some(r) = rig(false, false) else { return };
    std::fs::create_dir_all(&r.watch).unwrap();
    for n in 1..=999u32 {
        let name = if n == 1 { format!("{SLUG}.mxf") } else { format!("{SLUG}_{n}.mxf") };
        std::fs::write(r.watch.join(name), b"already on air").unwrap();
    }
    let id = r.queue().await;
    let job = r.repo.lease_job(OWNER, 600).unwrap().expect("leased");
    r.work(job).await;

    let job = r.job(id);
    assert_eq!(job.status, JobStatus::RequiresReview);
    assert_eq!(job.error_code.as_deref(), Some(ErrorCode::PipelineFailed.as_str()));
    assert_eq!(job.attempts, 1);
    assert!(!r.workspace(id).exists(), "the workspace of a refused delivery is removed");
    assert_eq!(r.watch_files().len(), 999, "nothing was added to or removed from the watchfolder");
}

/// What the waiting job's row and workspace must say after a run that found
/// the share down: PENDING (never review), DELIVERY_FAILED, waiting ~30 s,
/// `attempts` as before the run, the verified MXF on disk with its size and
/// SHA-256, nothing in the watchfolder.
fn assert_waiting(r: &Rig, id: i64, attempts_before: i32, backoff_secs: i64) {
    let job = r.job(id);
    assert_eq!(job.status, JobStatus::Pending, "not waiting: {:?} {:?}", job.error_code, job.error_message);
    assert!(job.delivery_wait);
    assert_eq!(job.attempts, attempts_before, "waiting for the share spent an attempt");
    assert_eq!(job.error_code.as_deref(), Some(ErrorCode::DeliveryFailed.as_str()));
    assert!(
        job.error_message.as_deref().is_some_and(|m| m.starts_with("Watchfolder not reachable; the finished video waits")),
        "{:?}",
        job.error_message
    );
    assert_eq!(job.lease_owner, None);
    let wait = job.not_before.expect("a back-off") - chrono::Utc::now();
    assert!(
        wait > chrono::Duration::seconds(backoff_secs - 10) && wait <= chrono::Duration::seconds(backoff_secs + 1),
        "expected about {backoff_secs} s, got {wait:?}"
    );
    let ready = PathBuf::from(job.ready_path.clone().expect("the waiting file"));
    assert!(ready.starts_with(r.workspace(id)), "{ready:?}");
    assert_eq!(job.ready_bytes, Some(std::fs::metadata(&ready).unwrap().len() as i64));
    assert_eq!(job.ready_sha256.as_deref(), Some(omni_core::held::sha256_file(&ready).unwrap().as_str()));
    assert!(r.watch_files().is_empty(), "something reached the watchfolder: {:?}", r.watch_files());
}

/// Test 2 and test 4 (archive on): the share is down at DELIVER, the verified
/// video waits (five failed tries later still waiting, attempts unchanged);
/// the share comes back and the very same bytes are delivered once.
#[tokio::test]
async fn an_unreachable_watchfolder_parks_the_video_and_delivers_it_once_when_it_answers() {
    let Some(r) = rig(true, true) else { return };
    let id = r.queue().await;
    let before = r.job(id).attempts;
    let job = r.repo.lease_job(OWNER, 600).unwrap().expect("leased");
    r.work(job).await;
    assert_waiting(&r, id, before, 30);
    let waits = |r: &Rig| r.events(id).iter().filter(|e| e.starts_with("Watchfolder not reachable; the finished video waits")).count();
    assert_eq!(waits(&r), 1, "the first park says why");
    let ready = PathBuf::from(r.job(id).ready_path.unwrap());
    let verified = std::fs::read(&ready).unwrap();

    // B2: 30 s after the first wait, then 60 s, 2 min, and 5 min for every later one.
    // B3: the same cause again writes no WARN line and no audit row; only each lease's own line.
    let events_before = r.repo.get_job_events(id, 1000).unwrap().len();
    let audits_before = r.repo.get_recent_audit_logs(1000).unwrap().len();
    for (n, backoff) in [60, 120, 300, 300, 300].into_iter().enumerate() {
        r.end_backoff(id);
        assert!(r.repo.lease_job_when(OWNER, 600, false).unwrap().is_none(), "leased while the share is down");
        let job = r.repo.lease_job_when(OWNER, 600, true).unwrap().expect("the waiting job");
        assert_eq!((job.id, job.stage), (id, JobStage::Deliver), "try {n}");
        r.work(job).await;
        assert_waiting(&r, id, before, backoff);
    }
    let all = r.repo.get_job_events(id, 1000).unwrap();
    let added: Vec<&str> = all[events_before..].iter().map(|e| e.message.as_str()).collect();
    // Before B3 these five tries wrote 20 rows (lease, stage, WARN wait, requeue line,
    // each try) and 5 audit rows; now only each run's own two lines.
    assert_eq!(added.len(), 10, "five tries of the same wait wrote: {added:?}");
    assert!(added.iter().all(|m| m.starts_with("Leased by") || *m == "Stage DELIVER"), "{added:?}");
    assert_eq!(r.repo.get_recent_audit_logs(1000).unwrap().len(), audits_before, "an audit row for a repeated wait");
    assert_eq!(waits(&r), 1);
    assert_eq!(std::fs::read(&ready).unwrap(), verified);

    r.unblock();
    r.end_backoff(id);
    let job = r.repo.lease_job_when(OWNER, 600, true).unwrap().expect("the waiting job");
    r.work(job).await;

    let job = r.job(id);
    assert_eq!(job.status, JobStatus::Completed);
    assert!(!job.delivery_wait);
    assert_eq!((job.ready_path, job.ready_bytes, job.ready_sha256), (None, None, None));
    assert_eq!(r.watch_files(), vec![format!("{SLUG}.mxf")], "delivered exactly once");
    assert_eq!(std::fs::read(r.watch.join(format!("{SLUG}.mxf"))).unwrap(), verified, "not the bytes that were verified");
    assert_eq!(job.file_path.as_deref(), Some(r.watch.join(format!("{SLUG}.mxf")).to_string_lossy().as_ref()));
    assert!(!r.workspace(id).exists(), "the workspace is removed after the delivery");
    let archived = walk(&r.root.join("archive"));
    assert!(
        archived.iter().any(|n| n.starts_with(&format!("{id}_{SLUG}")) && n.ends_with(".mp4")),
        "the source is archived: {archived:?}"
    );
}

/// Test 4 (archive off): the same, and nothing is archived.
#[tokio::test]
async fn with_the_archive_off_the_waiting_video_is_delivered_and_nothing_archived() {
    let Some(r) = rig(true, false) else { return };
    let id = r.queue().await;
    let job = r.repo.lease_job(OWNER, 600).unwrap().expect("leased");
    r.work(job).await;
    assert_waiting(&r, id, 0, 30);
    // Finding 1: its clip is named; MCR's rename answers TOO_LATE (as the route reads `None`).
    assert!(r.repo.rename_job_keyword(id, "ALLO").unwrap().is_none(), "a waiting video was renamed");

    r.unblock();
    r.end_backoff(id);
    let job = r.repo.lease_job_when(OWNER, 600, true).unwrap().expect("the waiting job");
    r.work(job).await;
    assert_eq!(r.job(id).status, JobStatus::Completed);
    assert_eq!(r.watch_files(), vec![format!("{SLUG}.mxf")]);
    assert!(!r.workspace(id).exists());
    assert!(!r.root.join("archive").exists());
    // The file is named as its clip: the material package name is the file's name.
    let delivered = r.watch.join(format!("{SLUG}.mxf"));
    let report = omni_broadcast::verify::verify_mxf_with_clip(&tool("ffprobe").unwrap(), &delivered, 2.0, Some(SLUG)).await.unwrap();
    let clip = report.checks.iter().find(|c| c.name == "material_package_name").expect("checked");
    assert!(clip.ok, "the file name and the clip name differ: {clip:?}");
}

/// Test 9: an approved held video and a share that is down: it waits (held
/// file kept, held row still approved, attempts unchanged), then is delivered
/// once when the share answers, by the held path's own end.
#[tokio::test]
async fn an_approved_held_video_waits_for_the_share_and_is_delivered_once() {
    let Some(r) = rig(true, false) else { return };
    let id = r.queue().await;
    let job = r.repo.lease_job(OWNER, 600).unwrap().expect("leased");
    let hold = omni_broadcast::pipeline::DeliveryMode::Hold { hold_hours: 24, held_max_bytes: 20 << 30 };
    let held = r.engine.process_job_mode(job, OWNER, None, None, None, None, hold).await.expect("held");
    assert_eq!(held, omni_broadcast::pipeline::RunOutcome::Held);
    assert!(finish_held(&r.repo, id, OWNER).unwrap());
    let row = r.repo.waiting_held(id).unwrap().expect("a held row");
    assert!(r.repo.approve_held(id, &row.sha256, "desk").unwrap());
    let original = std::fs::read(&row.path).unwrap();
    let before = r.job(id).attempts;

    let job = r.repo.lease_job(OWNER, 600).unwrap().expect("the approved job");
    r.work(job).await;

    let job = r.job(id);
    assert_eq!(job.status, JobStatus::Pending, "{:?} {:?}", job.error_code, job.error_message);
    assert!(job.delivery_wait);
    assert_eq!(job.attempts, before);
    assert_eq!(job.ready_path.as_deref(), Some(row.path.as_str()), "the held file waits where it is");
    assert_eq!(job.ready_sha256.as_deref(), Some(row.sha256.as_str()));
    assert_eq!(std::fs::read(&row.path).unwrap(), original, "the held file was touched");
    assert_eq!(r.repo.get_held(id).unwrap().unwrap().decision.as_deref(), Some("approved"));
    assert!(r.watch_files().is_empty());

    r.unblock();
    r.end_backoff(id);
    let job = r
        .repo
        .lease_approved_held_job_when(OWNER, 600, true)
        .unwrap()
        .expect("the waiting held job, no download slot needed");
    assert_eq!((job.id, job.stage, job.attempts), (id, JobStage::Deliver, before));
    r.work(job).await;

    let job = r.job(id);
    assert_eq!(job.status, JobStatus::Completed);
    assert!(!job.delivery_wait);
    assert_eq!(r.watch_files(), vec![format!("{SLUG}.mxf")]);
    assert_eq!(std::fs::read(r.watch.join(format!("{SLUG}.mxf"))).unwrap(), original);
    assert_eq!(r.repo.get_held(id).unwrap().unwrap().decision.as_deref(), Some("delivered"));
    assert!(!PathBuf::from(&row.path).exists(), "the held copy is removed once delivered");
}

/// Every file name under `dir`, recursively.
fn walk(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.extend(walk(&p));
            } else {
                out.push(e.file_name().to_string_lossy().into_owned());
            }
        }
    }
    out
}

// ---------------------------------------------------------------- no tools needed

/// A job leased by OWNER, parked and back in the queue, with its workspace.
fn parked_without_tools(repo: &Repository, root: &Path, n: u32) -> i64 {
    let new = NewJob::new(&format!("https://news.example.invalid/{n}"), &format!("{n}_ELENI_TEST"), "ELENI");
    let id = repo.enqueue(&new, omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();
    assert_eq!(repo.lease_job(OWNER, 600).unwrap().unwrap().id, id);
    let ws = root.join("temp").join("jobs").join(id.to_string());
    std::fs::create_dir_all(&ws).unwrap();
    let ready = ws.join(format!("{id}_output.mxf"));
    std::fs::write(&ready, b"verified").unwrap();
    assert!(repo.park_for_delivery(id, OWNER, &ready.to_string_lossy(), 8, "00", 1.0).unwrap());
    assert!(repo.requeue_waiting_delivery(id, OWNER, chrono::Duration::zero(), "waits").unwrap());
    id
}

/// Test 8: the start-up sweep keeps a waiting job's workspace and removes an
/// orphan; the nightly sweep spares it however old its row is.
#[tokio::test]
async fn the_sweeps_keep_the_workspace_of_a_waiting_video() {
    let dir = Scratch::new();
    let root = dir.0.clone();
    let repo = Repository::new(root.join("omni.db")).unwrap();
    let id = parked_without_tools(&repo, &root, 1);
    let jobs = root.join("temp").join("jobs");
    std::fs::create_dir_all(jobs.join("424242")).unwrap();
    std::fs::write(jobs.join("424242").join("source.mp4"), b"orphan").unwrap();

    assert_eq!(crate::startup_sweep(&repo, &jobs).await.unwrap(), 1);
    assert!(jobs.join(id.to_string()).join(format!("{id}_output.mxf")).exists(), "the waiting video was swept at start-up");
    assert!(!jobs.join("424242").exists(), "the orphan stayed");

    rusqlite::Connection::open(root.join("omni.db"))
        .unwrap()
        .execute("UPDATE queue SET updated_at = '2020-01-01T00:00:00Z' WHERE id = ?1", rusqlite::params![id])
        .unwrap();
    assert_eq!(crate::sweep_orphan_job_dirs(&repo, &jobs, 1).await, 0);
    assert!(jobs.join(id.to_string()).exists(), "the nightly sweep took a waiting video");

    // Control: once the wait is cancelled, the start-up sweep takes it.
    assert_eq!(repo.cancel_job(id).unwrap(), omni_core::repository::CancelOutcome::Cancelled);
    assert_eq!(crate::startup_sweep(&repo, &jobs).await.unwrap(), 1);
    assert!(!jobs.join(id.to_string()).exists());
}

/// Test 10 / risk (g): only a DELIVERY_FAILED run of a job that waits, by its
/// lease holder, is requeued to wait. A NETWORK failure (or any other code) on
/// the same job, or a DELIVERY_FAILED on a job that does not wait, goes to the
/// normal rules, which spend attempts and end in review.
#[test]
fn only_a_delivery_failure_of_a_waiting_job_waits() {
    let dir = Scratch::new();
    let root = dir.0.clone();
    let repo = Repository::new(root.join("omni.db")).unwrap();
    let engine = BroadcastEngine::new(repo.clone(), root.join("y"), root.join("f"), root.join("p"), root.join("b"), root.join("temp"), root.join("watch"));
    let id = parked_without_tools(&repo, &root, 1);
    assert_eq!(repo.lease_job_when(OWNER, 600, true).unwrap().unwrap().id, id);

    assert!(!wait_for_watchfolder(&repo, &engine, id, OWNER, ErrorCode::Network).unwrap());
    assert!(!wait_for_watchfolder(&repo, &engine, id, OWNER, ErrorCode::PipelineFailed).unwrap());
    assert!(!wait_for_watchfolder(&repo, &engine, id, "someone-else", ErrorCode::DeliveryFailed).unwrap());
    assert_eq!(repo.get_job(id).unwrap().unwrap().status, JobStatus::Running, "nothing was written");
    assert!(wait_for_watchfolder(&repo, &engine, id, OWNER, ErrorCode::DeliveryFailed).unwrap());
    let job = repo.get_job(id).unwrap().unwrap();
    assert_eq!((job.status, job.delivery_wait, job.attempts), (JobStatus::Pending, true, 0));

    let new = NewJob::new("https://news.example.invalid/plain", "2_ELENI_TEST", "ELENI");
    let plain = repo.enqueue(&new, omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();
    assert_eq!(repo.lease_job_when(OWNER, 600, false).unwrap().unwrap().id, plain);
    assert!(
        !wait_for_watchfolder(&repo, &engine, plain, OWNER, ErrorCode::DeliveryFailed).unwrap(),
        "a delivery failure that parked nothing keeps today's retry rule"
    );
    assert_eq!(repo.get_job(plain).unwrap().unwrap().status, JobStatus::Running);
}

/// A of the lead's P11.2a decisions: with the `disk` check red the worker loop
/// leases no ordinary job and no approved held video (they wait for room as
/// before), but a waiting delivery, which needs no temp space and frees some,
/// still goes, while the watchfolder answers.
#[test]
fn a_red_disk_check_leases_only_waiting_deliveries() {
    let dir = Scratch::new();
    let root = dir.0.clone();
    let repo = Repository::new(root.join("omni.db")).unwrap();
    let parked = parked_without_tools(&repo, &root, 1);
    let ordinary = repo
        .enqueue(&NewJob::new("https://news.example.invalid/ordinary", "2_ELENI_TEST", "ELENI"), omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS)
        .unwrap()
        .job_id();
    // An approved held video, back in the queue.
    let held = repo
        .enqueue(&NewJob::new("https://news.example.invalid/held", "3_ELENI_TEST", "ELENI"), omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS)
        .unwrap()
        .job_id();
    assert!(repo.lease_job_by_id(held, "host:1:9", 600).unwrap());
    let until = chrono::Utc::now() + chrono::Duration::hours(24);
    assert!(repo.hold_output(held, "host:1:9", "temp/held/3/x.mxf", 10, "00", Some(1.0), until).unwrap());
    assert!(finish_held(&repo, held, "host:1:9").unwrap());
    assert!(repo.approve_held(held, "00", "desk").unwrap());

    // Disk red, share down: nothing at all.
    assert!(crate::lease_now(&repo, OWNER, false, true, false).unwrap().is_none());
    // Disk red, share up: the waiting delivery, with or without a download slot.
    let got = crate::lease_now(&repo, OWNER, false, true, true).unwrap().expect("the waiting delivery");
    assert_eq!((got.id, got.stage), (parked, JobStage::Deliver));
    // ... and nothing else, however many download slots are free.
    assert!(crate::lease_now(&repo, OWNER, false, true, true).unwrap().is_none(), "an ordinary or held job was leased on a red disk");
    assert!(crate::lease_now(&repo, OWNER, false, false, true).unwrap().is_none());
    assert_eq!(repo.get_job(ordinary).unwrap().unwrap().status, JobStatus::Pending);
    assert_eq!(repo.get_job(held).unwrap().unwrap().status, JobStatus::Pending);

    // Room again: the others go as before (held without a slot, ordinary with one).
    assert_eq!(crate::lease_now(&repo, OWNER, true, false, true).unwrap().map(|j| j.id), Some(held));
    assert_eq!(crate::lease_now(&repo, OWNER, true, true, true).unwrap().map(|j| j.id), Some(ordinary));
}

/// A scratch database, an engine whose watchfolder is `root/watch` (no tools:
/// nothing on the waiting path runs one), and the repair context `run_job`
/// would pass.
fn toolless(dir: &Scratch) -> (Repository, Arc<BroadcastEngine>, crate::repair::RepairCtx) {
    let root = dir.0.clone();
    let repo = Repository::new(root.join("omni.db")).unwrap();
    std::fs::create_dir_all(root.join("watch")).unwrap();
    let p = |n: &str| root.join(n);
    let engine = Arc::new(BroadcastEngine::new(repo.clone(), p("y.exe"), p("f.exe"), p("p.exe"), p("b.exe"), p("temp"), p("watch")));
    let ctx = crate::repair::RepairCtx {
        repo: repo.clone(),
        cfg: crate::repair::RepairCfg {
            mode: RepairMode::Off,
            live: None,
            budget: Duration::from_secs(5),
            hold_hours: 24,
            held_max_bytes: 20 << 30,
            auto_deliver_after_approvals: 5,
            llm: crate::repair::LlmCfg::default(),
            egress_allow: Default::default(),
        },
        env: Arc::new(crate::repair::RealEnv::new(engine.clone(), p("p.exe"), p("y.exe"), p("f.exe"))),
    };
    (repo, engine, ctx)
}

fn watch_files_in(root: &Path) -> Vec<String> {
    std::fs::read_dir(root.join("watch")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect()
}

/// Finding 2 (lead decision): a rename was attempted (the marker is there),
/// the share dropped, and its file is not in the watchfolder now: MCR may have
/// taken it into Dalet by hand. The worker sends the job to review, copies
/// nothing, and keeps the workspace and the marker.
#[tokio::test]
async fn a_waiting_video_whose_attempted_rename_is_not_there_goes_to_review_not_to_air() {
    let dir = Scratch::new();
    let (repo, engine, ctx) = toolless(&dir);
    let root = dir.0.clone();
    let id = parked_without_tools(&repo, &root, 1);
    // What the parking run left: the marker of the rename it tried. (Re-leased to write it.)
    let ws = root.join("temp").join("jobs").join(id.to_string());
    let ready = ws.join(format!("{id}_output.mxf"));
    let sha = omni_core::held::sha256_file(&ready).unwrap();
    let job = repo.lease_job_when(OWNER, 600, true).unwrap().unwrap();
    let attempted = root.join("watch").join("1_ELENI_TEST.mxf");
    assert!(repo.mark_delivering(id, OWNER, &attempted.to_string_lossy(), 8).unwrap());
    assert!(repo.park_for_delivery(id, OWNER, &ready.to_string_lossy(), 8, &sha, 1.0).unwrap());
    assert!(repo.requeue_waiting_delivery(id, OWNER, chrono::Duration::zero(), "waits").unwrap());
    drop(job);
    let job = repo.lease_job_when(OWNER, 600, true).unwrap().unwrap();

    run_waiting_delivery(&repo, &engine, OWNER, &job, &CancellationToken::new(), &ctx).await.unwrap();

    assert!(watch_files_in(&root).is_empty(), "found a file in the watchfolder: {:?}", watch_files_in(&root));
    let job = repo.get_job(id).unwrap().unwrap();
    let msg = "The delivery of 1_ELENI_TEST.mxf was interrupted and the file is not in the watchfolder now. Check Dalet for 1_ELENI_TEST.mxf before retrying";
    assert_eq!((job.status, job.error_code.as_deref(), job.error_message.as_deref()), (JobStatus::RequiresReview, Some("DELIVERY_UNCONFIRMED"), Some(msg)));
    assert!(ready.exists(), "the workspace was removed");
    assert!(job.delivery_wait && job.ready_path.is_some());
    assert_eq!(repo.delivery_marker(id).unwrap().map(|m| m.0), Some(attempted.to_string_lossy().into_owned()), "the marker is kept");
    let events: Vec<String> = repo.get_job_events(id, 100).unwrap().into_iter().map(|e| e.message).collect();
    assert!(events.iter().any(|e| e == msg), "{events:?}");
    assert!(repo.get_recent_audit_logs(50).unwrap().iter().any(|a| a.level == "WARN" && a.category == "DELIVERY" && a.message.contains(msg)));
}

/// Finding 2 on the held path: the same rule for an approved held video that waited.
#[tokio::test]
async fn an_approved_held_video_whose_attempted_rename_is_not_there_goes_to_review_not_to_air() {
    let dir = Scratch::new();
    let (repo, engine, _ctx) = toolless(&dir);
    let root = dir.0.clone();
    let new = NewJob::new("https://news.example.invalid/held", "1_ELENI_TEST", "ELENI");
    let id = repo.enqueue(&new, omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();
    assert_eq!(repo.lease_job(OWNER, 600).unwrap().unwrap().id, id);
    let held_dir = root.join("temp").join("held").join(id.to_string());
    std::fs::create_dir_all(&held_dir).unwrap();
    let held_file = held_dir.join("1_ELENI_TEST.mxf");
    std::fs::write(&held_file, b"approved").unwrap();
    let sha = omni_core::held::sha256_file(&held_file).unwrap();
    let held_path = held_file.to_string_lossy().into_owned();
    let until = chrono::Utc::now() + chrono::Duration::hours(24);
    assert!(repo.hold_output(id, OWNER, &held_path, 8, &sha, Some(1.0), until).unwrap());
    assert!(finish_held(&repo, id, OWNER).unwrap());
    assert!(repo.approve_held(id, &sha, "desk").unwrap());
    assert_eq!(repo.lease_approved_held_job(OWNER, 600).unwrap().unwrap().id, id);
    let attempted = root.join("watch").join("1_ELENI_TEST.mxf");
    assert!(repo.mark_delivering(id, OWNER, &attempted.to_string_lossy(), 8).unwrap());
    assert!(repo.park_for_delivery(id, OWNER, &held_path, 8, &sha, 1.0).unwrap());
    assert!(repo.requeue_waiting_delivery(id, OWNER, chrono::Duration::zero(), "waits").unwrap());
    let job = repo.lease_approved_held_job_when(OWNER, 600, true).unwrap().unwrap();
    let held = repo.approved_held(id).unwrap().unwrap();

    run_approved_delivery(&repo, &engine, OWNER, &job, &held, &CancellationToken::new()).await.unwrap();

    assert!(watch_files_in(&root).is_empty(), "found a file in the watchfolder: {:?}", watch_files_in(&root));
    let job = repo.get_job(id).unwrap().unwrap();
    assert_eq!((job.status, job.error_code.as_deref()), (JobStatus::RequiresReview, Some("DELIVERY_UNCONFIRMED")));
    assert!(job.error_message.unwrap().starts_with("The delivery of 1_ELENI_TEST.mxf was interrupted"));
    assert!(held_file.exists(), "the held file was removed");
    assert_eq!(repo.delivery_marker(id).unwrap().map(|m| m.0), Some(attempted.to_string_lossy().into_owned()));
}

/// Finding 2's other side: no marker (the copy failed before any rename): the
/// video is delivered on its own when the share answers, as before.
#[tokio::test]
async fn a_waiting_video_with_no_attempted_rename_is_delivered_on_its_own() {
    let dir = Scratch::new();
    let (repo, engine, ctx) = toolless(&dir);
    let root = dir.0.clone();
    let id = parked_without_tools(&repo, &root, 1);
    let job = repo.lease_job_when(OWNER, 600, true).unwrap().unwrap();
    // `parked_without_tools` recorded "00" as the SHA-256: put the real one.
    let ready = root.join("temp").join("jobs").join(id.to_string()).join(format!("{id}_output.mxf"));
    let sha = omni_core::held::sha256_file(&ready).unwrap();
    assert!(repo.park_for_delivery(id, OWNER, &ready.to_string_lossy(), 8, &sha, 1.0).unwrap());
    let job = Job { ready_sha256: Some(sha), ..job };

    run_waiting_delivery(&repo, &engine, OWNER, &job, &CancellationToken::new(), &ctx).await.unwrap();

    assert_eq!(watch_files_in(&root), vec!["1_ELENI_TEST.mxf".to_string()]);
    assert_eq!(repo.get_job(id).unwrap().unwrap().status, JobStatus::Completed);
}

// ---------------------------------------------------------------- P11.2b

/// The bytes the crashed run was delivering.
const CUT: &[u8] = b"the video the crashed run was renaming";

/// P11.2b: a job of host `host` was renaming its video into `root/watch` when
/// the daemon died, and at start-up the share does not answer (the folder is
/// not there). Recovery makes it wait for the watchfolder with its marker and
/// no `ready_*` file; the crashed run's workspace still holds a stale output.
/// Then the share answers (the folder is back) and the wait is over. Returns
/// the job leased for the delivery after the wait.
fn recovered_waiting(repo: &Repository, root: &Path) -> Job {
    let new = NewJob::new("https://news.example.invalid/crash", "1_ELENI_TEST", "ELENI");
    let id = repo.enqueue(&new, omni_core::repository::DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();
    assert_eq!(repo.lease_job(OWNER, 600).unwrap().unwrap().id, id);
    assert!(repo.set_stage(id, OWNER, JobStage::Deliver).unwrap());
    let ws = root.join("temp").join("jobs").join(id.to_string());
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join(format!("{id}_output.mxf")), b"stale output of the crashed run").unwrap();
    let target = root.join("watch").join("1_ELENI_TEST.mxf");
    assert!(repo.mark_delivering(id, OWNER, &target.to_string_lossy(), CUT.len() as u64).unwrap());

    std::fs::remove_dir_all(root.join("watch")).unwrap();
    assert_eq!(repo.recover_on_startup("host").unwrap(), 1);
    let row = repo.get_job(id).unwrap().unwrap();
    assert!(row.delivery_wait && row.ready_path.is_none(), "{:?} {:?}", row.status, row.error_message);

    std::fs::create_dir_all(root.join("watch")).unwrap();
    rusqlite::Connection::open(root.join("omni.db"))
        .unwrap()
        .execute("UPDATE queue SET not_before = NULL WHERE id = ?1", rusqlite::params![id])
        .unwrap();
    let job = repo.lease_job_when(OWNER, 600, true).unwrap().expect("the recovered waiting job");
    assert_eq!((job.id, job.stage), (id, JobStage::Deliver));
    job
}

/// Test 4a: the rename had landed before the crash. Completed with that file,
/// nothing copied, one file in the watchfolder.
#[tokio::test]
async fn a_recovered_wait_whose_rename_landed_is_completed_without_a_copy() {
    let dir = Scratch::new();
    let (repo, engine, ctx) = toolless(&dir);
    let root = dir.0.clone();
    let job = recovered_waiting(&repo, &root);
    let landed = root.join("watch").join("1_ELENI_TEST.mxf");
    std::fs::write(&landed, CUT).unwrap();

    run_waiting_delivery(&repo, &engine, OWNER, &job, &CancellationToken::new(), &ctx).await.unwrap();

    assert_eq!(watch_files_in(&root), vec!["1_ELENI_TEST.mxf".to_string()], "a second copy reached the watchfolder");
    assert_eq!(std::fs::read(&landed).unwrap(), CUT, "the delivered file was touched");
    let row = repo.get_job(job.id).unwrap().unwrap();
    assert_eq!(row.status, JobStatus::Completed);
    assert_eq!(row.file_path.as_deref(), Some(landed.to_string_lossy().as_ref()));
    assert!(!row.delivery_wait);
}

/// Test 4b: the rename was attempted and its file is not there now (it may be
/// in Dalet already): review with DELIVERY_UNCONFIRMED, nothing copied.
#[tokio::test]
async fn a_recovered_wait_whose_file_is_not_there_goes_to_review_as_unconfirmed() {
    let dir = Scratch::new();
    let (repo, engine, ctx) = toolless(&dir);
    let root = dir.0.clone();
    let job = recovered_waiting(&repo, &root);

    run_waiting_delivery(&repo, &engine, OWNER, &job, &CancellationToken::new(), &ctx).await.unwrap();

    assert!(watch_files_in(&root).is_empty(), "copied: {:?}", watch_files_in(&root));
    let row = repo.get_job(job.id).unwrap().unwrap();
    assert_eq!((row.status, row.error_code.as_deref()), (JobStatus::RequiresReview, Some(ErrorCode::DeliveryUnconfirmed.as_str())));
    assert!(row.error_message.unwrap().starts_with("The delivery of 1_ELENI_TEST.mxf was interrupted"));
    assert!(repo.lease_job_when(OWNER, 600, true).unwrap().is_none(), "a job in review was leased");
}

/// Test 4c: a file of another size is at the marker: not this job's. With no
/// trusted `ready_*` file the job runs again from its link; the other file and
/// the stale workspace are never delivered or touched.
#[tokio::test]
async fn a_recovered_wait_with_another_file_at_the_marker_runs_again_from_its_link() {
    let dir = Scratch::new();
    let (repo, engine, ctx) = toolless(&dir);
    let root = dir.0.clone();
    let job = recovered_waiting(&repo, &root);
    let other = root.join("watch").join("1_ELENI_TEST.mxf");
    std::fs::write(&other, b"someone else's file").unwrap();

    run_waiting_delivery(&repo, &engine, OWNER, &job, &CancellationToken::new(), &ctx).await.unwrap();

    assert_eq!(watch_files_in(&root), vec!["1_ELENI_TEST.mxf".to_string()], "something was delivered");
    assert_eq!(std::fs::read(&other).unwrap(), b"someone else's file");
    let row = repo.get_job(job.id).unwrap().unwrap();
    assert_eq!((row.status, row.stage, row.delivery_wait), (JobStatus::Pending, JobStage::Queued, false));
    assert_eq!(repo.delivery_marker(job.id).unwrap(), None);
    assert!(!root.join("temp").join("jobs").join(job.id.to_string()).exists(), "the untrusted workspace stayed");
    let next = repo.lease_job_when(OWNER, 600, true).unwrap().expect("a full run");
    assert_eq!((next.id, next.stage), (job.id, JobStage::Extract));
}

/// P11.2b M2: the waiting file cannot be opened (held open elsewhere). Within
/// the limit it waits; past two hours from its first park a person is asked
/// (PIPELINE_FAILED), and the workspace is kept.
#[cfg(windows)]
#[tokio::test]
async fn a_waiting_video_unreadable_for_two_hours_goes_to_review_and_is_kept() {
    use std::os::windows::fs::OpenOptionsExt;
    let dir = Scratch::new();
    let (repo, engine, ctx) = toolless(&dir);
    let root = dir.0.clone();
    let id = parked_without_tools(&repo, &root, 1);
    let ready = root.join("temp").join("jobs").join(id.to_string()).join(format!("{id}_output.mxf"));
    let held_open = std::fs::OpenOptions::new().read(true).share_mode(0).open(&ready).unwrap();

    // Parked 1 h 59 min ago: it waits.
    let job = repo.lease_job_when(OWNER, 600, true).unwrap().unwrap();
    let job = Job { ready_at: Some(chrono::Utc::now() - chrono::Duration::minutes(119)), ..job };
    run_waiting_delivery(&repo, &engine, OWNER, &job, &CancellationToken::new(), &ctx).await.unwrap();
    let row = repo.get_job(id).unwrap().unwrap();
    assert_eq!((row.status, row.delivery_wait), (JobStatus::Pending, true), "{:?}", row.error_message);
    assert_eq!(row.error_code.as_deref(), Some(ErrorCode::DeliveryFailed.as_str()));

    // Parked 2 h 1 min ago: review.
    rusqlite::Connection::open(root.join("omni.db"))
        .unwrap()
        .execute("UPDATE queue SET not_before = NULL WHERE id = ?1", rusqlite::params![id])
        .unwrap();
    let job = repo.lease_job_when(OWNER, 600, true).unwrap().unwrap();
    let job = Job { ready_at: Some(chrono::Utc::now() - chrono::Duration::minutes(121)), ..job };
    run_waiting_delivery(&repo, &engine, OWNER, &job, &CancellationToken::new(), &ctx).await.unwrap();
    drop(held_open);

    let row = repo.get_job(id).unwrap().unwrap();
    assert_eq!((row.status, row.error_code.as_deref()), (JobStatus::RequiresReview, Some(ErrorCode::PipelineFailed.as_str())));
    let msg = row.error_message.clone().unwrap();
    assert!(msg.starts_with("The finished video in the workspace could not be read for 2 hours: "), "{msg}");
    assert_eq!(row.lease_owner, None);
    assert_eq!(std::fs::read(&ready).unwrap(), b"verified", "the workspace was not kept");
    let events: Vec<(String, String)> = repo.get_job_events(id, 100).unwrap().into_iter().map(|e| (e.level, e.message)).collect();
    assert!(events.iter().any(|(l, m)| l == "ERROR" && *m == msg), "{events:?}");
    assert!(repo.get_recent_audit_logs(50).unwrap().iter().any(|a| a.category == "DELIVERY" && a.message.contains(&msg)));
    assert!(watch_files_in(&root).is_empty());
}
