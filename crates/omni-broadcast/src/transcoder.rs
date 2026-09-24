use anyhow::{anyhow, Context, Result};
use std::time::Duration as StdDuration;

use omni_core::process::RunOpts;

use crate::probe::SourceProbe;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

pub struct TranscodeProgress {
    pub percent: f64,
    pub speed: String,
    pub eta: String,
}

pub struct Transcoder {
    ffmpeg_path: PathBuf,
    ffprobe_path: PathBuf,
}


/// Measured loudness of the programme audio, from the first `loudnorm` pass.
///
/// EBU R128 normalisation is two-pass: pass one measures, pass two corrects
/// with `linear=true`. Dynamic mode is never used: it pumps on speech, and its
/// 3 s lookahead holds audio back until after the video has ended, so with the
/// `-shortest` the pad channels require the output is cut short — or, for a
/// clip under 3 s, has no audio at all and ffmpeg fails writing the MXF
/// trailer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoudnessMeasurement {
    pub input_i: f64,
    pub input_lra: f64,
    pub input_tp: f64,
    pub input_thresh: f64,
    pub target_offset: f64,
}

/// Smallest `measured_LRA` handed to loudnorm (see `build_plan`).
pub const MIN_MEASURED_LRA: f64 = 0.01;

/// Loudness policy (config `audio.*`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoudnessTarget {
    pub enabled: bool,
    /// EBU R128 programme loudness, -23 LUFS for broadcast.
    pub lufs: f64,
    /// True-peak ceiling in dBTP.
    pub true_peak: f64,
    pub lra: f64,
}

impl Default for LoudnessTarget {
    fn default() -> Self {
        Self {
            enabled: true,
            lufs: -23.0,
            true_peak: -1.0,
            lra: 7.0,
        }
    }
}

/// The ffmpeg argument vector plus the branches that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscodePlan {
    pub args: Vec<String>,
    /// Which video branch of the decision matrix was taken.
    pub video_branch: &'static str,
    /// Which audio branch was taken.
    pub branch: &'static str,
    /// Human-readable note recorded on the job, so an operator can see why a
    /// given source was handled the way it was.
    pub note: String,
}

/// Silence source for the six EBU R48 pad channels.
pub const SILENCE_SOURCE: &str = "anullsrc=r=48000:cl=mono";

/// Scale and pad to the full 1920x1080 raster.
///
/// `force_original_aspect_ratio=decrease` plus `pad` is what pillarboxes a
/// vertical phone clip instead of stretching a face across the frame.
/// `interl=1` is set only on the interlaced pass-through branch, where the
/// scaler must treat the two fields separately or it blends them together.
fn scale_pad(interlaced_scaling: bool) -> String {
    format!(
        "scale=1920:1080:force_original_aspect_ratio=decrease:interl={}:flags=bicubic,\
         pad=1920:1080:(ow-iw)/2:(oh-ih)/2:black,format=yuv422p",
        if interlaced_scaling { 1 } else { 0 }
    )
}

