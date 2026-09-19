//! Source inspection via ffprobe (plan P1.5).
//!
//! Everything the transcoder needs to decide how to reach Sony XDCAM HD422
//! 1080i50 comes from here. Two rules matter more than the parsing:
//!
//! * **A failed probe is not "no audio"** (defect D-07). The old code mapped an
//!   ffprobe error to `(0.0, 0)` channels, so an unreadable source was
//!   transcoded to eight silent tracks and delivered — a silent clip on air
//!   with nothing in the log. [`probe`] returns an error; the pipeline sends the
//!   job to review.
//! * **Scan type decides the filter chain.** Feeding a 25p source through
//!   `tinterlace` without first doubling to 50 fps halves the frame rate
//!   (defect D-08), so [`ScanType`] is what the decision matrix keys on.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use omni_core::process::run_capture;
use serde::{Deserialize, Serialize};

/// ffprobe should answer in seconds; a minute means something is very wrong
/// (an unreadable network path, a source that is actually a live stream).
const PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// How the source is scanned. Drives the whole video filter chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ScanType {
    Progressive,
    /// Interlaced, top field first.
    InterlacedTff,
    /// Interlaced, bottom field first — needs field-order conversion.
    InterlacedBff,
    /// ffprobe reported `unknown`. Treated as progressive, which is the safe
    /// assumption for web sources: almost all of them are, and treating a
    /// progressive source as interlaced tears every frame.
    Unknown,
}

impl ScanType {
    fn from_field_order(raw: Option<&str>) -> Self {
        match raw.unwrap_or("").trim() {
            "tt" | "tb" => Self::InterlacedTff,
            "bb" | "bt" => Self::InterlacedBff,
            "progressive" => Self::Progressive,
            _ => Self::Unknown,
        }
    }

    pub fn is_interlaced(&self) -> bool {
        matches!(self, Self::InterlacedTff | Self::InterlacedBff)
    }
}

/// The selected video stream.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoStream {
    pub index: usize,
    pub codec: String,
    pub width: u32,
    pub height: u32,
    /// Frames per second, from `r_frame_rate` (falling back to `avg_frame_rate`).
    pub fps: f64,
    pub scan: ScanType,
    pub pix_fmt: Option<String>,
}

/// The selected audio stream, if the source has one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioStream {
    pub index: usize,
    pub codec: String,
    pub channels: u32,
    pub channel_layout: Option<String>,
    pub sample_rate: u32,
    pub language: Option<String>,
    pub is_default: bool,
}

/// Everything the transcoder needs to know about a source file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceProbe {
    pub duration_secs: f64,
    pub size_bytes: u64,
    pub video: VideoStream,
    /// `None` means the source genuinely has no audio stream. It never means
    /// "we could not tell" — that case is an `Err` from [`probe`].
    pub audio: Option<AudioStream>,
    /// How many audio streams the source has, for the job event when we pick one.
    pub audio_stream_count: usize,
}

impl SourceProbe {
    /// Frames the output should contain: duration at exactly 25 fps.
    ///
    /// The in-house suite compares this against the delivered MXF's frame count
    /// to catch rate errors that a codec/pixel-format check cannot see.
    pub fn expected_output_frames(&self) -> u64 {
        (self.duration_secs * 25.0).round().max(0.0) as u64
    }
}

/// Probe a media file.
///
/// Returns `Err` when ffprobe fails, returns unparseable JSON, or reports no
/// video stream. Callers must send the job to REQUIRES_REVIEW with
/// `error_code=PROBE_FAILED` rather than guessing (defect D-07).
pub async fn probe(ffprobe: &Path, media: &Path) -> Result<SourceProbe> {
    let media_arg = media.to_string_lossy().into_owned();
    let args = [
        "-v",
        "error",
        "-print_format",
        "json",
        "-show_streams",
        "-show_format",
        &media_arg,
    ];

    let out = run_capture(ffprobe, args, PROBE_TIMEOUT)
        .await
        .with_context(|| format!("Could not run ffprobe at {ffprobe:?}"))?;

    if !out.success {
        return Err(anyhow!(
            "ffprobe failed for {media:?}{}: {}",
            if out.timed_out { " (timed out)" } else { "" },
            if out.stderr_tail.trim().is_empty() {
                "no diagnostic output".to_string()
            } else {
                out.stderr_tail.trim().to_string()
            }
        ));
    }

    parse(&out.stdout).with_context(|| format!("Could not interpret ffprobe output for {media:?}"))
}

