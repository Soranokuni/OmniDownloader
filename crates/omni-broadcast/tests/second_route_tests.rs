//! The second route through the pipeline (plan P11.6), against the real tools
//! in `bin/` and synthetic broken sources made with ffmpeg: a transport stream
//! whose timestamps jump, an MP4 cut part way, a variable-frame-rate clip, and
//! a link whose first download is cut and whose second is whole (yt-dlp on a
//! loopback server; no network). Every name and address is invented.
//!
//! Skipped with a visible warning when `bin/` lacks a tool.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use omni_broadcast::pipeline::{BroadcastEngine, RunOutcome};
use omni_core::models::{JobStatus, NewJob};
use omni_core::repository::{Repository, DEFAULT_DEDUP_WINDOW_HOURS};

const OWNER: &str = "test:1:0";
const SLUG: &str = "1_KALOGEROU_LIMANI";
const ROUTE_LINE: &str = "second route:";

fn bin(name: &str) -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bin").join(format!("{name}.exe"));
    p.exists().then_some(p)
}

struct Tools {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
    bmx: PathBuf,
}

fn tools(test: &str) -> Option<Tools> {
    match (bin("ffmpeg"), bin("ffprobe"), bin("bmxtranswrap")) {
        (Some(ffmpeg), Some(ffprobe), Some(bmx)) => Some(Tools { ffmpeg, ffprobe, bmx }),
        _ => {
            eprintln!("WARNING: skipping {test} -- ffmpeg/ffprobe/bmxtranswrap not in bin/");
            None
        }
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: Repository,
    engine: BroadcastEngine,
}

fn rig(t: &Tools, ytdl: PathBuf, second_route: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let repo = Repository::new(root.join("omni.db")).unwrap();
    std::fs::create_dir_all(root.join("watchfolder")).unwrap();
    let engine = BroadcastEngine::new(
        repo.clone(),
        ytdl,
        t.ffmpeg.clone(),
        t.ffprobe.clone(),
        t.bmx.clone(),
        root.join("temp"),
        root.join("watchfolder"),
    )
    .with_second_route(second_route);
    Rig { _dir: dir, root, repo, engine }
}

impl Rig {
    /// An attachment job with `source` as its file, leased by OWNER.
    fn attachment(&self, source: &Path) -> omni_core::models::Job {
        let mut job = NewJob::new("attachment://m1@example.gr/2", SLUG, "KALOGEROU");
        job.keyword = "LIMANI".into();
        job.status = JobStatus::ManualDownload;
        job.extraction_method = Some("attachment".into());
        let id = self.repo.enqueue(&job, DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();
        assert!(self.repo.attach_source(id, &source.to_string_lossy()).unwrap());
        self.lease(id)
    }

    fn lease(&self, id: i64) -> omni_core::models::Job {
        assert!(self.repo.lease_job_by_id(id, OWNER, 600).unwrap(), "job {id} not leased");
        self.repo.get_job(id).unwrap().unwrap()
    }

    fn lines(&self, id: i64, needle: &str) -> Vec<String> {
        let mut found: Vec<String> =
            self.repo.get_job_events(id, 500).unwrap().into_iter().map(|e| e.message).filter(|m| m.contains(needle)).collect();
        found.reverse();
        found
    }

    fn delivered(&self) -> Vec<String> {
        std::fs::read_dir(self.root.join("watchfolder"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }
}

async fn ffmpeg(t: &Tools, args: &[&str]) {
    let made = omni_core::process::run(&t.ffmpeg, args, omni_core::process::RunOpts::new(std::time::Duration::from_secs(120)))
        .await
        .unwrap();
    assert!(made.success, "ffmpeg could not build a synthetic source: {}", made.stderr_tail);
}

/// A 1280x720 25p clip with a tone, `secs` long, as `format` (`mp4` is faststart).
async fn clip(t: &Tools, path: &Path, secs: u32, extra: &[&str]) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let video = format!("testsrc2=size=1280x720:rate=25:duration={secs}");
    let tone = format!("sine=frequency=1000:sample_rate=48000:duration={secs}");
    let mut args = vec!["-y", "-v", "error", "-f", "lavfi", "-i", &video, "-f", "lavfi", "-i", &tone];
    args.extend(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"]);
    args.extend(extra);
    let out = path.to_string_lossy().into_owned();
    args.push(&out);
    ffmpeg(t, &args).await;
}

/// Two 4 s transport streams, the second's clock 200 s on, as one file: the
/// container says 204 s, the pictures last 8.
async fn jumping_ts(t: &Tools, path: &Path) {
    let dir = path.parent().unwrap();
    let (a, b) = (dir.join("part_a.ts"), dir.join("part_b.ts"));
    clip(t, &a, 4, &["-f", "mpegts"]).await;
    clip(t, &b, 4, &["-output_ts_offset", "200", "-f", "mpegts"]).await;
    let mut bytes = std::fs::read(&a).unwrap();
    bytes.extend(std::fs::read(&b).unwrap());
    std::fs::write(path, bytes).unwrap();
}

/// A 10 s faststart MP4 cut to its first half: the index lists 10 s of samples.
async fn cut_mp4(t: &Tools, path: &Path) -> Vec<u8> {
    let whole = path.with_extension("whole.mp4");
    clip(t, &whole, 10, &["-movflags", "+faststart"]).await;
    let bytes = std::fs::read(&whole).unwrap();
    std::fs::write(path, &bytes[..bytes.len() / 2]).unwrap();
    bytes
}

async fn mxf_secs(t: &Tools, path: &Path) -> f64 {
    omni_broadcast::probe::probe(&t.ffprobe, path).await.expect("the delivered file probes").duration_secs
}

/// A stream whose clock jumps 200 s: the encode is cut short of what the
/// container says (TRANSCODE_FAILED). A remux would make an honest-looking
/// 8 s file of it, but nobody can tell whether the jump stands for missing
/// pictures, so the second route refuses it: the job keeps its first failure,
/// nothing is delivered, and the history names both lengths.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_whose_clock_jumps_is_not_shortened_and_goes_to_review() {
    let Some(t) = tools("the timestamp-jump test") else { return };
    let r = rig(&t, PathBuf::from("no-such-yt-dlp.exe"), true);
    let source = r.root.join("attachments").join("1").join("clip.ts");
    jumping_ts(&t, &source).await;

    let job = r.attachment(&source);
    let id = job.id;
    let err = r.engine.process_job(job, OWNER).await.expect_err("a shortened video is never delivered");
    assert_eq!(omni_broadcast::errors::code_in_text(&format!("{err:#}")), Some(omni_broadcast::errors::ErrorCode::TranscodeFailed), "{err:#}");
    assert!(r.delivered().is_empty(), "{:?}", r.delivered());
    let route = r.lines(id, ROUTE_LINE);
    assert_eq!(route.len(), 1, "{route:?}");
    assert!(route[0].starts_with("TRANSCODE_FAILED: second route:"), "{route:?}");
    let refused = r.lines(id, "The second route did not help");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(refused[0].contains("the remuxed file is 8.") && refused[0].contains("the source says 20"), "both lengths: {refused:?}");
    assert!(r.lines(id, "made a compliant file").is_empty());
    assert!(r.repo.second_route_at(id).unwrap().is_some());
    assert!(source.exists(), "the attachment itself is never touched");
    assert!(!r.root.join("temp").join("jobs").join(id.to_string()).exists(), "the workspace is cleaned up");
}

/// An 8 s Matroska clip whose header says it lasts `claimed` seconds.
async fn mislabelled_mkv(t: &Tools, path: &Path, claimed: f64) {
    clip(t, path, 8, &["-f", "matroska"]).await;
    let mut bytes = std::fs::read(path).unwrap();
    // The Segment Info's Duration: element 0x4489, an 8-byte float in milliseconds.
    let at = bytes.windows(3).position(|w| w == [0x44, 0x89, 0x88]).expect("a Duration element") + 3;
    bytes[at..at + 8].copy_from_slice(&(claimed * 1000.0).to_be_bytes());
    std::fs::write(path, bytes).unwrap();
}

/// Timestamps the remux genuinely repairs, losing nothing: a clip whose
/// container says it lasts 4 s holds 8 s of pictures, so the encode comes out
/// twice as long as the source claims and fails the gate (COMPLIANCE_FAILED).
/// The remux writes the real length, and the job delivers all 8 s through the
/// usual encode, wrap and gate, with the source's size in its history.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_clip_whose_container_misstates_its_length_is_delivered_whole_by_the_second_route() {
    let Some(t) = tools("the mislabelled-length test") else { return };
    let r = rig(&t, PathBuf::from("no-such-yt-dlp.exe"), true);
    let source = r.root.join("attachments").join("1").join("clip.mkv");
    mislabelled_mkv(&t, &source, 4.0).await;
    let claimed = omni_broadcast::probe::probe(&t.ffprobe, &source).await.unwrap().duration_secs;
    assert!((claimed - 4.0).abs() < 0.1, "the synthetic header did not take: {claimed}");

    let job = r.attachment(&source);
    let id = job.id;
    let outcome = r.engine.process_job(job, OWNER).await;
    assert!(outcome.is_ok(), "the second route should deliver it: {:#}", outcome.unwrap_err());

    assert_eq!(r.delivered(), vec![format!("{SLUG}.mxf")]);
    let secs = mxf_secs(&t, &r.root.join("watchfolder").join(format!("{SLUG}.mxf"))).await;
    assert!((secs - 8.0).abs() < 0.5, "every picture it holds: {secs} s");
    let route = r.lines(id, ROUTE_LINE);
    assert_eq!(route.len(), 1, "{route:?}");
    assert!(route[0].starts_with("COMPLIANCE_FAILED: second route:"), "{route:?}");
    let sized = r.lines(id, "Second route source: 1280x720");
    assert_eq!(sized.len(), 1, "{:?}", r.lines(id, "Second route"));
    assert_eq!(r.lines(id, "The second route made a compliant file").len(), 1);
    assert!(r.repo.second_route_at(id).unwrap().is_some());
    assert!(source.exists(), "the attachment itself is never touched");
}

/// A stopping daemon starts no second route and does not use up the job's:
/// the first failure stands and nothing is recorded.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stopping_daemon_does_not_take_the_second_route() {
    let Some(t) = tools("the stopping test") else { return };
    let r = rig(&t, PathBuf::from("no-such-yt-dlp.exe"), true);
    let source = r.root.join("attachments").join("1").join("clip.mkv");
    mislabelled_mkv(&t, &source, 4.0).await;
    r.engine.stop_new_deliveries();

    let job = r.attachment(&source);
    let id = job.id;
    // A second route taken anyway would make the file and then wait before the
    // delivery for good (the stop's rule), so the run is bounded here.
    let run = tokio::time::timeout(std::time::Duration::from_secs(120), r.engine.process_job(job, OWNER)).await;
    let err = run.expect("a stopping daemon took the second route and waited at the delivery").expect_err("the first route fails the gate");
    assert_eq!(omni_broadcast::errors::code_in_text(&format!("{err:#}")), Some(omni_broadcast::errors::ErrorCode::ComplianceFailed), "{err:#}");
    assert!(r.lines(id, ROUTE_LINE).is_empty());
    assert_eq!(r.repo.second_route_at(id).unwrap(), None, "the job keeps its second route for a later run");
    assert!(r.delivered().is_empty());
}

/// The control of the test above: the engine's second route is off unless the
/// daemon turns it on, and then the same source fails as it always did.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn without_the_second_route_the_same_stream_still_fails() {
    let Some(t) = tools("the second-route-off test") else { return };
    let r = rig(&t, PathBuf::from("no-such-yt-dlp.exe"), false);
    let source = r.root.join("attachments").join("1").join("clip.ts");
    jumping_ts(&t, &source).await;

    let job = r.attachment(&source);
    let id = job.id;
    let err = r.engine.process_job(job, OWNER).await.expect_err("the first route alone cannot make it");
    assert_eq!(omni_broadcast::errors::code_in_text(&format!("{err:#}")), Some(omni_broadcast::errors::ErrorCode::TranscodeFailed), "{err:#}");
    assert!(r.delivered().is_empty());
    assert!(r.lines(id, ROUTE_LINE).is_empty());
    assert_eq!(r.repo.second_route_at(id).unwrap(), None);
}

