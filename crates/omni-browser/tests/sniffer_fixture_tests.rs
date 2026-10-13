//! P3.7: the page sniffer against invented fixture pages in real Chrome/Edge.
//!
//! Each fixture reproduces the markup *structure* of a newsroom source
//! (JW Player playlist, video.js, article + sidebar YouTube, listicle, AMP,
//! consent wall, ERT WebTV iframe) with invented text. A local axum server
//! serves the page, its playlist and tiny media files; `{{BASE}}` in a
//! fixture becomes the server's `http://127.0.0.1:port`. Embed iframes point
//! at `{BASE}/yt-host/youtube.com/embed/<id>` so the sniffer's own address
//! patterns match while nothing leaves the machine.
//!
//! Skipped with a visible warning when no browser is installed.

use std::time::{Duration, Instant};

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;

use omni_browser::adblock::UnifiedAdBlocker;
use omni_browser::sniffer::{DiscoveredMedia, StreamSniffer};

/// Wall-clock bound for one page, browser launch included. A page with a
/// confident candidate settles in about 3.5 s (the sniffer's own minimum).
const PAGE_BUDGET: Duration = Duration::from_secs(8);

const HLS_MASTER: &str = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-STREAM-INF:BANDWIDTH=800000\nseg0.m3u8\n";

fn browser_installed() -> bool {
    [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    ]
    .iter()
    .any(|p| std::path::Path::new(p).exists())
}

