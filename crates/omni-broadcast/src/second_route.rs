//! The second route through the pipeline (plan P11.6).
//!
//! A source that failed the probe, the encode, the wrap or the compliance gate
//! gets one more try before review, by another way in: the link is downloaded
//! again with other formats ([`crate::downloader::FormatRoute::Second`]) and the
//! source is copied into a clean Matroska container before the encode, which
//! gives it fresh, monotonic timestamps (a broken mux, a timestamp jump in a
//! transport stream). The encode, the wrap and the compliance gate are the
//! usual ones, so the result is the same format or the job still goes to
//! review. The engine takes the route once per job ([`crate::pipeline`]).
//!
//! A remux never hides a cut: when the source's own index lists more video
//! frames than could be read from it (a download that stopped part way), the
//! remux is refused instead of handing the encoder a shorter, honest-looking
//! file; so is a copy shorter than the source says it lasts (a transport
//! stream keeps no index, and a jump in its clock may stand for lost pictures).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use omni_core::process::{run, run_capture, RunOpts};
use tokio_util::sync::CancellationToken;

use crate::errors::{code_in_text, ErrorCode};
use crate::probe::SOURCE_DEMUXERS;

/// The failures that take the second route: the source is on disk but could
/// not be made into a compliant file.
pub const CODES: [ErrorCode; 4] =
    [ErrorCode::ProbeFailed, ErrorCode::TranscodeFailed, ErrorCode::RewrapFailed, ErrorCode::ComplianceFailed];

/// The code of a failure that takes the second route, as the worker would
/// classify it; `None` for any other.
pub fn applies(e: &anyhow::Error) -> Option<ErrorCode> {
    code_in_text(&format!("{e:#}")).filter(|c| CODES.contains(c))
}

/// The remuxed source in the job's workspace. A `source.` name, so a video
/// that waits for the watchfolder still finds it for the archive.
pub const REMUX_NAME: &str = "source.remux.mkv";

const PROBE_TIMEOUT: Duration = Duration::from_secs(120);