/// A truncated source: a remux would turn a video cut in half into a shorter,
/// honest-looking file and put half a story on air. It is refused, the job
/// keeps its first failure, nothing reaches the watchfolder, and a second run
/// of the same job does not take the route again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cut_source_is_not_remuxed_into_a_shorter_video_and_the_route_is_taken_once() {
    let Some(t) = tools("the cut-source test") else { return };
    let r = rig(&t, PathBuf::from("no-such-yt-dlp.exe"), true);
    let source = r.root.join("attachments").join("1").join("clip.mp4");
    cut_mp4(&t, &source).await;

    let job = r.attachment(&source);
    let id = job.id;
    let err = r.engine.process_job(job, OWNER).await.expect_err("half a video is never delivered");
    assert_eq!(omni_broadcast::errors::code_in_text(&format!("{err:#}")), Some(omni_broadcast::errors::ErrorCode::TranscodeFailed), "{err:#}");
    assert!(r.delivered().is_empty(), "{:?}", r.delivered());
    assert_eq!(r.lines(id, ROUTE_LINE).len(), 1);
    let refused = r.lines(id, "The second route did not help");
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert!(refused[0].contains("part of it is missing"), "{refused:?}");
    assert!(!r.root.join("temp").join("jobs").join(id.to_string()).exists());

    // The job ends and is run again (a retry, a restart): no second route this time.
    assert!(r.repo.finish(id, OWNER, JobStatus::RequiresReview, Some("TRANSCODE_FAILED"), Some("cut"), None).unwrap());
    assert!(r.repo.retry_job(id, None).unwrap());
    let job = r.lease(id);
    let err = r.engine.process_job(job, OWNER).await.expect_err("still cut");
    assert_eq!(omni_broadcast::errors::code_in_text(&format!("{err:#}")), Some(omni_broadcast::errors::ErrorCode::TranscodeFailed));
    assert_eq!(r.lines(id, ROUTE_LINE).len(), 1, "the route is taken once per job");
    assert!(r.delivered().is_empty());
}

