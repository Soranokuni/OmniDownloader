//! The repair gate and the image hash with real ffmpeg/ffprobe and a local HTTP
//! server (plan P10.9). Needs `bin/ffmpeg.exe` and `bin/ffprobe.exe`; skipped
//! with a visible warning without them. No network: the media is served from
//! 127.0.0.1 and a fake public name is mapped to it.

use std::collections::BTreeSet;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::{extract::State, routing::get, Router};
use omni_broadcast::imagehash::{best_match, frame_hashes, hamming, image_hash, is_match};
use omni_broadcast::repair_gate::{
    verify, verify_allowing_loopback_for_tests, Candidate, GateFacts, ProbeInfo, Prober, Resolver, ToolProber,
};
use omni_core::net::EgressError;
use omni_core::process::{self, RunOpts};

fn bin(name: &str) -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bin").join(format!("{name}.exe"));
    p.exists().then_some(p)
}

fn tools() -> Option<(PathBuf, PathBuf)> {
    match (bin("ffmpeg"), bin("ffprobe")) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => {
            eprintln!("WARNING: bin/ffmpeg.exe or bin/ffprobe.exe is missing; repair gate integration test skipped");
            None
        }
    }
}

async fn ffmpeg(ffmpeg: &Path, args: &[&str]) {
    let a: Vec<String> = ["-v", "error", "-y"].iter().chain(args.iter()).map(|s| s.to_string()).collect();
    let out = process::run(ffmpeg, &a, RunOpts::new(Duration::from_secs(120))).await.unwrap();
    assert!(out.success, "ffmpeg {args:?} failed: {}", out.stderr_tail);
}

/// A clip with a moving test picture and a tone; faststart so a plain HTTP
/// server without range support can serve it.
async fn make_clip(ff: &Path, dir: &Path, name: &str, source: &str, secs: u32) -> PathBuf {
    let out = dir.join(name);
    let src = format!("{source}=size=640x360:rate=25");
    let tone = "sine=frequency=440:sample_rate=48000";
    ffmpeg(
        ff,
        &[
            "-f", "lavfi", "-i", &src, "-f", "lavfi", "-i", tone, "-t", &secs.to_string(), "-c:v", "libx264", "-preset", "ultrafast",
            "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest", "-movflags", "+faststart", &out.to_string_lossy(),
        ],
    )
    .await;
    out
}