/// Build the video filter chain for a source (plan P1.5, defect D-08).
///
/// `tinterlace=interleave_top` weaves **two input frames into one output
/// frame**, so it halves the frame rate. It is correct only when fed 50
/// progressive frames per second. The old chain applied it at the source rate,
/// so:
///
/// * a 25p source became 12.5 fps, which `-r 25` then duplicated back up --
///   visible judder on every pan, and the one rate most web video arrives in;
/// * an already-interlaced source was interlaced *again*, tearing the fields.
///
/// Only a 50p source -- which is what the in-house suite generated -- came out
/// correct, which is precisely why the defect survived (T-03).
///
/// Returns `(chain, branch_label)`.
pub fn build_video_chain(video: &crate::probe::VideoStream) -> (String, &'static str) {
    use crate::probe::ScanType;

    let near = |a: f64, b: f64| (a - b).abs() < 0.05;

    match video.scan {
        // Interlaced, top field first, already at 25 fps: this is the target
        // format's own scan. Pass the fields through untouched. Deinterlacing
        // and re-interlacing here would cost a generation of vertical detail
        // for no reason.
        ScanType::InterlacedTff if near(video.fps, 25.0) => (
            format!("setfield=tff,{}", scale_pad(true)),
            "interlaced_tff_25_passthrough",
        ),

        // Bottom field first at 25: the field order must be flipped, and the
        // only reliable way is to separate the fields (yadif at field rate
        // yields 50 progressive frames) and weave them back top-first.
        ScanType::InterlacedBff if near(video.fps, 25.0) => (
            format!(
                "yadif=mode=send_field:parity=bff,fps=50,{},tinterlace=mode=interleave_top:flags=vlpf",
                scale_pad(false)
            ),
            "interlaced_bff_25_reorder",
        ),

        // Interlaced at any other rate (29.97i and 30i agency feeds). Rate
        // conversion needs a progressive intermediate, so deinterlace to field
        // rate, resample to 50, then weave.
        ScanType::InterlacedTff | ScanType::InterlacedBff => (
            format!(
                "yadif=mode=send_field,fps=50,{},tinterlace=mode=interleave_top:flags=vlpf",
                scale_pad(false)
            ),
            "interlaced_rate_convert",
        ),

        // Progressive (and unknown, which is overwhelmingly progressive for web
        // sources). Take the source to exactly 50 progressive frames per second
        // first -- drop/duplicate for 30p and 60p, doubling for 25p -- then weave
        // pairs into 25 interlaced frames. A 25p source becomes PsF: both fields
        // of each output frame come from one source frame, so there is no
        // interline twitter and the motion cadence is the source's own.
        ScanType::Progressive | ScanType::Unknown => (
            format!(
                "fps=50,{},tinterlace=mode=interleave_top:flags=vlpf",
                scale_pad(false)
            ),
            "progressive_to_25i",
        ),
    }
}

/// Stereo downmix for the selected audio stream.
///
/// The old code took `[0:a:0]` and split channels 0 and 1 blindly: a 5.1 source
/// lost the centre channel, which on a news package is the dialogue.
fn downmix(channels: u32, layout: Option<&str>) -> (String, &'static str) {
    match channels {
        1 => ("pan=stereo|c0=c0|c1=c0".to_string(), "mono"),
        2 => ("pan=stereo|c0=c0|c1=c1".to_string(), "stereo"),
        // 5.1 and above: ITU-R BS.775 style fold-down. Centre is what carries
        // dialogue, so it must reach both programme channels.
        6..=8 => (
            "pan=stereo|FL=FC+0.30FL+0.30BL|FR=FC+0.30FR+0.30BR".to_string(),
            "multichannel_bs775",
        ),
        _ => {
            let _ = layout;
            ("pan=stereo|c0=c0|c1=c1".to_string(), "unknown_layout")
        }
    }
}

/// ffmpeg arguments for the loudness *measurement* pass (plan P1.5).
///
/// Decodes audio only and writes nothing; the measurement comes back as JSON on
/// stderr.
pub fn build_loudness_measure_args(
    probe: &crate::probe::SourceProbe,
    target: &LoudnessTarget,
    source: &Path,
) -> Option<Vec<String>> {
    let audio = probe.audio.as_ref()?;
    if !target.enabled {
        return None;
    }
    let (down, _) = downmix(audio.channels, audio.channel_layout.as_deref());
    Some(vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-i".into(),
        source.to_string_lossy().into_owned(),
        "-map".into(),
        format!("0:{}", audio.index),
        "-vn".into(),
        "-af".into(),
        format!(
            "{down},aresample=48000,loudnorm=I={}:LRA={}:TP={}:print_format=json",
            target.lufs, target.lra, target.true_peak
        ),
        "-f".into(),
        "null".into(),
        "-".into(),
    ])
}