/// A variable-frame-rate clip (frames dropped unevenly) survives the remux
/// with its pictures and still makes a compliant file through the usual encode
/// and gate: the remux the second route adds never breaks a source it is given.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_variable_frame_rate_clip_is_remuxed_and_still_makes_a_compliant_file() {
    let Some(t) = tools("the VFR test") else { return };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("vfr.mp4");
    // Every third and fourth frame of each ten dropped, timestamps kept: VFR.
    ffmpeg(
        &t,
        &[
            "-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30:duration=6", "-f", "lavfi", "-i",
            "sine=frequency=1000:sample_rate=48000:duration=6", "-vf", "select='not(between(mod(n\\,10)\\,3\\,4))'",
            "-fps_mode", "vfr", "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest",
            &src.to_string_lossy(),
        ],
    )
    .await;

    let work = dir.path().join("work");
    let remuxed = omni_broadcast::second_route::remux(&t.ffmpeg, &t.ffprobe, &src, &work, None).await.expect("remux");
    assert_eq!(remuxed.path, work.join(omni_broadcast::second_route::REMUX_NAME));
    let remuxed = remuxed.path;
    let before = omni_broadcast::probe::probe(&t.ffprobe, &src).await.unwrap();
    let after = omni_broadcast::probe::probe(&t.ffprobe, &remuxed).await.unwrap();
    assert!((before.duration_secs - after.duration_secs).abs() < 0.2, "{} vs {}", before.duration_secs, after.duration_secs);
    assert!(after.audio.is_some(), "the audio is copied too");

    let (mxf, secs) = omni_broadcast::Transcoder::new(&t.ffmpeg, &t.ffprobe)
        .transcode(3, &remuxed, &work, |_| {})
        .await
        .expect("encode");
    let wrapped = omni_broadcast::Rewrapper::new(&t.bmx).rewrap_with_clip(3, &mxf, &work, Some(SLUG), None).await.expect("wrap");
    let report = omni_broadcast::verify::verify_mxf_with_clip(&t.ffprobe, &wrapped, secs, Some(SLUG)).await.expect("gate ran");
    assert!(report.pass, "{}", report.summary());
}

