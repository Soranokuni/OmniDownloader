//! A small browser-playable preview of a held MXF, and a contact sheet (P10.6).
//!
//! The held on-air file is only ever an INPUT here: MCR watches a 540p H.264
//! made from exactly the file that will air, so what they approve is what
//! airs. Nothing in this module touches the on-air format, the delivery code
//! or the watchfolder. Failure is never an error for the caller: a hold
//! without a preview is still a hold.

use std::path::{Path, PathBuf};
use std::time::Duration;

use omni_core::process::{self, RunOpts};
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// File names inside `temp/held/{id}/`. Lowercase and underscore-led, so
/// they can never collide with a slug (`[A-Z0-9_]`, always `.mxf`).
pub const VIDEO_FILE: &str = "_preview.mp4";
pub const AUDIO_FILE: &str = "_preview.m4a";
pub const CONTACT_FILE: &str = "_contact.jpg";
/// Filled by a later item; served when present.
pub const ARTICLE_FILE: &str = "_article.jpg";

/// Which H.264 encoder this ffmpeg has. `None` means audio-only previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewEncoder {
    X264,
    MediaFoundation,
    None,
}

impl PreviewEncoder {
    pub fn label(self) -> &'static str {
        match self {
            PreviewEncoder::X264 => "libx264",
            PreviewEncoder::MediaFoundation => "h264_mf",
            PreviewEncoder::None => "none (audio-only previews)",
        }
    }
}

/// libx264, else the Windows Media Foundation encoder, else nothing. Reads the
/// text of `ffmpeg -encoders`, whose lines look like ` V....D libx264  ...`.
pub fn pick_encoder(encoders_stdout: &str) -> PreviewEncoder {
    let has = |name: &str| {
        encoders_stdout
            .lines()
            .any(|l| l.split_whitespace().nth(1) == Some(name) && l.trim_start().starts_with('V'))
    };
    if has("libx264") {
        PreviewEncoder::X264
    } else if has("h264_mf") {
        PreviewEncoder::MediaFoundation
    } else {
        PreviewEncoder::None
    }
}

/// Ask `ffmpeg` which encoders it has. `None` when the question itself failed
/// (so the caller does not remember a wrong answer); `Some(PreviewEncoder::None)`
/// when ffmpeg answered and has no H.264 encoder.
pub async fn discover(ffmpeg: &Path) -> Option<PreviewEncoder> {
    let out = process::run(ffmpeg, ["-hide_banner", "-encoders"], RunOpts::new(Duration::from_secs(10)).capture_stdout()).await;
    match out {
        Ok(o) if o.success => Some(pick_encoder(&o.stdout)),
        Ok(o) => {
            warn!("Preview: ffmpeg -encoders failed ({:?}); previews are audio-only until it answers", o.exit_code);
            None
        }
        Err(e) => {
            warn!("Preview: could not run ffmpeg -encoders: {e:#}; previews are audio-only until it answers");
            None
        }
    }
}

const AUDIO_MIX: &str = "[0:a:0][0:a:1]amerge=inputs=2[a]";

/// ffmpeg arguments for the preview: programme L+R (tracks 1 and 2 of the
/// MXF) and, unless the encoder is `None`, a deinterlaced 540p picture.
pub fn preview_args(input: &Path, output: &Path, encoder: PreviewEncoder) -> Vec<String> {
    let mut a: Vec<String> = ["-hide_banner", "-nostdin", "-y", "-threads", "2", "-i"].iter().map(|s| s.to_string()).collect();
    a.push(input.to_string_lossy().into_owned());
    let video: &[&str] = match encoder {
        PreviewEncoder::X264 => &["-c:v", "libx264", "-preset", "veryfast", "-crf", "26", "-g", "50"],
        PreviewEncoder::MediaFoundation => &["-c:v", "h264_mf", "-b:v", "1500k", "-g", "50"],
        PreviewEncoder::None => &[],
    };
    if encoder == PreviewEncoder::None {
        a.extend(["-filter_complex", AUDIO_MIX, "-map", "[a]", "-vn"].map(String::from));
    } else {
        let graph = format!("[0:v:0]yadif=0:-1:0,scale=-2:540,format=yuv420p[v];{AUDIO_MIX}");
        a.extend(["-filter_complex".to_string(), graph]);
        a.extend(["-map", "[v]", "-map", "[a]"].map(String::from));
        a.extend(video.iter().map(|s| s.to_string()));
    }
    a.extend(["-c:a", "aac", "-b:a", "128k", "-ac", "2", "-movflags", "+faststart", "-write_tmcd", "0", "-threads", "2"].map(String::from));
    a.push(output.to_string_lossy().into_owned());
    a
}

