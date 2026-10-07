//! A video attached to an email goes to air without yt-dlp (plan P4.6).
//!
//! The mail watcher saves the file and records `source_path`; the engine must
//! build from that file and never try to "download" an `attachment://` URL.
//! yt-dlp is pointed at a path that does not exist, so any attempt to use it
//! fails the test.

use std::path::{Path, PathBuf};

use omni_broadcast::pipeline::BroadcastEngine;
use omni_core::models::{JobStatus, NewJob};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};

fn bin(name: &str) -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bin").join(format!("{name}.exe"));
    p.exists().then_some(p)
}

struct Rig {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: Repository,
    engine: BroadcastEngine,
}

fn rig(ffmpeg: PathBuf, ffprobe: PathBuf, bmx: PathBuf) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let repo = Repository::new(root.join("omni.db")).unwrap();
    std::fs::create_dir_all(root.join("watchfolder")).unwrap();
    let engine = BroadcastEngine::new(
        repo.clone(),
        root.join("no-such-yt-dlp.exe"),
        ffmpeg,
        ffprobe,
        bmx,
        root.join("temp"),
        root.join("watchfolder"),
    );
    Rig { _dir: dir, root, repo, engine }
}

/// Queue an attachment job the way the watcher does: parked, then released
/// with its source.
fn queue_attachment(r: &Rig, source: Option<&Path>) -> i64 {
    queue_attachment_as(r, source, "attachment://m1@example.gr/2", "1_DIMITRIOU_LIMANI")
}

fn queue_attachment_as(r: &Rig, source: Option<&Path>, url: &str, slug: &str) -> i64 {
    let mut job = NewJob::new(url, slug, "DIMITRIOU");
    job.keyword = "LIMANI".into();
    job.status = JobStatus::ManualDownload;
    job.extraction_method = Some("attachment".into());
    let id = r.repo.enqueue(&job, DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();
    let path = source.map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|| {
        r.root.join("attachments").join(id.to_string()).join("gone.mp4").to_string_lossy().into_owned()
    });
    assert!(r.repo.attach_source(id, &path).unwrap());
    id
}

/// A 2 s 720p25 clip with a tone at `path`.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_downloaded_job_frees_its_download_slot_and_queues_for_the_encoder() {
    // P1.13: one slot covered download *and* encode, so a job encoding held
    // a download slot and the next link waited for someone's transcode.
    // Two jobs, one encoder: one must wait for it, both must deliver, and
    // neither may keep its download slot once its source is on disk.
    let (Some(ffmpeg), Some(ffprobe), Some(bmx)) = (bin("ffmpeg"), bin("ffprobe"), bin("bmxtranswrap")) else {
        eprintln!("WARNING: skipping encoder slot test -- ffmpeg/ffprobe/bmxtranswrap not in bin/");
        return;
    };
    let mut r = rig(ffmpeg.clone(), ffprobe, bmx);
    r.engine = r.engine.clone().with_encoder_slots(1);

    let downloads = std::sync::Arc::new(tokio::sync::Semaphore::new(2));
    let mut leased = Vec::new();
    for (n, slug) in [(1, "1_DIMITRIOU_LIMANI"), (2, "2_DIMITRIOU_LIMANI")] {
        let source = r.root.join("attachments").join(n.to_string()).join("source.mp4");
        make_source(&ffmpeg, &source).await;
        queue_attachment_as(&r, Some(&source), &format!("attachment://m1@example.gr/{n}"), slug);
        let owner = format!("test:1:{n}");
        let job = r.repo.lease_job(&owner, 180).unwrap().expect("released job is leasable");
        r.engine.hold_download_slot(job.id, downloads.clone().acquire_owned().await.unwrap());
        leased.push((job, owner));
    }
    // Both sources are on disk before either starts, so they reach the
    // encoder together.
    let runs: Vec<_> = leased
        .into_iter()
        .map(|(job, owner)| {
            let engine = r.engine.clone();
            tokio::spawn(async move {
                let id = job.id;
                engine.process_job(job, &owner).await.map(|_| id)
            })
        })
        .collect();
    let mut ids = Vec::new();
    for run in runs {
        ids.push(run.await.unwrap().expect("both jobs deliver"));
    }

    assert_eq!(downloads.available_permits(), 2, "a finished job kept its download slot");
    assert!(ids.iter().all(|id| !r.engine.holds_download_slot(*id)));
    let waited = ids
        .iter()
        .filter(|id| {
            r.repo.get_job_events(**id, 100).unwrap().iter().any(|e| e.message.contains("waiting for a free encoder"))
        })
        .count();
    assert_eq!(waited, 1, "with one encoder exactly one of two simultaneous jobs waits for it");
    for slug in ["1_DIMITRIOU_LIMANI", "2_DIMITRIOU_LIMANI"] {
        assert!(r.root.join("watchfolder").join(format!("{slug}.mxf")).exists(), "{slug} not delivered");
    }
}