async fn serve(dir: PathBuf) -> SocketAddr {
    async fn file(State(dir): State<Arc<PathBuf>>, axum::extract::Path(name): axum::extract::Path<String>) -> Vec<u8> {
        std::fs::read(dir.join(name)).unwrap_or_default()
    }
    let app = Router::new().route("/:name", get(file)).with_state(Arc::new(dir));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

const FAKE_HOST: &str = "video.example.test";

/// Maps the fake public name to the local server before the real probe.
struct Rewriting {
    inner: ToolProber,
    addr: SocketAddr,
}

#[async_trait]
impl Prober for Rewriting {
    async fn probe(&self, cand: &Candidate) -> Result<ProbeInfo, String> {
        let mut c = cand.clone();
        c.url = cand.url.replace(FAKE_HOST, &self.addr.to_string());
        self.inner.probe(&c).await
    }
}

struct MapsToLoopback;

#[async_trait]
impl Resolver for MapsToLoopback {
    async fn resolve(&self, _host: &str, _port: u16) -> Result<Vec<IpAddr>, EgressError> {
        Ok(vec!["127.0.0.1".parse().unwrap()])
    }
}

fn candidate(url: &str) -> Candidate {
    Candidate { url: url.into(), referer: Some("https://news.example/story".into()), origin: "recipe:abcd1234".into(), position: None, in_article: None }
}

#[tokio::test]
async fn real_ffprobe_reads_the_clips_and_the_gate_keeps_the_long_one() {
    let Some((ff, fp)) = tools() else { return };
    let dir = tempfile::tempdir().unwrap();
    make_clip(&ff, dir.path(), "short.mp4", "testsrc2", 8).await;
    make_clip(&ff, dir.path(), "long.mp4", "testsrc2", 20).await;
    let addr = serve(dir.path().to_path_buf()).await;

    // The real prober, straight at the local server.
    let tool = ToolProber::new(fp.clone(), None);
    let info = tool.probe(&candidate(&format!("http://{addr}/long.mp4"))).await.unwrap();
    assert_eq!(info.height, 360);
    assert_eq!(info.video_streams, 1);
    assert!((info.duration_secs.unwrap() - 20.0).abs() < 0.5, "{info:?}");
    let info = tool.probe(&candidate(&format!("http://{addr}/short.mp4"))).await.unwrap();
    assert!((info.duration_secs.unwrap() - 8.0).abs() < 0.5, "{info:?}");
    assert!(tool.probe(&candidate(&format!("http://{addr}/missing.mp4"))).await.is_err());

    // The gate, with the fake name mapped to the server.
    let facts = GateFacts { observed_hosts: BTreeSet::from([FAKE_HOST.to_string()]), ..Default::default() };
    let prober = Rewriting { inner: ToolProber::new(fp, None), addr };
    let cands = vec![candidate(&format!("http://{FAKE_HOST}/short.mp4")), candidate(&format!("http://{FAKE_HOST}/long.mp4"))];
    let deadline = Instant::now() + Duration::from_secs(60);
    let (ok, rej) = verify_allowing_loopback_for_tests(cands.clone(), &facts, &|_| false, &prober, &MapsToLoopback, deadline).await;
    assert_eq!(ok.len(), 1, "{rej:?}");
    assert!(ok[0].candidate.url.ends_with("long.mp4"));
    assert!((ok[0].duration_secs - 20.0).abs() < 0.5);
    assert_eq!(ok[0].height, 360);
    assert_eq!(rej.len(), 1);
    assert!(rej[0].candidate.url.ends_with("short.mp4"));

    // The production entry point refuses the same loopback answer.
    let (ok, rej) = verify(cands, &facts, &|_| false, &prober, &MapsToLoopback, deadline).await;
    assert!(ok.is_empty());
    assert_eq!(rej.len(), 2);
    assert!(rej.iter().all(|r| r.checks.last().unwrap().name == "egress"));
}

#[tokio::test]
async fn frame_hashes_match_a_reencoded_frame_and_tell_other_pictures_apart() {
    let Some((ff, _)) = tools() else { return };
    let dir = tempfile::tempdir().unwrap();
    let a = make_clip(&ff, dir.path(), "a.mp4", "testsrc2", 60).await;
    let b = make_clip(&ff, dir.path(), "b.mp4", "mandelbrot", 12).await;
    let work = dir.path().join("work");
    std::fs::create_dir(&work).unwrap();

    let frames = frame_hashes(&ff, &a, 60.0, &work).await;
    assert!((17..=20).contains(&frames.len()), "{} frames", frames.len());
    assert!(frames.windows(2).all(|w| w[1].0 > w[0].0));
    assert!(std::fs::read_dir(&work).unwrap().next().is_none(), "the raw file is removed");

    // The same moment as a PNG and as a heavily compressed JPEG.
    let png = dir.path().join("f.png");
    let jpg = dir.path().join("f.jpg");
    ffmpeg(&ff, &["-ss", "3", "-i", &a.to_string_lossy(), "-frames:v", "1", &png.to_string_lossy()]).await;
    ffmpeg(&ff, &["-i", &png.to_string_lossy(), "-q:v", "10", &jpg.to_string_lossy()]).await;
    let hp = image_hash(&ff, &png, &work).await.unwrap();
    let hj = image_hash(&ff, &jpg, &work).await.unwrap();
    assert!(hamming(hp, hj) <= 10, "re-encode moved the hash by {}", hamming(hp, hj));
    let (_, d) = best_match(hj, &frames).unwrap();
    assert!(is_match(d), "the frame is among the sampled ones, distance {d}");

    // A different picture does not match any frame of this clip.
    let other = dir.path().join("o.png");
    ffmpeg(&ff, &["-ss", "3", "-i", &b.to_string_lossy(), "-frames:v", "1", &other.to_string_lossy()]).await;
    let ho = image_hash(&ff, &other, &work).await.unwrap();
    let (_, d) = best_match(ho, &frames).unwrap();
    assert!(d >= 20, "a different picture must stay far away, distance {d}");

    assert!(image_hash(&ff, &dir.path().join("missing.png"), &work).await.is_none());
}