/// Parse the JSON object `loudnorm` prints to stderr on the measurement pass.
///
/// ffmpeg writes it after the usual banner and progress lines, so the object is
/// located rather than parsed from the start of the stream.
pub fn parse_loudness_measurement(stderr: &str) -> Option<LoudnessMeasurement> {
    let start = stderr.rfind('{')?;
    let end = stderr[start..].rfind('}')? + start + 1;
    let v: serde_json::Value = serde_json::from_str(&stderr[start..end]).ok()?;

    let num = |k: &str| -> Option<f64> {
        let raw = v.get(k)?.as_str()?;
        // loudnorm reports "-inf" for digital silence and for audio too short
        // to gate (under ~0.4 s); treat that as unusable rather than feeding
        // -inf to pass two.
        raw.parse::<f64>().ok().filter(|f| f.is_finite())
    };

    let m = LoudnessMeasurement {
        input_i: num("input_i")?,
        input_lra: num("input_lra")?,
        input_tp: num("input_tp")?,
        input_thresh: num("input_thresh")?,
        target_offset: num("target_offset").unwrap_or(0.0),
    };
    // A gating threshold at the -70 LUFS floor means nothing was above it:
    // silence. loudnorm refuses linear mode for it and would go dynamic.
    (m.input_thresh > -70.0).then_some(m)
}

/// Build the full ffmpeg argument vector for a transcode (plan P1.5).
///
/// Pure: no I/O, no clock, no filesystem, so every branch is unit-testable
/// without ffmpeg or a media file. Every broadcast-critical flag in AGENTS.md
/// section 2 is asserted against this function's output in
/// `broadcast_compliance_tests`.
///
/// `measurement` is the result of the loudness measurement pass; `None` means
/// either loudness is disabled or the measurement was unusable (silence, a
/// clip too short to gate, a failed pass). Then normalisation is **skipped**
/// and the plan note says so: dynamic `loudnorm` is not a safe fallback in
/// this graph (see [`LoudnessMeasurement`]).
pub fn build_plan(
    probe: &crate::probe::SourceProbe,
    target: &LoudnessTarget,
    measurement: Option<&LoudnessMeasurement>,
    source: &Path,
    output: &Path,
) -> TranscodePlan {
    let (video_chain, video_branch) = build_video_chain(&probe.video);

    let mut args: Vec<String> = vec![
        "-y".into(),
        "-nostdin".into(),
        "-hide_banner".into(),
        "-threads".into(),
        "0".into(),
        "-i".into(),
        source.to_string_lossy().into_owned(),
        // Silence source for the pad channels.
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        SILENCE_SOURCE.into(),
    ];

    // ---- filter graph -----------------------------------------------------
    let mut graph = format!("[0:v:0]{video_chain}[v]");
    let mut audio_branch = "no_audio";
    let note;
    let has_audio = probe.audio.is_some();

    if let Some(audio) = &probe.audio {
        let (down, down_branch) = downmix(audio.channels, audio.channel_layout.as_deref());
        audio_branch = down_branch;

        let loudness = if !target.enabled {
            String::new()
        } else if let Some(m) = measurement {
            // Two-pass: correct by the measured values, linearly.
            //
            // measured_LRA is floored at 0.01 LU. loudnorm takes a value equal
            // to its option default (0) as "not supplied", refuses linear mode
            // and silently runs dynamic mode instead -- and every clip under
            // about 3 s measures LRA 0.00, because loudness range needs several
            // gating blocks. Dynamic mode then truncated the programme or, for
            // a 1-2 s clip, left no audio and no file. In linear mode the gain
            // depends only on I and the offset, so the floor changes nothing
            // audible.
            format!(
                ",loudnorm=I={}:LRA={}:TP={}:measured_I={:.2}:measured_LRA={:.2}:\
                 measured_TP={:.2}:measured_thresh={:.2}:offset={:.2}:linear=true",
                target.lufs,
                target.lra,
                target.true_peak,
                m.input_i,
                m.input_lra.max(MIN_MEASURED_LRA),
                m.input_tp,
                m.input_thresh,
                m.target_offset
            )
        } else {
            // No usable measurement: leave the level alone. Dynamic loudnorm
            // was the old fallback and it cut the programme short (or, under
            // 3 s, produced no audio and no file at all).
            String::new()
        };

        // Downmix, resample, normalise, then split into two discrete mono
        // streams -- the MXF carries eight mono tracks, never a stereo pair.
        graph.push_str(&format!(
            ";[0:{}]{down},aresample=48000:async=1:first_pts=0{loudness},asplit=2[a1][a2];\
             [a1]pan=mono|c0=c0[al];[a2]pan=mono|c0=c1[ar]",
            audio.index
        ));

        note = format!(
            "video: {video_branch} ({}x{} {:.3} fps {:?}); audio: {down_branch} ({} ch, stream {}){}",
            probe.video.width,
            probe.video.height,
            probe.video.fps,
            probe.video.scan,
            audio.channels,
            audio.index,
            match (target.enabled, measurement.is_some()) {
                (false, _) => "; loudness off",
                (true, true) => "; R128 two-pass",
                (true, false) => "; R128 skipped: loudness measurement unusable (silence or too short to gate), level left as received",
            }
        );
    } else {
        note = format!(
            "video: {video_branch} ({}x{} {:.3} fps {:?}); audio: source has no audio stream, \
             delivering eight silent channels",
            probe.video.width, probe.video.height, probe.video.fps, probe.video.scan
        );
    }

    args.push("-filter_complex".into());
    args.push(graph);

    // ---- stream mapping ---------------------------------------------------
    args.push("-map".into());
    args.push("[v]".into());
    if has_audio {
        for label in ["[al]", "[ar]"] {
            args.push("-map".into());
            args.push(label.into());
        }
    }
    // Pad to exactly eight discrete mono streams. Dalet rejects anything else.
    let pads = if has_audio { 6 } else { 8 };
    for _ in 0..pads {
        args.push("-map".into());
        args.push("1:a".into());
    }

    // ---- output format (AGENTS.md section 2; do not change casually) ------
    for a in [
        "-c:v",
        "mpeg2video",
        "-b:v",
        "50M",
        "-minrate",
        "50M",
        "-maxrate",
        "50M",
        "-bufsize",
        "17825792",
        "-profile:v",
        "0",
        "-level:v",
        "2",
        "-pix_fmt",
        "yuv422p",
        "-g",
        "12",
        "-bf",
        "2",
        "-flags",
        "+ildct+ilme",
        "-trellis",
        "0",
        // Higher DC precision: better detail at the same bitrate, and XDCAM
        // decoders handle it. Verified by the compliance gate.
        "-dc",
        "10",
        "-top",
        "1",
        "-r",
        "25",
        "-aspect",
        "16:9",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
        "-colorspace",
        "bt709",
        "-color_range",
        "tv",
        "-c:a",
        "pcm_s24le",
        "-ar",
        "48000",
        "-shortest",
        // Machine-readable progress on stdout instead of scraping stderr.
        "-progress",
        "pipe:1",
        "-nostats",
    ] {
        args.push(a.into());
    }
    args.push(output.to_string_lossy().into_owned());

    TranscodePlan {
        args,
        video_branch,
        branch: audio_branch,
        note,
    }
}