/// The download itself: the link's first download arrives cut (its encode is
/// short of what the file says, TRANSCODE_FAILED); the second route downloads
/// it again, whole this time, and delivers it. yt-dlp from `bin/` fetches from
/// a loopback server that serves the whole file only once the job has taken
/// its second route, so the order is the engine's, not a timer's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_link_whose_first_download_was_cut_is_downloaded_again_and_delivered() {
    let Some(t) = tools("the second-download test") else { return };
    let Some(ytdl) = bin("yt-dlp") else {
        eprintln!("WARNING: skipping the second-download test -- yt-dlp not in bin/");
        return;
    };
    let r = rig(&t, ytdl, true);
    let served = r.root.join("served").join("clip.mp4");
    let whole = cut_mp4(&t, &served).await;
    let cut = std::fs::read(&served).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/clip.mp4", listener.local_addr().unwrap());
    let id = r.repo.enqueue(&NewJob::new(url.as_str(), SLUG, "KALOGEROU"), DEFAULT_DEDUP_WINDOW_HOURS).unwrap().job_id();
    serve(listener, r.repo.clone(), id, cut, whole);
    let job = r.lease(id);

    let outcome = r.engine.process_job_mode(job, OWNER, None, None, None, None, omni_broadcast::pipeline::DeliveryMode::Deliver).await;
    match outcome {
        Ok(RunOutcome::Delivered) => {}
        other => panic!("the second download should deliver: {other:?}"),
    }
    let route = r.lines(id, ROUTE_LINE);
    assert_eq!(route.len(), 1, "{route:?}");
    assert!(route[0].contains("downloading again with other formats"), "{route:?}");
    assert_eq!(r.lines(id, "Second route source: 1280x720 h264").len(), 1, "the new download's size is named");
    let secs = mxf_secs(&t, &r.root.join("watchfolder").join(format!("{SLUG}.mxf"))).await;
    assert!((secs - 10.0).abs() < 0.5, "the whole video: {secs} s");
}

/// Serves `cut` at `/clip.mp4` until job `id` has taken its second route, then `whole`.
fn serve(listener: tokio::net::TcpListener, repo: Repository, id: i64, cut: Vec<u8>, whole: Vec<u8>) {
    use axum::{extract::State, http::header, routing::get, Router};
    struct Files {
        repo: Repository,
        id: i64,
        cut: Vec<u8>,
        whole: Vec<u8>,
    }
    async fn clip(State(f): State<Arc<Files>>) -> ([(header::HeaderName, &'static str); 1], Vec<u8>) {
        let taken = f.repo.second_route_at(f.id).unwrap().is_some();
        ([(header::CONTENT_TYPE, "video/mp4")], if taken { f.whole.clone() } else { f.cut.clone() })
    }
    let app = Router::new().route("/clip.mp4", get(clip)).with_state(Arc::new(Files { repo, id, cut, whole }));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
}
