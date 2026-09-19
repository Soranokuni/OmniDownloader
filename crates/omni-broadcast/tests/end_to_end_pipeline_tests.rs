//! End-to-end checks against real ffmpeg/ffprobe/bmxtranswrap (plan P1.5, P1.6).
//!
//! The unit tests assert the argument vectors and the gate's logic. These prove
//! the two actually agree with the tools: that the decision matrix produces a
//! file the compliance gate passes, and that the gate's expectations match what
//! ffprobe really reports for an MXF this pipeline made.
//!
//! A gate written only against hand-authored JSON is a gate that can be subtly
//! wrong about the real world in both directions — passing bad files, or
//! rejecting good ones until an operator learns to ignore it.
//!
//! Skipped with a visible warning when `bin/` has no toolchain, rather than
//! silently passing.

use std::path::{Path, PathBuf};

use omni_broadcast::probe;
use omni_broadcast::transcoder::{LoudnessTarget, Transcoder};
use omni_broadcast::verify::verify_mxf;

fn bin_dir() -> PathBuf {
    // tests run from the crate directory; bin/ lives at the workspace root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("bin")
}

fn tool(name: &str) -> Option<PathBuf> {
    let p = bin_dir().join(format!("{name}.exe"));
    p.exists().then_some(p)
}

/// Returns (ffmpeg, ffprobe) or `None` after printing why the test is skipped.
fn toolchain() -> Option<(PathBuf, PathBuf)> {
    match (tool("ffmpeg"), tool("ffprobe")) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => {
            eprintln!(
                "WARNING: skipping end-to-end pipeline test -- ffmpeg/ffprobe not found in {:?}. \
                 This test proves the transcoder and the compliance gate agree with the real \
                 tools; without it, only the argument vectors are covered.",
                bin_dir()
            );
            None
        }
    }
}

/// Build a synthetic source at a given rate and scan.
async fn make_source(
    ffmpeg: &Path,
    dest: &Path,
    fps: &str,
    duration: u32,
    with_audio: bool,
) -> bool {
    let mut args: Vec<String> = vec![
        "-y".into(),
        "-v".into(),
        "error".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        format!("testsrc2=size=1280x720:rate={fps}:duration={duration}"),
    ];
    if with_audio {
        args.extend([
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            format!("sine=frequency=1000:sample_rate=48000:duration={duration}"),
            "-c:a".into(),
            "aac".into(),
        ]);
    }
    args.extend([
        "-c:v".into(),
        "libx264".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-shortest".into(),
        dest.to_string_lossy().into_owned(),
    ]);

    omni_core::process::run(ffmpeg, &args, omni_core::process::RunOpts::new(std::time::Duration::from_secs(120)))
        .await
        .map(|o| o.success)
        .unwrap_or(false)
}

/// The core claim of P1.5 + P1.6: sources at the rates a newsroom actually
/// receives are transcoded into files that pass the compliance gate.
#[tokio::test]
async fn sources_at_every_common_rate_produce_a_compliant_mxf() {
    let Some((ffmpeg, ffprobe)) = toolchain() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let transcoder = Transcoder::new(&ffmpeg, &ffprobe);

    // 25p is the rate that exposed D-08; 30p and 50p exercise the other
    // progressive paths. The in-house suite only ever generated 50p, which is
    // the one rate that hid the bug (defect T-03).
    for (label, fps) in [("25p", "25"), ("30p", "30"), ("50p", "50")] {
        let src = dir.path().join(format!("src_{label}.mp4"));
        assert!(
            make_source(&ffmpeg, &src, fps, 6, true).await,
            "could not build the {label} synthetic source"
        );

        let probed = probe::probe(&ffprobe, &src)
            .await
            .unwrap_or_else(|e| panic!("{label}: probe failed: {e:?}"));

        let (mxf, duration) = transcoder
            .transcode_with_target(
                1,
                &src,
                &dir.path().join(label),
                // Loudness off: the two-pass measurement triples the runtime of
                // this test and is covered by its own unit tests.
                &LoudnessTarget {
                    enabled: false,
                    ..LoudnessTarget::default()
                },
                |_| {},
            )
            .await
            .unwrap_or_else(|e| panic!("{label}: transcode failed: {e:?}"));

        let report = verify_mxf(&ffprobe, &mxf, duration)
            .await
            .unwrap_or_else(|e| panic!("{label}: could not verify: {e:?}"));

        assert!(
            report.pass,
            "{label} source produced a non-compliant file: {}",
            report.summary()
        );
        assert!(
            (probed.duration_secs - duration).abs() < 0.1,
            "{label}: probe and transcode disagree about duration"
        );
    }
}