impl Transcoder {
    pub fn new<P: AsRef<Path>, Q: AsRef<Path>>(ffmpeg_path: P, ffprobe_path: Q) -> Self {
        Self {
            ffmpeg_path: ffmpeg_path.as_ref().to_path_buf(),
            ffprobe_path: ffprobe_path.as_ref().to_path_buf(),
        }
    }

    /// Probe the source. An unreadable source is an error, never "no audio".
    ///
    /// The old `get_media_info` returned `(0.0, 0)` when ffprobe failed, which
    /// the transcoder read as "this source has no audio" and rendered as eight
    /// silent tracks. A corrupt download therefore produced a silent clip that
    /// went to air with nothing in the log (defect D-07).
    pub async fn probe_source(&self, media_path: &Path) -> Result<SourceProbe> {
        crate::probe::probe(&self.ffprobe_path, media_path).await
    }

    /// Measure programme loudness (pass one of EBU R128 normalisation).
    ///
    /// Returns `None` -- not an error -- when loudness is disabled, the source
    /// has no audio, or the measurement is unusable (digital silence, or audio
    /// too short to gate, for which loudnorm reports `-inf`). The plan then
    /// skips normalisation and records why.
    ///
    /// Short clips are measured like any other: a 1 s clip gates fine, and
    /// skipping the measurement below 3 s is what used to send them down the
    /// dynamic path that produced no file.
    pub async fn measure_loudness(
        &self,
        probe: &SourceProbe,
        target: &LoudnessTarget,
        source: &Path,
    ) -> Option<LoudnessMeasurement> {
        let args = build_loudness_measure_args(probe, target, source)?;

        // Generous but bounded: the measurement pass decodes audio only, so it
        // is fast, but a pathological source must not stall the stage.
        let timeout = StdDuration::from_secs_f64((probe.duration_secs * 2.0).clamp(60.0, 900.0));
        let out = omni_core::process::run(
            &self.ffmpeg_path,
            &args,
            RunOpts::new(timeout),
        )
        .await
        .ok()?;

        if !out.success {
            warn!("Loudness measurement pass failed; normalisation skipped for this clip");
            return None;
        }
        let measured = parse_loudness_measurement(&out.stderr_tail);
        if measured.is_none() {
            warn!("Loudness measurement unusable (silence or too short); normalisation skipped for this clip");
        }
        measured
    }

