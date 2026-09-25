//! Broadcast compliance contract tests (plan P0.4 / P1.5, defects T-01, D-08).
//!
//! The original version of this file asserted constants against themselves
//! (`let codec = "mpeg2video"; assert_eq!(codec, "mpeg2video");`) — three tests
//! that could not fail, guarding the one part of the system where a mistake
//! goes to air.
//!
//! These drive the real argument builders, `transcoder::build_plan` and
//! `rewrapper::build_args`, across every branch of the P1.5 decision matrix,
//! and assert the output against AGENTS.md section 2.

use std::path::Path;

use omni_broadcast::probe::{AudioStream, ScanType, SourceProbe, VideoStream};
use omni_broadcast::transcoder::{
    build_plan, build_video_chain, parse_loudness_measurement, LoudnessMeasurement, LoudnessTarget,
    TranscodePlan,
};

// ------------------------------------------------------------- test helpers --

fn video(width: u32, height: u32, fps: f64, scan: ScanType) -> VideoStream {
    VideoStream {
        index: 0,
        codec: "h264".into(),
        width,
        height,
        fps,
        scan,
        pix_fmt: Some("yuv420p".into()),
    }
}

fn audio(channels: u32) -> AudioStream {
    AudioStream {
        index: 1,
        codec: "aac".into(),
        channels,
        channel_layout: None,
        sample_rate: 48_000,
        language: Some("ell".into()),
        is_default: true,
    }
}

fn probe_of(v: VideoStream, a: Option<AudioStream>) -> SourceProbe {
    SourceProbe {
        duration_secs: 92.0,
        size_bytes: 40_000_000,
        video: v,
        audio_stream_count: a.is_some() as usize,
        audio: a,
    }
}

/// A plan with loudness disabled, so the filter graph stays readable in the
/// assertions that are not about loudness.
fn plan_for(p: &SourceProbe) -> TranscodePlan {
    build_plan(
        p,
        &LoudnessTarget {
            enabled: false,
            ..LoudnessTarget::default()
        },
        None,
        Path::new("C:/temp/jobs/7/source.mp4"),
        Path::new("C:/temp/jobs/7/7_transcode.mxf"),
    )
}

/// Assert `flag` is present and immediately followed by `value`.
///
/// Position matters: `-b:v 50M` and `-b:v` followed by something else produce
/// very different files, and a substring check on the joined command would pass
/// for both.
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

fn filter_graph(args: &[String]) -> &str {
    let i = args
        .iter()
        .position(|a| a == "-filter_complex")
        .expect("-filter_complex present");
    &args[i + 1]
}

fn map_targets(args: &[String]) -> Vec<&str> {
    args.iter()
        .enumerate()
        .filter(|(_, a)| a.as_str() == "-map")
        .filter_map(|(i, _)| args.get(i + 1).map(|s| s.as_str()))
        .collect()
}

// ------------------------------------------------------- video output format --

#[test]
fn video_matches_sony_xdcam_hd422_pal_1080i50() {
    let args = plan_for(&probe_of(video(1920, 1080, 25.0, ScanType::Progressive), Some(audio(2)))).args;

    assert_flag(&args, "-c:v", "mpeg2video");
    // Constant 50 Mbps needs all three, or playout sees a VBR file.
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
    // Interlaced, top field first. -top 1 without +ildct+ilme yields a
    // progressive file that merely claims to be TFF.
    assert_flag(&args, "-flags", "+ildct+ilme");
    assert_flag(&args, "-field_order", "tt");
    // FFmpeg 9 rejects the whole command if the removed `-top` is present.
    assert!(!args.iter().any(|a| a == "-top"), "-top is gone in FFmpeg 9: {args:?}");
    assert_flag(&args, "-intra_dc_precision", "2");
    assert!(!args.iter().any(|a| a == "-dc"), "-dc is deprecated; use -intra_dc_precision: {args:?}");
    assert_flag(&args, "-r", "25");
    assert_flag(&args, "-aspect", "16:9");
    // Rec.709, legal range.
    assert_flag(&args, "-color_primaries", "bt709");
    assert_flag(&args, "-color_trc", "bt709");
    assert_flag(&args, "-colorspace", "bt709");
    assert_flag(&args, "-color_range", "tv");
}

