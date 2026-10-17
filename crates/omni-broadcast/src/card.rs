//! What the approval card shows next to a held repaired video (plan P10.11b):
//! the article's own picture, and the frame of the preview that looks most like
//! it (a dHash, see [`crate::imagehash`]).
//!
//! The picture is a positive signal only: many article pictures are designed
//! graphics, so "no match" is neutral and never a reason to refuse. Everything
//! here is best effort and bounded ([`CARD_CAP`]); it only ever READS the
//! 540p `_preview.mp4`, never the held MXF, so what MCR approves is what airs.
//! The picture's bytes are untrusted: they are checked for an image header,
//! re-encoded by ffmpeg into a small JPEG, and the raw download is deleted.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use omni_core::process::{run, RunOpts};
use tracing::{debug, warn};

use crate::imagehash::{best_match, image_hash, keyframe_hashes};
use crate::preview::{ARTICLE_FILE, FRAME_FILE, VIDEO_FILE};
use crate::repair_gate::GateFacts;

/// The whole card evidence step may take this long; the hold does not wait longer.
pub const CARD_CAP: Duration = Duration::from_secs(45);
/// Pictures tried per hold.
pub const MAX_TRIES: usize = 3;
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(20);
/// The downloaded bytes, before ffmpeg re-encodes them; always deleted.
pub const SOURCE_FILE: &str = "_article.src";
/// Where one run keeps its temporary files, inside the held directory; removed as a whole.
pub const WORK_DIR: &str = "_card_work";
const CLEANUP_PATIENCE: Duration = Duration::from_secs(2);

/// Fetches an article picture's raw bytes under the egress rule (the real one
/// is `omni_browser::HttpFetcher`).
#[async_trait]
pub trait ImageSource: Send + Sync {
    async fn get_image(&self, url: &str, deadline: Instant) -> Result<Vec<u8>>;
}

/// What the attempt row records. All `None`: no article picture was kept.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ImageEvidence {
    /// Distance to the closest preview frame; `None` without a preview video.
    pub distance: Option<u32>,
    /// Time of that frame in the preview.
    pub frame_secs: Option<f64>,
    /// The host the picture came from.
    pub host: Option<String>,
}

/// The same address rule the repair gate applies: http(s), public literal or
/// name, no rooted name, an unusual port only if the page used it, and a host
/// the page used or on the page's own registrable domain. The host, or why not.
/// (The fetcher resolves again and refuses non-public answers itself.)
pub fn page_may_use(raw: &str, facts: &GateFacts) -> Result<String, String> {
    let (url, host) = omni_core::net::parse_public_url(raw)?;
    if url.host_str().is_some_and(|h| h.ends_with('.')) {
        return Err(format!("host {host} ends with a dot"));
    }
    let key = url.host_str().unwrap_or("").trim_end_matches('.').to_ascii_lowercase();
    if let Some(port) = url.port() {
        if port != 80 && port != 443 && !facts.observed_hosts.contains(&format!("{key}:{port}")) {
            return Err(format!("unusual port on {host}"));
        }
    }
    let own_domain = facts.page_domain.is_some()
        && omni_core::urlnorm::registrable_domain(&format!("{}://{}/", url.scheme(), key)) == facts.page_domain;
    if !facts.observed_hosts.contains(&key) && !own_domain {
        return Err(format!("{host} was not used by the page"));
    }
    Ok(host)
}

/// A JPEG, PNG, GIF or WebP header. Anything else (an SVG, a playlist, a page)
/// is never handed to ffmpeg.
pub fn looks_like_image(b: &[u8]) -> bool {
    b.starts_with(&[0xFF, 0xD8, 0xFF])
        || b.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A])
        || b.starts_with(b"GIF87a")
        || b.starts_with(b"GIF89a")
        || (b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP")
}

