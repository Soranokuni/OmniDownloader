//! The 2026-09-25 trial, replayed against the live sites: a newsbomb.gr
//! article embedding three X videos, which delivered one of them four times.
//!
//! Each test pins one decision of the chain the daemon makes for that
//! article: which posts the sniffer finds, which video each post resolves to,
//! which links become jobs, and which pages are sniffed.
//!
//! Network tests are `#[ignore]`: they need the internet, Chrome/Edge and
//! `bin/yt-dlp.exe`, and the sites can change under them. Run with
//!
//! ```text
//! cargo test --test trial_article_x_videos -- --ignored --nocapture
//! ```
//!
//! Reply posts are referenced by status id only: their authors are private
//! people, and the repo is public.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use omni_broadcast::article::plan_article_videos;
use omni_broadcast::errors::classify_download_error;
use omni_browser::adblock::UnifiedAdBlocker;
use omni_browser::sniffer::StreamSniffer;
use omni_core::process::{run, RunOpts};
use omni_core::urlnorm::normalize;

const ARTICLE: &str = "https://www.newsbomb.gr/kosmos/story/1764192/gioxanesmpourgk-metrane-ptomata-9-gynaikes-nekres-fovoi-gia-kata-syrroi-dolofono";

/// The three posts in the article: status id, the id of its video, and the
/// video's duration in seconds (read with yt-dlp on 2026-09-25).
const POSTS: [(&str, &str, f64); 3] = [
    ("2100509173943288138", "2100508610686054400", 163.48), // SABC News
    ("2100295905912213750", "2100280828412473344", 60.877), // IOL
    ("2100498241028591624", "2100498150641283072", 43.318), // SA News Link
];

/// Replies shown under the IOL post. Neither has a video; the trial queued
/// both and delivered IOL's video under their names.
const REPLIES: [&str; 2] = ["2100673890057941139", "2100468218682900898"];

fn post_url(id: &str) -> String {
    format!("https://x.com/i/status/{id}")
}

/// The status id of an X post URL, whatever handle is in front of it.
fn status_id(url: &str) -> Option<String> {
    let (_, rest) = url.split_once("/status/")?;
    let id: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    (!id.is_empty()).then_some(id)
}

fn yt_dlp() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bin").join("yt-dlp.exe")
}

/// yt-dlp's metadata for `url` without downloading: `(video id, duration)`,
/// or its error line.
async fn resolve(url: &str) -> Result<(String, f64), String> {
    let out = run(
        &yt_dlp(),
        ["--no-warnings", "--simulate", "--no-playlist", "--print", "%(id)s %(duration)s", url],
        RunOpts::new(Duration::from_secs(90)).capture_stdout(),
    )
    .await
    .map_err(|e| format!("{e:#}"))?;
    if !out.success {
        return Err(out.stderr_tail.trim().to_string());
    }
    let line = out.stdout.lines().last().unwrap_or("").trim().to_string();
    let (id, dur) = line.split_once(' ').ok_or_else(|| format!("unexpected output: {line:?}"))?;
    let dur: f64 = dur.parse().map_err(|_| format!("unexpected duration: {line:?}"))?;
    Ok((id.to_string(), dur))
}

fn init_adblock() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    UnifiedAdBlocker::init(dir.path().to_path_buf());
    dir
}

/// Offline: both spellings the trial produced for the IOL post are one job.
#[test]
fn both_spellings_of_the_iol_post_are_one_dedup_key() {
    let a = normalize(&post_url(POSTS[1].0));
    let b = normalize(&format!("https://x.com/IOL/status/{}", POSTS[1].0));
    assert_eq!(a, b);
    // …and the three posts stay three.
    let keys: BTreeSet<String> = POSTS.iter().map(|p| normalize(&post_url(p.0))).collect();
    assert_eq!(keys.len(), 3);
}

