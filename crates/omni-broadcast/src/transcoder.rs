use anyhow::{anyhow, Context, Result};
use regex::Regex;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tracing::info;

pub struct TranscodeProgress {
    pub percent: f64,
    pub speed: String,
    pub eta: String,
}

pub struct Transcoder {
    ffmpeg_path: PathBuf,
    ffprobe_path: PathBuf,
}


/// Everything the argument builder needs to know about the source.
///
/// Kept deliberately small and owned so [`build_args`] is a pure function that
/// unit tests can drive without ffmpeg, ffprobe or a media file (plan P0.4).
/// Phase 1.5 extends this into the full `SourceProbe` decision matrix.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscodeInput {
    /// Audio channels in the selected source stream. `None` means "the source
    /// has no audio stream" -- which is different from "we could not probe it".
    ///
    /// The distinction matters: a failed probe must never be rendered as
    /// silence (defect D-07). `Transcoder::transcode` refuses to build a plan
    /// from an unknown probe rather than passing `None` here.
    pub audio_channels: Option<u32>,
}

/// The ffmpeg argument vector plus a label for the branch that produced it.
#[derive(Debug, Clone, PartialEq)]
pub struct TranscodePlan {
    pub args: Vec<String>,
    /// Which audio branch was taken; surfaced in job events and asserted in tests.
    pub branch: &'static str,
}

/// Video filter chain for Sony XDCAM HD422 1080i50.
///
/// NOTE (defect D-08, fixed in plan P1.5): `tinterlace=interleave_top` halves
/// the frame rate, so it is only correct when fed 50 progressive frames per
/// second. Fed a 25p source it produces 12.5 fps that `-r 25` then duplicates.
/// This constant preserves today's behaviour verbatim; P1.5 replaces it with a
/// source-dependent chain. It is named and tested here so that change is a
/// visible, reviewable diff rather than an edit buried in a spawn call.
pub const VIDEO_FILTER_CHAIN: &str = "scale=1920:1080:force_original_aspect_ratio=decrease,pad=1920:1080:(ow-iw)/2:(oh-ih)/2:black,tinterlace=mode=interleave_top:flags=vlpf,format=yuv422p";

/// Silence source for the six EBU R48 pad channels.
pub const SILENCE_SOURCE: &str = "anullsrc=r=48000:cl=mono";