#[test]
fn output_frame_rate_is_exactly_25_for_every_source_rate() {
    // 1080i50 PAL playout fed 29.97 or 50p is the most damaging mistake this
    // pipeline could make, so check every source rate, not just the easy one.
    for (fps, scan) in [
        (25.0, ScanType::Progressive),
        (30.0, ScanType::Progressive),
        (50.0, ScanType::Progressive),
        (59.94, ScanType::Progressive),
        (25.0, ScanType::InterlacedTff),
        (25.0, ScanType::InterlacedBff),
        (29.97, ScanType::InterlacedTff),
        (23.976, ScanType::Progressive),
    ] {
        let args = plan_for(&probe_of(video(1920, 1080, fps, scan), Some(audio(2)))).args;
        assert_flag(&args, "-r", "25");
        for forbidden in ["29.97", "30000/1001", "30", "50", "59.94", "60", "23.976"] {
            let i = args.iter().position(|a| a == "-r").unwrap();
            assert_ne!(
                args[i + 1], forbidden,
                "source {fps} fps {scan:?} produced output rate {forbidden}"
            );
        }
    }
}

#[test]
fn scaling_pillarboxes_to_1920x1080_without_stretching() {
    // A vertical phone clip must be pillarboxed, not stretched across the frame.
    let args = plan_for(&probe_of(video(1080, 1920, 30.0, ScanType::Progressive), Some(audio(2)))).args;
    let chain = filter_graph(&args);

    assert!(
        chain.contains("scale=1920:1080:force_original_aspect_ratio=decrease"),
        "scale must preserve aspect ratio: {chain}"
    );
    assert!(
        chain.contains("pad=1920:1080:(ow-iw)/2:(oh-ih)/2:black"),
        "output must be padded to the full raster with black: {chain}"
    );
    assert!(chain.contains("format=yuv422p"), "chain must reach 4:2:2: {chain}");
}

// ----------------------------------------- the D-08 decision matrix itself --

#[test]
fn a_progressive_source_is_doubled_to_50_before_it_is_interlaced() {
    // Defect D-08, and the case that matters most: almost all web video is 25p
    // or 30p. `tinterlace=interleave_top` weaves two input frames into one, so
    // fed at the source rate it *halves* the frame rate -- a 25p source became
    // 12.5 fps that `-r 25` then duplicated back up, visible as judder on every
    // pan. It is only correct fed 50 progressive frames per second.
    for fps in [23.976, 25.0, 30.0, 50.0, 60.0] {
        let plan = plan_for(&probe_of(
            video(1920, 1080, fps, ScanType::Progressive),
            Some(audio(2)),
        ));
        let chain = filter_graph(&plan.args);

        assert_eq!(plan.video_branch, "progressive_to_25i", "source {fps} fps");
        assert!(
            chain.contains("fps=50"),
            "a {fps} fps progressive source must be taken to 50 fps before \
             tinterlace, or the output rate is halved (D-08): {chain}"
        );

        let fps_at = chain.find("fps=50").expect("fps=50 present");
        let weave_at = chain.find("tinterlace").expect("tinterlace present");
        assert!(
            fps_at < weave_at,
            "fps=50 must come *before* tinterlace, otherwise the rate is halved \
             first and doubled after: {chain}"
        );
    }
}

#[test]
fn an_interlaced_25_tff_source_passes_its_fields_through_untouched() {
    // This is already the target scan. Deinterlacing and re-interlacing costs a
    // generation of vertical detail, and re-weaving already-woven fields tears
    // them -- which is what the old unconditional tinterlace did.
    let plan = plan_for(&probe_of(
        video(1920, 1080, 25.0, ScanType::InterlacedTff),
        Some(audio(2)),
    ));
    let chain = filter_graph(&plan.args);

    assert_eq!(plan.video_branch, "interlaced_tff_25_passthrough");
    assert!(
        !chain.contains("tinterlace"),
        "an interlaced 25 TFF source must not be interlaced again: {chain}"
    );
    assert!(
        !chain.contains("yadif"),
        "an interlaced 25 TFF source must not be deinterlaced: {chain}"
    );
    assert!(chain.contains("setfield=tff"), "field order must be asserted: {chain}");
    assert!(
        chain.contains("interl=1"),
        "the scaler must treat fields separately or it blends them: {chain}"
    );
}