    /// Transcode to the intermediate Sony XDCAM HD422 MXF.
    ///
    /// Returns the intermediate file and the source duration.
    pub async fn transcode<F>(
        &self,
        job_id: i64,
        input_path: &Path,
        temp_dir: &Path,
        on_progress: F,
    ) -> Result<(PathBuf, f64)>
    where
        F: FnMut(TranscodeProgress) + Send + 'static,
    {
        self.transcode_with_target(job_id, input_path, temp_dir, &LoudnessTarget::default(), on_progress)
            .await
    }

    /// Transcode with an explicit loudness policy.
    pub async fn transcode_with_target<F>(
        &self,
        job_id: i64,
        input_path: &Path,
        temp_dir: &Path,
        loudness: &LoudnessTarget,
        mut on_progress: F,
    ) -> Result<(PathBuf, f64)>
    where
        F: FnMut(TranscodeProgress) + Send + 'static,
    {
        tokio::fs::create_dir_all(temp_dir).await?;

        // Probe first, and fail the job if we cannot. Guessing here is what put
        // silent clips on air (defect D-07).
        let probe = self
            .probe_source(input_path)
            .await
            .with_context(|| format!("PROBE_FAILED for job #{job_id}"))?;
        let duration = probe.duration_secs;

        let measurement = self.measure_loudness(&probe, loudness, input_path).await;

        let intermediate_mxf = temp_dir.join(format!("{job_id}_transcode.mxf"));
        if intermediate_mxf.exists() {
            let _ = tokio::fs::remove_file(&intermediate_mxf).await;
        }

        let plan = build_plan(&probe, loudness, measurement.as_ref(), input_path, &intermediate_mxf);
        info!("Job #{job_id}: {}", plan.note);

        // `-progress pipe:1` emits key=value lines; out_time_us against the
        // probed duration is an honest percentage, unlike scraping "time=" out
        // of the human-readable stderr.
        let total_us = (duration * 1_000_000.0).max(1.0);
        let opts = RunOpts::new(StdDuration::from_secs_f64(
            (duration * 6.0).clamp(600.0, 10_800.0),
        ))
        .on_stdout_line(move |line| {
            if let Some(v) = line.strip_prefix("out_time_us=") {
                if let Ok(us) = v.trim().parse::<f64>() {
                    on_progress(TranscodeProgress {
                        percent: ((us / total_us) * 100.0).clamp(0.0, 99.9),
                        speed: "Transcoding".into(),
                        eta: "--:--".into(),
                    });
                }
            }
        });

        let outcome = omni_core::process::run(&self.ffmpeg_path, &plan.args, opts)
            .await
            .with_context(|| format!("Failed running ffmpeg at {:?}", self.ffmpeg_path))?;
        outcome.ok_or_err("ffmpeg")?;

        if !intermediate_mxf.exists() {
            return Err(anyhow!(
                "ffmpeg reported success but produced no output at {intermediate_mxf:?}"
            ));
        }

        Ok((intermediate_mxf, duration))
    }
}
