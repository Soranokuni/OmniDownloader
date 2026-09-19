//! Pre-delivery compliance gate (plan P1.6, defect D-06).
//!
//! Nothing used to verify the MXF before it went into the watchfolder. Every
//! check in the pipeline was on the *input* side: if ffmpeg exited zero, the
//! file was delivered. But ffmpeg exits zero for plenty of files Dalet will
//! reject or mis-play — a filter chain that silently dropped to 4:2:0, an audio
//! map that produced one stereo stream instead of eight mono ones, a truncated
//! output from a source that ended early.
//!
//! This module is the last thing between the transcoder and air. A failed
//! report sends the job to REQUIRES_REVIEW with `COMPLIANCE_FAILED` and nothing
//! is delivered: an operator seeing a job in review is a minor annoyance, a
//! wrong file in the running order is not.

use std::path::Path;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use omni_core::process::run_capture;
use serde::{Deserialize, Serialize};

const VERIFY_TIMEOUT: Duration = Duration::from_secs(120);

/// One assertion about the delivered file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Check {
    pub name: String,
    pub expected: String,
    pub actual: String,
    pub ok: bool,
}

impl Check {
    fn eq(name: &str, expected: impl std::fmt::Display, actual: impl std::fmt::Display) -> Self {
        let (e, a) = (expected.to_string(), actual.to_string());
        Self {
            ok: e == a,
            name: name.to_string(),
            expected: e,
            actual: a,
        }
    }

    fn assert(name: &str, expected: &str, actual: impl std::fmt::Display, ok: bool) -> Self {
        Self {
            name: name.to_string(),
            expected: expected.to_string(),
            actual: actual.to_string(),
            ok,
        }
    }
}

/// The full report, stored on the job so MCR can see exactly what failed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComplianceReport {
    pub pass: bool,
    pub checks: Vec<Check>,
    /// Measured loudness, when the transcode normalised the audio.
    pub output_lufs: Option<f64>,
}

impl ComplianceReport {
    pub fn failures(&self) -> Vec<&Check> {
        self.checks.iter().filter(|c| !c.ok).collect()
    }