#[test]
fn a_bottom_field_first_source_has_its_field_order_corrected() {
    // BFF delivered as TFF plays with the fields swapped: motion judders
    // backwards and forwards every frame. Very visible, easy to miss on a
    // progressive monitor.
    let plan = plan_for(&probe_of(
        video(1920, 1080, 25.0, ScanType::InterlacedBff),
        Some(audio(2)),
    ));
    let chain = filter_graph(&plan.args);

    assert_eq!(plan.video_branch, "interlaced_bff_25_reorder");
    assert!(
        chain.contains("yadif=mode=send_field:parity=bff"),
        "BFF must be separated at field rate to reorder: {chain}"
    );
    assert!(chain.contains("fps=50"), "{chain}");
    assert!(
        chain.contains("tinterlace=mode=interleave_top"),
        "fields must be rewoven top-first: {chain}"
    );
}

#[test]
fn interlaced_ntsc_rates_are_rate_converted_through_a_progressive_intermediate() {
    for fps in [29.97, 30.0] {
        let plan = plan_for(&probe_of(
            video(1920, 1080, fps, ScanType::InterlacedTff),
            Some(audio(2)),
        ));
        let chain = filter_graph(&plan.args);
        assert_eq!(plan.video_branch, "interlaced_rate_convert", "{fps} fps");
        assert!(chain.contains("yadif=mode=send_field"), "{chain}");
        assert!(chain.contains("fps=50"), "{chain}");
        assert!(chain.contains("tinterlace=mode=interleave_top"), "{chain}");
    }
}

#[test]
fn an_unknown_scan_type_is_treated_as_progressive() {
    // ffprobe reports `unknown` for most web MP4s. Treating those as interlaced
    // would tear every frame; treating them as progressive is right in the
    // overwhelming majority and harmless in the rest.
    let plan = plan_for(&probe_of(
        video(1280, 720, 25.0, ScanType::Unknown),
        Some(audio(2)),
    ));
    assert_eq!(plan.video_branch, "progressive_to_25i");
    assert!(filter_graph(&plan.args).contains("fps=50"));
}

#[test]
fn every_branch_still_ends_at_the_full_raster_in_4_2_2() {
    // Whatever route a source takes, the output geometry is not negotiable.
    for scan in [
        ScanType::Progressive,
        ScanType::InterlacedTff,
        ScanType::InterlacedBff,
        ScanType::Unknown,
    ] {
        for fps in [25.0, 29.97, 50.0] {
            let (chain, branch) = build_video_chain(&video(640, 360, fps, scan));
            assert!(
                chain.contains("scale=1920:1080:force_original_aspect_ratio=decrease"),
                "branch {branch} does not scale to the full raster: {chain}"
            );
            assert!(
                chain.contains("pad=1920:1080"),
                "branch {branch} does not pad: {chain}"
            );
            assert!(
                chain.ends_with("format=yuv422p")
                    || chain.contains("format=yuv422p,tinterlace"),
                "branch {branch} does not reach 4:2:2: {chain}"
            );
        }
    }
}

// ------------------------------------------------------------ EBU R48 audio --

#[test]
fn audio_is_always_exactly_eight_discrete_mono_streams() {
    // Dalet rejects a 2-channel interleaved stream, and 7 or 9 streams put
    // programme audio on the wrong fader. Every branch must produce 8.
    for channels in [None, Some(1), Some(2), Some(6), Some(8)] {
        let plan = plan_for(&probe_of(
            video(1920, 1080, 25.0, ScanType::Progressive),
            channels.map(audio),
        ));

        let maps = map_targets(&plan.args);
        let video_maps = maps.iter().filter(|m| m.starts_with("[v]")).count();
        let audio_maps = maps.len() - video_maps;

        assert_eq!(video_maps, 1, "exactly one video stream, got {maps:?}");
        assert_eq!(
            audio_maps, 8,
            "exactly 8 discrete audio streams required for {channels:?} source channels, \
             got {audio_maps} ({maps:?})"
        );

        assert_flag(&plan.args, "-c:a", "pcm_s24le");
        assert_flag(&plan.args, "-ar", "48000");
    }
}