/// Serve `/page` (the fixture), `/jw/playlist.json` and any `*.m3u8` /
/// `*.mp4` under `/media/`; everything else is 404. Returns the base URL.
async fn serve(page: &'static str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let b = base.clone();
    let app = Router::new().fallback(move |uri: Uri| {
        let base = b.clone();
        async move { respond(uri.path(), &base, page) }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

fn respond(path: &str, base: &str, page: &str) -> Response {
    let fill = |s: &str| s.replace("{{BASE}}", base);
    if path == "/page" {
        return ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], fill(page)).into_response();
    }
    if path == "/jw/playlist.json" {
        let json = include_str!("fixtures/jwplayer_playlist.json");
        return ([(header::CONTENT_TYPE, "application/json")], fill(json)).into_response();
    }
    if path.starts_with("/media/") && path.ends_with(".m3u8") {
        return ([(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")], HLS_MASTER).into_response();
    }
    if path.starts_with("/media/") && path.ends_with(".mp4") {
        return ([(header::CONTENT_TYPE, "video/mp4")], vec![0u8; 32]).into_response();
    }
    StatusCode::NOT_FOUND.into_response()
}

/// One browser at a time, so the measured wall time is the sniffer's own and
/// not six Chromes competing for the CPU.
static ONE_BROWSER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Sniff one fixture; returns the result with the wall time taken.
async fn sniff(page: &'static str) -> (String, DiscoveredMedia, Duration) {
    let _turn = ONE_BROWSER.lock().await;
    let adblock = tempfile::tempdir().unwrap();
    UnifiedAdBlocker::init(adblock.path().to_path_buf());
    let base = serve(page).await;
    let started = Instant::now();
    let media = StreamSniffer::extract_media_bundle(&format!("{base}/page"), 8)
        .await
        .unwrap_or_else(|e| panic!("sniffer found nothing: {e:#}"));
    let took = started.elapsed();
    eprintln!("fixture sniff took {took:?}; streams: {:?}", media.all_streams);
    (base, media, took)
}

fn within_budget(took: Duration) {
    assert!(took < PAGE_BUDGET, "sniff took {took:?}, budget {PAGE_BUDGET:?}");
}

macro_rules! need_browser {
    () => {
        if !browser_installed() {
            eprintln!("WARNING: skipping sniffer fixture test -- no Chrome/Edge installed");
            return;
        }
    };
}

/// protothema.gr shape: playlist JSON, then the HLS master it names.
#[tokio::test]
async fn jwplayer_playlist_json_leads_to_the_hls_master() {
    need_browser!();
    let (base, media, took) = sniff(include_str!("fixtures/jwplayer_playlist.html")).await;
    assert_eq!(media.primary_stream, format!("{base}/media/jw/master.m3u8"));
    within_budget(took);
}

/// in.gr shape: a plain `<video><source>` with an HLS address.
#[tokio::test]
async fn videojs_source_element_is_found() {
    need_browser!();
    let (base, media, took) = sniff(include_str!("fixtures/videojs_source.html")).await;
    assert_eq!(media.primary_stream, format!("{base}/media/vjs/index.m3u8"));
    within_budget(took);
}

/// gazzetta.gr shape: the article's own embed comes before an unrelated
/// sidebar embed, and must be the primary.
#[tokio::test]
async fn article_youtube_embed_comes_before_the_sidebar_one() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/youtube_article_and_sidebar.html")).await;
    let yt = |id: &str| format!("https://www.youtube.com/watch?v={id}");
    assert_eq!(media.primary_stream, yt("aRtIcLeVid1"), "all: {:?}", media.all_streams);
    let main = media.all_streams.iter().position(|u| *u == yt("aRtIcLeVid1"));
    let side = media.all_streams.iter().position(|u| *u == yt("sIdEbAr0001"));
    assert!(main.is_some() && main < side, "all: {:?}", media.all_streams);
    within_budget(took);
}

/// news247 shape: three in-article videos, all offered, in document order.
#[tokio::test]
async fn listicle_offers_all_three_videos_in_document_order() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/listicle_three_videos.html")).await;
    let found: Vec<&str> = media
        .all_streams
        .iter()
        .map(String::as_str)
        .filter(|u| u.contains("lIsT000000"))
        .collect();
    assert_eq!(
        found,
        vec![
            "https://www.youtube.com/watch?v=lIsT0000001",
            "https://www.youtube.com/watch?v=lIsT0000002",
            "https://www.youtube.com/watch?v=lIsT0000003",
        ],
        "all: {:?}",
        media.all_streams
    );
    within_budget(took);
}

/// ertnews.gr shape: the WebTV iframe's `f=` path becomes the HLS address.
#[tokio::test]
async fn ert_webtv_iframe_becomes_an_hls_address() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/ert_webtv_iframe.html")).await;
    assert_eq!(
        media.primary_stream,
        "https://mediastream.ert.gr/vodedge/_definst_/mp4:dvrorigin/fixture/2026/clip01.mp4/playlist.m3u8"
    );
    within_budget(took);
}

/// A consent wall that builds the player only after "accept": the sniffer's
/// existing consent handling must click it.
#[tokio::test]
async fn player_behind_a_consent_wall_is_found_after_accept() {
    need_browser!();
    let (base, media, took) = sniff(include_str!("fixtures/consent_overlay.html")).await;
    assert_eq!(media.primary_stream, format!("{base}/media/consent/master.m3u8"));
    within_budget(took);
}

/// iefimerida AMP shape. The sniffer does not read `amp-video` / `amp-youtube`
/// yet.
#[tokio::test]
async fn amp_video_and_amp_youtube_are_found() {
    need_browser!();
    let (base, media, took) = sniff(include_str!("fixtures/amp_article.html")).await;
    assert!(
        media.all_streams.contains(&"https://www.youtube.com/watch?v=aMpYt000001".to_string()),
        "all: {:?}",
        media.all_streams
    );
    assert!(
        media.all_streams.contains(&format!("{base}/media/amp/clip.mp4")),
        "all: {:?}",
        media.all_streams
    );
    within_budget(took);
}

/// A contextual glomex player is resolved by asking glomex's own API, which
/// cannot be answered offline; this fixture documents the markup and runs
/// only where the network is allowed (`cargo test -- --ignored`).
#[tokio::test]
#[ignore = "needs network: a glomex player is resolved through glomex's API"]
async fn glomex_contextual_player_is_offered() {
    need_browser!();
    let (_, media, _) = sniff(include_str!("fixtures/glomex_contextual.html")).await;
    assert!(
        media.all_streams.iter().any(|u| u.contains("glomex.com")),
        "all: {:?}",
        media.all_streams
    );
}
