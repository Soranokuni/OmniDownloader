//! The approval card's evidence (plan P10.11b) with real ffmpeg: the article's
//! picture against the frames of a generated `_preview.mp4`. Needs
//! `bin/ffmpeg.exe`; skipped with a visible warning without it. No network:
//! the picture comes from a fake source. All names and addresses are invented.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use omni_broadcast::card::{card_evidence_capped, ImageEvidence, ImageSource, CARD_CAP, SOURCE_FILE};
use omni_broadcast::imagehash::is_match;
use omni_broadcast::preview::{ARTICLE_FILE, FRAME_FILE, VIDEO_FILE};
use omni_broadcast::repair_gate::GateFacts;
use omni_core::process::{self, RunOpts};

fn ffmpeg_path() -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bin/ffmpeg.exe");
    if p.exists() {
        Some(p)
    } else {
        eprintln!("WARNING: bin/ffmpeg.exe is missing; card evidence integration test skipped");
        None
    }
}

async fn ff(ffmpeg: &Path, args: &[&str]) {
    let a: Vec<String> = ["-v", "error", "-y", "-nostdin"].iter().chain(args.iter()).map(|s| s.to_string()).collect();
    let out = process::run(ffmpeg, &a, RunOpts::new(Duration::from_secs(120))).await.unwrap();
    assert!(out.success, "ffmpeg {args:?} failed: {}", out.stderr_tail);
}

/// A `_preview.mp4` of `secs` seconds from a lavfi `source`, no audio needed.
async fn make_preview(ffmpeg: &Path, dir: &Path, source: &str, secs: u32) {
    let src = format!("{source}=size=640x360:rate=25");
    ff(
        ffmpeg,
        &["-f", "lavfi", "-i", &src, "-t", &secs.to_string(), "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", &dir.join(VIDEO_FILE).to_string_lossy()],
    )
    .await;
}

/// One frame of a lavfi source at second `at`, as a PNG body.
async fn picture(ffmpeg: &Path, dir: &Path, source: &str, at: u32, ext: &str) -> Vec<u8> {
    let out = dir.join(format!("pic_{source}_{at}.{ext}"));
    let src = format!("{source}=size=640x360:rate=25");
    ff(ffmpeg, &["-f", "lavfi", "-i", &src, "-ss", &at.to_string(), "-frames:v", "1", &out.to_string_lossy()]).await;
    std::fs::read(out).unwrap()
}

fn facts() -> GateFacts {
    GateFacts {
        observed_hosts: BTreeSet::from(["img.news.example".to_string()]),
        page_domain: Some("news.example".to_string()),
        ..Default::default()
    }
}

const URL: &str = "https://img.news.example/a.png";

enum Reply {
    Bytes(Vec<u8>),
    Fail,
    Hang,
}

/// Answers by call order (the last answer repeats) and counts the requests.
struct Fake {
    replies: Vec<Reply>,
    calls: AtomicUsize,
}