async fn ffmpeg_ok(ffmpeg: &Path, args: Vec<OsString>, what: &str) -> bool {
    match run(ffmpeg, args, RunOpts::new(FFMPEG_TIMEOUT).below_normal_priority()).await {
        Ok(o) if o.success => true,
        Ok(o) => {
            debug!("card evidence: ffmpeg ({what}) failed (exit {:?})", o.exit_code);
            false
        }
        Err(e) => {
            warn!("card evidence: ffmpeg ({what}) could not run: {e:#}");
            false
        }
    }
}

fn os(parts: &[&str]) -> Vec<OsString> {
    parts.iter().map(OsString::from).collect()
}

/// A picture with more pixels than this is never decoded (a small file can
/// declare an enormous canvas).
const MAX_PIXELS: &str = "40000000";

/// One frame of `input` as a 320-wide JPEG at `out`. `at` seeks first;
/// `picture` adds the decoder's pixel limit (an untrusted picture, not our preview).
async fn small_jpeg(ffmpeg: &Path, input: &Path, out: &Path, at: Option<f64>, picture: bool, what: &str) -> bool {
    let mut a = os(&["-v", "error", "-nostdin", "-y", "-protocol_whitelist", "file"]);
    if picture {
        a.extend(os(&["-max_pixels", MAX_PIXELS]));
    }
    if let Some(t) = at {
        a.extend(os(&["-ss", &format!("{t:.3}")]));
    }
    a.push("-i".into());
    a.push(input.as_os_str().to_owned());
    a.extend(os(&["-frames:v", "1", "-vf", "scale=320:-2", "-q:v", "4", "-update", "1"]));
    a.push(out.as_os_str().to_owned());
    ffmpeg_ok(ffmpeg, a, what).await
}

