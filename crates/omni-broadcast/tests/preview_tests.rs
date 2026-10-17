//! The preview and contact sheet of a held MXF, with real ffmpeg (plan P10.6).
//!
//! A real 1080i50 RDD9 MXF is made through the real engine's hold, then
//! `preview::make` is run on it. Needs ffmpeg, ffprobe and bmxtranswrap in
//! `bin/`; skipped with a visible warning without them.

use std::path::{Path, PathBuf};
use std::time::Duration;

use omni_broadcast::pipeline::{BroadcastEngine, DeliveryMode, RunOutcome};
use omni_broadcast::preview::{self, PreviewEncoder, AUDIO_FILE, CONTACT_FILE, VIDEO_FILE};
use omni_core::models::{JobStatus, NewJob};
use omni_core::process::{self, RunOpts};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};
use tokio_util::sync::CancellationToken;

const SLUG: &str = "1_DIMITRIOU_LIMANI";
const HOLD: DeliveryMode = DeliveryMode::Hold { hold_hours: 24, held_max_bytes: 20 * 1024 * 1024 * 1024 };

fn bin(name: &str) -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bin").join(format!("{name}.exe"));
    p.exists().then_some(p)
}

async fn probe(ffprobe: &Path, file: &Path) -> serde_json::Value {
    let args = ["-v", "error", "-show_streams", "-show_format", "-of", "json", &file.to_string_lossy()].map(String::from);
    let out = process::run(ffprobe, &args, RunOpts::new(Duration::from_secs(60)).capture_stdout()).await.unwrap();
    assert!(out.success, "ffprobe failed on {file:?}: {}", out.stderr_tail);
    serde_json::from_str(&out.stdout).unwrap()
}

/// Top-level MP4 box names, in file order.
fn top_level_boxes(file: &Path) -> Vec<String> {
    let data = std::fs::read(file).unwrap();
    let (mut at, mut names) = (0usize, Vec::new());
    while at + 8 <= data.len() {
        let size = u32::from_be_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        names.push(String::from_utf8_lossy(&data[at + 4..at + 8]).into_owned());
        let size = match size {
            0 => data.len() - at,
            1 => u64::from_be_bytes(data[at + 8..at + 16].try_into().unwrap()) as usize,
            n => n,
        };
        assert!(size >= 8, "corrupt box size");
        at += size;
    }
    names
}