    /// One-line summary for the MCR card and the job event.
    pub fn summary(&self) -> String {
        if self.pass {
            format!("All {} compliance checks passed", self.checks.len())
        } else {
            let failed = self.failures();
            format!(
                "{} of {} checks failed: {}",
                failed.len(),
                self.checks.len(),
                failed
                    .iter()
                    .map(|c| format!("{} (expected {}, got {})", c.name, c.expected, c.actual))
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        }
    }
}

/// Probe the finished MXF and check it against the Dalet specification.
pub async fn verify_mxf(
    ffprobe: &Path,
    mxf: &Path,
    source_duration_secs: f64,
) -> Result<ComplianceReport> {
    let mxf_arg = mxf.to_string_lossy().into_owned();
    let out = run_capture(
        ffprobe,
        [
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_streams",
            "-show_format",
            &mxf_arg,
        ],
        VERIFY_TIMEOUT,
    )
    .await
    .with_context(|| format!("Could not run ffprobe at {ffprobe:?}"))?;

    if !out.success {
        return Err(anyhow!(
            "ffprobe could not read the delivered MXF {mxf:?}: {}",
            out.stderr_tail.trim()
        ));
    }

    let size_bytes = tokio::fs::metadata(mxf).await.map(|m| m.len()).unwrap_or(0);
    check_probe_json(&out.stdout, size_bytes, source_duration_secs)
}

/// Apply the checks to ffprobe JSON. Separated so the gate can be tested
/// against recorded output for files we cannot easily produce on demand.
pub fn check_probe_json(
    json: &str,
    size_bytes: u64,
    source_duration_secs: f64,
) -> Result<ComplianceReport> {
    let v: serde_json::Value =
        serde_json::from_str(json).context("ffprobe returned invalid JSON for the output MXF")?;
    let empty = Vec::new();
    let streams = v
        .get("streams")
        .and_then(|s| s.as_array())
        .unwrap_or(&empty);

    let mut checks = Vec::new();

    // ---- container -------------------------------------------------------
    let format_name = v
        .pointer("/format/format_name")
        .and_then(|f| f.as_str())
        .unwrap_or("");
    checks.push(Check::assert(
        "container",
        "mxf",
        format_name,
        format_name.contains("mxf"),
    ));

    // ---- video -----------------------------------------------------------
    let videos: Vec<&serde_json::Value> = streams
        .iter()
        .filter(|s| s.get("codec_type").and_then(|t| t.as_str()) == Some("video"))
        .collect();

    checks.push(Check::eq("video_stream_count", 1, videos.len()));

    if let Some(vs) = videos.first() {
        let s = |k: &str| vs.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let n = |k: &str| vs.get(k).and_then(|x| x.as_u64()).unwrap_or(0);

        checks.push(Check::eq("video_codec", "mpeg2video", s("codec_name")));
        checks.push(Check::eq("width", 1920, n("width")));
        checks.push(Check::eq("height", 1080, n("height")));
        checks.push(Check::eq("pixel_format", "yuv422p", s("pix_fmt")));
        // Exactly 25/1. A 25000/1000 or 50/2 would also be 25 fps, so compare
        // numerically rather than on the string.
        let rate = s("r_frame_rate");
        let fps = parse_rational(&rate).unwrap_or(0.0);
        checks.push(Check::assert(
            "frame_rate",
            "25/1",
            &rate,
            (fps - 25.0).abs() < 0.001,
        ));
        // Top field first. `tt` is what a correctly flagged 1080i50 file says;
        // `tb` also means top-coded-first and is accepted.
        let field_order = s("field_order");
        checks.push(Check::assert(
            "field_order",
            "tt (top field first)",
            if field_order.is_empty() { "unset" } else { &field_order },
            field_order == "tt" || field_order == "tb",
        ));
        // 4:2:2 Profile @ Main Level. ffprobe reports the profile name.
        let profile = s("profile");
        checks.push(Check::assert(
            "video_profile",
            "4:2:2",
            if profile.is_empty() { "unset" } else { &profile },
            profile.contains("422") || profile.contains("4:2:2"),
        ));

        for (name, key, expected) in [
            ("color_primaries", "color_primaries", "bt709"),
            ("color_transfer", "color_transfer", "bt709"),
            ("color_space", "color_space", "bt709"),
        ] {
            let actual = vs.get(key).and_then(|x| x.as_str()).unwrap_or("unset");
            // Some ffprobe builds leave these unset on MXF even when the
            // bitstream carries them. Unset is tolerated; a *different* value is
            // not, because that one would actually shift the colours.
            checks.push(Check::assert(
                name,
                expected,
                actual,
                actual == expected || actual == "unset",
            ));
        }

        if let Some(br) = vs
            .get("bit_rate")
            .and_then(|x| x.as_str())
            .and_then(|x| x.parse::<f64>().ok())
        {
            let mbps = br / 1_000_000.0;
            checks.push(Check::assert(
                "video_bitrate_mbps",
                "45-55",
                format!("{mbps:.1}"),
                (45.0..=55.0).contains(&mbps),
            ));
        }
    }

    // ---- audio: EBU R48 ---------------------------------------------------
    let audios: Vec<&serde_json::Value> = streams
        .iter()
        .filter(|s| s.get("codec_type").and_then(|t| t.as_str()) == Some("audio"))
        .collect();

    // The single most important audio check: Dalet expects eight discrete mono
    // tracks and mis-routes anything else.
    checks.push(Check::eq("audio_stream_count", 8, audios.len()));

    for (i, a) in audios.iter().enumerate() {
        let codec = a.get("codec_name").and_then(|x| x.as_str()).unwrap_or("");
        let channels = a.get("channels").and_then(|x| x.as_u64()).unwrap_or(0);
        let rate = a
            .get("sample_rate")
            .and_then(|x| x.as_str())
            .and_then(|x| x.parse::<u64>().ok())
            .unwrap_or(0);

        checks.push(Check::eq(&format!("audio{}_codec", i + 1), "pcm_s24le", codec));
        checks.push(Check::eq(&format!("audio{}_channels", i + 1), 1, channels));
        checks.push(Check::eq(&format!("audio{}_sample_rate", i + 1), 48000, rate));
    }

    // ---- duration and size ------------------------------------------------
    let duration = v
        .pointer("/format/duration")
        .and_then(|d| d.as_str())
        .and_then(|d| d.parse::<f64>().ok())
        .unwrap_or(0.0);

    checks.push(Check::assert(
        "duration_present",
        "> 0.5 s",
        format!("{duration:.2} s"),
        duration > 0.5,
    ));

    if source_duration_secs > 0.5 {
        // A transcode that stopped early is the classic way a story reaches air
        // with its last twenty seconds missing.
        let drift = (duration - source_duration_secs).abs();
        let tolerance = (source_duration_secs * 0.02).max(0.5);
        checks.push(Check::assert(
            "duration_matches_source",
            &format!("within {tolerance:.2} s of {source_duration_secs:.2} s"),
            format!("{duration:.2} s (off by {drift:.2} s)"),
            drift <= tolerance,
        ));
    }

    checks.push(Check::assert(
        "file_size",
        "> 1 MB",
        format!("{:.1} MB", size_bytes as f64 / 1_048_576.0),
        size_bytes > 1_048_576,
    ));

    if duration > 0.5 && size_bytes > 0 {
        // 50 Mbps video plus 8 x 1152 kbps audio. A file far below this did not
        // actually encode at the bitrate it claims.
        let expected_bytes = duration * (50_000_000.0 + 8.0 * 1_152_000.0) / 8.0;
        let ratio = size_bytes as f64 / expected_bytes;
        checks.push(Check::assert(
            "size_matches_bitrate",
            "within 20% of 50 Mbps x duration",
            format!("{:.0}% of expected", ratio * 100.0),
            (0.8..=1.2).contains(&ratio),
        ));
    }

    let pass = checks.iter().all(|c| c.ok);
    Ok(ComplianceReport {
        pass,
        checks,
        output_lufs: None,
    })
}

fn parse_rational(raw: &str) -> Option<f64> {
    let (num, den) = raw.trim().split_once('/')?;
    let num: f64 = num.parse().ok()?;
    let den: f64 = den.parse().ok()?;
    if den == 0.0 {
        return None;
    }
    Some(num / den)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ffprobe output for a correct Sony XDCAM HD422 1080i50 MXF.
    fn compliant_json(duration: f64) -> String {
        let audio: Vec<String> = (1..=8)
            .map(|i| {
                format!(
                    r#"{{ "index": {i}, "codec_type": "audio", "codec_name": "pcm_s24le",
                          "channels": 1, "sample_rate": "48000" }}"#
                )
            })
            .collect();
        format!(
            r#"{{
              "streams": [
                {{ "index": 0, "codec_type": "video", "codec_name": "mpeg2video",
                   "profile": "4:2:2", "width": 1920, "height": 1080,
                   "pix_fmt": "yuv422p", "r_frame_rate": "25/1", "field_order": "tt",
                   "color_primaries": "bt709", "color_transfer": "bt709",
                   "color_space": "bt709", "bit_rate": "50000000" }},
                {}
              ],
              "format": {{ "format_name": "mxf", "duration": "{duration:.6}" }}
            }}"#,
            audio.join(",\n")
        )
    }

    /// Size a compliant file of this duration would actually have.
    fn plausible_size(duration: f64) -> u64 {
        (duration * (50_000_000.0 + 8.0 * 1_152_000.0) / 8.0) as u64
    }

    #[test]
    fn a_compliant_file_passes_every_check() {
        let r = check_probe_json(&compliant_json(92.0), plausible_size(92.0), 92.0).unwrap();
        assert!(r.pass, "compliant file was rejected: {}", r.summary());
        assert!(r.checks.len() > 30, "suspiciously few checks: {}", r.checks.len());
    }

    #[test]
    fn a_stereo_interleaved_file_is_rejected() {
        // The single most likely audio mistake, and the one Dalet mis-routes:
        // one 2-channel stream instead of eight discrete mono tracks.
        let json = r#"{
          "streams": [
            { "index": 0, "codec_type": "video", "codec_name": "mpeg2video",
              "profile": "4:2:2", "width": 1920, "height": 1080, "pix_fmt": "yuv422p",
              "r_frame_rate": "25/1", "field_order": "tt" },
            { "index": 1, "codec_type": "audio", "codec_name": "pcm_s24le",
              "channels": 2, "sample_rate": "48000" }
          ],
          "format": { "format_name": "mxf", "duration": "92.0" }
        }"#;
        let r = check_probe_json(json, plausible_size(92.0), 92.0).unwrap();
        assert!(!r.pass);
        let names: Vec<&str> = r.failures().iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"audio_stream_count"), "{names:?}");
        assert!(names.contains(&"audio1_channels"), "{names:?}");
    }

    #[test]
    fn a_720p_file_is_rejected() {
        let json = compliant_json(92.0)
            .replace(r#""width": 1920"#, r#""width": 1280"#)
            .replace(r#""height": 1080"#, r#""height": 720"#);
        let r = check_probe_json(&json, plausible_size(92.0), 92.0).unwrap();
        assert!(!r.pass);
        let names: Vec<&str> = r.failures().iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"width") && names.contains(&"height"), "{names:?}");
    }

    #[test]
    fn a_progressive_or_wrongly_flagged_file_is_rejected() {
        // The output of a filter-chain regression: correct in every other
        // respect, but not actually interlaced.
        let json = compliant_json(92.0).replace(r#""field_order": "tt""#, r#""field_order": "progressive""#);
        let r = check_probe_json(&json, plausible_size(92.0), 92.0).unwrap();
        assert!(!r.pass);
        assert!(r.failures().iter().any(|c| c.name == "field_order"));

        // Bottom field first is just as wrong: it plays with the fields swapped.
        let json = compliant_json(92.0).replace(r#""field_order": "tt""#, r#""field_order": "bb""#);
        assert!(!check_probe_json(&json, plausible_size(92.0), 92.0).unwrap().pass);
    }

    #[test]
    fn a_420_chroma_file_is_rejected() {
        let json = compliant_json(92.0).replace(r#""pix_fmt": "yuv422p""#, r#""pix_fmt": "yuv420p""#);
        let r = check_probe_json(&json, plausible_size(92.0), 92.0).unwrap();
        assert!(!r.pass);
        assert!(r.failures().iter().any(|c| c.name == "pixel_format"));
    }

    #[test]
    fn a_wrong_frame_rate_is_rejected_but_an_equivalent_rational_is_not() {
        let json = compliant_json(92.0).replace(r#""r_frame_rate": "25/1""#, r#""r_frame_rate": "30000/1001""#);
        assert!(!check_probe_json(&json, plausible_size(92.0), 92.0).unwrap().pass);

        // 50/2 is still exactly 25 fps; rejecting it would be a false alarm.
        let json = compliant_json(92.0).replace(r#""r_frame_rate": "25/1""#, r#""r_frame_rate": "50/2""#);
        let r = check_probe_json(&json, plausible_size(92.0), 92.0).unwrap();
        assert!(r.pass, "an equivalent rational rate was rejected: {}", r.summary());
    }

    #[test]
    fn a_truncated_transcode_is_rejected() {
        // ffmpeg exits zero when a source ends early. The file looks perfect and
        // is twenty seconds short -- the failure nobody notices until air.
        let r = check_probe_json(&compliant_json(70.0), plausible_size(70.0), 92.0).unwrap();
        assert!(!r.pass);
        assert!(
            r.failures().iter().any(|c| c.name == "duration_matches_source"),
            "{}",
            r.summary()
        );
    }

    #[test]
    fn small_honest_rounding_differences_do_not_fail_a_good_file() {
        // A frame or two of difference is normal and must not send good work to
        // review; an operator who learns to ignore this gate is worse than none.
        let r = check_probe_json(&compliant_json(92.04), plausible_size(92.04), 92.0).unwrap();
        assert!(r.pass, "{}", r.summary());
    }

    #[test]
    fn an_empty_or_tiny_file_is_rejected() {
        let r = check_probe_json(&compliant_json(92.0), 1024, 92.0).unwrap();
        assert!(!r.pass);
        let names: Vec<&str> = r.failures().iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"file_size"), "{names:?}");
        assert!(names.contains(&"size_matches_bitrate"), "{names:?}");
    }

    #[test]
    fn a_non_mxf_container_is_rejected() {
        let json = compliant_json(92.0).replace(r#""format_name": "mxf""#, r#""format_name": "mov,mp4,m4a""#);
        let r = check_probe_json(&json, plausible_size(92.0), 92.0).unwrap();
        assert!(!r.pass);
        assert!(r.failures().iter().any(|c| c.name == "container"));
    }

    #[test]
    fn unset_colour_metadata_is_tolerated_but_wrong_metadata_is_not() {
        // Some ffprobe builds leave these blank on MXF even when the bitstream
        // carries them, so blank must not fail a good file...
        let json = compliant_json(92.0)
            .replace(r#""color_primaries": "bt709","#, "")
            .replace(r#""color_transfer": "bt709","#, "")
            .replace(r#""color_space": "bt709","#, "");
        assert!(check_probe_json(&json, plausible_size(92.0), 92.0).unwrap().pass);

        // ...but Rec.601 primaries would actually shift the colours on air.
        let json = compliant_json(92.0).replace(r#""color_primaries": "bt709""#, r#""color_primaries": "smpte170m""#);
        let r = check_probe_json(&json, plausible_size(92.0), 92.0).unwrap();
        assert!(!r.pass);
        assert!(r.failures().iter().any(|c| c.name == "color_primaries"));
    }

    #[test]
    fn the_summary_names_what_failed_so_an_operator_can_act() {
        let json = compliant_json(92.0).replace(r#""pix_fmt": "yuv422p""#, r#""pix_fmt": "yuv420p""#);
        let r = check_probe_json(&json, plausible_size(92.0), 92.0).unwrap();
        let summary = r.summary();
        assert!(summary.contains("pixel_format"), "{summary}");
        assert!(summary.contains("yuv422p"), "{summary}");
        assert!(summary.contains("yuv420p"), "{summary}");
    }

    #[test]
    fn a_second_video_stream_is_rejected() {
        // A stray thumbnail stream confuses Dalet's ingest.
        let json = compliant_json(92.0).replace(
            r#""streams": ["#,
            r#""streams": [
                { "index": 99, "codec_type": "video", "codec_name": "mjpeg",
                  "width": 320, "height": 180, "r_frame_rate": "25/1" },"#,
        );
        let r = check_probe_json(&json, plausible_size(92.0), 92.0).unwrap();
        assert!(!r.pass);
        assert!(r.failures().iter().any(|c| c.name == "video_stream_count"));
    }
}
