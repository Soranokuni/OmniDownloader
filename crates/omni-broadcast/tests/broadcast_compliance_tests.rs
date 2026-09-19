//! Broadcast compliance contract tests (plan P0.4, defect T-01).
//!
//! The previous version of this file asserted constants against themselves
//! (`let codec = "mpeg2video"; assert_eq!(codec, "mpeg2video");`). Three tests
//! that could not fail, guarding the one part of the system where a mistake
//! goes to air.
//!
//! These assert the actual ffmpeg argument vector produced by
//! `omni_broadcast::transcoder::build_args` against AGENTS.md section 2. If
//! someone changes the bitrate, the GOP, the field order or the channel count,
//! one of these fails.

use std::path::Path;

use omni_broadcast::transcoder::{build_args, TranscodeInput, TranscodePlan};

fn plan_for(audio_channels: Option<u32>) -> TranscodePlan {
    build_args(
        &TranscodeInput { audio_channels },
        Path::new("C:/temp/jobs/7/source.mp4"),
        Path::new("C:/temp/jobs/7/7_transcode.mxf"),
    )
}

/// Assert that `flag` is present and immediately followed by `value`.
///
/// Position matters: `-b:v 50M` and `-b:v` followed by something else are very
/// different files. A `contains` check on the joined string would pass for both.
#[track_caller]
fn assert_flag(args: &[String], flag: &str, value: &str) {
    let idx = args
        .iter()
        .position(|a| a == flag)
        .unwrap_or_else(|| panic!("flag {flag} missing from ffmpeg args: {args:?}"));
    let actual = args
        .get(idx + 1)
        .unwrap_or_else(|| panic!("flag {flag} has no value; args: {args:?}"));
    assert_eq!(
        actual, value,
        "{flag} must be {value} (AGENTS.md section 2), got {actual}"
    );
}

#[test]
fn video_matches_sony_xdcam_hd422_pal_1080i50() {
    let args = plan_for(Some(2)).args;

    assert_flag(&args, "-c:v", "mpeg2video");
    // Constant 50 Mbps: all three of these, or playout sees a VBR file.
    assert_flag(&args, "-b:v", "50M");
    assert_flag(&args, "-minrate", "50M");
    assert_flag(&args, "-maxrate", "50M");
    assert_flag(&args, "-bufsize", "17825792");
    // 4:2:2 Profile @ Main Level.
    assert_flag(&args, "-profile:v", "0");
    assert_flag(&args, "-level:v", "2");
    assert_flag(&args, "-pix_fmt", "yuv422p");
    // Long GOP 12 with 2 B-frames.
    assert_flag(&args, "-g", "12");
    assert_flag(&args, "-bf", "2");
    // Interlaced, top field first. -top 1 without +ildct+ilme produces a
    // progressive file that merely claims to be TFF.
    assert_flag(&args, "-flags", "+ildct+ilme");
    assert_flag(&args, "-top", "1");
    assert_flag(&args, "-r", "25");
    assert_flag(&args, "-aspect", "16:9");
    // Rec.709.
    assert_flag(&args, "-color_primaries", "bt709");
    assert_flag(&args, "-color_trc", "bt709");
    assert_flag(&args, "-colorspace", "bt709");
}

#[test]
fn frame_rate_is_never_ntsc_or_progressive_double_rate() {
    // The single most damaging mistake this pipeline could make: 1080i50 PAL
    // playout fed 29.97 or 50p. Assert no such rate appears anywhere.
    let args = plan_for(Some(2)).args;
    for forbidden in ["29.97", "30000/1001", "30", "50", "59.94", "60"] {
        let idx = args.iter().position(|a| a == "-r").unwrap();
        assert_ne!(
            args[idx + 1], forbidden,
            "output frame rate must be exactly 25, found {forbidden}"
        );
    }
}

#[test]
fn scaling_pillarboxes_to_1920x1080_without_stretching() {
    let args = plan_for(Some(2)).args;
    let idx = args.iter().position(|a| a == "-vf").expect("-vf present");
    let chain = &args[idx + 1];

    // force_original_aspect_ratio=decrease + pad is what makes a vertical phone
    // clip pillarbox instead of stretching a face across the frame.
    assert!(
        chain.contains("scale=1920:1080:force_original_aspect_ratio=decrease"),
        "scale must preserve aspect ratio: {chain}"
    );
    assert!(
        chain.contains("pad=1920:1080:(ow-iw)/2:(oh-ih)/2:black"),
        "output must be padded to full raster with black: {chain}"
    );
    assert!(
        chain.contains("format=yuv422p"),
        "filter chain must end in 4:2:2: {chain}"
    );
}

