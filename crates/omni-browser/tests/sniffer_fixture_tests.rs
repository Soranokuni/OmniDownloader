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

const GLOMEX_PLAYER: &str = "https://player.glomex.com/integration/1/iframe-player.html?integrationId=i-fixture00000001&playlistId=";

/// P3.14, gazzetta.gr 2026-10-17: a contextual glomex playlist (a live match)
/// was the primary while the article's own Facebook video was only a sibling,
/// and a site widget's YouTube show was queued next to it.
#[tokio::test]
async fn contextual_glomex_and_sidebar_youtube_do_not_beat_the_articles_own_video() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/article_contextual_glomex.html")).await;
    let fb = "https://www.facebook.com/examplepage/videos/4242424242/";
    let cl = format!("{GLOMEX_PLAYER}cl-fixture0001");
    let side = "https://www.youtube.com/watch?v=sIdEbAr0002";
    let nav = "https://www.youtube.com/watch?v=sTuDiOsHoW1";
    assert_eq!(media.primary_stream, fb, "all: {:?}", media.all_streams);
    assert_eq!(media.meta[fb].in_article, Some(true));
    assert!(media.meta[&cl].contextual, "meta: {:?}", media.meta);
    assert!(!media.meta[fb].contextual);
    assert_eq!(media.meta[side].in_article, Some(false));
    assert_eq!(media.meta[nav].in_article, Some(false));
    // Everything is still found: the planner decides what is offered.
    let pos = |u: &str| media.all_streams.iter().position(|x| x == u);
    assert!(pos(&cl).is_some() && pos(side).is_some(), "all: {:?}", media.all_streams);
    assert!(pos(fb) < pos(&cl) && pos(fb) < pos(side), "all: {:?}", media.all_streams);
    within_budget(took);
}

/// P3.14, the Montero article: the in-article X post is the primary, the
/// related-videos YouTube is known to be outside the article.
#[tokio::test]
async fn an_in_article_x_post_is_primary_and_the_related_youtube_is_outside() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/article_x_and_sidebar_youtube.html")).await;
    let x = "https://x.com/examplepage/status/1900000000000000077".to_string();
    assert_eq!(media.primary_stream, x, "all: {:?}", media.all_streams);
    assert_eq!(media.meta[&x].in_article, Some(true));
    assert_eq!(media.meta["https://www.youtube.com/watch?v=sIdEbAr0003"].in_article, Some(false));
    // A tweet in a sidebar that comes first in the document does not become the primary.
    let side = "https://x.com/examplepage/status/1900000000000000078";
    assert_eq!(media.meta[side].in_article, Some(false), "meta: {:?}", media.meta);
    within_budget(took);
}

/// P3.14: the tweet section is read before YouTube, so a sidebar tweet used
/// to beat the article's own YouTube video.
#[tokio::test]
async fn an_in_article_youtube_beats_a_sidebar_tweet() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/article_youtube_and_sidebar_x.html")).await;
    let yt = "https://www.youtube.com/watch?v=aRtIcLeVid2";
    assert_eq!(media.primary_stream, yt, "all: {:?}", media.all_streams);
    assert_eq!(media.meta["https://x.com/examplepage/status/1900000000000000088"].in_article, Some(false));
    within_budget(took);
}

/// Control: a contextual playlist that is the page's only video is still the
/// primary, as before.
#[tokio::test]
async fn a_lone_contextual_glomex_player_is_still_the_primary() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/glomex_contextual_only.html")).await;
    let cl = format!("{GLOMEX_PLAYER}cl-fixture0002");
    assert_eq!(media.primary_stream, cl, "all: {:?}", media.all_streams);
    assert!(media.meta[&cl].contextual);
    within_budget(took);
}

/// Control (iefimerida / news247 shape): a glomex clip the article names is
/// an ordinary primary, ahead of a sidebar embed.
#[tokio::test]
async fn a_concrete_glomex_clip_in_the_article_is_unchanged() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/glomex_concrete_clip.html")).await;
    let clip = format!("{GLOMEX_PLAYER}v-fixtureclip01");
    assert_eq!(media.primary_stream, clip, "all: {:?}", media.all_streams);
    assert!(!media.meta[&clip].contextual);
    assert_eq!(media.meta[&clip].in_article, Some(true));
    within_budget(took);
}

fn yt(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

/// P3.14 control: no article element at all: nothing is classified, and the
/// lead embed stays ahead of a network pre-roll as before.
#[tokio::test]
async fn a_page_without_an_article_element_is_not_classified() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/lead_video_no_article_element.html")).await;
    assert_eq!(media.primary_stream, yt("lEaDvId0001"), "all: {:?}", media.all_streams);
    assert_eq!(media.meta[&yt("lEaDvId0001")].in_article, None);
    within_budget(took);
}

/// Teaser cards are `<article>` elements elsewhere on the page; the lead
/// player outside them is unknown, not outside.
#[tokio::test]
async fn teaser_cards_elsewhere_do_not_make_the_lead_player_outside() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/lead_video_beside_teaser_cards.html")).await;
    assert_eq!(media.primary_stream, yt("lEaDvId0002"), "all: {:?}", media.all_streams);
    assert_eq!(media.meta[&yt("lEaDvId0002")].in_article, None);
    within_budget(took);
}