#[test]
fn programme_audio_lands_on_channels_one_and_two() {
    // Ch1 = programme left, Ch2 = programme right, 3-8 silence. If the order
    // inverts, the pads land on 1/2 and the clip is silent on air.
    for channels in [1, 2, 6] {
        let plan = plan_for(&probe_of(
            video(1920, 1080, 25.0, ScanType::Progressive),
            Some(audio(channels)),
        ));
        let maps = map_targets(&plan.args);
        assert_eq!(
            &maps[..3],
            &["[v]", "[al]", "[ar]"],
            "{channels}-channel source: video then programme L/R must come first, got {maps:?}"
        );
        assert!(
            maps[3..].iter().all(|m| *m == "1:a"),
            "{channels}-channel source: channels 3-8 must be silence, got {maps:?}"
        );
    }
}

#[test]
fn a_source_with_no_audio_gets_eight_silent_channels_and_says_so() {
    let plan = plan_for(&probe_of(
        video(1920, 1080, 25.0, ScanType::Progressive),
        None,
    ));
    let maps = map_targets(&plan.args);
    assert_eq!(maps[0], "[v]");
    assert!(maps[1..].iter().all(|m| *m == "1:a"), "{maps:?}");
    assert_eq!(plan.branch, "no_audio");
    // The operator must be able to see why the clip is silent.
    assert!(
        plan.note.contains("no audio stream"),
        "the plan does not record that the source was silent: {}",
        plan.note
    );
}

#[test]
fn a_five_point_one_source_keeps_its_centre_channel() {
    // Defect D-09. The old chain took channels 0 and 1 of a 5.1 source, which
    // are front left and front right -- the centre channel, where the dialogue
    // of a news package lives, was dropped entirely.
    let plan = plan_for(&probe_of(
        video(1920, 1080, 25.0, ScanType::Progressive),
        Some(audio(6)),
    ));
    let chain = filter_graph(&plan.args);

    assert_eq!(plan.branch, "multichannel_bs775");
    assert!(
        chain.contains("FL=FC+") && chain.contains("FR=FC+"),
        "a 5.1 downmix must fold the centre channel into both programme \
         channels or the dialogue is lost: {chain}"
    );
}

#[test]
fn a_mono_source_is_duplicated_to_both_programme_channels() {
    let plan = plan_for(&probe_of(
        video(1920, 1080, 25.0, ScanType::Progressive),
        Some(audio(1)),
    ));
    let chain = filter_graph(&plan.args);
    assert_eq!(plan.branch, "mono");
    assert!(
        chain.contains("pan=stereo|c0=c0|c1=c0"),
        "a mono source must feed both programme channels, not just the left: {chain}"
    );
}

#[test]
fn the_selected_audio_stream_is_the_one_the_probe_chose() {
    // The probe picks the Greek/default track; the plan must actually map that
    // stream index rather than blindly taking 0:a:0 as the old code did.
    let mut a = audio(2);
    a.index = 3;
    let plan = plan_for(&probe_of(
        video(1920, 1080, 25.0, ScanType::Progressive),
        Some(a),
    ));
    assert!(
        filter_graph(&plan.args).contains("[0:3]"),
        "the plan ignored the probe's audio stream selection: {}",
        filter_graph(&plan.args)
    );
}

// ------------------------------------------------------------ EBU R128 loudness --

#[test]
fn two_pass_loudness_uses_the_measured_values() {
    let p = probe_of(video(1920, 1080, 25.0, ScanType::Progressive), Some(audio(2)));
    let measured = LoudnessMeasurement {
        input_i: -18.42,
        input_lra: 9.10,
        input_tp: -0.55,
        input_thresh: -28.90,
        target_offset: -0.30,
    };
    let plan = build_plan(
        &p,
        &LoudnessTarget::default(),
        Some(&measured),
        Path::new("src.mp4"),
        Path::new("out.mxf"),
    );
    let chain = filter_graph(&plan.args);

    assert!(chain.contains("loudnorm=I=-23"), "{chain}");
    assert!(chain.contains("TP=-1"), "{chain}");
    assert!(chain.contains("measured_I=-18.42"), "{chain}");
    assert!(chain.contains("measured_TP=-0.55"), "{chain}");
    assert!(
        chain.contains("linear=true"),
        "two-pass must correct linearly; dynamic mode pumps on speech: {chain}"
    );
}