/// D-08 in the terms that matter on air: motion cadence, not frame count.
///
/// Both the old and the new chain emit 25 fps and `field_order=tt` for a 25p
/// source, because `-r 25` duplicates the halved output back up. The difference
/// only shows in how many frames are *distinct* — which is what an editor sees
/// as judder on a pan.
#[tokio::test]
async fn a_25p_source_keeps_its_full_motion_cadence() {
    let Some((ffmpeg, ffprobe)) = toolchain() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src25p.mp4");
    assert!(make_source(&ffmpeg, &src, "25", 10, false).await);

    let transcoder = Transcoder::new(&ffmpeg, &ffprobe);
    let (mxf, _) = transcoder
        .transcode_with_target(
            1,
            &src,
            &dir.path().join("work"),
            &LoudnessTarget {
                enabled: false,
                ..LoudnessTarget::default()
            },
            |_| {},
        )
        .await
        .expect("transcode");

    // mpdecimate drops frames identical to their predecessor. A correct 25p ->
    // 25i conversion keeps all 250; the pre-P1.5 chain kept 125.
    let out = omni_core::process::run(
        &ffmpeg,
        [
            "-v".to_string(),
            "error".to_string(),
            "-i".to_string(),
            mxf.to_string_lossy().into_owned(),
            "-vf".to_string(),
            "mpdecimate=hi=64*12:lo=64*5:frac=0.33".to_string(),
            "-f".to_string(),
            "null".to_string(),
            "-".to_string(),
            "-stats".to_string(),
        ],
        omni_core::process::RunOpts::new(std::time::Duration::from_secs(180)),
    )
    .await
    .expect("mpdecimate run");

    let unique: u32 = out
        .stderr_tail
        .rsplit("frame=")
        .next()
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    assert!(
        unique >= 240,
        "only {unique} of 250 frames are distinct: the source rate is being \
         halved and duplicated back up, which is judder on every pan (defect \
         D-08). Check that fps=50 precedes tinterlace."
    );
}

/// A source the tools cannot read must fail the job, not silently become a
/// silent clip (defect D-07).
#[tokio::test]
async fn an_unreadable_source_fails_instead_of_producing_silence() {
    let Some((ffmpeg, ffprobe)) = toolchain() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();

    // A truncated download: the right extension, not actually media.
    let broken = dir.path().join("source.mp4");
    tokio::fs::write(&broken, b"\x00\x00\x00\x18ftypmp42not-really-a-video")
        .await
        .unwrap();

    assert!(
        probe::probe(&ffprobe, &broken).await.is_err(),
        "an unreadable source probed successfully"
    );

    let transcoder = Transcoder::new(&ffmpeg, &ffprobe);
    let err = transcoder
        .transcode_with_target(
            1,
            &broken,
            &dir.path().join("work"),
            &LoudnessTarget::default(),
            |_| {},
        )
        .await
        .expect_err("an unreadable source must fail the transcode, not deliver silence");

    assert!(
        format!("{err:?}").contains("PROBE_FAILED"),
        "the failure should be classifiable as PROBE_FAILED: {err:?}"
    );
}

/// The gate must reject a real file that is wrong, not just hand-authored JSON.
#[tokio::test]
async fn the_compliance_gate_rejects_a_real_but_wrong_file() {
    let Some((ffmpeg, ffprobe)) = toolchain() else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let wrong = dir.path().join("wrong.mxf");

    // 720p, progressive, 4:2:0, one stereo audio stream: plausible output of a
    // filter-chain regression, and exactly what must never reach Dalet.
    let ok = omni_core::process::run(
        &ffmpeg,
        [
            "-y", "-v", "error",
            "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=25:duration=4",
            "-f", "lavfi", "-i", "sine=frequency=1000:sample_rate=48000:duration=4",
            "-c:v", "mpeg2video", "-b:v", "10M", "-pix_fmt", "yuv420p",
            "-c:a", "pcm_s24le", "-ar", "48000", "-shortest",
            wrong.to_string_lossy().as_ref(),
        ],
        omni_core::process::RunOpts::new(std::time::Duration::from_secs(120)),
    )
    .await
    .map(|o| o.success)
    .unwrap_or(false);
    assert!(ok, "could not build the deliberately wrong file");

    let report = verify_mxf(&ffprobe, &wrong, 4.0).await.expect("verify ran");
    assert!(
        !report.pass,
        "the gate passed a 720p 4:2:0 stereo file: {}",
        report.summary()
    );

    let failed: Vec<&str> = report.failures().iter().map(|c| c.name.as_str()).collect();
    for expected in ["width", "height", "pixel_format", "audio_stream_count"] {
        assert!(
            failed.contains(&expected),
            "the gate did not catch {expected}; it flagged {failed:?}"
        );
    }
}
