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

/// A thumbnail whose brightest and darkest pixel differ by less than this is
/// "flat" (black, white, a plain colour): its hash says nothing about the picture.
pub const FLAT_RANGE: u8 = 16;

/// Whether a 9x8 thumbnail has too little contrast to be compared.
pub fn is_flat(px: &[u8; FRAME_BYTES]) -> bool {
    let (lo, hi) = px.iter().fold((u8::MAX, u8::MIN), |(lo, hi), p| (lo.min(*p), hi.max(*p)));
    hi.saturating_sub(lo) < FLAT_RANGE
}

/// One hash per 9x8 chunk of `raw`; `None` for a flat one.
fn chunks_to_hashes(raw: &[u8]) -> Vec<Option<u64>> {
    raw.chunks_exact(FRAME_BYTES)
        .map(|c| {
            let mut px = [0u8; FRAME_BYTES];
            px.copy_from_slice(c);
            (!is_flat(&px)).then(|| dhash_from_gray9x8(&px))
        })
        .collect()
}

/// Pair each chunk's hash with its time; flat frames are dropped here, so
/// `best_match` never sees them.
fn timed_hashes(raw: &[u8], times: &[f64]) -> Vec<(f64, u64)> {
    chunks_to_hashes(raw)
        .into_iter()
        .enumerate()
        .filter_map(|(i, h)| h.map(|h| (times.get(i).copied().unwrap_or(0.0), h)))
        .collect()
}

/// Most key frames hashed per clip.
pub const MAX_KEY_FRAMES: usize = 600;

/// At most `max` items, evenly thinned, order kept.
pub fn thin<T: Clone>(items: &[T], max: usize) -> Vec<T> {
    if items.len() <= max || max == 0 {
        return items.to_vec();
    }
    (0..max).map(|i| items[i * items.len() / max].clone()).collect()
}