#[tokio::test]
async fn the_preview_of_a_real_held_mxf_plays_in_a_browser_and_matches_the_clip() {
    let (Some(ffmpeg), Some(ffprobe), Some(bmx)) = (bin("ffmpeg"), bin("ffprobe"), bin("bmxtranswrap")) else {
        eprintln!("WARNING: skipping preview test -- ffmpeg/ffprobe/bmxtranswrap not in bin/");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let repo = Repository::new(root.join("omni.db")).unwrap();
    std::fs::create_dir_all(root.join("watchfolder")).unwrap();
    let engine = BroadcastEngine::new(
        repo.clone(),
        root.join("no-such-yt-dlp.exe"),
        ffmpeg.clone(),
        ffprobe.clone(),
        bmx,
        root.join("temp"),
        root.join("watchfolder"),
    );

    // A ~6 s attachment, held through the real pipeline.
    let source = root.join("attachments").join("1").join("source.mp4");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    let args = [
        "-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=25:duration=6", "-f", "lavfi", "-i",
        "sine=frequency=1000:sample_rate=48000:duration=6", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-c:a", "aac",
        "-shortest", &source.to_string_lossy(),
    ]
    .map(String::from);
    assert!(process::run(&ffmpeg, &args, RunOpts::new(Duration::from_secs(120))).await.unwrap().success);
    let mut job = NewJob::new("attachment://m1@example.gr/1", SLUG, "DIMITRIOU");
    job.keyword = "LIMANI".into();
    job.status = JobStatus::ManualDownload;
    job.extraction_method = Some("attachment".into());
    let id = repo.enqueue(&job, DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();
    assert!(repo.attach_source(id, &source.to_string_lossy()).unwrap());
    let job = repo.lease_job("w1", 180).unwrap().unwrap();
    let outcome = engine.process_job_mode(job, "w1", None, None, None, None, HOLD).await.unwrap();
    assert_eq!(outcome, RunOutcome::Held);

    // The hold itself already made them, next to the MXF.
    let held_dir = root.join("temp").join("held").join(id.to_string());
    let held_mxf = held_dir.join(format!("{SLUG}.mxf"));
    assert!(held_dir.join(VIDEO_FILE).is_file() && held_dir.join(CONTACT_FILE).is_file(), "the hold must leave a preview");
    let held_sha = omni_core::held::sha256_file(&held_mxf).unwrap();
    assert_eq!(held_sha, repo.waiting_held(id).unwrap().unwrap().sha256, "the preview never touches the MXF");

    // And again, directly, timed.
    let out_dir = root.join("preview_out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let encoder = engine.preview_encoder().await;
    assert_ne!(encoder, PreviewEncoder::None, "the bundled ffmpeg has an H.264 encoder");
    let src = probe(&ffprobe, &held_mxf).await;
    let src_dur: f64 = src["format"]["duration"].as_str().unwrap().parse().unwrap();

    let started = std::time::Instant::now();
    let files = preview::make(&ffmpeg, &held_mxf, &out_dir, encoder, src_dur, &CancellationToken::new()).await;
    eprintln!("preview + contact sheet of a {src_dur:.1} s clip took {:.1} s", started.elapsed().as_secs_f64());

    let video = files.video.expect("a video preview");
    let contact = files.contact.expect("a contact sheet");
    assert!(files.audio.is_none());

    let v = probe(&ffprobe, &video).await;
    let streams = v["streams"].as_array().unwrap();
    assert_eq!(streams.len(), 2, "one video and one audio stream: {streams:?}");
    let vs = streams.iter().find(|s| s["codec_type"] == "video").unwrap();
    let au = streams.iter().find(|s| s["codec_type"] == "audio").unwrap();
    assert_eq!(vs["codec_name"], "h264");
    assert_eq!((vs["width"].as_i64(), vs["height"].as_i64()), (Some(960), Some(540)));
    assert_eq!(vs["avg_frame_rate"], "25/1");
    assert_eq!(au["codec_name"], "aac");
    assert_eq!(au["channels"].as_i64(), Some(2));
    let dur: f64 = v["format"]["duration"].as_str().unwrap().parse().unwrap();
    assert!((dur - src_dur).abs() <= 0.2, "preview {dur} s vs source {src_dur} s");

    let boxes = top_level_boxes(&video);
    let moov = boxes.iter().position(|b| b == "moov").expect("a moov box");
    let mdat = boxes.iter().position(|b| b == "mdat").expect("an mdat box");
    assert!(moov < mdat, "faststart: moov must precede mdat, got {boxes:?}");

    let c = probe(&ffprobe, &contact).await;
    let cs = &c["streams"][0];
    assert_eq!(cs["codec_name"], "mjpeg");
    assert_eq!((cs["width"].as_i64(), cs["height"].as_i64()), (Some(1280), Some(540)), "4x3 tiles of 320x180");

    let leftovers: Vec<_> = std::fs::read_dir(&out_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "half-written files left: {leftovers:?}");

    // Audio-only fallback (no H.264 encoder): an .m4a with two channels, no picture.
    let audio_dir = root.join("audio_out");
    std::fs::create_dir_all(&audio_dir).unwrap();
    let files = preview::make(&ffmpeg, &held_mxf, &audio_dir, PreviewEncoder::None, src_dur, &CancellationToken::new()).await;
    let audio = files.audio.expect("an audio preview");
    assert_eq!(audio.file_name().unwrap().to_string_lossy(), AUDIO_FILE);
    assert!(files.video.is_none());
    let a = probe(&ffprobe, &audio).await;
    assert_eq!(a["streams"].as_array().unwrap().len(), 1);
    assert_eq!(a["streams"][0]["channels"].as_i64(), Some(2));
    assert_eq!(omni_core::held::sha256_file(&held_mxf).unwrap(), held_sha, "the MXF is input only");
}

#[tokio::test]
async fn a_failing_ffmpeg_gives_no_files_and_no_error() {
    let dir = tempfile::tempdir().unwrap();
    let files = preview::make(
        Path::new("omni-definitely-not-ffmpeg"),
        &dir.path().join("x.mxf"),
        dir.path(),
        PreviewEncoder::X264,
        6.0,
        &CancellationToken::new(),
    )
    .await;
    assert!(files.is_empty());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0, "nothing left behind");
}
