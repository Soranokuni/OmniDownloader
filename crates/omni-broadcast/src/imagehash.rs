//! A small perceptual image hash, so the MCR card can say "this frame matches
//! the article's image" (plan P10.9).
//!
//! The hash is the standard difference hash: the picture is reduced to 9x8
//! grey pixels and each of the 8 rows contributes 8 bits, one per pair of
//! neighbours (is the left one brighter). The same reduction is used for the
//! video frames and for the article image, so the two hashes are comparable.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use omni_core::process::{run, RunOpts};
use tracing::warn;

/// Two hashes at most this far apart count as the same picture.
pub const MATCH_DISTANCE: u32 = 10;

const W: usize = 9;
const H: usize = 8;
const FRAME_BYTES: usize = W * H;
const BASE_TIMEOUT_SECS: f64 = 60.0;
const MAX_TIMEOUT_SECS: f64 = 900.0;
/// Distinguishes the raw files of concurrent calls in one directory.
static RAW_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A decode reads the whole clip, so the limit grows with its length.
fn timeout_for(duration: f64) -> Duration {
    let d = if duration.is_finite() { duration.max(0.0) } else { 0.0 };
    Duration::from_secs_f64((BASE_TIMEOUT_SECS + 2.0 * d).min(MAX_TIMEOUT_SECS))
}

fn raw_name(stem: &str) -> String {
    format!("{stem}_{}_{}.raw", std::process::id(), RAW_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}
/// The reduction both sides use. `area` averages, so a re-encode barely moves it.
const REDUCE: &str = "scale=9:8:flags=area,format=gray";

/// Difference hash of a 9x8 grey picture, row by row.
pub fn dhash_from_gray9x8(px: &[u8; FRAME_BYTES]) -> u64 {
    let mut hash = 0u64;
    for row in 0..H {
        for col in 0..W - 1 {
            hash <<= 1;
            if px[row * W + col] > px[row * W + col + 1] {
                hash |= 1;
            }
        }
    }
    hash
}

/// Number of differing bits.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// The frame closest to the article's image: its time and distance (the
/// earlier frame on a tie). `None` when there are no frames.
pub fn best_match(article: u64, frames: &[(f64, u64)]) -> Option<(f64, u32)> {
    frames
        .iter()
        .map(|(t, h)| (*t, hamming(article, *h)))
        .min_by(|a, b| a.1.cmp(&b.1).then(a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal)))
}

/// Whether a distance counts as a match.
pub fn is_match(distance: u32) -> bool {
    distance <= MATCH_DISTANCE
}

/// The times sampled: one per second for the first 10 s, then 10 evenly
/// spread over the clip. Times closer than half a second to an earlier one are
/// dropped (a short clip has fewer samples, never duplicates).
pub fn sample_times(duration: f64) -> Vec<f64> {
    let mut t: Vec<f64> = (0..10).map(|s| s as f64).collect();
    if duration.is_finite() && duration > 0.0 {
        t.retain(|x| *x < duration);
        t.extend((0..10).map(|k| duration * (k as f64 + 0.5) / 10.0));
    }
    t.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut out: Vec<f64> = Vec::new();
    for x in t {
        if out.last().map_or(true, |l| x - l >= 0.5) {
            out.push(x);
        }
    }
    if out.is_empty() {
        out.push(0.0);
    }
    out
}

/// `select` expression taking, for each time, the first frame at or after it.
fn select_expr(times: &[f64]) -> String {
    let terms: Vec<String> = times
        .iter()
        .map(|t| format!("gte(t,{t:.3})*(isnan(prev_selected_t)+lt(prev_selected_t,{t:.3}))"))
        .collect();
    terms.join("+")
}

fn chunks_to_hashes(raw: &[u8]) -> Vec<u64> {
    raw.chunks_exact(FRAME_BYTES)
        .map(|c| {
            let mut px = [0u8; FRAME_BYTES];
            px.copy_from_slice(c);
            dhash_from_gray9x8(&px)
        })
        .collect()
}

async fn run_ffmpeg(ffmpeg: &Path, args: &[OsString], timeout: Duration) -> bool {
    match run(ffmpeg, args, RunOpts::new(timeout).below_normal_priority()).await {
        Ok(o) if o.success => true,
        Ok(o) => {
            warn!("imagehash: ffmpeg failed (exit {:?})", o.exit_code);
            false
        }
        Err(e) => {
            warn!("imagehash: ffmpeg could not run: {e:#}");
            false
        }
    }
}