/// Remove a directory with everything in it, trying again for a couple of
/// seconds: an ffmpeg that was just killed can still hold a file for a moment
/// on Windows. A directory that stays is logged, never an error.
pub async fn remove_dir_retrying(dir: &Path) {
    let end = Instant::now() + CLEANUP_PATIENCE;
    loop {
        match std::fs::remove_dir_all(dir) {
            Ok(()) => return,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Err(e) if Instant::now() >= end => {
                warn!("card evidence: could not remove its work directory: {e}");
                return;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// A fresh, empty work directory for this run's temporary files.
async fn fresh_work_dir(held_dir: &Path) -> Option<PathBuf> {
    let work = held_dir.join(WORK_DIR);
    remove_dir_retrying(&work).await;
    std::fs::create_dir_all(&work).ok().map(|_| work)
}

/// Phase 1b: re-encode the picture into `_article.jpg`. It is written inside
/// `work` and moved into place only when complete, so a stopped run never
/// leaves half a picture. `false` when ffmpeg cannot decode it.
async fn make_article(ffmpeg: &Path, held_dir: &Path, work: &Path, picture: &Path) -> bool {
    let tmp = work.join("article.tmp.jpg");
    if small_jpeg(ffmpeg, picture, &tmp, None, true, "picture").await && std::fs::rename(&tmp, held_dir.join(ARTICLE_FILE)).is_ok() {
        return true;
    }
    let _ = std::fs::remove_file(&tmp);
    false
}

/// Phase 2: compare `_article.jpg` with the key frames of `_preview.mp4` and
/// write the closest as `_frame.jpg`. `(None, None)`: nothing compared (no
/// preview video, a flat picture, no readable frames).
async fn compare(ffmpeg: &Path, held_dir: &Path, work: &Path, duration: f64) -> (Option<u32>, Option<f64>) {
    let frame = held_dir.join(FRAME_FILE);
    let _ = std::fs::remove_file(&frame);
    let preview = held_dir.join(VIDEO_FILE);
    if !preview.is_file() {
        return (None, None);
    }
    let Some(hash) = image_hash(ffmpeg, &held_dir.join(ARTICLE_FILE), work).await else {
        return (None, None);
    };
    let frames = keyframe_hashes(ffmpeg, &preview, duration, work).await;
    let Some((secs, distance)) = best_match(hash, &frames) else {
        return (None, None);
    };
    let tmp = work.join("frame.tmp.jpg");
    if small_jpeg(ffmpeg, &preview, &tmp, Some(secs), false, "frame").await {
        let _ = std::fs::rename(&tmp, &frame);
    }
    // The distance is true even when the frame picture could not be made.
    (Some(distance), Some(secs))
}

/// From a local picture file (the dev-hold path; no time cap): `_article.jpg`,
/// then the comparison. `None` when the picture cannot be decoded.
pub async fn card_from_file(ffmpeg: &Path, held_dir: &Path, picture: &Path, duration: f64) -> Option<(Option<u32>, Option<f64>)> {
    let _ = std::fs::remove_file(held_dir.join(ARTICLE_FILE));
    let work = fresh_work_dir(held_dir).await?;
    let out = if make_article(ffmpeg, held_dir, &work, picture).await {
        Some(compare(ffmpeg, held_dir, &work, duration).await)
    } else {
        None
    };
    remove_dir_retrying(&work).await;
    out
}

/// Phase 1: the first candidate that passes the egress rule, fetches, looks
/// like an image and decodes. The host it came from.
async fn picture_phase(
    ffmpeg: &Path,
    held_dir: &Path,
    work: &Path,
    candidates: &[String],
    facts: &GateFacts,
    source: &dyn ImageSource,
    deadline: Instant,
) -> Option<String> {
    let src_path = work.join(SOURCE_FILE);
    let mut tried = 0;
    for url in candidates {
        if tried >= MAX_TRIES {
            break;
        }
        let host = match page_may_use(url, facts) {
            Ok(h) => h,
            Err(why) => {
                debug!("card evidence: picture skipped: {why}");
                continue;
            }
        };
        tried += 1;
        let bytes = match source.get_image(url, deadline).await {
            Ok(b) => b,
            Err(e) => {
                // The error text names the host only (see HttpFetcher).
                warn!(host = %host, "card evidence: the article picture could not be fetched: {e:#}");
                continue;
            }
        };
        if !looks_like_image(&bytes) {
            warn!(host = %host, "card evidence: the article picture is not a jpeg, png, gif or webp");
            continue;
        }
        if std::fs::write(&src_path, &bytes).is_err() {
            let _ = std::fs::remove_file(&src_path);
            warn!(host = %host, "card evidence: the article picture could not be written");
            continue;
        }
        let ok = make_article(ffmpeg, held_dir, work, &src_path).await;
        let _ = std::fs::remove_file(&src_path);
        if ok {
            return Some(host);
        }
        warn!(host = %host, "card evidence: the article picture could not be decoded");
    }
    None
}

/// The card's evidence for a held video whose files are in `held_dir`:
/// `candidates` are the page's picture addresses in order (at most
/// [`MAX_TRIES`] that pass the egress rule are tried). `duration` is the
/// held clip's length. Two phases share `cap`: the picture, then the
/// comparison. Once the picture is complete it is kept even when the
/// comparison fails or runs out of time (distance `None`); an unfinished
/// picture is never left. Never fails the hold.
pub async fn card_evidence_capped(
    ffmpeg: &Path,
    held_dir: &Path,
    candidates: &[String],
    facts: &GateFacts,
    duration: f64,
    source: &dyn ImageSource,
    cap: Duration,
) -> ImageEvidence {
    let started = Instant::now();
    let deadline = started + cap;
    let left = || deadline.saturating_duration_since(Instant::now());
    // A hold made again must not keep the earlier picture.
    let (article, frame) = (held_dir.join(ARTICLE_FILE), held_dir.join(FRAME_FILE));
    let _ = std::fs::remove_file(&article);
    let _ = std::fs::remove_file(&frame);
    let Some(work) = fresh_work_dir(held_dir).await else { return ImageEvidence::default() };

    let host = match tokio::time::timeout(left(), picture_phase(ffmpeg, held_dir, &work, candidates, facts, source, deadline)).await {
        Ok(Some(host)) => host,
        other => {
            if other.is_err() {
                warn!("card evidence: stopped after {} s while fetching the picture; the hold is unaffected", cap.as_secs());
            }
            // Not recorded, so not shown: a picture that finished at the very end goes too.
            let _ = std::fs::remove_file(&article);
            remove_dir_retrying(&work).await;
            return ImageEvidence::default();
        }
    };
    let (distance, frame_secs) = match tokio::time::timeout(left(), compare(ffmpeg, held_dir, &work, duration)).await {
        Ok(r) => r,
        Err(_) => {
            warn!("card evidence: the comparison ran out of time; the picture is kept without one");
            let _ = std::fs::remove_file(&frame);
            (None, None)
        }
    };
    remove_dir_retrying(&work).await;
    ImageEvidence { distance, frame_secs, host: Some(host) }
}

/// [`card_evidence_capped`] with the real cap ([`CARD_CAP`]).
pub async fn card_evidence(
    ffmpeg: &Path,
    held_dir: &Path,
    candidates: &[String],
    facts: &GateFacts,
    duration: f64,
    source: &dyn ImageSource,
) -> ImageEvidence {
    card_evidence_capped(ffmpeg, held_dir, candidates, facts, duration, source, CARD_CAP).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> GateFacts {
        GateFacts {
            observed_hosts: ["img.cdn.example", "static.other.example", "static.other.example:8443"].iter().map(|s| s.to_string()).collect(),
            page_domain: Some("news.example".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn the_page_may_use_its_own_domain_and_hosts_it_loaded() {
        let f = facts();
        assert_eq!(page_may_use("https://www.news.example/a.jpg", &f).unwrap(), "www.news.example");
        assert_eq!(page_may_use("https://img.cdn.example/a.jpg", &f).unwrap(), "img.cdn.example");
        assert!(page_may_use("https://evil.example/a.jpg", &f).is_err(), "a host the page never used");
        assert!(page_may_use("https://127.0.0.1/a.jpg", &f).is_err());
        assert!(page_may_use("http://localhost/a.jpg", &f).is_err());
        assert!(page_may_use("https://img.cdn.example./a.jpg", &f).is_err(), "rooted name");
        assert!(page_may_use("https://img.cdn.example:8080/a.jpg", &f).is_err(), "port the page did not use");
        assert!(page_may_use("https://static.other.example:8443/a.jpg", &f).is_ok());
        assert!(page_may_use("ftp://news.example/a.jpg", &f).is_err());
    }

    #[test]
    fn only_image_headers_reach_ffmpeg() {
        assert!(looks_like_image(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]));
        assert!(looks_like_image(b"\x89PNG\r\n\x1a\n...."));
        assert!(looks_like_image(b"GIF89a.."));
        assert!(looks_like_image(b"RIFF\x10\0\0\0WEBPVP8 "));
        assert!(!looks_like_image(b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>"));
        assert!(!looks_like_image(b"#EXTM3U\n#EXT-X-VERSION:3\n"));
        assert!(!looks_like_image(b"RIFF\x10\0\0\0WAVEfmt "));
        assert!(!looks_like_image(b""));
    }

    /// A killed ffmpeg can still hold a file for a moment; the directory goes once it lets go.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_work_directory_a_dying_process_still_holds_is_removed_once_it_lets_go() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let work = dir.path().join(WORK_DIR);
        std::fs::create_dir_all(&work).unwrap();
        // Opened without sharing: nothing can delete it while this handle lives.
        let held = std::fs::OpenOptions::new().write(true).create(true).share_mode(0).open(work.join("x.raw")).unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(700));
            drop(held);
        });
        let started = Instant::now();
        remove_dir_retrying(&work).await;
        release.join().unwrap();
        assert!(!work.exists(), "the directory must be gone");
        assert!(started.elapsed() >= Duration::from_millis(600), "it had to wait for the handle");
        // A directory that is not there is fine.
        remove_dir_retrying(&work).await;
    }
}
