//! Page evidence capture (plan P10.1) against local pages in real Chrome/Edge.
//!
//! Skipped with a visible warning when no browser is installed.

use axum::http::header::CONTENT_TYPE;
use axum::response::Html;
use axum::routing::get;
use axum::Router;

use omni_browser::adblock::UnifiedAdBlocker;
use omni_browser::sniffer::StreamSniffer;
use omni_browser::BrowserError;

const NO_VIDEO_PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8">
<title>Fixture story</title>
<meta property="og:title" content="Fixture headline">
<meta itemprop="duration" content="PT1M30S">
<script type="application/ld+json">{"@type":"NewsArticle","headline":"Fixture headline"}</script>
</head><body>
<article><h1>Fixture story</h1><p>No video here.</p>
<div class="article-player"><span>player placeholder</span></div></article>
<script>setTimeout(() => fetch('/api/data.json').then(r => r.json()).catch(() => {}), 2000);</script>
</body></html>"#;

const HLS_PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Fixture video</title></head><body>
<h1>Fixture video</h1>
<script>setTimeout(() => fetch('/v/master.m3u8').catch(() => {}), 2000);</script>
</body></html>"#;

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

async fn serve() -> std::net::SocketAddr {
    let app = Router::new()
        .route("/story", get(|| async { Html(NO_VIDEO_PAGE) }))
        .route("/video", get(|| async { Html(HLS_PAGE) }))
        .route(
            "/api/data.json",
            get(|| async { ([(CONTENT_TYPE, "application/json")], r#"{"ok":true}"#) }),
        )
        .route(
            "/v/master.m3u8",
            get(|| async { ([(CONTENT_TYPE, "application/vnd.apple.mpegurl")], "#EXTM3U\n") }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

#[tokio::test]
async fn a_page_without_video_keeps_what_the_browser_saw() {
    if !browser_installed() {
        eprintln!("WARNING: skipping evidence test -- no Chrome/Edge installed");
        return;
    }
    let adblock = tempfile::tempdir().unwrap();
    UnifiedAdBlocker::init(adblock.path().to_path_buf());
    let addr = serve().await;

    let sniffed = StreamSniffer::extract_with_evidence(&format!("http://{addr}/story"), 8).await;

    let err = sniffed.media.expect_err("the page has no video");
    assert!(
        matches!(err.downcast_ref::<BrowserError>(), Some(BrowserError::NoStreamFound(_))),
        "{err:#}"
    );
    let snap = sniffed.snapshot;
    let api = snap
        .network
        .iter()
        .find(|n| n.url.ends_with("/api/data.json"))
        .unwrap_or_else(|| panic!("network: {:?}", snap.network));
    assert_eq!(api.status, Some(200));
    assert_eq!(api.mime.as_deref(), Some("application/json"));
    assert_eq!(snap.title.as_deref(), Some("Fixture story"));
    assert!(snap.meta.iter().any(|(k, v)| k == "og:title" && v == "Fixture headline"), "{:?}", snap.meta);
    assert!(snap.meta.iter().any(|(k, v)| k == "duration" && v == "PT1M30S"), "itemprop meta: {:?}", snap.meta);
    assert!(snap.ld_json.iter().any(|j| j.contains("NewsArticle")), "{:?}", snap.ld_json);
    assert!(
        snap.players.iter().any(|p| p.html.contains("article-player") && p.in_article),
        "{:?}",
        snap.players
    );
    assert!(snap.html.as_deref().is_some_and(|h| h.contains("No video here")));
}

#[tokio::test]
async fn a_page_with_a_stream_returns_the_same_streams_and_carries_the_network_log_but_no_page_facts() {
    if !browser_installed() {
        eprintln!("WARNING: skipping evidence test -- no Chrome/Edge installed");
        return;
    }
    let adblock = tempfile::tempdir().unwrap();
    UnifiedAdBlocker::init(adblock.path().to_path_buf());
    let addr = serve().await;
    let url = format!("http://{addr}/video");

    let sniffed = StreamSniffer::extract_with_evidence(&url, 12).await;
    let media = sniffed.media.expect("the page loads a manifest");
    assert_eq!(media.all_streams, vec![format!("http://{addr}/v/master.m3u8")]);
    let m3u8 = sniffed
        .snapshot
        .network
        .iter()
        .find(|n| n.url.ends_with("/v/master.m3u8"))
        .unwrap_or_else(|| panic!("network: {:?}", sniffed.snapshot.network));
    assert_eq!(m3u8.status, Some(200));
    assert_eq!(m3u8.mime.as_deref(), Some("application/vnd.apple.mpegurl"));
    assert!(sniffed.snapshot.html.is_none());
    assert!(sniffed.snapshot.title.is_none());

}