/// Hashes of about 20 frames of `video`, each with its sampled time (the time
/// asked for; the frame is the first at or after it). One ffmpeg run writes
/// every frame to a single raw file under `out_dir`, which is removed after.
/// A failed run is logged and gives no frames.
pub async fn frame_hashes(ffmpeg: &Path, video: &Path, duration: f64, out_dir: &Path) -> Vec<(f64, u64)> {
    let times = sample_times(duration);
    let raw_path = out_dir.join(raw_name("frame_hashes"));
    let vf = format!("select='{}',{REDUCE}", select_expr(&times));
    let mut args: Vec<OsString> = ["-v", "error", "-y", "-an", "-sn", "-protocol_whitelist", "file", "-i"].iter().map(OsString::from).collect();
    args.push(video.as_os_str().to_owned());
    args.extend(["-vf", vf.as_str(), "-fps_mode", "passthrough", "-pix_fmt", "gray", "-f", "rawvideo"].iter().map(OsString::from));
    args.push(raw_path.as_os_str().to_owned());
    let ok = run_ffmpeg(ffmpeg, &args, timeout_for(duration)).await;
    let raw = if ok { std::fs::read(&raw_path).unwrap_or_default() } else { Vec::new() };
    let _ = std::fs::remove_file(&raw_path);
    chunks_to_hashes(&raw).into_iter().enumerate().map(|(i, h)| (times.get(i).copied().unwrap_or(0.0), h)).collect()
}

/// Hash of a jpg, png or webp picture, reduced the same way as the frames.
pub async fn image_hash(ffmpeg: &Path, image: &Path, out_dir: &Path) -> Option<u64> {
    let raw_path = out_dir.join(raw_name("image_hash"));
    let mut args: Vec<OsString> = ["-v", "error", "-y", "-protocol_whitelist", "file", "-i"].iter().map(OsString::from).collect();
    args.push(image.as_os_str().to_owned());
    args.extend(["-vf", REDUCE, "-frames:v", "1", "-pix_fmt", "gray", "-f", "rawvideo"].iter().map(OsString::from));
    args.push(raw_path.as_os_str().to_owned());
    let ok = run_ffmpeg(ffmpeg, &args, timeout_for(0.0)).await;
    let raw = if ok { std::fs::read(&raw_path).unwrap_or_default() } else { Vec::new() };
    let _ = std::fs::remove_file(&raw_path);
    chunks_to_hashes(&raw).first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dhash_is_the_row_by_row_neighbour_comparison() {
        // Strictly decreasing rows: every pair has left > right.
        let mut px = [0u8; FRAME_BYTES];
        for r in 0..H {
            for c in 0..W {
                px[r * W + c] = (200 - c * 10) as u8;
            }
        }
        assert_eq!(dhash_from_gray9x8(&px), u64::MAX);
        // Strictly increasing: none.
        for r in 0..H {
            for c in 0..W {
                px[r * W + c] = (c * 10) as u8;
            }
        }
        assert_eq!(dhash_from_gray9x8(&px), 0);
        // Equal neighbours are not "brighter".
        assert_eq!(dhash_from_gray9x8(&[7u8; FRAME_BYTES]), 0);
        // Only the first pair of the first row set: the top bit.
        let mut px = [0u8; FRAME_BYTES];
        px[0] = 9;
        assert_eq!(dhash_from_gray9x8(&px), 1u64 << 63);
        // Only the last pair of the last row set: the bottom bit.
        let mut px = [0u8; FRAME_BYTES];
        px[FRAME_BYTES - 2] = 9;
        assert_eq!(dhash_from_gray9x8(&px), 1);
    }

    #[test]
    fn hamming_and_best_match() {
        assert_eq!(hamming(0, u64::MAX), 64);
        assert_eq!(hamming(0b1010, 0b1001), 2);
        assert_eq!(best_match(0, &[]), None);
        let frames = [(1.0, u64::MAX), (4.0, 0b111), (9.0, 0b1)];
        assert_eq!(best_match(0, &frames), Some((9.0, 1)));
        // Equal distance: the earlier frame.
        assert_eq!(best_match(0, &[(5.0, 0b1), (2.0, 0b10)]), Some((2.0, 1)));
        assert!(is_match(10) && !is_match(11));
    }

    #[test]
    fn sample_times_cover_start_and_spread_without_duplicates() {
        let t = sample_times(120.0);
        assert!((19..=20).contains(&t.len()), "{t:?}");
        assert_eq!(t[0], 0.0);
        assert!(t.windows(2).all(|w| w[1] > w[0]));
        assert!(*t.last().unwrap() < 120.0);
        let short = sample_times(3.0);
        assert!(short.len() < 20 && short.iter().all(|x| *x < 3.0));
        assert!(short.windows(2).all(|w| w[1] - w[0] >= 0.5));
        assert_eq!(sample_times(f64::NAN).len(), 10);
        assert_eq!(sample_times(0.2), vec![0.0]);
    }

    #[test]
    fn the_timeout_grows_with_the_clip_and_is_capped_and_raw_names_are_unique() {
        assert_eq!(timeout_for(0.0), Duration::from_secs(60));
        assert_eq!(timeout_for(120.0), Duration::from_secs(300));
        assert_eq!(timeout_for(1e9), Duration::from_secs(900));
        assert_eq!(timeout_for(f64::NAN), Duration::from_secs(60));
        assert_ne!(raw_name("a"), raw_name("a"));
    }

    #[test]
    fn raw_chunks_become_one_hash_each_and_a_partial_tail_is_dropped() {
        let mut raw = vec![0u8; FRAME_BYTES * 2 + 5];
        raw[FRAME_BYTES] = 9;
        let h = chunks_to_hashes(&raw);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0], 0);
        assert_eq!(h[1], 1u64 << 63);
    }
}