#[test]
fn audio_is_always_exactly_eight_discrete_mono_streams() {
    // EBU R48. Dalet rejects a 2-channel interleaved stream, and 7 or 9 streams
    // put programme audio on the wrong fader. Every branch must produce 8.
    for (channels, expected_branch) in [
        (None, "no_audio"),
        (Some(0), "no_audio"),
        (Some(1), "mono"),
        (Some(2), "stereo_or_multichannel"),
        (Some(6), "stereo_or_multichannel"),
    ] {
        let plan = plan_for(channels);
        assert_eq!(
            plan.branch, expected_branch,
            "unexpected branch for {channels:?} channels"
        );

        let maps: Vec<&String> = plan
            .args
            .iter()
            .enumerate()
            .filter(|(i, a)| *a == "-map" && plan.args.get(i + 1).is_some())
            .map(|(i, _)| &plan.args[i + 1])
            .collect();

        let video_maps = maps.iter().filter(|m| m.as_str() == "0:v").count();
        let audio_maps = maps.len() - video_maps;

        assert_eq!(video_maps, 1, "exactly one video stream, got {maps:?}");
        assert_eq!(
            audio_maps, 8,
            "exactly 8 discrete audio streams required for {channels:?} channels, got {audio_maps} ({maps:?})"
        );

        assert_flag(&plan.args, "-c:a", "pcm_s24le");
        assert_flag(&plan.args, "-ar", "48000");
    }
}

#[test]
fn programme_audio_lands_on_channels_one_and_two() {
    // Ch1 = programme left, Ch2 = programme right; 3-8 are silence pads. If the
    // order inverts, the pads land on 1/2 and the clip is silent on air.
    let mono = plan_for(Some(1));
    let mono_maps: Vec<&str> = map_targets(&mono.args);
    assert_eq!(
        &mono_maps[..3],
        &["0:v", "[l]", "[r]"],
        "mono: video then programme L/R first, got {mono_maps:?}"
    );
    assert!(
        mono_maps[3..].iter().all(|m| *m == "1:a"),
        "mono: channels 3-8 must be silence, got {mono_maps:?}"
    );

    let stereo = plan_for(Some(2));
    let stereo_maps: Vec<&str> = map_targets(&stereo.args);
    assert_eq!(&stereo_maps[..3], &["0:v", "[l]", "[r]"]);
    assert!(stereo_maps[3..].iter().all(|m| *m == "1:a"));

    // With no audio at all, all eight are silence -- and nothing claims otherwise.
    let silent = plan_for(None);
    let silent_maps: Vec<&str> = map_targets(&silent.args);
    assert_eq!(silent_maps[0], "0:v");
    assert!(
        silent_maps[1..].iter().all(|m| *m == "1:a"),
        "no-audio source must map eight silence streams, got {silent_maps:?}"
    );
}

#[test]
fn mono_is_duplicated_and_stereo_is_split_not_downmixed() {
    let mono = plan_for(Some(1));
    let fc = filter_complex(&mono.args).expect("mono needs a filter_complex");
    assert!(
        fc.contains("asplit=2"),
        "a mono source must feed both programme channels, got {fc}"
    );

    let stereo = plan_for(Some(2));
    let fc = filter_complex(&stereo.args).expect("stereo needs a filter_complex");
    assert!(
        fc.contains("c0=c0") && fc.contains("c0=c1"),
        "stereo must split L and R to discrete mono, not sum them: {fc}"
    );
}

#[test]
fn output_is_the_last_argument_and_is_an_mxf() {
    let args = plan_for(Some(2)).args;
    let last = args.last().unwrap();
    assert!(
        last.ends_with(".mxf"),
        "ffmpeg output must be the final argument and an MXF, got {last}"
    );
}

fn map_targets(args: &[String]) -> Vec<&str> {
    args.iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == "-map")
        .filter_map(|(i, _)| args.get(i + 1).map(|s| s.as_str()))
        .collect()
}

fn filter_complex(args: &[String]) -> Option<&str> {
    args.iter()
        .position(|a| a == "-filter_complex")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

/// The bmxtranswrap contract, asserted against the real argument builder rather
/// than against a restated literal.
#[test]
fn rewrap_targets_rdd9_op1a_at_25_fps() {
    let args = omni_broadcast::rewrapper::build_args(
        Path::new("C:/temp/jobs/7/7_transcode.mxf"),
        Path::new("C:/temp/jobs/7/7_output.mxf"),
    );

    assert_flag(&args, "-t", "rdd9");
    assert_flag(&args, "--tc-rate", "25");
    assert_flag(&args, "-o", "C:/temp/jobs/7/7_output.mxf");
    assert_eq!(
        args.last().map(|s| s.as_str()),
        Some("C:/temp/jobs/7/7_transcode.mxf"),
        "the source MXF must be the final argument: {args:?}"
    );
}
