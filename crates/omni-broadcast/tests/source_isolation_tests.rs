//! A source file can never make ffprobe or ffmpeg open a network address, a UNC
//! path or another file (plan P2.11).
//!
//! An emailed `video/*` attachment is saved as `source.<original extension>`, so
//! `source.m3u8` reaches ffprobe with no human step. The argument tests pin the
//! `-protocol_whitelist file` input option; the integration tests prove the real
//! tools honour it. Integration tests skip with a visible warning when `bin/` has
//! no ffmpeg/ffprobe.

use std::path::{Path, PathBuf};
use std::time::Duration;

use omni_broadcast::preview::{contact_args, preview_args, PreviewEncoder};
use omni_broadcast::probe::{self, AudioStream, ScanType, SourceProbe, VideoStream, SOURCE_DEMUXERS};
use omni_broadcast::transcoder::{build_loudness_measure_args, build_plan, LoudnessTarget};
use omni_core::process::{run, RunOpts};

fn source_probe() -> SourceProbe {
    SourceProbe {
        duration_secs: 10.0,
        size_bytes: 1000,
        video: VideoStream {
            index: 0,
            codec: "h264".into(),
            width: 1280,
            height: 720,
            fps: 25.0,
            scan: ScanType::Progressive,
            pix_fmt: Some("yuv420p".into()),
        },
        audio: Some(AudioStream {
            index: 1,
            codec: "aac".into(),
            channels: 2,
            channel_layout: None,
            sample_rate: 48_000,
            language: None,
            is_default: true,
        }),
        audio_stream_count: 1,
        container: "mov,mp4".into(),
    }
}

/// Every position of `value` in `args` that is an input (`-i value`).
fn input_positions(args: &[String], value: &str) -> Vec<usize> {
    (1..args.len()).filter(|&i| args[i] == value && args[i - 1] == "-i").collect()
}

/// `-protocol_whitelist file` sits directly before the `-i` of `source`.
fn assert_whitelisted(args: &[String], source: &str) {
    assert_protocol_whitelisted(args, source);
    let i = input_positions(args, source)[0];
    assert!(i >= 6, "{args:?}");
    assert_eq!(&args[i - 5..i - 3], ["-format_whitelist", SOURCE_DEMUXERS], "{args:?}");
}

/// Only the `-protocol_whitelist file` half (the preview builders read our own MXF).
fn assert_protocol_whitelisted(args: &[String], source: &str) {
    let at = input_positions(args, source);
    assert_eq!(at.len(), 1, "source input not found exactly once in {args:?}");
    let i = at[0]; // index of the path; i-1 is "-i"
    assert!(i >= 4, "{args:?}");
    assert_eq!(&args[i - 3..i - 1], ["-protocol_whitelist", "file"], "{args:?}");
}

#[test]
fn the_main_encode_whitelists_the_source_but_not_the_lavfi_silence_input() {
    let src = "C:/temp/jobs/7/source.m3u8";
    let plan = build_plan(
        &source_probe(),
        &LoudnessTarget::default(),
        None,
        Path::new(src),
        Path::new("C:/temp/jobs/7/7_transcode.mxf"),
    );
    assert_whitelisted(&plan.args, src);
    assert_eq!(plan.args.iter().filter(|a| *a == "-protocol_whitelist").count(), 1, "only the source input");
    assert_eq!(plan.args.iter().filter(|a| *a == "-format_whitelist").count(), 1, "only the source input");
    // The silence input is still `-f lavfi -i <graph>`, untouched.
    let lavfi = plan.args.iter().position(|a| a == "lavfi").unwrap();
    assert_eq!(plan.args[lavfi - 1], "-f");
    assert_eq!(plan.args[lavfi + 1], "-i");
}

#[test]
fn the_loudness_measurement_pass_whitelists_the_source() {
    let src = "C:/temp/jobs/7/source.m3u8";
    let args = build_loudness_measure_args(&source_probe(), &LoudnessTarget::default(), Path::new(src)).unwrap();
    assert_whitelisted(&args, src);
}

#[test]
fn both_preview_builders_whitelist_the_input() {
    let src = "C:/temp/held/clip.mxf";
    for enc in [PreviewEncoder::X264, PreviewEncoder::None] {
        let args = preview_args(Path::new(src), Path::new("C:/temp/p.mp4"), enc);
        assert_protocol_whitelisted(&args, src);
    }
    assert_protocol_whitelisted(&contact_args(Path::new(src), Path::new("C:/temp/c.jpg"), 30.0), src);
}

#[test]
fn ffprobe_args_whitelist_the_source() {
    let a = probe::probe_args(Path::new("C:/temp/jobs/7/source.m3u8"));
    let n = a.len();
    assert_eq!(&a[n - 5..], ["-format_whitelist", SOURCE_DEMUXERS, "-protocol_whitelist", "file", "C:/temp/jobs/7/source.m3u8"]);
}

// ------------------------------------------------------------- real tools --

fn tool(name: &str) -> Option<PathBuf> {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join("bin").join(format!("{name}.exe"));
    p.exists().then_some(p)
}

fn toolchain() -> Option<(PathBuf, PathBuf)> {
    match (tool("ffmpeg"), tool("ffprobe")) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => {
            eprintln!(
                "WARNING: skipping source-isolation integration test -- ffmpeg/ffprobe not found in bin/. \
                 Without it only the argument vectors are covered, not the tools' behaviour."
            );
            None
        }
    }
}