/// The article: the sniffer finds exactly the three posts, and the plan makes
/// them 1A, 1B, 1C.
#[tokio::test]
#[ignore = "network: newsbomb.gr + Chrome"]
async fn the_article_yields_exactly_its_three_posts_as_1a_1b_1c() {
    let _adblock = init_adblock();
    let media = StreamSniffer::extract_media_bundle(ARTICLE, 25)
        .await
        .expect("the sniffer should find the article's embeds");

    let found: BTreeSet<String> = media.all_streams.iter().filter_map(|u| status_id(u)).collect();
    let expected: BTreeSet<String> = POSTS.iter().map(|p| p.0.to_string()).collect();
    assert_eq!(found, expected, "X posts found: {:?}", media.all_streams);

    let plan = plan_article_videos(ARTICLE, &media.primary_stream, &media.all_streams, "1");
    println!("primary {} as {}", media.primary_stream, plan.primary_index);
    for (u, i) in &plan.siblings {
        println!("sibling {u} as {i}");
    }
    assert_eq!(plan.primary_index, "1A");
    let indexes: Vec<&str> = plan.siblings.iter().map(|(_, i)| i.as_str()).collect();
    assert_eq!(indexes, ["1B", "1C"]);

    // One job per post, none twice.
    let mut jobs: Vec<String> = vec![status_id(&media.primary_stream).expect("primary is an X post")];
    jobs.extend(plan.siblings.iter().filter_map(|(u, _)| status_id(u)));
    let unique: BTreeSet<String> = jobs.iter().cloned().collect();
    assert_eq!(unique, expected, "jobs: {jobs:?}");
    assert_eq!(jobs.len(), 3, "jobs: {jobs:?}");
    // Raw streams on the page are offers, never jobs.
    assert!(plan.offered.iter().all(|o| status_id(&o.url).is_none()), "{:?}", plan.offered);
}

/// Each post resolves to its own video: three different files, not one.
#[tokio::test]
#[ignore = "network: x.com via yt-dlp"]
async fn each_post_resolves_to_its_own_video() {
    let mut videos = BTreeSet::new();
    for (status, video, duration) in POSTS {
        let (id, dur) = resolve(&post_url(status))
            .await
            .unwrap_or_else(|e| panic!("post {status}: yt-dlp failed: {e}"));
        println!("post {status} -> video {id} ({dur} s)");
        assert_eq!(id, video, "post {status} resolved to the wrong video");
        assert!((dur - duration).abs() < 0.5, "post {status}: {dur} s, expected {duration} s");
        videos.insert(id);
    }
    assert_eq!(videos.len(), 3);
}

/// The replies have no video. yt-dlp's answer is final: they are not sniffed
/// (their page plays the IOL video above them).
#[tokio::test]
#[ignore = "network: x.com via yt-dlp"]
async fn replies_without_a_video_are_not_sniffed() {
    for status in REPLIES {
        let url = post_url(status);
        let err = match resolve(&url).await {
            Ok((id, _)) => panic!("reply {status} resolved to video {id}; it had none on 2026-09-25"),
            Err(e) => e,
        };
        println!("reply {status}: {err}");
        assert!(
            err.contains("No video could be found"),
            "reply {status}: expected yt-dlp's no-video answer, got: {err}"
        );
        let code = classify_download_error(&err);
        assert!(!code.should_sniff(&url), "reply {status}: {code} would open the browser");
    }
}

/// The IOL post's own page, sniffed: whatever other posts it shows, none of
/// them becomes a job. This is the page that produced 1B2, 1B3, 1B4.
#[tokio::test]
#[ignore = "network: x.com + Chrome"]
async fn the_iol_post_page_queues_nothing_else() {
    let _adblock = init_adblock();
    let page = post_url(POSTS[1].0);
    let media = match StreamSniffer::extract_media_bundle(&page, 25).await {
        Ok(m) => m,
        // x.com showing a login wall with nothing on it is also safe.
        Err(e) => {
            println!("nothing sniffed on the post page ({e:#}); nothing can be queued");
            return;
        }
    };
    let others: BTreeSet<String> = media
        .all_streams
        .iter()
        .filter_map(|u| status_id(u))
        .filter(|id| id != POSTS[1].0)
        .collect();
    println!("other posts visible on the page: {}", others.len());

    let plan = plan_article_videos(&page, &media.primary_stream, &media.all_streams, "1B");
    assert_eq!(plan.primary_index, "1B");
    assert!(plan.siblings.is_empty(), "would queue: {:?}", plan.siblings);
    assert!(plan.offered.is_empty(), "would offer: {:?}", plan.offered);
}