/// `n` and `pts_time` of each `showinfo` line in ffmpeg's log.
pub fn parse_showinfo(log: &str) -> Vec<(usize, f64)> {
    log.lines()
        .filter(|l| l.contains("showinfo"))
        .filter_map(|l| {
            let n = l.split("n:").nth(1)?.split_whitespace().next()?.parse::<usize>().ok()?;
            let t = l.split("pts_time:").nth(1)?.split_whitespace().next()?.parse::<f64>().ok()?;
            Some((n, t))
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
    timed_hashes(&raw, &times)
}

/// Hashes of the key frames of `video` (at most [`MAX_KEY_FRAMES`], evenly
/// thinned), each with its real time. The decoder skips every other frame, so
/// a 20-minute preview costs seconds. Fewer than `max(5, duration / 10)` key
/// frames (an encoder with sparse key frames) falls back to [`frame_hashes`]'
/// fixed samples. Flat frames are dropped. Raw files go under `out_dir`.
pub async fn keyframe_hashes(ffmpeg: &Path, video: &Path, duration: f64, out_dir: &Path) -> Vec<(f64, u64)> {
    let want = ((duration.max(0.0) / 10.0) as usize).max(5);
    let raw_path = out_dir.join(raw_name("key_hashes"));
    let mut args: Vec<OsString> =
        ["-hide_banner", "-nostdin", "-v", "info", "-y", "-an", "-sn", "-protocol_whitelist", "file", "-skip_frame", "nokey", "-i"]
            .iter()
            .map(OsString::from)
            .collect();
    args.push(video.as_os_str().to_owned());
    let vf = format!("showinfo,{REDUCE}");
    args.extend(["-vf", vf.as_str(), "-fps_mode", "passthrough", "-pix_fmt", "gray", "-f", "rawvideo"].iter().map(OsString::from));
    args.push(raw_path.as_os_str().to_owned());
    // Every frame logs one line (about 200 bytes): keep enough tail for many.
    let mut opts = RunOpts::new(timeout_for(duration)).below_normal_priority();
    opts.stderr_tail_bytes = 4 << 20;
    let log = match run(ffmpeg, &args, opts).await {
        Ok(o) if o.success => Some(o.stderr_tail),
        Ok(o) => {
            warn!("imagehash: ffmpeg (key frames) failed (exit {:?})", o.exit_code);
            None
        }
        Err(e) => {
            warn!("imagehash: ffmpeg (key frames) could not run: {e:#}");
            None
        }
    };
    let raw = if log.is_some() { std::fs::read(&raw_path).unwrap_or_default() } else { Vec::new() };
    let _ = std::fs::remove_file(&raw_path);
    let found = log.map(|l| keyed(&raw, &parse_showinfo(&l))).unwrap_or_default();
    if found.len() >= want {
        return thin(&found, MAX_KEY_FRAMES);
    }
    frame_hashes(ffmpeg, video, duration, out_dir).await
}

/// Raw chunk `n` belongs to the log line numbered `n`; the log may have lost
/// its first lines, never the raw file's.
fn keyed(raw: &[u8], info: &[(usize, f64)]) -> Vec<(f64, u64)> {
    let hashes = chunks_to_hashes(raw);
    info.iter().filter_map(|(n, t)| hashes.get(*n).copied().flatten().map(|h| (*t, h))).collect()
}

/// Hash of a jpg, png or webp picture, reduced the same way as the frames.
/// `None` when it cannot be read or is flat (see [`is_flat`]).
pub async fn image_hash(ffmpeg: &Path, image: &Path, out_dir: &Path) -> Option<u64> {
    let raw_path = out_dir.join(raw_name("image_hash"));
    let mut args: Vec<OsString> = ["-v", "error", "-y", "-protocol_whitelist", "file", "-i"].iter().map(OsString::from).collect();
    args.push(image.as_os_str().to_owned());
    args.extend(["-vf", REDUCE, "-frames:v", "1", "-pix_fmt", "gray", "-f", "rawvideo"].iter().map(OsString::from));
    args.push(raw_path.as_os_str().to_owned());
    let ok = run_ffmpeg(ffmpeg, &args, timeout_for(0.0)).await;
    let raw = if ok { std::fs::read(&raw_path).unwrap_or_default() } else { Vec::new() };
    let _ = std::fs::remove_file(&raw_path);
    chunks_to_hashes(&raw).first().copied().flatten()
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
        raw[FRAME_BYTES] = 200;
        let h = chunks_to_hashes(&raw);
        assert_eq!(h.len(), 2);
        assert_eq!(h[0], None, "all zero is flat");
        assert_eq!(h[1], Some(1u64 << 63));
    }

    #[test]
    fn a_thumbnail_with_little_contrast_is_flat_and_never_compared() {
        assert!(is_flat(&[0u8; FRAME_BYTES]));
        assert!(is_flat(&[255u8; FRAME_BYTES]));
        let mut px = [100u8; FRAME_BYTES];
        px[3] = 100 + FLAT_RANGE - 1;
        assert!(is_flat(&px), "a spread of 15 is flat");
        px[3] = 100 + FLAT_RANGE;
        assert!(!is_flat(&px), "a spread of 16 is not");
        // Flat frames are dropped before matching, with the times of the others kept.
        let mut raw = vec![0u8; FRAME_BYTES * 3];
        raw[FRAME_BYTES] = 200; // the middle frame has contrast
        assert_eq!(timed_hashes(&raw, &[1.0, 2.0, 3.0]), vec![(2.0, 1u64 << 63)]);
    }

    #[test]
    fn showinfo_lines_give_frame_numbers_and_times_and_other_lines_are_ignored() {
        let log = "Input #0, mov\n[Parsed_showinfo_0 @ 0000] n:   0 pts:      0 pts_time:0       duration: 1\n[Parsed_showinfo_0 @ 0000] n:   1 pts:  50000 pts_time:2.5     duration: 1\nframe=2 fps=0.0\nnoise n: pts_time:9";
        assert_eq!(parse_showinfo(log), vec![(0, 0.0), (1, 2.5)]);
    }

    #[test]
    fn thinning_keeps_order_spreads_evenly_and_never_exceeds_the_cap() {
        let v: Vec<usize> = (0..1000).collect();
        let t = thin(&v, MAX_KEY_FRAMES);
        assert_eq!(t.len(), 600);
        assert!(t.windows(2).all(|w| w[1] > w[0]));
        assert_eq!(t[0], 0);
        assert!(*t.last().unwrap() > 990, "the end of the clip is still covered");
        assert_eq!(thin(&v[..10], 600).len(), 10);
    }

    #[test]
    fn key_frames_are_matched_to_the_log_by_number() {
        let mut raw = vec![0u8; FRAME_BYTES * 3];
        for f in 0..3 {
            raw[f * FRAME_BYTES] = 200;
        }
        // Frame 0 never logged (or flat): only n = 1 and 2 are known.
        let k = keyed(&raw, &[(1, 4.0), (2, 8.0)]);
        assert_eq!(k.iter().map(|x| x.0).collect::<Vec<_>>(), vec![4.0, 8.0]);
    }
}