async fn make_ts(ffmpeg: &Path, dest: &Path) {
    let args: Vec<String> = [
        "-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=25:duration=2", "-c:v", "libx264", "-pix_fmt",
        "yuv420p", "-f", "mpegts",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain(std::iter::once(dest.to_string_lossy().into_owned()))
    .collect();
    let o = run(ffmpeg, &args, RunOpts::new(Duration::from_secs(60))).await.unwrap();
    assert!(o.success, "could not make the test .ts: {}", o.stderr_tail);
}

fn playlist(segment: &str) -> String {
    format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2.0,\n{segment}\n#EXT-X-ENDLIST\n")
}

#[tokio::test]
async fn an_hls_playlist_source_is_refused_at_probe() {
    let Some((ffmpeg, ffprobe)) = toolchain() else { return };
    let dir = tempfile::tempdir().unwrap();
    make_ts(&ffmpeg, &dir.path().join("seg.ts")).await;
    let m3u8 = dir.path().join("source.m3u8");
    std::fs::write(&m3u8, playlist("seg.ts")).unwrap();

    let err = probe::probe(&ffprobe, &m3u8).await.expect_err("an HLS playlist must not probe as media");
    assert!(format!("{err:#}").contains("hls"), "{err:#}");
}

/// A DASH manifest the 4 KiB sniff cannot see: the `<mpd` element (lower case)
/// starts after ~5000 bytes of comment, and its segment is a real local file.
fn padded_dash(segment: &str) -> String {
    format!(
        "<?xml version=\"1.0\"?>\n<!--{}-->\n<mpd xmlns=\"urn:mpeg:dash:schema:mpd:2011\" \
         profiles=\"urn:mpeg:dash:profile:isoff-on-demand:2011\" type=\"static\" mediaPresentationDuration=\"PT2S\">\
         <Period><AdaptationSet mimeType=\"video/mp2t\"><Representation id=\"1\" bandwidth=\"1000\">\
         <BaseURL>{segment}</BaseURL><SegmentBase indexRange=\"0-1\"/></Representation></AdaptationSet></Period></mpd>\n",
        "x".repeat(5000)
    )
}

#[tokio::test]
async fn a_dash_manifest_the_sniff_misses_is_refused_by_ffprobe_itself() {
    let Some((ffmpeg, ffprobe)) = toolchain() else { return };
    let dir = tempfile::tempdir().unwrap();
    make_ts(&ffmpeg, &dir.path().join("seg.ts")).await;
    let mpd = dir.path().join("source.mpd");
    std::fs::write(&mpd, padded_dash("seg.ts")).unwrap();

    let err = probe::probe(&ffprobe, &mpd).await.expect_err("a DASH manifest must not probe");
    let msg = format!("{err:#}");
    // The container refusal in `parse` would mean ffprobe already picked the dash
    // demuxer (and could have opened segments). The format whitelist must stop it
    // before that.
    assert!(!msg.contains("playlist container"), "ffprobe chose the dash demuxer: {msg}");
    // And it must be the whitelist that stopped it, not some other ffprobe failure.
    assert!(msg.contains("not on whitelist"), "{msg}");
}

#[tokio::test]
async fn an_avisynth_script_source_is_refused() {
    let Some((_ffmpeg, ffprobe)) = toolchain() else { return };
    let dir = tempfile::tempdir().unwrap();
    let avs = dir.path().join("source.avs");
    std::fs::write(&avs, "Version()\n").unwrap();
    // Without AviSynth+ installed the demuxer fails on its own; only the
    // whitelist message proves the script was never handed to it.
    let msg = format!("{:#}", probe::probe(&ffprobe, &avs).await.expect_err("an .avs must not probe"));
    assert!(msg.contains("not on whitelist"), "{msg}");
}

#[tokio::test]
async fn ordinary_media_containers_still_probe() {
    let Some((ffmpeg, ffprobe)) = toolchain() else { return };
    let dir = tempfile::tempdir().unwrap();
    for (name, container, codec) in [
        ("a.mp4", "mp4", "libx264"),
        ("a.mkv", "matroska", "libx264"),
        ("a.ts", "mpegts", "libx264"),
        ("a.mov", "mov", "libx264"),
        ("a.avi", "avi", "mpeg4"),
        ("a.wmv", "asf", "wmv2"),
        ("a.mpg", "mpeg", "mpeg2video"),
    ] {
        let p = dir.path().join(name);
        let args: Vec<String> = ["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=25:duration=2", "-c:v", codec, "-pix_fmt", "yuv420p"]
            .iter()
            .map(|s| s.to_string())
            .chain(std::iter::once(p.to_string_lossy().into_owned()))
            .collect();
        let o = run(&ffmpeg, &args, RunOpts::new(Duration::from_secs(60))).await.unwrap();
        assert!(o.success, "{name}: {}", o.stderr_tail);
        let got = probe::probe(&ffprobe, &p).await.unwrap_or_else(|e| panic!("{name} must probe: {e:#}"));
        assert!(got.container.contains(container), "{name}: container {:?}", got.container);
    }
    // A still picture probed fine before this item and must keep doing so.
    let jpg = dir.path().join("a.jpg");
    let args: Vec<String> = ["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=320x180:rate=25", "-frames:v", "1"]
        .iter()
        .map(|s| s.to_string())
        .chain(std::iter::once(jpg.to_string_lossy().into_owned()))
        .collect();
    assert!(run(&ffmpeg, &args, RunOpts::new(Duration::from_secs(60))).await.unwrap().success);
    let got = probe::probe(&ffprobe, &jpg).await.expect("a still image probes");
    assert_eq!(got.container, "image2");
}