/// ffmpeg arguments for one JPEG holding a 4x3 grid of 12 frames spread over
/// the clip.
pub fn contact_args(input: &Path, output: &Path, duration: f64) -> Vec<String> {
    let d = if duration.is_finite() { duration.max(1.0) } else { 1.0 };
    let mut a: Vec<String> = ["-hide_banner", "-nostdin", "-y", "-threads", "2", "-i"].iter().map(|s| s.to_string()).collect();
    a.push(input.to_string_lossy().into_owned());
    a.extend(["-an", "-frames:v", "1", "-vf"].map(String::from));
    a.push(format!("fps=12/{d:.3},scale=320:-2,tile=4x3"));
    // `-update 1`: one image, so a `%` in the path is never read as a sequence pattern.
    a.extend(["-q:v", "4", "-update", "1"].map(String::from));
    a.push(output.to_string_lossy().into_owned());
    a
}

/// What `make` produced; each field is `None` when that step failed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PreviewFiles {
    pub video: Option<PathBuf>,
    pub audio: Option<PathBuf>,
    pub contact: Option<PathBuf>,
    /// Why there is no playable preview (video or audio), when there is none.
    pub why: Option<String>,
}

impl PreviewFiles {
    /// Something MCR can watch or listen to.
    pub fn has_preview(&self) -> bool {
        self.video.is_some() || self.audio.is_some()
    }

    pub fn is_empty(&self) -> bool {
        self.video.is_none() && self.audio.is_none() && self.contact.is_none()
    }
}

/// `name.ext` -> `name.tmp.ext`: still the right extension for ffmpeg.
fn tmp_name(final_name: &str) -> String {
    match final_name.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}.tmp.{ext}"),
        None => format!("{final_name}.tmp"),
    }
}