/// A lead video in the story container's own header is in the article.
#[tokio::test]
async fn a_lead_video_in_the_articles_own_header_is_in_the_article() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/lead_video_in_article_header.html")).await;
    assert_eq!(media.meta[&yt("hEaDeR00001")].in_article, Some(true), "meta: {:?}", media.meta);
    within_budget(took);
}

/// A layout wrapper called "content-with-sidebar" around the article does not
/// make its embed an outsider.
#[tokio::test]
async fn a_sidebar_named_layout_wrapper_above_the_article_does_not_count() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/article_in_sidebar_layout.html")).await;
    assert_eq!(media.meta[&yt("lAyOuT00001")].in_article, Some(true), "meta: {:?}", media.meta);
    within_budget(took);
}

/// The same video in a sidebar strip (earlier in the page) and in the article
/// body is the article's.
#[tokio::test]
async fn a_video_repeated_in_a_sidebar_and_in_the_article_is_in_the_article() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/article_embed_repeated_in_sidebar.html")).await;
    assert_eq!(media.meta[&yt("rEpEaT00001")].in_article, Some(true), "meta: {:?}", media.meta);
    assert_eq!(media.all_streams.iter().filter(|u| **u == yt("rEpEaT00001")).count(), 1);
    within_budget(took);
}

/// P3.14: only whole marker tokens make an embed an outsider; a state or
/// category label does not, a comment always does, and a header outside any
/// container is unknown.
#[tokio::test]
async fn marker_tokens_comments_and_headers_are_told_apart() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/placement_markers.html")).await;
    let at = |id: &str| media.meta.get(&yt(id)).map(|m| m.in_article);
    assert_eq!(at("cAtEgOrY001"), Some(Some(true)), "category-trending is a label: {:?}", media.meta);
    assert_eq!(at("nOsIdEbAr01"), Some(Some(true)), "no-sidebar is a label: {:?}", media.meta);
    assert_eq!(at("rElAtEd0001"), Some(Some(false)), "related-videos is a widget: {:?}", media.meta);
    assert_eq!(at("cOmMeNt0001"), Some(Some(false)), "a commenter's link: {:?}", media.meta);
    assert_eq!(at("hEaDoUt0001"), Some(None), "a header outside any container is unknown: {:?}", media.meta);
    within_budget(took);
}

/// A bare `<article>` that is a card inside an `<aside>` is not the article;
/// a real `<article>` in `<main>` under a `with-sidebar` wrapper is.
#[tokio::test]
async fn a_card_article_inside_an_aside_is_outside_a_real_article_under_a_layout_wrapper_is_not() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/article_card_in_aside.html")).await;
    assert_eq!(media.meta[&yt("cArDvId0001")].in_article, Some(false), "meta: {:?}", media.meta);
    assert_eq!(media.meta[&yt("rEaLaRt0001")].in_article, Some(true), "meta: {:?}", media.meta);
    assert_eq!(media.primary_stream, yt("rEaLaRt0001"), "all: {:?}", media.all_streams);
    within_budget(took);
}

/// gazzetta.gr waiter page (2026-10-17 re-run): the site's own show is a plain
/// link in the page header, outside any container. It tied the article's
/// Facebook video at 95 and, being found first, became the primary.
#[tokio::test]
async fn a_header_link_does_not_beat_the_articles_embedded_video() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/article_header_link_show.html")).await;
    let fb = "https://www.facebook.com/examplepage/videos/4242424243/";
    let show = yt("sTuDiOsHoW1");
    assert_eq!(media.primary_stream, fb, "all: {:?}", media.all_streams);
    assert!(media.meta[&show].link && media.meta[&show].in_article != Some(true), "meta: {:?}", media.meta);
    assert!(!media.meta[fb].link);
    let pos = |u: &str| media.all_streams.iter().position(|x| x == u);
    assert!(pos(fb) < pos(&show), "all: {:?}", media.all_streams);
    within_budget(took);
}

/// Control: a link inside the article body, with no embed, is the primary.
#[tokio::test]
async fn a_link_in_the_article_body_is_still_the_primary() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/link_in_article_body_only.html")).await;
    assert_eq!(media.primary_stream, yt("bOdYlInK001"), "all: {:?}", media.all_streams);
    assert_eq!(media.meta[&yt("bOdYlInK001")].in_article, Some(true));
    within_budget(took);
}

/// Control: a page whose only candidate is a header link still yields it.
#[tokio::test]
async fn a_lone_header_link_is_still_the_primary() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/link_in_header_only.html")).await;
    assert_eq!(media.primary_stream, yt("hEaDeRlNk01"), "all: {:?}", media.all_streams);
    within_budget(took);
}

/// A header link (unknown placement) comes first on the page; the link in the
/// article body is still found and is the primary.
#[tokio::test]
async fn a_body_link_is_found_after_a_header_link_and_wins() {
    need_browser!();
    let (_, media, took) = sniff(include_str!("fixtures/link_in_header_and_in_body.html")).await;
    let (head, body) = (yt("sHoWhEaDer1"), yt("aRtIcLeLnk1"));
    assert_eq!(media.primary_stream, body, "all: {:?}", media.all_streams);
    assert_eq!(media.meta[&body].in_article, Some(true));
    assert!(media.meta[&head].link && media.meta[&head].in_article != Some(true), "meta: {:?}", media.meta);
    within_budget(took);
}