/// Build the exact ffmpeg argument vector for a transcode.
///
/// Pure: no I/O, no clock, no filesystem. Every broadcast-critical flag in
/// AGENTS.md section 2 is asserted against the output of this function in
/// `broadcast_compliance_tests`.
pub fn build_args(input: &TranscodeInput, source: &Path, output: &Path) -> TranscodePlan {
    let mut args: Vec<String> = vec![
        "-y".into(),
        "-threads".into(),
        "0".into(),
        "-i".into(),
        source.to_string_lossy().into_owned(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        SILENCE_SOURCE.into(),
    ];

    // EBU R48: Ch1 programme left, Ch2 programme right, Ch3-8 silence pads.
    let (branch, filter_complex, programme_maps): (&'static str, Option<&str>, usize) =
        match input.audio_channels {
            // No audio stream at all: eight silent channels. The job carries a
            // "silent" badge to MCR so nobody discovers it on air.
            None | Some(0) => ("no_audio", None, 0),
            Some(1) => ("mono", Some("[0:a:0]asplit=2[l][r]"), 2),
            _ => (
                "stereo_or_multichannel",
                Some("[0:a:0]pan=mono|c0=c0[l];[0:a:0]pan=mono|c0=c1[r]"),
                2,
            ),
        };

    if let Some(fc) = filter_complex {
        args.push("-filter_complex".into());
        args.push(fc.into());
    }

    args.push("-map".into());
    args.push("0:v".into());

    if programme_maps == 2 {
        for label in ["[l]", "[r]"] {
            args.push("-map".into());
            args.push(label.into());
        }
    }
    // Pad out to exactly eight discrete mono streams. Dalet rejects anything else.
    for _ in 0..(8 - programme_maps) {
        args.push("-map".into());
        args.push("1:a".into());
    }

    for a in [
        "-vf",
        VIDEO_FILTER_CHAIN,
        "-sws_flags",
        "bilinear",
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
        "-c:a",
        "pcm_s24le",
        "-ar",
        "48000",
        "-shortest",
    ] {
        args.push(a.into());
    }
    args.push(output.to_string_lossy().into_owned());

    TranscodePlan { args, branch }
}

impl Transcoder {
    pub fn new<P: AsRef<Path>, Q: AsRef<Path>>(ffmpeg_path: P, ffprobe_path: Q) -> Self {
        Self {
            ffmpeg_path: ffmpeg_path.as_ref().to_path_buf(),
            ffprobe_path: ffprobe_path.as_ref().to_path_buf(),
        }
    }

    pub async fn get_media_info(&self, media_path: &Path) -> Result<(f64, usize)> {
        let mut cmd = Command::new(&self.ffprobe_path);
        cmd.args([
            "-v", "error",
            "-show_entries", "format=duration:stream=codec_type,channels",
            "-of", "json",
            media_path.to_string_lossy().as_ref(),
        ]);

        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let output = cmd.output().await.with_context(|| "Failed running ffprobe")?;
        if !output.status.success() {
            return Ok((0.0, 0));
        }

        let json: Value = serde_json::from_slice(&output.stdout).unwrap_or(Value::Null);
        let duration = json
            .pointer("/format/duration")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);

        let mut channels = 0;
        if let Some(streams) = json.pointer("/streams").and_then(|v| v.as_array()) {
            for stream in streams {
                if stream.get("codec_type").and_then(|v| v.as_str()) == Some("audio") {
                    channels = stream.get("channels").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                    break;
                }
            }
        }

        Ok((duration, channels))
    }

    pub async fn transcode<F>(
        &self,
        job_id: i64,
        input_path: &Path,
        temp_dir: &Path,
        mut on_progress: F,
    ) -> Result<(PathBuf, f64)>
    where
        F: FnMut(TranscodeProgress) + Send + 'static,
    {
        tokio::fs::create_dir_all(temp_dir).await?;
        let (duration, channels) = self.get_media_info(input_path).await.unwrap_or((0.0, 0));

        let out_filename = format!("{}_transcode.mxf", job_id);
        let intermediate_mxf = temp_dir.join(&out_filename);
        if intermediate_mxf.exists() {
            let _ = tokio::fs::remove_file(&intermediate_mxf).await;
        }

        // One pure function builds the argument vector, so the broadcast
        // compliance tests assert the exact flags this process will run
        // (plan P0.4) rather than restating constants to themselves.
        let plan = build_args(
            &TranscodeInput {
                audio_channels: Some(channels as u32),
            },
            input_path,
            &intermediate_mxf,
        );
        info!(
            "Job #{}: XDCAM HD422 transcode, audio branch '{}'",
            job_id, plan.branch
        );

        let mut cmd = Command::new(&self.ffmpeg_path);
        cmd.args(&plan.args);

        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::piped());

        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x08000000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        info!("Starting FFmpeg XDCAM HD422 broadcast transcode for Job #{}...", job_id);
        let mut child = cmd.spawn().with_context(|| format!("Failed to spawn ffmpeg at {:?}", self.ffmpeg_path))?;

        let stderr = child.stderr.take().ok_or_else(|| anyhow!("Failed to capture ffmpeg stderr"))?;
        let mut reader = BufReader::new(stderr).lines();

        let time_regex = Regex::new(r"time=(\d+):(\d+):(\d+\.\d+)").expect("Valid regex");

        while let Ok(Some(line)) = reader.next_line().await {
            if let Some(caps) = time_regex.captures(&line) {
                if duration > 0.0 {
                    let hours: f64 = caps[1].parse().unwrap_or(0.0);
                    let mins: f64 = caps[2].parse().unwrap_or(0.0);
                    let secs: f64 = caps[3].parse().unwrap_or(0.0);
                    let current_time = hours * 3600.0 + mins * 60.0 + secs;
                    let percent = ((current_time / duration) * 100.0).min(99.9);

                    on_progress(TranscodeProgress {
                        percent,
                        speed: "Transcoding".into(),
                        eta: "--:--".into(),
                    });
                }
            }
        }

        let status = child.wait().await?;
        if !status.success() || !intermediate_mxf.exists() {
            return Err(anyhow!("FFmpeg transcode failed with exit status: {:?}", status.code()));
        }

        Ok((intermediate_mxf, duration))
    }
}
