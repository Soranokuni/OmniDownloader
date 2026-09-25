//! Start-up encoder self-test.
//!
//! `omni_core::selftest` proves each tool *starts*. That is not the same as
//! proving the toolchain can make the file that goes to air: an FFmpeg 9 build
//! printed its version happily and then rejected `-top`, so every job failed
//! at transcode while the panel said "tools ok".
//!
//! This runs the real chain once on a 2-second synthetic clip — the same
//! `Transcoder` (default loudness policy), `Rewrapper` and compliance gate a
//! job uses — so a tool that cannot produce a compliant RDD9 file is caught
//! before the first job, with the step that failed.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use omni_core::process::{self, RunOpts};

use crate::rewrapper::Rewrapper;
use crate::transcoder::Transcoder;
use crate::verify::verify_mxf_with_clip;

/// Clip name written into the test file, and the gate checks it.
const TEST_CLIP: &str = "0_OMNI_SELFTEST";

pub struct EncoderReport {
    pub elapsed: Duration,
}

/// Make a clip, transcode, rewrap, verify. `Err` names the failing step.
///
/// Works in `{work_dir}/selftest`, which is removed afterwards either way.
pub async fn encoder_self_test(
    ffmpeg: &Path,
    ffprobe: &Path,
    bmxtranswrap: &Path,
    work_dir: &Path,
) -> Result<EncoderReport> {
    let started = Instant::now();
    let dir = work_dir.join("selftest");
    let _ = tokio::fs::remove_dir_all(&dir).await;
    tokio::fs::create_dir_all(&dir).await.context("cannot create the self-test directory")?;

    let result = run_chain(ffmpeg, ffprobe, bmxtranswrap, &dir).await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    result.map(|()| EncoderReport { elapsed: started.elapsed() })
}

async fn run_chain(ffmpeg: &Path, ffprobe: &Path, bmxtranswrap: &Path, dir: &Path) -> Result<()> {
    // Source: codecs every FFmpeg build has (no libx264 needed), 25p with a
    // tone, so the progressive→25i branch and two-pass loudness both run.
    let source = dir.join("source.mov");
    let args: Vec<String> = [
        "-y", "-nostdin", "-v", "error", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=25:duration=2",
        "-f", "lavfi", "-i", "sine=frequency=1000:sample_rate=48000:duration=2", "-c:v", "mpeg2video",
        "-q:v", "4", "-c:a", "pcm_s16le", "-shortest",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([source.to_string_lossy().into_owned()])
    .collect();
    let made = process::run(ffmpeg, &args, RunOpts::new(Duration::from_secs(60)))
        .await
        .context("could not run ffmpeg to make the test clip")?;
    if !made.success || !source.is_file() {
        return Err(anyhow!("ffmpeg could not make the test clip: {}", last_line(&made.stderr_tail)));
    }

    let (intermediate, duration) = Transcoder::new(ffmpeg, ffprobe)
        .transcode(0, &source, dir, |_| {})
        .await
        .map_err(|e| anyhow!("transcode failed: {}", last_line(&format!("{e:#}"))))?;

    let final_mxf: PathBuf = Rewrapper::new(bmxtranswrap)
        .rewrap_with_clip(0, &intermediate, dir, Some(TEST_CLIP), None)
        .await
        .map_err(|e| anyhow!("RDD9 rewrap failed: {}", last_line(&format!("{e:#}"))))?;

    let report = verify_mxf_with_clip(ffprobe, &final_mxf, duration, Some(TEST_CLIP))
        .await
        .map_err(|e| anyhow!("compliance check could not run: {e:#}"))?;
    if !report.pass {
        return Err(anyhow!("test file failed the compliance gate: {}", report.summary()));
    }
    Ok(())
}

/// The line of a tool's output that says what went wrong. ffmpeg names the
/// cause ("… is not a encoding option", "Unrecognized option") and then adds a
/// generic "Error opening output file"; the cause is what an operator needs.
fn last_line(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let has = |l: &str, needles: &[&str]| {
        let lower = l.to_ascii_lowercase();
        needles.iter().any(|n| lower.contains(n))
    };
    // Most specific first: "Invalid argument" is also how ffmpeg ends the
    // generic line, so it must not outrank the line naming the option.
    let tiers: [&[&str]; 3] = [
        &["is not a", "unrecognized", "no such", "not found", "unknown encoder", "option not found"],
        &["invalid", "unknown"],
        &["error", "failed"],
    ];
    let pick = tiers
        .iter()
        .find_map(|needles| lines.iter().rev().find(|l| has(l, needles)))
        .or(lines.last())
        .copied()
        .unwrap_or("no output");
    pick.chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reported_reason_is_the_tools_own_error_line() {
        let stderr = "Input #0, lavfi\n  Stream #0:0: Video\n\
                      [out#0/mxf @ 0000] Codec AVOption top (top field first) is not a encoding option.\n\
                      Error opening output file x.mxf.\n\
                      Error opening output files: Invalid argument\n";
        assert!(last_line(stderr).contains("top (top field first) is not a encoding option"), "{}", last_line(stderr));
        assert_eq!(last_line("frame=1\nConversion failed!"), "Conversion failed!");
        assert_eq!(last_line(""), "no output");
    }
}
