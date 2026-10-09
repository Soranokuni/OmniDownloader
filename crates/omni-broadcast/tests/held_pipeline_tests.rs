//! A video held for MCR's approval, through the real engine (plan P10.5).
//!
//! The finished MXF must stop after the compliance gate, never reach the
//! watchfolder, wait bound to its SHA-256, and on approval go out through the
//! same delivery code as any other video. Needs ffmpeg, ffprobe and
//! bmxtranswrap in `bin/`; skipped with a visible warning without them.

use std::path::{Path, PathBuf};

use omni_broadcast::pipeline::{BroadcastEngine, DeliveryMode, RunOutcome};
use omni_core::models::{Job, JobStatus, NewJob};
use omni_core::repository::{HeldOutput, Repository, DEFAULT_DEDUP_WINDOW_HOURS};

const SLUG: &str = "1_DIMITRIOU_LIMANI";
const HOLD: DeliveryMode = DeliveryMode::Hold { hold_hours: 24, held_max_bytes: 20 * 1024 * 1024 * 1024 };

fn bin(name: &str) -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bin").join(format!("{name}.exe"));
    p.exists().then_some(p)
}

struct Rig {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: Repository,
    engine: BroadcastEngine,
    ffmpeg: PathBuf,
}

fn rig() -> Option<Rig> {
    let (Some(ffmpeg), Some(ffprobe), Some(bmx)) = (bin("ffmpeg"), bin("ffprobe"), bin("bmxtranswrap")) else {
        eprintln!("WARNING: skipping held-video pipeline test -- ffmpeg/ffprobe/bmxtranswrap not in bin/");
        return None;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let repo = Repository::new(root.join("omni.db")).unwrap();
    std::fs::create_dir_all(root.join("watchfolder")).unwrap();
    let engine = BroadcastEngine::new(
        repo.clone(),
        root.join("no-such-yt-dlp.exe"),
        ffmpeg.clone(),
        ffprobe,
        bmx,
        root.join("temp"),
        root.join("watchfolder"),
    );
    Some(Rig { _dir: dir, root, repo, engine, ffmpeg })
}

async fn make_source(ffmpeg: &Path, path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let args: Vec<String> = [
        "-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=25:duration=2", "-f", "lavfi",
        "-i", "sine=frequency=1000:sample_rate=48000:duration=2", "-c:v", "libx264", "-pix_fmt", "yuv420p",
        "-c:a", "aac", "-shortest",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([path.to_string_lossy().into_owned()])
    .collect();
    let made = omni_core::process::run(ffmpeg, &args, omni_core::process::RunOpts::new(std::time::Duration::from_secs(120)))
        .await
        .unwrap();
    assert!(made.success, "could not build the synthetic attachment");
}

/// Queue an attachment job with a real source and lease it as "w1".
async fn leased_job(r: &Rig) -> Job {
    let source = r.root.join("attachments").join("1").join("source.mp4");
    make_source(&r.ffmpeg, &source).await;
    let mut job = NewJob::new("attachment://m1@example.gr/1", SLUG, "DIMITRIOU");
    job.keyword = "LIMANI".into();
    job.status = JobStatus::ManualDownload;
    job.extraction_method = Some("attachment".into());
    let id = r.repo.enqueue(&job, DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();
    assert!(r.repo.attach_source(id, &source.to_string_lossy()).unwrap());
    r.repo.lease_job("w1", 180).unwrap().expect("released job is leasable")
}

fn watchfolder_files(r: &Rig) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(r.root.join("watchfolder"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

fn held_dir(r: &Rig, id: i64) -> PathBuf {
    r.root.join("temp").join("held").join(id.to_string())
}

/// Run a job to the hold, park it, approve it and lease it again: what the
/// worker has in hand when it delivers.
async fn approved(r: &Rig) -> (Job, HeldOutput) {
    let job = leased_job(r).await;
    let id = job.id;
    let outcome = r
        .engine
        .process_job_mode(job, "w1", None, None, None, None, HOLD)
        .await
        .expect("the hold succeeds");
    assert_eq!(outcome, RunOutcome::Held);
    let sha = r.repo.waiting_held(id).unwrap().expect("a waiting row").sha256;
    assert!(r.repo.finish(id, "w1", JobStatus::AwaitingApproval, None, None, None).unwrap());
    assert!(r.repo.approve_held(id, &sha, "desk").unwrap());
    let job = r.repo.lease_job("w2", 180).unwrap().expect("an approved job is leased again");
    assert_eq!(job.id, id);
    let held = r.repo.approved_held(id).unwrap().expect("approved row");
    (job, held)
}

#[tokio::test]
async fn a_held_video_stops_after_the_gate_and_never_reaches_the_watchfolder() {
    let Some(r) = rig() else { return };
    let job = leased_job(&r).await;
    let id = job.id;

    let outcome = r
        .engine
        .process_job_mode(job, "w1", None, None, None, None, HOLD)
        .await
        .expect("a held video is a success for the pipeline");
    assert_eq!(outcome, RunOutcome::Held, "the worker parks the job from this, not from the database");

    assert!(watchfolder_files(&r).is_empty(), "nothing may reach the watchfolder: {:?}", watchfolder_files(&r));
    let file = held_dir(&r, id).join(format!("{SLUG}.mxf"));
    assert!(file.is_file(), "the held file is missing: {file:?}");
    let row = r.repo.waiting_held(id).unwrap().expect("a waiting held row");
    assert_eq!(row.sha256, omni_core::held::sha256_file(&file).unwrap(), "the row names the file's SHA-256");
    assert_eq!(row.bytes as u64, std::fs::metadata(&file).unwrap().len());
    assert!(row.duration.is_some());
    assert!(row.expires_at.unwrap() > row.created_at.unwrap() + chrono::Duration::hours(23));
    assert!(!r.root.join("temp").join("jobs").join(id.to_string()).exists(), "the job's workspace is removed");
    // The engine sets no status: the worker holding the lease does.
    assert_eq!(r.repo.get_job(id).unwrap().unwrap().status, JobStatus::Running);
    assert!(r.repo.get_job(id).unwrap().unwrap().file_path.is_none());
}

#[tokio::test]
async fn a_hold_over_the_byte_budget_is_refused_as_low_disk_and_holds_nothing() {
    let Some(r) = rig() else { return };
    let job = leased_job(&r).await;
    let id = job.id;

    let err = r
        .engine
        .process_job_mode(job, "w1", None, None, None, None, DeliveryMode::Hold { hold_hours: 24, held_max_bytes: 10 })
        .await
        .expect_err("ten bytes cannot hold a video");

    assert!(format!("{err:#}").contains("LOW_DISK"), "{err:#}");
    assert!(r.repo.get_held(id).unwrap().is_none(), "a refused hold leaves no row");
    assert!(!held_dir(&r, id).exists(), "a refused hold leaves no file");
    assert!(watchfolder_files(&r).is_empty());
}

#[tokio::test]
async fn an_approved_video_is_delivered_byte_for_byte_through_the_normal_delivery() {
    let Some(r) = rig() else { return };
    let (job, held) = approved(&r).await;
    let id = job.id;
    let original = std::fs::read(&held.path).unwrap();

    r.engine.deliver_held(&job, "w2", &held, None).await.expect("approved video delivers");

    assert_eq!(watchfolder_files(&r), vec![format!("{SLUG}.mxf")], "exactly the video, no .tmp left behind");
    assert_eq!(std::fs::read(r.root.join("watchfolder").join(format!("{SLUG}.mxf"))).unwrap(), original);
    // The engine records the copy but decides nothing: the row is still
    // approved and the held file still there until the worker finishes.
    let row = r.repo.get_held(id).unwrap().unwrap();
    assert_eq!(row.decision.as_deref(), Some("approved"));
    assert!(held_dir(&r, id).exists());
    let after = r.repo.get_job(id).unwrap().unwrap();
    assert!(after.file_path.as_deref().is_some_and(|p| p.ends_with(&format!("{SLUG}.mxf"))));
    assert_eq!(row.delivered_path, after.file_path);
    assert_eq!(after.status, JobStatus::Running);

    // The worker finishes job and row together.
    assert!(r.repo.finish_held_delivery(id, "w2").unwrap());
    r.engine.discard_held_dir(id);
    assert_eq!(r.repo.get_held(id).unwrap().unwrap().decision.as_deref(), Some("delivered"));
    assert_eq!(r.repo.get_job(id).unwrap().unwrap().status, JobStatus::Completed);
    assert!(!held_dir(&r, id).exists(), "the held copy is removed once delivered");
}

#[tokio::test]
async fn an_existing_file_of_the_same_name_is_never_overwritten() {
    let Some(r) = rig() else { return };
    let (job, held) = approved(&r).await;
    let existing = r.root.join("watchfolder").join(format!("{SLUG}.mxf"));
    std::fs::write(&existing, b"already on air").unwrap();

    r.engine.deliver_held(&job, "w2", &held, None).await.expect("delivers next to it");

    assert_eq!(std::fs::read(&existing).unwrap(), b"already on air", "the file in the watchfolder was touched");
    assert_eq!(watchfolder_files(&r), vec![format!("{SLUG}.mxf"), format!("{SLUG}_2.mxf")]);
}

#[tokio::test]
async fn a_held_file_changed_after_approval_is_not_delivered() {
    let Some(r) = rig() else { return };
    let (job, held) = approved(&r).await;
    // One byte appended: still a file, no longer the approved one.
    let mut bytes = std::fs::read(&held.path).unwrap();
    bytes.push(0);
    std::fs::write(&held.path, bytes).unwrap();

    let err = r.engine.deliver_held(&job, "w2", &held, None).await.expect_err("a changed file must not go out");

    assert!(format!("{err:#}").contains("HELD_FILE_CHANGED"), "{err:#}");
    assert!(watchfolder_files(&r).is_empty(), "nothing may reach the watchfolder: {:?}", watchfolder_files(&r));
    assert!(r.repo.get_job(job.id).unwrap().unwrap().file_path.is_none());
}

#[tokio::test]
async fn a_held_file_that_is_gone_is_not_delivered() {
    let Some(r) = rig() else { return };
    let (job, held) = approved(&r).await;
    std::fs::remove_file(&held.path).unwrap();

    let err = r.engine.deliver_held(&job, "w2", &held, None).await.expect_err("nothing to deliver");

    assert!(format!("{err:#}").contains("HELD_FILE_CHANGED"), "{err:#}");
    assert!(watchfolder_files(&r).is_empty());
}

#[tokio::test]
async fn a_crash_after_the_copy_is_recovered_without_a_second_file() {
    let Some(r) = rig() else { return };
    let (job, held) = approved(&r).await;
    r.engine.deliver_held(&job, "w2", &held, None).await.expect("first delivery");
    assert_eq!(watchfolder_files(&r).len(), 1);

    // The worker died before finishing the job; it is requeued and another
    // worker leases it with the row and file_path the copy recorded.
    r.repo.requeue_after(job.id, "w2", chrono::Duration::seconds(0), "T", "crash").unwrap();
    let again = r.repo.lease_job("w3", 180).unwrap().expect("leased again");
    let held_now = r.repo.approved_held(job.id).unwrap().expect("still approved");
    assert!(held_now.delivered_path.is_some() && held_now.delivered_path == again.file_path);
    r.engine.deliver_held(&again, "w3", &held_now, None).await.expect("nothing more to copy");

    assert_eq!(watchfolder_files(&r), vec![format!("{SLUG}.mxf")], "a second copy was delivered");
    assert!(r.repo.finish_held_delivery(job.id, "w3").unwrap());
    assert_eq!(r.repo.get_job(job.id).unwrap().unwrap().status, JobStatus::Completed);
}

#[tokio::test]
async fn a_stale_file_path_neither_parks_as_delivered_nor_hides_the_approved_delivery() {
    let Some(r) = rig() else { return };
    let job = leased_job(&r).await;
    let id = job.id;
    // An earlier run left a delivered path on the row.
    assert!(r.repo.set_delivered_file(id, "w1", "C:/old/1_OLD.mxf", 1.0).unwrap());

    let outcome = r.engine.process_job_mode(job, "w1", None, None, None, None, HOLD).await.unwrap();
    assert_eq!(outcome, RunOutcome::Held);
    assert!(r.repo.get_job(id).unwrap().unwrap().file_path.is_none(), "the hold must clear the stale path");

    let sha = r.repo.waiting_held(id).unwrap().unwrap().sha256;
    assert!(r.repo.finish(id, "w1", JobStatus::AwaitingApproval, None, None, None).unwrap());
    assert!(r.repo.approve_held(id, &sha, "desk").unwrap());
    let job = r.repo.lease_job("w2", 180).unwrap().unwrap();
    assert!(job.file_path.is_none());
    let held = r.repo.approved_held(id).unwrap().unwrap();
    r.engine.deliver_held(&job, "w2", &held, None).await.expect("the held file is really delivered");
    assert_eq!(watchfolder_files(&r), vec![format!("{SLUG}.mxf")]);
}

#[tokio::test]
async fn a_file_changed_without_changing_size_is_not_delivered() {
    let Some(r) = rig() else { return };
    let (job, held) = approved(&r).await;
    let mut bytes = std::fs::read(&held.path).unwrap();
    let at = bytes.len() * 6 / 10;
    bytes[at] ^= 0xFF;
    std::fs::write(&held.path, bytes).unwrap();

    let err = r.engine.deliver_held(&job, "w2", &held, None).await.expect_err("a changed file must not go out");

    let chain = format!("{err:#}");
    assert!(chain.contains("HELD_FILE_CHANGED") || chain.contains("COMPLIANCE_FAILED"), "{chain}");
    assert!(watchfolder_files(&r).is_empty(), "nothing may reach the watchfolder");
}

#[tokio::test]
async fn a_job_that_lost_its_lease_delivers_nothing_and_keeps_the_held_file() {
    let Some(r) = rig() else { return };
    let (job, held) = approved(&r).await;
    assert_eq!(r.repo.cancel_job(job.id).unwrap(), omni_core::repository::CancelOutcome::Cancelled);

    let err = r.engine.deliver_held(&job, "w2", &held, None).await.expect_err("not ours any more");

    assert!(err.downcast_ref::<omni_broadcast::pipeline::LeaseLost>().is_some(), "{err:#}");
    assert!(watchfolder_files(&r).is_empty());
    assert_eq!(r.repo.get_held(job.id).unwrap().unwrap().decision.as_deref(), Some("approved"));
    assert!(held_dir(&r, job.id).exists(), "the held file must stay");
}

#[tokio::test]
async fn a_deliver_only_job_hands_back_its_download_slot_and_needs_no_encoder() {
    let Some(r) = rig() else { return };
    let (job, held) = approved(&r).await;
    // The worker leased it under a download slot; nothing is downloaded.
    let downloads = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
    r.engine.hold_download_slot(job.id, downloads.clone().acquire_owned().await.unwrap());
    // Every encoder slot is taken: delivery must not wait for one.
    let engine = r.engine.clone().with_encoder_slots(1);
    let _only_encoder = engine.encoder_slots().try_acquire_owned().expect("the one encoder slot");
    assert_eq!(engine.encoder_slots().available_permits(), 0);

    tokio::time::timeout(std::time::Duration::from_secs(60), engine.deliver_held(&job, "w2", &held, None))
        .await
        .expect("delivery waited")
        .expect("delivers");

    assert!(!engine.holds_download_slot(job.id), "the download slot was kept");
    assert_eq!(downloads.available_permits(), 1);
    assert_eq!(watchfolder_files(&r).len(), 1);
}
