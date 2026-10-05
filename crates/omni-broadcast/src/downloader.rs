//! yt-dlp runner (plan P1.8; defects D-17, D-18).
//!
//! Everything here goes through `omni_core::process::run`, so the download has a
//! timeout and dies with its whole process tree — yt-dlp spawns ffmpeg to mux
//! fragments, and an orphaned ffmpeg holds the job's workspace open (defect
//! D-03).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use omni_core::process::{run, RunOpts};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::errors::{classify_download_error, ErrorCode};

pub struct DownloadProgress {
    pub percent: f64,
    pub speed: String,
    pub eta: String,
}

/// A download failure with its classification already applied, so the pipeline
/// does not have to re-read stderr to decide whether a retry could help.
#[derive(Debug)]
pub struct DownloadError {
    pub code: ErrorCode,
    pub message: String,
}

impl std::fmt::Display for DownloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DownloadError {}

/// Options for one download.
pub struct DownloadOpts<'a> {
    /// Sent with the request. Many Greek portals 403 a bare fetch of a stream
    /// URL but serve it happily with the article page as the referer.
    pub referer: Option<&'a str>,
    pub user_agent: Option<&'a str>,
    pub cookie_header: Option<&'a str>,
    /// Netscape cookie jar for this domain (plan P3.5).
    pub cookie_jar: Option<&'a Path>,
    /// Skip TLS verification. Off by default (defect D-17): a station that
    /// ingests whatever a MITM hands it is not a station with working ingest.
    pub insecure_tls: bool,
    pub max_height: u32,
    pub concurrent_fragments: u32,
    pub timeout: Duration,
    pub cancel: Option<CancellationToken>,
}

impl Default for DownloadOpts<'_> {
    fn default() -> Self {
        Self {
            referer: None,
            user_agent: None,
            cookie_header: None,
            cookie_jar: None,
            insecure_tls: false,
            max_height: 1080,
            concurrent_fragments: 4,
            timeout: Duration::from_secs(1800),
            cancel: None,
        }
    }
}

/// Hosts yt-dlp has a native extractor for, which want to be fetched as
/// themselves, never with another site's page context.
const VIDEO_PLATFORMS: &[&str] = &[
    "youtube.com",
    "youtu.be",
    "youtube-nocookie.com",
    "x.com",
    "twitter.com",
    "facebook.com",
    "fb.watch",
    "instagram.com",
    "tiktok.com",
    "vimeo.com",
    "dailymotion.com",
    "dai.ly",
    "streamable.com",
];