/// No usable measurement → no normalisation, said plainly in the note.
///
/// The old fallback was single-pass dynamic loudnorm. Its 3 s lookahead holds
/// audio back past the end of the video, and `-shortest` (required by the pad
/// channels) then cut the programme short — or, for a clip under 3 s, left no
/// audio at all and ffmpeg failed writing the MXF. Dynamic mode must never
/// appear in the graph.
#[test]
fn an_unusable_measurement_skips_normalisation_instead_of_going_dynamic() {
    let p = probe_of(video(1920, 1080, 25.0, ScanType::Progressive), Some(audio(2)));
    let plan = build_plan(
        &p,
        &LoudnessTarget::default(),
        None,
        Path::new("src.mp4"),
        Path::new("out.mxf"),
    );
    let chain = filter_graph(&plan.args);
    assert!(
        !chain.contains("loudnorm"),
        "without a measurement there must be no loudnorm (dynamic mode truncates): {chain}"
    );
    assert!(plan.note.contains("R128 skipped"), "{}", plan.note);
    // The programme audio is still there, still eight mono tracks.
    assert!(chain.contains("[al]") && chain.contains("[ar]"), "{chain}");
}

/// A short clip measures LRA 0.00. Passed through as-is, loudnorm reads it as
/// "not supplied" and silently switches the linear pass to dynamic mode,
/// which truncated the programme (and produced no file under 3 s).
#[test]
fn a_zero_loudness_range_is_never_passed_to_the_linear_pass() {
    let p = probe_of(video(1920, 1080, 25.0, ScanType::Progressive), Some(audio(2)));
    let short_clip = LoudnessMeasurement {
        input_i: -21.05,
        input_lra: 0.0,
        input_tp: -16.60,
        input_thresh: -31.05,
        target_offset: 0.05,
    };
    let plan = build_plan(&p, &LoudnessTarget::default(), Some(&short_clip), Path::new("src.mp4"), Path::new("out.mxf"));
    let chain = filter_graph(&plan.args);
    assert!(chain.contains("measured_LRA=0.01"), "{chain}");
    assert!(!chain.contains("measured_LRA=0.00"), "{chain}");
    assert!(chain.contains("measured_I=-21.05") && chain.contains("linear=true"), "{chain}");
}

/// Station policy: when reaching -23 LUFS would lift true peak over -1 dBTP,
/// the ceiling wins. A plain linear gain, no limiter -- and no loudnorm, which
/// would switch itself to dynamic mode here and truncate the programme.
#[test]
fn clipped_audio_is_gained_to_the_peak_ceiling_and_lands_under_target() {
    let p = probe_of(video(1920, 1080, 25.0, ScanType::Progressive), Some(audio(2)));
    // Quiet programme, peaks near full scale: +7 dB to target would put
    // peaks at +4 dBTP.
    let clipped = LoudnessMeasurement {
        input_i: -30.0,
        input_lra: 4.0,
        input_tp: -3.0,
        input_thresh: -40.0,
        target_offset: 0.0,
    };
    let plan = build_plan(&p, &LoudnessTarget::default(), Some(&clipped), Path::new("src.mp4"), Path::new("out.mxf"));
    let chain = filter_graph(&plan.args);
    assert!(chain.contains("volume=2.00dB"), "{chain}");
    assert!(!chain.contains("loudnorm"), "loudnorm would go dynamic and truncate: {chain}");
    assert!(plan.note.contains("peak-limited"), "{}", plan.note);
    assert!(plan.note.contains("-28.0 LUFS"), "{}", plan.note);

    // A hot clip needing *less* level is never limited: cutting gain lowers
    // the peaks too.
    let hot = LoudnessMeasurement { input_i: -12.0, input_tp: 0.5, ..clipped };
    let plan = build_plan(&p, &LoudnessTarget::default(), Some(&hot), Path::new("src.mp4"), Path::new("out.mxf"));
    assert!(filter_graph(&plan.args).contains("linear=true"), "{}", filter_graph(&plan.args));
}