/// Parse ffprobe's JSON. Split out so the decision matrix can be tested against
/// recorded output without running ffprobe.
pub fn parse(json: &str) -> Result<SourceProbe> {
    let v: serde_json::Value =
        serde_json::from_str(json).context("ffprobe did not return valid JSON")?;

    let streams = v
        .get("streams")
        .and_then(|s| s.as_array())
        .ok_or_else(|| anyhow!("ffprobe output has no streams array"))?;

    // Video: the largest raster that is not cover art. Some news-site MP4s carry
    // a poster image as a video stream; picking it would transcode a still.
    let video = streams
        .iter()
        .filter(|s| s.get("codec_type").and_then(|t| t.as_str()) == Some("video"))
        .filter(|s| {
            s.pointer("/disposition/attached_pic")
                .and_then(|d| d.as_i64())
                .unwrap_or(0)
                == 0
        })
        .max_by_key(|s| {
            let w = s.get("width").and_then(|x| x.as_u64()).unwrap_or(0);
            let h = s.get("height").and_then(|x| x.as_u64()).unwrap_or(0);
            w * h
        })
        .ok_or_else(|| anyhow!("Source has no usable video stream"))?;

    let width = video.get("width").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    let height = video.get("height").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
    if width == 0 || height == 0 {
        return Err(anyhow!("Video stream reports a zero dimension"));
    }

    let fps = parse_rational(video.get("r_frame_rate").and_then(|x| x.as_str()))
        .or_else(|| parse_rational(video.get("avg_frame_rate").and_then(|x| x.as_str())))
        .unwrap_or(0.0);
    if !(fps.is_finite() && fps > 0.0) {
        return Err(anyhow!("Video stream reports no usable frame rate"));
    }

    let video = VideoStream {
        index: video.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as usize,
        codec: video
            .get("codec_name")
            .and_then(|x| x.as_str())
            .unwrap_or("unknown")
            .to_string(),
        width,
        height,
        fps,
        scan: ScanType::from_field_order(video.get("field_order").and_then(|x| x.as_str())),
        pix_fmt: video
            .get("pix_fmt")
            .and_then(|x| x.as_str())
            .map(str::to_string),
    };

    let audio_streams: Vec<&serde_json::Value> = streams
        .iter()
        .filter(|s| s.get("codec_type").and_then(|t| t.as_str()) == Some("audio"))
        .collect();

    // Preference order: the stream flagged default, then Greek, then English,
    // then whichever has the most channels. A foreign-language second track on
    // an agency feed is a real thing to get wrong on air.
    let chosen = audio_streams
        .iter()
        .copied()
        .max_by_key(|s| {
            let is_default = s
                .pointer("/disposition/default")
                .and_then(|d| d.as_i64())
                .unwrap_or(0);
            let lang = s
                .pointer("/tags/language")
                .and_then(|l| l.as_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let lang_rank = match lang.as_str() {
                "el" | "ell" | "gre" => 3,
                "en" | "eng" => 2,
                "" => 1,
                _ => 0,
            };
            let channels = s.get("channels").and_then(|c| c.as_u64()).unwrap_or(0);
            (is_default, lang_rank, channels)
        })
        .map(|s| AudioStream {
            index: s.get("index").and_then(|x| x.as_u64()).unwrap_or(0) as usize,
            codec: s
                .get("codec_name")
                .and_then(|x| x.as_str())
                .unwrap_or("unknown")
                .to_string(),
            channels: s.get("channels").and_then(|c| c.as_u64()).unwrap_or(0) as u32,
            channel_layout: s
                .get("channel_layout")
                .and_then(|x| x.as_str())
                .map(str::to_string),
            sample_rate: s
                .get("sample_rate")
                .and_then(|x| x.as_str())
                .and_then(|x| x.parse().ok())
                .unwrap_or(0),
            language: s
                .pointer("/tags/language")
                .and_then(|l| l.as_str())
                .map(str::to_string),
            is_default: s
                .pointer("/disposition/default")
                .and_then(|d| d.as_i64())
                .unwrap_or(0)
                == 1,
        })
        // A stream that reports zero channels is not usable audio.
        .filter(|a| a.channels > 0);

    let duration_secs = v
        .pointer("/format/duration")
        .and_then(|d| d.as_str())
        .and_then(|d| d.parse::<f64>().ok())
        .or_else(|| {
            streams
                .iter()
                .filter_map(|s| s.get("duration").and_then(|d| d.as_str()))
                .filter_map(|d| d.parse::<f64>().ok())
                .fold(None, |acc: Option<f64>, d| Some(acc.map_or(d, |a| a.max(d))))
        })
        .unwrap_or(0.0);

    Ok(SourceProbe {
        duration_secs,
        size_bytes: v
            .pointer("/format/size")
            .and_then(|s| s.as_str())
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        video,
        audio: chosen,
        audio_stream_count: audio_streams.len(),
    })
}

/// ffprobe reports rates as `"25/1"` or `"30000/1001"`.
fn parse_rational(raw: Option<&str>) -> Option<f64> {
    let raw = raw?.trim();
    let (num, den) = raw.split_once('/')?;
    let num: f64 = num.trim().parse().ok()?;
    let den: f64 = den.trim().parse().ok()?;
    if den == 0.0 {
        return None;
    }
    Some(num / den)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_with(video_extra: &str, audio: &str) -> String {
        format!(
            r#"{{
              "streams": [
                {{ "index": 0, "codec_type": "video", "codec_name": "h264",
                   "width": 1920, "height": 1080 {video_extra} }}
                {audio}
              ],
              "format": {{ "duration": "12.500000", "size": "10485760" }}
            }}"#
        )
    }

    #[test]
    fn reads_rate_scan_and_geometry() {
        let p = parse(&json_with(
            r#", "r_frame_rate": "25/1", "field_order": "progressive", "pix_fmt": "yuv420p""#,
            "",
        ))
        .unwrap();
        assert_eq!(p.video.width, 1920);
        assert_eq!(p.video.fps, 25.0);
        assert_eq!(p.video.scan, ScanType::Progressive);
        assert_eq!(p.duration_secs, 12.5);
        assert!(p.audio.is_none());
        assert_eq!(p.expected_output_frames(), 313); // 12.5 * 25 = 312.5 -> 313
    }

    #[test]
    fn ntsc_rational_rates_are_read_exactly() {
        let p = parse(&json_with(r#", "r_frame_rate": "30000/1001""#, "")).unwrap();
        assert!((p.video.fps - 29.97).abs() < 0.001, "got {}", p.video.fps);
    }

    #[test]
    fn field_order_maps_to_scan_type() {
        for (raw, expected) in [
            ("tt", ScanType::InterlacedTff),
            ("tb", ScanType::InterlacedTff),
            ("bb", ScanType::InterlacedBff),
            ("bt", ScanType::InterlacedBff),
            ("progressive", ScanType::Progressive),
            ("unknown", ScanType::Unknown),
        ] {
            let p = parse(&json_with(
                &format!(r#", "r_frame_rate": "25/1", "field_order": "{raw}""#),
                "",
            ))
            .unwrap();
            assert_eq!(p.video.scan, expected, "field_order={raw}");
        }
    }

    #[test]
    fn cover_art_is_never_chosen_as_the_video_stream() {
        // A news-site MP4 with an embedded poster image. Picking it would
        // transcode a still frame and deliver a 12-second photo.
        let json = r#"{
          "streams": [
            { "index": 0, "codec_type": "video", "codec_name": "mjpeg",
              "width": 3840, "height": 2160, "r_frame_rate": "90000/1",
              "disposition": { "attached_pic": 1 } },
            { "index": 1, "codec_type": "video", "codec_name": "h264",
              "width": 1280, "height": 720, "r_frame_rate": "25/1" }
          ],
          "format": { "duration": "8.0", "size": "100" }
        }"#;
        let p = parse(json).unwrap();
        assert_eq!(p.video.codec, "h264");
        assert_eq!(p.video.width, 1280);
    }

    #[test]
    fn the_largest_real_video_stream_wins() {
        let json = r#"{
          "streams": [
            { "index": 0, "codec_type": "video", "codec_name": "h264",
              "width": 640, "height": 360, "r_frame_rate": "25/1" },
            { "index": 1, "codec_type": "video", "codec_name": "h264",
              "width": 1920, "height": 1080, "r_frame_rate": "25/1" }
          ],
          "format": { "duration": "8.0", "size": "100" }
        }"#;
        assert_eq!(parse(json).unwrap().video.width, 1920);
    }

    #[test]
    fn audio_selection_prefers_default_then_greek_then_english() {
        // An agency feed with international sound and a Greek commentary track:
        // picking the wrong one puts the wrong language on air.
        let json = r#"{
          "streams": [
            { "index": 0, "codec_type": "video", "codec_name": "h264",
              "width": 1920, "height": 1080, "r_frame_rate": "25/1" },
            { "index": 1, "codec_type": "audio", "codec_name": "aac", "channels": 2,
              "sample_rate": "48000", "tags": { "language": "eng" } },
            { "index": 2, "codec_type": "audio", "codec_name": "aac", "channels": 2,
              "sample_rate": "48000", "tags": { "language": "ell" } }
          ],
          "format": { "duration": "8.0", "size": "100" }
        }"#;
        let p = parse(json).unwrap();
        assert_eq!(p.audio.as_ref().unwrap().index, 2);
        assert_eq!(p.audio_stream_count, 2);

        // ...but an explicit default disposition beats language.
        let json = json.replace(
            r#"{ "index": 1, "codec_type": "audio", "codec_name": "aac", "channels": 2,
              "sample_rate": "48000", "tags": { "language": "eng" } }"#,
            r#"{ "index": 1, "codec_type": "audio", "codec_name": "aac", "channels": 2,
              "sample_rate": "48000", "disposition": { "default": 1 },
              "tags": { "language": "eng" } }"#,
        );
        assert_eq!(parse(&json).unwrap().audio.unwrap().index, 1);
    }

    #[test]
    fn a_zero_channel_audio_stream_is_not_treated_as_audio() {
        let json = json_with(
            r#", "r_frame_rate": "25/1""#,
            r#", { "index": 1, "codec_type": "audio", "codec_name": "aac", "channels": 0 }"#,
        );
        assert!(parse(&json).unwrap().audio.is_none());
    }

    #[test]
    fn unusable_sources_are_errors_not_silent_defaults() {
        // Every one of these used to collapse into "(0.0, 0)" -- duration zero,
        // no audio -- and get transcoded to a silent clip (defect D-07).
        assert!(parse("").is_err(), "empty output must be an error");
        assert!(parse("not json").is_err());
        assert!(parse(r#"{"format":{}}"#).is_err(), "missing streams array");
        assert!(
            parse(r#"{"streams":[],"format":{"duration":"5"}}"#).is_err(),
            "a source with no video stream must be an error"
        );
        // A video stream with no usable frame rate cannot be rate-converted.
        assert!(parse(&json_with(r#", "r_frame_rate": "0/0""#, "")).is_err());
        // Zero geometry.
        assert!(parse(
            r#"{"streams":[{"index":0,"codec_type":"video","width":0,"height":0,
                "r_frame_rate":"25/1"}],"format":{"duration":"5"}}"#
        )
        .is_err());
    }

    #[test]
    fn duration_falls_back_to_the_stream_when_the_container_omits_it() {
        // Common for fragmented MP4 pulled from an HLS manifest.
        let json = r#"{
          "streams": [
            { "index": 0, "codec_type": "video", "codec_name": "h264",
              "width": 1920, "height": 1080, "r_frame_rate": "25/1", "duration": "42.0" }
          ],
          "format": { "size": "100" }
        }"#;
        assert_eq!(parse(json).unwrap().duration_secs, 42.0);
    }
}