impl Fake {
    fn new(replies: Vec<Reply>) -> Self {
        Self { replies, calls: AtomicUsize::new(0) }
    }
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl ImageSource for Fake {
    async fn get_image(&self, _url: &str, _deadline: Instant) -> Result<Vec<u8>> {
        let i = self.calls.fetch_add(1, Ordering::SeqCst).min(self.replies.len() - 1);
        match &self.replies[i] {
            Reply::Bytes(b) => Ok(b.clone()),
            Reply::Fail => Err(anyhow::anyhow!("scripted failure")),
            Reply::Hang => {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                unreachable!()
            }
        }
    }
}

async fn run(ffmpeg: &Path, dir: &Path, urls: &[&str], src: &Fake, secs: f64) -> ImageEvidence {
    let urls: Vec<String> = urls.iter().map(|s| s.to_string()).collect();
    card_evidence_capped(ffmpeg, dir, &urls, &facts(), secs, src, CARD_CAP).await
}

fn leftovers(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

#[tokio::test]
async fn a_picture_that_is_a_frame_of_the_clip_matches_that_frame() {
    let Some(ffm) = ffmpeg_path() else { return };
    let dir = tempfile::tempdir().unwrap();
    make_preview(&ffm, dir.path(), "mandelbrot", 12).await;
    let body = picture(&ffm, dir.path(), "mandelbrot", 6, "png").await;
    let src = Fake::new(vec![Reply::Bytes(body)]);
    let ev = run(&ffm, dir.path(), &[URL], &src, 12.0).await;
    assert_eq!(ev.host.as_deref(), Some("img.news.example"));
    let d = ev.distance.expect("compared");
    assert!(is_match(d), "the same picture must match, distance {d}");
    let t = ev.frame_secs.unwrap();
    assert!((t - 6.0).abs() <= 1.0, "closest frame at {t}, true time 6");
    assert!(dir.path().join(ARTICLE_FILE).is_file() && dir.path().join(FRAME_FILE).is_file());
    assert!(!dir.path().join(SOURCE_FILE).exists(), "the raw download is always deleted");
    // Small JPEGs: 320 wide.
    assert!(std::fs::read(dir.path().join(ARTICLE_FILE)).unwrap().starts_with(&[0xFF, 0xD8]));
}

#[tokio::test]
async fn an_unrelated_picture_is_no_match_but_is_still_kept_for_the_card() {
    let Some(ffm) = ffmpeg_path() else { return };
    let dir = tempfile::tempdir().unwrap();
    make_preview(&ffm, dir.path(), "mandelbrot", 12).await;
    let body = picture(&ffm, dir.path(), "testsrc2", 3, "png").await;
    let ev = run(&ffm, dir.path(), &[URL], &Fake::new(vec![Reply::Bytes(body)]), 12.0).await;
    let d = ev.distance.expect("compared");
    assert!(!is_match(d), "an unrelated picture must not match, distance {d}");
    assert_eq!(ev.host.as_deref(), Some("img.news.example"), "no match is neutral: the picture is still shown");
    assert!(dir.path().join(ARTICLE_FILE).is_file());
}

#[tokio::test]
async fn a_svg_garbage_or_undecodable_body_leaves_no_picture_and_no_raw_file() {
    let Some(ffm) = ffmpeg_path() else { return };
    let bodies: Vec<Vec<u8>> = vec![
        b"<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"10\" height=\"10\"><rect width=\"10\" height=\"10\"/></svg>".to_vec(),
        b"#EXTM3U\n#EXT-X-VERSION:3\nfile:///C:/Windows/win.ini\n".to_vec(),
        vec![0u8; 5000],
        // A JPEG header, then nothing a decoder can use: ffmpeg must say no.
        [&[0xFF, 0xD8, 0xFF, 0xE0][..], &[7u8; 300][..]].concat(),
    ];
    for (i, body) in bodies.into_iter().enumerate() {
        let dir = tempfile::tempdir().unwrap();
        make_preview(&ffm, dir.path(), "mandelbrot", 4).await;
        let ev = run(&ffm, dir.path(), &[URL], &Fake::new(vec![Reply::Bytes(body)]), 4.0).await;
        assert_eq!(ev, ImageEvidence::default(), "body #{i}");
        assert_eq!(leftovers(dir.path()), [VIDEO_FILE], "body #{i}: nothing but the preview is left");
    }
}

#[tokio::test]
async fn a_host_the_page_never_used_or_a_private_one_is_refused_before_any_request() {
    let Some(ffm) = ffmpeg_path() else { return };
    let dir = tempfile::tempdir().unwrap();
    make_preview(&ffm, dir.path(), "mandelbrot", 4).await;
    let body = picture(&ffm, dir.path(), "mandelbrot", 2, "png").await;
    let src = Fake::new(vec![Reply::Bytes(body)]);
    let urls = ["https://elsewhere.example/a.png", "http://127.0.0.1/a.png", "http://10.0.0.5/a.png", "http://localhost/a.png", "file:///C:/a.png"];
    let ev = run(&ffm, dir.path(), &urls, &src, 4.0).await;
    assert_eq!(ev, ImageEvidence::default());
    assert_eq!(src.calls(), 0, "no request may be made to any of them");
}

#[tokio::test]
async fn no_preview_video_keeps_the_picture_with_no_distance() {
    let Some(ffm) = ffmpeg_path() else { return };
    let dir = tempfile::tempdir().unwrap();
    let body = picture(&ffm, dir.path(), "mandelbrot", 2, "png").await;
    let ev = run(&ffm, dir.path(), &[URL], &Fake::new(vec![Reply::Bytes(body)]), 4.0).await;
    assert_eq!(ev.host.as_deref(), Some("img.news.example"));
    assert_eq!((ev.distance, ev.frame_secs), (None, None), "nothing to compare with");
    assert!(dir.path().join(ARTICLE_FILE).is_file());
    assert!(!dir.path().join(FRAME_FILE).exists());
}

#[tokio::test]
async fn the_next_address_is_tried_only_after_a_failure_and_never_more_than_three() {
    let Some(ffm) = ffmpeg_path() else { return };
    let dir = tempfile::tempdir().unwrap();
    make_preview(&ffm, dir.path(), "mandelbrot", 4).await;
    let good = picture(&ffm, dir.path(), "mandelbrot", 2, "png").await;
    // fetch fails, garbage, then a good one.
    let src = Fake::new(vec![Reply::Fail, Reply::Bytes(vec![1, 2, 3]), Reply::Bytes(good.clone())]);
    let urls = ["https://img.news.example/1.png", "https://img.news.example/2.png", "https://img.news.example/3.png", "https://img.news.example/4.png"];
    let ev = run(&ffm, dir.path(), &urls, &src, 4.0).await;
    assert!(ev.host.is_some());
    assert_eq!(src.calls(), 3, "stops at the first picture that decodes");
    // Three failures: the fourth is never requested.
    let dir2 = tempfile::tempdir().unwrap();
    let src = Fake::new(vec![Reply::Fail]);
    let ev = run(&ffm, dir2.path(), &urls, &src, 4.0).await;
    assert_eq!(ev, ImageEvidence::default());
    assert_eq!(src.calls(), 3);
    // A working first picture means the others are not even asked for.
    let src = Fake::new(vec![Reply::Bytes(good)]);
    run(&ffm, dir.path(), &urls, &src, 4.0).await;
    assert_eq!(src.calls(), 1);
}

#[tokio::test]
async fn the_cap_ends_a_hanging_fetch_and_leaves_nothing_behind() {
    let Some(ffm) = ffmpeg_path() else { return };
    let dir = tempfile::tempdir().unwrap();
    make_preview(&ffm, dir.path(), "mandelbrot", 4).await;
    // A leftover from an earlier hold must not survive either.
    std::fs::write(dir.path().join(ARTICLE_FILE), b"old").unwrap();
    let src = Fake::new(vec![Reply::Hang]);
    let started = Instant::now();
    let urls = vec![URL.to_string()];
    let ev = card_evidence_capped(&ffm, dir.path(), &urls, &facts(), 4.0, &src, Duration::from_millis(700)).await;
    assert!(started.elapsed() < Duration::from_secs(5), "the cap must end the wait, took {:?}", started.elapsed());
    assert_eq!(ev, ImageEvidence::default());
    assert_eq!(leftovers(dir.path()), [VIDEO_FILE]);
}

#[tokio::test]
async fn a_picture_declaring_an_enormous_canvas_is_refused_quickly() {
    let Some(ffm) = ffmpeg_path() else { return };
    let dir = tempfile::tempdir().unwrap();
    make_preview(&ffm, dir.path(), "mandelbrot", 4).await;
    // 9000 x 9000 of one colour: a small file, 81 megapixels once decoded.
    ff(&ffm, &["-f", "lavfi", "-i", "color=c=red:size=9000x9000", "-frames:v", "1", &dir.path().join("big.png").to_string_lossy()]).await;
    let body = std::fs::read(dir.path().join("big.png")).unwrap();
    assert!(body.len() < 2 << 20, "the test file must fit the 2 MiB cap ({} bytes)", body.len());
    let started = Instant::now();
    let ev = run(&ffm, dir.path(), &[URL], &Fake::new(vec![Reply::Bytes(body)]), 4.0).await;
    assert_eq!(ev, ImageEvidence::default());
    // What proves the refusal is the missing picture; the bound only catches
    // a decode that runs on and on (it was 5 s and failed under a loaded gate).
    assert!(started.elapsed() < Duration::from_secs(30), "took {:?}", started.elapsed());
    assert!(!dir.path().join(ARTICLE_FILE).exists());
    assert!(!dir.path().join("_card_work").exists(), "the work directory is removed");
}

#[tokio::test]
async fn a_flat_picture_is_kept_but_never_compared() {
    let Some(ffm) = ffmpeg_path() else { return };
    let dir = tempfile::tempdir().unwrap();
    // A black preview: every frame is flat too, so a naive comparison would call it a perfect match.
    ff(&ffm, &["-f", "lavfi", "-i", "color=c=black:size=640x360:rate=25", "-t", "4", "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", &dir.path().join(VIDEO_FILE).to_string_lossy()]).await;
    ff(&ffm, &["-f", "lavfi", "-i", "color=c=black:size=640x360", "-frames:v", "1", &dir.path().join("flat.png").to_string_lossy()]).await;
    let body = std::fs::read(dir.path().join("flat.png")).unwrap();
    let ev = run(&ffm, dir.path(), &[URL], &Fake::new(vec![Reply::Bytes(body)]), 4.0).await;
    assert_eq!(ev.host.as_deref(), Some("img.news.example"), "the picture is still shown");
    assert_eq!((ev.distance, ev.frame_secs), (None, None), "but a flat picture is no comparison");
    assert!(dir.path().join(ARTICLE_FILE).is_file() && !dir.path().join(FRAME_FILE).exists());
}

/// A preview with a key frame every second: the match is found at the true second.
#[tokio::test]
async fn key_frames_give_the_true_time_not_the_nearest_fixed_sample() {
    let Some(ffm) = ffmpeg_path() else { return };
    let dir = tempfile::tempdir().unwrap();
    let src = "mandelbrot=size=640x360:rate=25";
    ff(
        &ffm,
        &["-f", "lavfi", "-i", src, "-t", "40", "-c:v", "libx264", "-preset", "ultrafast", "-g", "25", "-keyint_min", "25", "-sc_threshold", "0", "-pix_fmt", "yuv420p", &dir.path().join(VIDEO_FILE).to_string_lossy()],
    )
    .await;
    // 21 s is a key frame; the fixed samples of a 40 s clip are at 0-9 s, 10, 14, 18, 22 ...
    let body = picture(&ffm, dir.path(), "mandelbrot", 21, "png").await;
    let ev = run(&ffm, dir.path(), &[URL], &Fake::new(vec![Reply::Bytes(body)]), 40.0).await;
    assert!(ev.distance.is_some_and(is_match), "{ev:?}");
    let t = ev.frame_secs.unwrap();
    assert!((t - 21.0).abs() < 0.3, "the key frame at 21 s, got {t}");
}

/// ffmpeg as the comparison sees it: the key-frame pass never ends (a cmd shim),
/// every other call is the real one.
#[tokio::test]
async fn when_the_comparison_runs_out_of_time_the_complete_picture_stays_without_a_distance() {
    let Some(ffm) = ffmpeg_path() else { return };
    let dir = tempfile::tempdir().unwrap();
    make_preview(&ffm, dir.path(), "mandelbrot", 4).await;
    let body = picture(&ffm, dir.path(), "mandelbrot", 2, "png").await;
    let shim = dir.path().join("ffshim.cmd");
    let real = std::fs::canonicalize(&ffm).unwrap();
    let real = real.to_string_lossy().replace("\\\\?\\", "").to_string();
    std::fs::write(&shim, format!("@echo off\r\nfor %%a in (%*) do if \"%%a\"==\"-skip_frame\" ping -n 60 127.0.0.1 >nul\r\n\"{real}\" %*\r\n")).unwrap();
    // The shim passes ordinary calls through untouched.
    let probe = process::run(&shim, ["-v", "error", "-y", "-i", &dir.path().join(VIDEO_FILE).to_string_lossy(), "-frames:v", "1", "-vf", "scale=64:-2", &dir.path().join("probe.jpg").to_string_lossy()], RunOpts::new(Duration::from_secs(30))).await.unwrap();
    assert!(probe.success && dir.path().join("probe.jpg").is_file(), "the shim must pass calls through: {} / {}", probe.stderr_tail, std::fs::read_to_string(&shim).unwrap());
    let urls = vec![URL.to_string()];
    let src = Fake::new(vec![Reply::Bytes(body)]);
    let started = Instant::now();
    // 20 s: the picture phase must finish inside the cap even on a loaded
    // machine (6 s failed during a full gate); the shim's comparison hangs 60 s.
    let ev = card_evidence_capped(&shim, dir.path(), &urls, &facts(), 4.0, &src, Duration::from_secs(20)).await;
    assert!(started.elapsed() < Duration::from_secs(40), "took {:?}", started.elapsed());
    assert_eq!(ev.host.as_deref(), Some("img.news.example"));
    assert_eq!((ev.distance, ev.frame_secs), (None, None));
    assert!(dir.path().join(ARTICLE_FILE).is_file(), "the finished picture is kept");
    assert!(!dir.path().join(FRAME_FILE).exists());
    assert!(!dir.path().join("_card_work").exists());
}