#[test]
fn the_peak_decision_errs_on_the_safe_side_of_the_ceiling() {
    use omni_broadcast::transcoder::peak_limited_gain;
    let t = LoudnessTarget::default();
    let m = |i: f64, tp: f64| LoudnessMeasurement {
        input_i: i,
        input_lra: 3.0,
        input_tp: tp,
        input_thresh: i - 10.0,
        target_offset: 0.0,
    };
    // Comfortably under: two-pass loudnorm as before.
    assert_eq!(peak_limited_gain(&m(-20.0, -10.0), &t), None);
    // 0.05 dB under the ceiling: too close to trust loudnorm's own rounding,
    // but the target still fits, so the gain is the full correction.
    let (g, landed) = peak_limited_gain(&m(-20.0, 1.95), &t).unwrap();
    assert!((g - -3.0).abs() < 1e-9 && (landed - -23.0).abs() < 1e-9, "{g} {landed}");
    // Over: peaks go exactly to the ceiling and loudness lands under target.
    let (g, landed) = peak_limited_gain(&m(-22.75, 0.18), &t).unwrap();
    assert!((g - -1.18).abs() < 1e-9, "{g}");
    assert!(landed < -23.0, "{landed}");
}

#[test]
fn a_measurement_at_the_silence_floor_is_unusable() {
    // Near-silence gates nothing: loudnorm reports input_thresh -70 and would
    // silently switch a linear pass to dynamic mode.
    let stderr = r#"{ "input_i" : "-68.20", "input_tp" : "-60.00", "input_lra" : "0.00",
                      "input_thresh" : "-70.00", "target_offset" : "0.00" }"#;
    assert!(parse_loudness_measurement(stderr).is_none());
}

#[test]
fn loudness_can_be_switched_off_for_a_station_that_normalises_downstream() {
    let p = probe_of(video(1920, 1080, 25.0, ScanType::Progressive), Some(audio(2)));
    let chain_args = plan_for(&p).args;
    assert!(
        !filter_graph(&chain_args).contains("loudnorm"),
        "audio.loudnorm_enabled=false must not normalise: {}",
        filter_graph(&chain_args)
    );
}

#[test]
fn loudnorm_measurement_json_is_parsed_out_of_ffmpeg_stderr() {
    // ffmpeg prints the object after its usual banner and progress noise.
    let stderr = r#"
frame= 1234 fps=250 q=-1.0 size=N/A time=00:00:49.36 bitrate=N/A speed=9.99x
[Parsed_loudnorm_2 @ 000001]
{
	"input_i" : "-18.42",
	"input_tp" : "-0.55",
	"input_lra" : "9.10",
	"input_thresh" : "-28.90",
	"output_i" : "-23.01",
	"target_offset" : "-0.30"
}
"#;
    let m = parse_loudness_measurement(stderr).expect("measurement must parse");
    assert_eq!(m.input_i, -18.42);
    assert_eq!(m.input_tp, -0.55);
    assert_eq!(m.input_lra, 9.10);
    assert_eq!(m.input_thresh, -28.90);
    assert_eq!(m.target_offset, -0.30);
}

#[test]
fn digital_silence_does_not_produce_an_infinite_measurement() {
    // loudnorm reports "-inf" for a silent track. Feeding that into pass two
    // yields an ffmpeg error mid-transcode; skipping normalisation does not.
    let stderr = r#"{ "input_i" : "-inf", "input_tp" : "-inf", "input_lra" : "0.00",
                      "input_thresh" : "-inf", "target_offset" : "0.00" }"#;
    assert!(
        parse_loudness_measurement(stderr).is_none(),
        "an -inf measurement must be rejected, not passed to the second pass"
    );
}

// ------------------------------------------------------------------ rewrap --

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

#[test]
fn output_is_the_last_argument_and_is_an_mxf() {
    let args = plan_for(&probe_of(
        video(1920, 1080, 25.0, ScanType::Progressive),
        Some(audio(2)),
    ))
    .args;
    let last = args.last().unwrap();
    assert!(
        last.ends_with(".mxf"),
        "ffmpeg output must be the final argument and an MXF, got {last}"
    );
}