#[tokio::test]
async fn an_attached_video_is_built_from_its_saved_file() {
    let (Some(ffmpeg), Some(ffprobe), Some(bmx)) = (bin("ffmpeg"), bin("ffprobe"), bin("bmxtranswrap")) else {
        eprintln!("WARNING: skipping attachment pipeline test -- ffmpeg/ffprobe/bmxtranswrap not in bin/");
        return;
    };
    let r = rig(ffmpeg.clone(), ffprobe, bmx);

    let source = r.root.join("attachments").join("saved").join("source.mp4");
    // 2 s, as short social clips are: this is also the original reproduction
    // of the short-clip loudness failure, through the whole engine.
    make_source(&ffmpeg, &source).await;

    let id = queue_attachment(&r, Some(&source));
    let job = r.repo.lease_job("test:1:1", 180).unwrap().expect("released job is leasable");
    assert_eq!(job.id, id);

    r.engine.process_job(job, "test:1:1").await.expect("attachment job should deliver");

    let delivered = r.root.join("watchfolder").join("1_DIMITRIOU_LIMANI.mxf");
    assert!(delivered.exists(), "no MXF in the watchfolder");
    // The saved attachment survives the workspace cleanup: a re-run (or the
    // archive) still has its source.
    assert!(source.exists(), "the pipeline deleted the email attachment");
}

#[tokio::test]
async fn a_missing_attachment_is_a_manual_download_not_a_yt_dlp_call() {
    let r = rig(PathBuf::from("ffmpeg.exe"), PathBuf::from("ffprobe.exe"), PathBuf::from("bmx.exe"));
    queue_attachment(&r, None);
    let job = r.repo.lease_job("test:1:1", 180).unwrap().unwrap();

    let err = r.engine.process_job(job, "test:1:1").await.unwrap_err();
    let chain = format!("{err:#}");
    assert!(chain.contains("MANUAL_DOWNLOAD"), "{chain}");
    assert!(chain.contains("attachment is not on disk"), "{chain}");
}

#[test]
fn a_clip_too_big_for_the_disk_is_refused_before_it_is_made() {
    // P1.4: LOW_DISK was a code nothing raised. A clip whose two temp
    // copies cannot fit is refused before the encoder spends minutes on it.
    let d = tempfile::tempdir().unwrap();
    let ten_years = 10.0 * 365.0 * 24.0 * 3600.0;
    let why = omni_broadcast::pipeline::room_for(ten_years, d.path(), d.path()).unwrap_err();
    assert!(why.contains("temp has") && why.contains("needs"), "{why}");
    assert!(omni_broadcast::pipeline::room_for(60.0, d.path(), d.path()).is_ok(), "a minute fits on the test disk");
}

#[tokio::test]
async fn a_video_renamed_while_it_downloads_is_delivered_under_the_new_name() {
    // P7.12: the worker leased the job as 1_DIMITRIOU_LIMANI; MCR renamed
    // it before the rewrap. The clip and the file carry the new name.
    let (Some(ffmpeg), Some(ffprobe), Some(bmx)) = (bin("ffmpeg"), bin("ffprobe"), bin("bmxtranswrap")) else {
        eprintln!("WARNING: skipping rename pipeline test -- ffmpeg/ffprobe/bmxtranswrap not in bin/");
        return;
    };
    let r = rig(ffmpeg.clone(), ffprobe, bmx);
    let source = r.root.join("attachments").join("ren").join("source.mp4");
    make_source(&ffmpeg, &source).await;
    let id = queue_attachment(&r, Some(&source));
    let job = r.repo.lease_job("test:1:1", 180).unwrap().expect("leasable");
    assert_eq!(job.slug, "1_DIMITRIOU_LIMANI");

    r.repo.rename_job_keyword(id, "LIMANICHANION").unwrap().expect("renamable while running");
    r.engine.process_job(job, "test:1:1").await.expect("delivers");

    assert!(r.root.join("watchfolder").join("1_DIMITRIOU_LIMANICHANION.mxf").exists(), "delivered under the new name");
    assert!(!r.root.join("watchfolder").join("1_DIMITRIOU_LIMANI.mxf").exists());
}
