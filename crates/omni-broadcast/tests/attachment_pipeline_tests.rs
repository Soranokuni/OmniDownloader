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
    let mut job = NewJob::new("attachment://m1@example.gr/2", "1_DIMITRIOU_LIMANI", "DIMITRIOU");
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

#[tokio::test]
async fn an_attached_video_is_built_from_its_saved_file() {
    let (Some(ffmpeg), Some(ffprobe), Some(bmx)) = (bin("ffmpeg"), bin("ffprobe"), bin("bmxtranswrap")) else {
        eprintln!("WARNING: skipping attachment pipeline test -- ffmpeg/ffprobe/bmxtranswrap not in bin/");
        return;
    };
    let r = rig(ffmpeg.clone(), ffprobe, bmx);

    let source = r.root.join("attachments").join("saved").join("source.mp4");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    // 6 s: longer than the loudness measurement's 3 s window.
    let args: Vec<String> = [
        "-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=25:duration=6", "-f", "lavfi",
        "-i", "sine=frequency=1000:sample_rate=48000:duration=6", "-c:v", "libx264", "-pix_fmt", "yuv420p",
        "-c:a", "aac", "-shortest",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([source.to_string_lossy().into_owned()])
    .collect();
    let made = omni_core::process::run(&ffmpeg, &args, omni_core::process::RunOpts::new(std::time::Duration::from_secs(120)))
        .await
        .unwrap();
    assert!(made.success, "could not build the synthetic attachment");

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