/// Run one ffmpeg into a `.tmp` name and rename it on success, so a
/// half-written file is never served.
async fn run_to(
    ffmpeg: &Path,
    args_for: impl Fn(&Path) -> Vec<String>,
    dir: &Path,
    final_name: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<PathBuf, String> {
    let tmp = dir.join(tmp_name(final_name));
    let dest = dir.join(final_name);
    let opts = RunOpts::new(timeout).below_normal_priority().with_cancel(cancel.clone());
    let outcome = process::run(ffmpeg, args_for(&tmp), opts).await;
    let why = match outcome {
        Ok(o) if o.success => match tokio::fs::rename(&tmp, &dest).await {
            Ok(()) => return Ok(dest),
            Err(e) => format!("could not rename {tmp:?}: {e}"),
        },
        Ok(o) if o.cancelled => "cancelled".to_string(),
        Ok(o) if o.timed_out => format!("timed out after {:.0}s", o.elapsed.as_secs_f64()),
        Ok(o) => format!("ffmpeg exited with {:?}: {}", o.exit_code, o.stderr_tail.trim()),
        Err(e) => format!("{e:#}"),
    };
    warn!("Preview: {final_name} not made: {why}");
    let _ = tokio::fs::remove_file(&tmp).await;
    Err(why)
}

/// Make the preview and the contact sheet of `held_mxf` in `out_dir`. Never
/// fails: whatever could not be made is `None`.
pub async fn make(
    ffmpeg: &Path,
    held_mxf: &Path,
    out_dir: &Path,
    encoder: PreviewEncoder,
    duration: f64,
    cancel: &CancellationToken,
) -> PreviewFiles {
    let timeout = Duration::from_secs_f64(2.0 * duration.max(0.0) + 60.0);
    let mut files = PreviewFiles::default();

    if encoder != PreviewEncoder::None {
        match run_to(ffmpeg, |tmp| preview_args(held_mxf, tmp, encoder), out_dir, VIDEO_FILE, timeout, cancel).await {
            Ok(p) => files.video = Some(p),
            Err(why) => files.why = Some(why),
        }
    }
    // No picture (no encoder, or the encode failed): sound alone is still a check.
    if files.video.is_none() && !cancel.is_cancelled() {
        match run_to(ffmpeg, |tmp| preview_args(held_mxf, tmp, PreviewEncoder::None), out_dir, AUDIO_FILE, timeout, cancel).await {
            Ok(p) => files.audio = Some(p),
            Err(why) => files.why = Some(why),
        }
    }
    match run_to(ffmpeg, |tmp| contact_args(held_mxf, tmp, duration), out_dir, CONTACT_FILE, timeout, cancel).await {
        Ok(p) => files.contact = Some(p),
        Err(why) => {
            if files.why.is_none() {
                files.why = Some(why);
            }
        }
    }
    if files.has_preview() {
        files.why = None;
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    const WITH_X264: &str = " Encoders:\n V..... = Video\n ------\n V....D libx264              libx264 H.264 (codec h264)\n V....D h264_mf              H264 via MediaFoundation (codec h264)\n A....D aac                  AAC (Advanced Audio Coding)\n";
    const MF_ONLY: &str = " V....D h264_mf              H264 via MediaFoundation (codec h264)\n V....D h264_nvenc           NVIDIA NVENC H.264 encoder\n A....D aac                  AAC\n";
    const NEITHER: &str = " V....D mpeg2video           MPEG-2 video\n V....D h264_nvenc           NVIDIA NVENC H.264 encoder\n A....D aac                  AAC\n";

    #[test]
    fn the_encoder_is_libx264_then_media_foundation_then_none() {
        assert_eq!(pick_encoder(WITH_X264), PreviewEncoder::X264);
        assert_eq!(pick_encoder(MF_ONLY), PreviewEncoder::MediaFoundation);
        assert_eq!(pick_encoder(NEITHER), PreviewEncoder::None);
        assert_eq!(pick_encoder(""), PreviewEncoder::None);
        // An audio line or a decoder-looking mention is not an encoder.
        assert_eq!(pick_encoder(" A....D libx264 not video\n"), PreviewEncoder::None);
    }

    fn joined(a: &[String]) -> String {
        a.join(" ")
    }

    #[test]
    fn the_x264_preview_mixes_programme_l_r_and_is_a_small_progressive_h264() {
        let a = preview_args(Path::new("held.mxf"), Path::new("out.mp4"), PreviewEncoder::X264);
        let j = joined(&a);
        assert!(j.contains("-i held.mxf"));
        assert!(j.contains("[0:a:0][0:a:1]amerge=inputs=2[a]"), "tracks 1 and 2 only: {j}");
        assert!(!j.contains("0:a:2"));
        assert!(j.contains("yadif=0:-1:0,scale=-2:540,format=yuv420p"));
        assert!(j.contains("-c:v libx264 -preset veryfast -crf 26 -g 50"));
        assert!(j.contains("-c:a aac -b:a 128k -ac 2"));
        assert!(j.contains("-movflags +faststart"));
        assert!(j.contains("-threads 2"));
        assert!(j.contains("-write_tmcd 0"), "no timecode track, a browser needs one video and one audio: {j}");
        assert!(j.contains("-nostdin"));
        assert_eq!(a.last().unwrap(), "out.mp4");
        assert!(!j.contains("mpeg2video") && !j.contains("watchfolder"));
    }

    #[test]
    fn the_media_foundation_preview_uses_its_own_codec_and_bitrate() {
        let a = preview_args(Path::new("held.mxf"), Path::new("out.mp4"), PreviewEncoder::MediaFoundation);
        let j = joined(&a);
        assert!(j.contains("-c:v h264_mf -b:v 1500k -g 50"));
        assert!(!j.contains("libx264"));
        assert!(j.contains("[0:a:0][0:a:1]amerge=inputs=2[a]") && j.contains("scale=-2:540"));
        assert!(j.contains("-movflags +faststart"));
    }

    #[test]
    fn without_an_h264_encoder_the_preview_is_audio_only() {
        let a = preview_args(Path::new("held.mxf"), Path::new("out.m4a"), PreviewEncoder::None);
        let j = joined(&a);
        assert!(j.contains("-vn"));
        assert!(!j.contains("[0:v:0]") && !j.contains("-c:v") && !j.contains("-map [v]"));
        assert!(j.contains("[0:a:0][0:a:1]amerge=inputs=2[a]") && j.contains("-map [a]"));
        assert!(j.contains("-c:a aac") && j.contains("-movflags +faststart"));
        assert_eq!(a.last().unwrap(), "out.m4a");
    }

    #[test]
    fn the_contact_sheet_is_one_tiled_jpeg_spread_over_the_clip() {
        let a = contact_args(Path::new("held.mxf"), Path::new("c.jpg"), 60.0);
        let j = joined(&a);
        assert!(j.contains("-frames:v 1"));
        assert!(j.contains("fps=12/60.000,scale=320:-2,tile=4x3"), "{j}");
        assert!(j.contains("-q:v 4") && j.contains("-update 1") && j.contains("-an"));
        assert_eq!(a.last().unwrap(), "c.jpg");
        // A clip shorter than a second, or a bad duration, never divides by zero.
        assert!(joined(&contact_args(Path::new("h"), Path::new("c"), 0.0)).contains("fps=12/1.000"));
        assert!(joined(&contact_args(Path::new("h"), Path::new("c"), f64::NAN)).contains("fps=12/1.000"));
    }

    #[test]
    fn temp_names_keep_the_extension_and_never_match_a_final_name() {
        assert_eq!(tmp_name(VIDEO_FILE), "_preview.tmp.mp4");
        assert_eq!(tmp_name(CONTACT_FILE), "_contact.tmp.jpg");
        for f in [VIDEO_FILE, AUDIO_FILE, CONTACT_FILE, ARTICLE_FILE] {
            assert!(f.starts_with('_') && f == f.to_lowercase(), "{f} could collide with a slug");
        }
    }
}