/// Whether `url` is a page on a video platform (as opposed to a raw stream
/// or a news portal's article).
pub fn is_video_platform(url: &str) -> bool {
    let host = url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(url)
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .rsplit('@')
        .next()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    VIDEO_PLATFORMS
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

pub struct Downloader {
    ytdl_path: PathBuf,
}

impl Downloader {
    pub fn new<P: AsRef<Path>>(ytdl_path: P) -> Self {
        Self {
            ytdl_path: ytdl_path.as_ref().to_path_buf(),
        }
    }

    /// Build the yt-dlp argument vector.
    ///
    /// Pure, so the format selection and the hardening flags are unit-testable
    /// without a network.
    pub fn build_args(url: &str, output_dir: &Path, opts: &DownloadOpts<'_>) -> Vec<String> {
        let h = opts.max_height;
        let mut args: Vec<String> = vec![
            // Never pull 4K to downscale it to 1080 (defect D-18): on a news
            // deadline that is minutes of wasted download for no visible gain,
            // since the output is 1080i50 either way. Prefer H.264/AAC so the
            // merge is a remux rather than a re-encode.
            "-f".into(),
            format!(
                "bestvideo[height<={h}][vcodec^=avc1]+bestaudio[acodec^=mp4a]/\
                 bestvideo[height<={h}]+bestaudio/best[height<={h}]/best"
            ),
            "--merge-output-format".into(),
            "mp4".into(),
            // A journalist's link often carries a playlist id; ingesting the
            // whole playlist would flood the watchfolder.
            "--no-playlist".into(),
            // --no-playlist does not stop a news page that yt-dlp's generic
            // extractor reads as a playlist of its embeds: an iefimerida.gr
            // article with four Streamable videos downloaded all four into
            // one job and delivered one of them. The page's other videos are
            // queued as their own jobs (`page_videos`).
            "--playlist-items".into(),
            "1".into(),
            "--retries".into(),
            "5".into(),
            "--fragment-retries".into(),
            "10".into(),
            "--retry-sleep".into(),
            "exp=1:30".into(),
            "--socket-timeout".into(),
            "30".into(),
            "--concurrent-fragments".into(),
            opts.concurrent_fragments.to_string(),
            "--newline".into(),
            "--no-warnings".into(),
            "--no-color".into(),
            // Machine-readable progress instead of scraping the human format.
            "--progress-template".into(),
            "download:OMNIPROGRESS %(progress.downloaded_bytes)s %(progress.total_bytes_estimate)s \
             %(progress.speed)s %(progress.eta)s"
                .into(),
            // The definitive output path, instead of guessing by scanning the
            // directory for a name we hope matches (defect D-18).
            "--print".into(),
            "after_move:filepath".into(),
            "-o".into(),
            output_dir
                .join("source.%(ext)s")
                .to_string_lossy()
                .into_owned(),
        ];

        if opts.insecure_tls {
            args.push("--no-check-certificates".into());
        }
        // The article page's session (referer, browser UA, its cookies) is for
        // raw streams on the portal's own CDN. A video platform the sniffer
        // found embedded in the article is fetched by yt-dlp's own extractor,
        // and the platform rejects a foreign referer: X's video CDN answers
        // 403 to an x.com post fetched "from" a news article.
        if !is_video_platform(url) {
            if let Some(r) = opts.referer {
                args.push("--referer".into());
                args.push(r.to_string());
            }
            if let Some(ua) = opts.user_agent {
                args.push("--user-agent".into());
                args.push(ua.to_string());
            }
            if let Some(c) = opts.cookie_header {
                args.push("--add-header".into());
                args.push(format!("Cookie: {c}"));
            }
        }
        if let Some(jar) = opts.cookie_jar {
            args.push("--cookies".into());
            args.push(jar.to_string_lossy().into_owned());
        }

        args.push(url.to_string());
        args
    }

    /// Download `url` into `output_dir`, reporting progress.
    ///
    /// Returns the downloaded file, or a [`DownloadError`] carrying the
    /// classified reason.
    pub async fn download<F>(
        &self,
        job_id: i64,
        url: &str,
        output_dir: &Path,
        opts: DownloadOpts<'_>,
        mut on_progress: F,
    ) -> Result<PathBuf>
    where
        F: FnMut(DownloadProgress) + Send + 'static,
    {
        tokio::fs::create_dir_all(output_dir).await?;
        let args = Self::build_args(url, output_dir, &opts);

        info!("Job #{job_id}: downloading {url}");

        // yt-dlp prints the final path on stdout as well as progress lines, so
        // both are collected from the same stream.
        let printed_paths = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = printed_paths.clone();

        let mut run_opts = RunOpts::new(opts.timeout).on_stdout_line(move |line| {
            if let Some(rest) = line.strip_prefix("OMNIPROGRESS ") {
                if let Some(p) = parse_progress(rest) {
                    on_progress(p);
                }
            } else if !line.trim().is_empty() {
                sink.lock().unwrap().push(line.trim().to_string());
            }
        });
        if let Some(token) = opts.cancel {
            run_opts = run_opts.with_cancel(token);
        }

        let outcome = run(&self.ytdl_path, &args, run_opts)
            .await
            .with_context(|| format!("Could not run yt-dlp at {:?}", self.ytdl_path))?;

        if !outcome.success {
            let code = if outcome.timed_out {
                ErrorCode::DownloadTimeout
            } else if outcome.cancelled {
                ErrorCode::PipelineFailed
            } else {
                classify_download_error(&outcome.stderr_tail)
            };
            return Err(DownloadError {
                code,
                message: first_error_line(&outcome.stderr_tail),
            }
            .into());
        }

        // `--print after_move:filepath` gives the real path. Fall back to a
        // directory scan only if that produced nothing usable, because a wrong
        // guess here feeds the wrong file to the transcoder.
        let candidates = printed_paths.lock().unwrap().clone();
        for line in candidates.iter().rev() {
            let p = PathBuf::from(line);
            if p.is_file() {
                return Ok(p);
            }
        }

        warn!("Job #{job_id}: yt-dlp did not print an output path; scanning {output_dir:?}");
        find_downloaded_file(output_dir)
            .await
            .ok_or_else(|| anyhow!("yt-dlp reported success but produced no file in {output_dir:?}"))
    }
}

/// The videos yt-dlp sees on a page (`--flat-playlist -J`), in page order,
/// when it reads the page as several; empty for a single video or anything
/// else. Only http(s) entry URLs are kept, each once.
pub fn parse_page_videos(json: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    if v.get("_type").and_then(|t| t.as_str()) != Some("playlist") {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    for e in v.get("entries").and_then(|e| e.as_array()).into_iter().flatten() {
        let Some(u) = e.get("url").or_else(|| e.get("webpage_url")).and_then(|u| u.as_str()) else {
            continue;
        };
        if !(u.starts_with("https://") || u.starts_with("http://")) {
            continue;
        }
        let u = canonical_streamable(u).unwrap_or_else(|| u.to_string());
        if !out.contains(&u) {
            out.push(u);
        }
    }
    out
}

/// `https://streamable.com/ID` for the player forms (`/e/ID`, `/o/ID`,
/// `/s/ID`), the form the sniffer reports too, so one video found by both
/// is one job.
pub fn canonical_streamable(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    let path = rest.strip_prefix("streamable.com/")?;
    let mut parts = path.split(['/', '?', '#']).filter(|p| !p.is_empty());
    let first = parts.next()?;
    let id = if matches!(first, "e" | "o" | "s") { parts.next()? } else { first };
    id.chars().all(|c| c.is_ascii_alphanumeric()).then(|| format!("https://streamable.com/{id}"))
}

impl Downloader {
    /// The videos on a news page, when yt-dlp reads it as several (see
    /// [`parse_page_videos`]). A failure is no videos: the download that
    /// follows reports it properly.
    pub async fn page_videos(&self, url: &str, timeout: Duration) -> Vec<String> {
        let args = ["--flat-playlist", "-J", "--no-warnings", "--socket-timeout", "30", url];
        match omni_core::process::run_capture(&self.ytdl_path, args, timeout).await {
            Ok(o) if o.success => parse_page_videos(&o.stdout),
            _ => Vec::new(),
        }
    }
}

/// `downloaded total speed eta`, with `NA` for values yt-dlp does not know yet.
fn parse_progress(rest: &str) -> Option<DownloadProgress> {
    let f: Vec<&str> = rest.split_whitespace().collect();
    if f.len() < 4 {
        return None;
    }
    let downloaded: f64 = f[0].parse().ok()?;
    let total: f64 = f[1].parse().unwrap_or(0.0);
    let percent = if total > 0.0 {
        ((downloaded / total) * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };
    let speed = match f[2].parse::<f64>() {
        Ok(bps) if bps > 0.0 => format!("{:.1} MB/s", bps / 1_048_576.0),
        _ => "--".to_string(),
    };
    let eta = match f[3].parse::<f64>() {
        Ok(s) if s > 0.0 => format!("{:02}:{:02}", (s as u64) / 60, (s as u64) % 60),
        _ => "--:--".to_string(),
    };
    Some(DownloadProgress {
        percent,
        speed,
        eta,
    })
}

/// The largest complete file in the job's workspace.
///
/// Only used when yt-dlp printed no path. `.part` and `.ytdl` are in-progress
/// artefacts; picking one would hand a truncated file to the transcoder.
async fn find_downloaded_file(dir: &Path) -> Option<PathBuf> {
    let mut best: Option<(u64, PathBuf)> = None;
    let mut entries = tokio::fs::read_dir(dir).await.ok()?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if name.ends_with(".part") || name.ends_with(".ytdl") || name.ends_with(".tmp") {
            continue;
        }
        let Ok(meta) = entry.metadata().await else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        if best.as_ref().map_or(true, |(size, _)| meta.len() > *size) {
            best = Some((meta.len(), path));
        }
    }
    best.map(|(_, p)| p)
}

/// The first `ERROR:` line, which is the one worth showing an operator.
fn first_error_line(stderr: &str) -> String {
    stderr
        .lines()
        .find(|l| l.trim_start().to_uppercase().starts_with("ERROR"))
        .or_else(|| stderr.lines().rev().find(|l| !l.trim().is_empty()))
        .unwrap_or("yt-dlp failed with no diagnostic output")
        .trim()
        .chars()
        .take(500)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args_for(opts: &DownloadOpts<'_>) -> Vec<String> {
        Downloader::build_args(
            "https://www.youtube.com/watch?v=abc",
            Path::new("C:/temp/jobs/7"),
            opts,
        )
    }

    #[test]
    fn tls_verification_is_on_unless_explicitly_disabled() {
        // Defect D-17: --no-check-certificates was unconditional.
        let args = args_for(&DownloadOpts::default());
        assert!(
            !args.iter().any(|a| a == "--no-check-certificates"),
            "TLS verification must be on by default: {args:?}"
        );

        let args = args_for(&DownloadOpts {
            insecure_tls: true,
            ..Default::default()
        });
        assert!(args.iter().any(|a| a == "--no-check-certificates"));
    }

    #[test]
    fn the_format_selector_never_pulls_more_than_1080() {
        // Defect D-18: the old selector downloaded 4K and then downscaled it,
        // costing minutes of a news deadline for no visible gain.
        let args = args_for(&DownloadOpts::default());
        let i = args.iter().position(|a| a == "-f").expect("-f present");
        let selector = &args[i + 1];
        assert!(
            selector.contains("height<=1080"),
            "format selector must cap the height: {selector}"
        );
        assert!(
            !selector.contains("2160") && !selector.contains("1440"),
            "format selector must not ask for 4K: {selector}"
        );
        // Every fallback must be capped too, or the last one pulls 4K anyway.
        for alt in selector.split('/') {
            assert!(
                alt.contains("height<=1080") || alt == "best",
                "fallback {alt:?} is not height-capped"
            );
        }
    }

    #[test]
    fn resilience_flags_are_present() {
        let args = args_for(&DownloadOpts::default());
        for flag in [
            "--retries",
            "--fragment-retries",
            "--socket-timeout",
            "--concurrent-fragments",
            "--no-playlist",
        ] {
            assert!(args.iter().any(|a| a == flag), "{flag} missing: {args:?}");
        }
    }

    /// iefimerida.gr, 2026-10-05: four Streamable embeds read as a generic
    /// playlist; one job downloaded all four.
    #[test]
    fn only_the_first_video_of_a_page_is_downloaded_into_a_job() {
        let args = args_for(&DownloadOpts::default());
        let i = args.iter().position(|a| a == "--playlist-items").expect("--playlist-items");
        assert_eq!(args[i + 1], "1");
    }

    #[test]
    fn a_page_read_as_several_videos_lists_them_in_order() {
        let json = r#"{"_type": "playlist", "extractor": "generic", "entries": [
            {"_type": "url", "url": "https://streamable.com/e/531cym", "ie_key": "Streamable"},
            {"_type": "url", "url": "https://streamable.com/e/k98n7n", "ie_key": "Streamable"},
            {"_type": "url", "url": "https://streamable.com/e/531cym", "ie_key": "Streamable"},
            {"_type": "url", "url": "javascript:void(0)"}]}"#;
        assert_eq!(parse_page_videos(json), vec!["https://streamable.com/531cym", "https://streamable.com/k98n7n"]);
        assert!(parse_page_videos(r#"{"_type": "video", "id": "x", "webpage_url": "https://a/b"}"#).is_empty());
        assert!(parse_page_videos("ERROR: Unsupported URL").is_empty());
        assert!(is_video_platform("https://streamable.com/e/531cym"));
    }

    #[test]
    fn the_output_path_is_requested_explicitly() {
        // Defect D-18: the old code scanned the directory for a filename it
        // hoped matched. --print after_move:filepath is authoritative.
        let args = args_for(&DownloadOpts::default());
        let i = args.iter().position(|a| a == "--print").expect("--print");
        assert_eq!(args[i + 1], "after_move:filepath");
    }

    #[test]
    fn session_context_is_forwarded_when_given() {
        // Greek portals routinely 403 a bare fetch of a sniffed stream URL but
        // serve it with the article page as the referer.
        let jar = PathBuf::from("C:/temp/x.com.cookies.txt");
        let args = Downloader::build_args(
            "https://cdn.in.gr/media/2026/09/clip/master.m3u8",
            Path::new("C:/temp/jobs/7"),
            &DownloadOpts {
                referer: Some("https://www.in.gr/article"),
                user_agent: Some("Mozilla/5.0 omni"),
                cookie_header: Some("sid=1"),
                cookie_jar: Some(&jar),
                ..Default::default()
            },
        );
        let joined = args.join(" ");
        assert!(joined.contains("--referer https://www.in.gr/article"), "{joined}");
        assert!(joined.contains("--user-agent Mozilla/5.0 omni"), "{joined}");
        assert!(joined.contains("Cookie: sid=1"), "{joined}");
        assert!(joined.contains("--cookies"), "{joined}");
    }

    /// Regression: a newsbomb.gr article embedding an X post. The sniffer
    /// returned the post URL with the article's context, and X's video CDN
    /// answered 403 to the article referer; a plain fetch works.
    #[test]
    fn a_platform_link_found_in_an_article_is_fetched_without_the_article_context() {
        let jar = PathBuf::from("C:/temp/x.com.cookies.txt");
        let opts = DownloadOpts {
            referer: Some("https://www.newsbomb.gr/kosmos/story/1764192/article"),
            user_agent: Some("Mozilla/5.0 omni"),
            cookie_header: Some("_ga=GA1.2.1"),
            cookie_jar: Some(&jar),
            ..Default::default()
        };
        for url in [
            "https://x.com/i/status/2100509173943288138",
            "https://twitter.com/user/status/1",
            "https://www.youtube.com/watch?v=abc",
            "https://m.facebook.com/watch/?v=1",
            "https://www.instagram.com/reel/abc/",
        ] {
            let joined = Downloader::build_args(url, Path::new("C:/temp/jobs/7"), &opts).join(" ");
            assert!(!joined.contains("--referer"), "{url}: {joined}");
            assert!(!joined.contains("--user-agent"), "{url}: {joined}");
            assert!(!joined.contains("Cookie:"), "{url}: {joined}");
            // The station's own login for the platform (P3.5) still applies.
            assert!(joined.contains("--cookies"), "{url}: {joined}");
        }
    }

    #[test]
    fn platform_hosts_are_matched_exactly_not_by_substring() {
        assert!(is_video_platform("https://x.com/i/status/1"));
        assert!(is_video_platform("https://mobile.twitter.com/a/status/1"));
        assert!(is_video_platform("HTTPS://WWW.YOUTUBE.COM/watch?v=1"));
        assert!(!is_video_platform("https://www.newsbomb.gr/x.com/story"));
        assert!(!is_video_platform("https://notyoutube.com/watch?v=1"));
        assert!(!is_video_platform("https://box.com/v"));
        assert!(!is_video_platform("https://cdn.in.gr/master.m3u8?ref=youtube.com"));
    }

    #[test]
    fn the_url_is_always_the_last_argument() {
        // yt-dlp treats anything after the URL as another URL.
        let args = args_for(&DownloadOpts {
            referer: Some("https://example.gr"),
            insecure_tls: true,
            ..Default::default()
        });
        assert_eq!(args.last().unwrap(), "https://www.youtube.com/watch?v=abc");
    }

    #[test]
    fn progress_lines_parse_into_percent_speed_and_eta() {
        let p = parse_progress("5242880 10485760 2097152 5").expect("parses");
        assert!((p.percent - 50.0).abs() < 0.01, "{}", p.percent);
        assert_eq!(p.speed, "2.0 MB/s");
        assert_eq!(p.eta, "00:05");
    }

    #[test]
    fn unknown_progress_values_do_not_produce_nonsense() {
        // yt-dlp emits NA for the total until it knows the size; showing 0% is
        // honest, showing NaN% or 100% is not.
        let p = parse_progress("5242880 NA NA NA").expect("parses");
        assert_eq!(p.percent, 0.0);
        assert_eq!(p.speed, "--");
        assert_eq!(p.eta, "--:--");
        assert!(parse_progress("garbage").is_none());
    }

    #[tokio::test]
    async fn the_fallback_scan_ignores_partial_downloads() {
        let dir = tempfile::tempdir().unwrap();
        // A finished small file and a large in-progress one: picking the .part
        // would feed a truncated file to the transcoder.
        tokio::fs::write(dir.path().join("source.mp4"), vec![0u8; 1000])
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("source.mkv.part"), vec![0u8; 500_000])
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("source.ytdl"), vec![0u8; 900_000])
            .await
            .unwrap();

        let found = find_downloaded_file(dir.path()).await.expect("a file");
        assert_eq!(found.file_name().unwrap(), "source.mp4");
    }

    #[test]
    fn the_operator_facing_message_is_the_error_line_not_the_last_line() {
        let stderr = "[youtube] Extracting URL\n\
                      ERROR: [youtube] abc: Private video. Sign in if you've been granted access\n\
                      [debug] Exiting with code 1";
        let msg = first_error_line(stderr);
        assert!(msg.contains("Private video"), "{msg}");
        assert!(!msg.contains("Exiting with code"), "{msg}");
    }
}