/// ffmpeg's arguments for the remux of `source` into `dest`: the video stream
/// the probe chose (`video_index`) and every audio stream, copied, never
/// re-encoded; timestamps generated where missing and shifted to start at
/// zero. Local files and the source demuxers only, as for the encode (P2.11).
pub fn remux_args(source: &Path, video_index: usize, dest: &Path) -> Vec<String> {
    let mut args: Vec<String> = [
        "-hide_banner",
        "-nostdin",
        "-v",
        "error",
        "-y",
        "-format_whitelist",
        SOURCE_DEMUXERS,
        "-protocol_whitelist",
        "file",
        "-fflags",
        "+genpts",
        "-i",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(source.to_string_lossy().into_owned());
    args.extend(
        ["-map".to_string(), format!("0:{video_index}"), "-map".into(), "0:a?".into()]
            .into_iter()
            .chain(["-dn", "-sn", "-c", "copy", "-avoid_negative_ts", "make_zero", "-f", "matroska"].map(String::from)),
    );
    args.push(dest.to_string_lossy().into_owned());
    args
}

/// Whether `read` video frames are fewer than the `declared` ones of the
/// source's index, beyond 0.5 % (at least two frames).
pub fn frames_missing(declared: u64, read: u64) -> bool {
    let tolerance = (declared / 200).max(2);
    read.saturating_add(tolerance) < declared
}

/// The frame count the source's index lists for stream `index` (an MP4's
/// sample table); `None` when the container keeps none (a transport stream).
async fn declared_frames(ffprobe: &Path, source: &Path, index: usize) -> Option<u64> {
    let mut args: Vec<String> = ["-v", "error", "-format_whitelist", SOURCE_DEMUXERS, "-protocol_whitelist", "file"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    args.extend(["-select_streams".into(), index.to_string(), "-show_entries".into(), "stream=nb_frames".into()]);
    args.extend(["-of".into(), "default=nw=1:nk=1".into(), source.to_string_lossy().into_owned()]);
    let out = run_capture(ffprobe, args, PROBE_TIMEOUT).await.ok()?;
    if !out.success {
        return None;
    }
    out.stdout.lines().find_map(|l| l.trim().parse::<u64>().ok()).filter(|n| *n > 0)
}

/// The video packets actually read from the remuxed file.
async fn read_video_packets(ffprobe: &Path, remuxed: &Path) -> Result<u64> {
    let args: Vec<String> = [
        "-v",
        "error",
        "-protocol_whitelist",
        "file",
        "-count_packets",
        "-select_streams",
        "v:0",
        "-show_entries",
        "stream=nb_read_packets",
        "-of",
        "default=nw=1:nk=1",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain(std::iter::once(remuxed.to_string_lossy().into_owned()))
    .collect();
    let out = run_capture(ffprobe, args, PROBE_TIMEOUT).await?.ok_or_err("ffprobe")?;
    out.stdout
        .lines()
        .find_map(|l| l.trim().parse::<u64>().ok())
        .ok_or_else(|| anyhow!("ffprobe counted no video packets in the remuxed file"))
}

/// Copy `source` into a clean Matroska container in `dir` ([`REMUX_NAME`]),
/// killed with `cancel`. `Err` when the source cannot be read, ffmpeg fails,
/// the copy holds fewer video frames than the source's index lists, or it is
/// shorter than the source says ([`shortened`]). On an error a partial file
/// may be left in `dir`; the caller removes the workspace.
pub async fn remux(
    ffmpeg: &Path,
    ffprobe: &Path,
    source: &Path,
    dir: &Path,
    cancel: Option<CancellationToken>,
) -> Result<Remuxed> {
    let probed = crate::probe::probe(ffprobe, source)
        .await
        .context("the source cannot be read for a remux")
        .context(ErrorCode::ProbeFailed.as_str())?;
    let declared = declared_frames(ffprobe, source, probed.video.index).await;
    tokio::fs::create_dir_all(dir).await.context(ErrorCode::ProbeFailed.as_str())?;
    let dest = dir.join(REMUX_NAME);
    let _ = tokio::fs::remove_file(&dest).await;

    // A copy reads and writes once: bounded well above any real clip's need.
    let timeout = Duration::from_secs_f64(probed.duration_secs.clamp(0.0, 3600.0).max(300.0));
    let mut opts = RunOpts::new(timeout);
    if let Some(t) = cancel {
        opts = opts.with_cancel(t);
    }
    run(ffmpeg, remux_args(source, probed.video.index, &dest), opts)
        .await
        .and_then(|o| o.ok_or_err("ffmpeg"))
        .context("the remux into a clean container failed")
        .context(ErrorCode::ProbeFailed.as_str())?;
    if !tokio::fs::try_exists(&dest).await.unwrap_or(false) {
        return Err(anyhow!("ffmpeg reported success but wrote no remuxed file").context(ErrorCode::ProbeFailed.as_str()));
    }
    if let Some(declared) = declared {
        let read = read_video_packets(ffprobe, &dest).await.context(ErrorCode::ProbeFailed.as_str())?;
        if frames_missing(declared, read) {
            return Err(anyhow!(
                "the source holds {read} of the {declared} video frames its index lists: part of it is missing, so it is not remuxed"
            )
            .context(ErrorCode::ProbeFailed.as_str()));
        }
    }
    // A container with no frame index (a transport stream) says what is
    // missing only through its clock: a copy shorter than the source claims,
    // beyond the encoder's own truncation tolerance, is refused too.
    let copied = crate::probe::probe(ffprobe, &dest)
        .await
        .context("the remuxed file cannot be read")
        .context(ErrorCode::ProbeFailed.as_str())?;
    if let Some(why) = shortened(probed.duration_secs, copied.duration_secs) {
        return Err(anyhow!("{why}").context(ErrorCode::ProbeFailed.as_str()));
    }
    Ok(Remuxed { path: dest, source: copied })
}

/// What [`remux`] made: the file, and what ffprobe reads in it.
#[derive(Debug, Clone)]
pub struct Remuxed {
    pub path: PathBuf,
    pub source: crate::probe::SourceProbe,
}

/// Why a remux of `source_secs` into `copy_secs` is refused, when the copy is
/// shorter by more than [`crate::transcoder::truncation`]'s tolerance.
pub fn shortened(source_secs: f64, copy_secs: f64) -> Option<String> {
    crate::transcoder::truncation(source_secs, copy_secs).map(|_| {
        format!(
            "the remuxed file is {copy_secs:.2} s but the source says {source_secs:.2} s: pictures may be missing, so it is not used"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_source_that_failed_after_the_download_takes_the_second_route() {
        let coded = |c: ErrorCode| anyhow!("boom").context(c.as_str());
        for code in ErrorCode::ALL {
            let want = CODES.contains(&code);
            // A code the classifier does not read is never in the chain's text as itself.
            if code_in_text(code.as_str()) != Some(code) {
                assert!(!want, "{code:?}");
                continue;
            }
            assert_eq!(applies(&coded(code)), want.then_some(code), "{code:?}");
        }
        // The pipeline's compliance failure is a bare text line, not a context.
        assert_eq!(applies(&anyhow!("COMPLIANCE_FAILED: width 1280 != 1920")), Some(ErrorCode::ComplianceFailed));
        // A full disk during the wrap is the disk, not the source.
        assert_eq!(applies(&anyhow!("bmx: No space").context("LOW_DISK").context("REWRAP_FAILED")), None);
        assert_eq!(applies(&anyhow!("ffmpeg exceeded 600 s").context("TRANSCODE_TIMEOUT")), None);
        assert_eq!(applies(&anyhow!("plain")), None);
    }

    #[test]
    fn the_remux_copies_the_chosen_streams_from_a_local_file_only() {
        let args = remux_args(Path::new("C:/temp/jobs/7/source.mp4"), 2, Path::new("C:/temp/jobs/7/source.remux.mkv"));
        let joined = args.join(" ");
        let i = args.iter().position(|a| a == "-i").unwrap();
        // The whitelists come before the input they guard.
        let before_input = &args[..i];
        assert!(before_input.windows(2).any(|w| w[0] == "-protocol_whitelist" && w[1] == "file"), "{joined}");
        assert!(before_input.windows(2).any(|w| w[0] == "-format_whitelist" && w[1] == SOURCE_DEMUXERS), "{joined}");
        assert!(joined.contains("-map 0:2 -map 0:a?"), "the probed video stream and the audio: {joined}");
        assert!(joined.contains("-c copy"), "never re-encoded: {joined}");
        assert!(!args.iter().any(|a| a.starts_with("-c:v") || a.starts_with("-vf") || a.starts_with("-af") || a == "-r"), "{joined}");
        assert!(joined.ends_with("-f matroska C:/temp/jobs/7/source.remux.mkv"), "{joined}");
    }

    #[test]
    fn a_remux_that_lost_frames_is_refused() {
        assert!(!frames_missing(250, 250));
        assert!(!frames_missing(250, 248), "two frames of slack");
        assert!(frames_missing(250, 247));
        assert!(frames_missing(250, 125), "a download cut in half");
        assert!(!frames_missing(10_000, 9_950), "0.5 % of a long clip");
        assert!(frames_missing(10_000, 9_949));
        assert!(!frames_missing(0, 0));
    }

    /// A copy shorter than the source claims is refused, beyond the encoder's
    /// tolerance (0.5 %, at least 1 s); a longer or equal one is not.
    #[test]
    fn a_remux_shorter_than_the_source_says_is_refused() {
        let why = shortened(203.94, 8.09).expect("8 s of a 204 s source");
        assert!(why.contains("8.09 s") && why.contains("203.94 s"), "{why}");
        assert!(shortened(10.0, 9.1).is_none(), "within a second");
        assert!(shortened(10.0, 8.9).is_some());
        assert!(shortened(4.0, 8.0).is_none(), "a header that said too little loses nothing");
        assert!(shortened(0.0, 8.0).is_none(), "no length claimed");
    }
}
