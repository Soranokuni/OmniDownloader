//! Page evidence capture (plan P10.1) against local pages in real Chrome/Edge.
//!
//! Skipped with a visible warning when no browser is installed.

use axum::http::header::CONTENT_TYPE;
use axum::response::Html;
use axum::routing::get;
use axum::Router;

use omni_browser::adblock::UnifiedAdBlocker;
use omni_browser::evidence::{classify, EvidenceClass};
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

const PROBE_PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Fixture probe</title></head><body>
<h1>Fixture probe</h1>
<script>
navigator.requestMediaKeySystemAccess('com.widevine.alpha', [{initDataTypes:['cenc'], videoCapabilities:[{contentType:'video/mp4; codecs="avc1.42E01E"'}]}]).catch(() => {});
</script>
</body></html>"#;

const DRM_PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Fixture protected</title></head><body>
<h1>Fixture protected</h1>
<script>
navigator.requestMediaKeySystemAccess('org.w3.clearkey', [{initDataTypes:['cenc','webm'], videoCapabilities:[{contentType:'video/mp4; codecs="avc1.42E01E"'}]}])
  .then(access => access.createMediaKeys())
  .then(keys => { const v = document.createElement('video'); document.body.appendChild(v); return v.setMediaKeys(keys); })
  .catch(() => {});
</script>
</body></html>"#;

const VIDEO_TAG_PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Fixture clip</title></head><body>
<h1>Fixture clip</h1>
<video muted autoplay src="/v/clip.mp4"></video>
</body></html>"#;

const TEXT_PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Fixture text</title></head><body>
<article><h1>Fixture text</h1><p>Only words here.</p></article>
</body></html>"#;

const BLOCKED_PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Just a moment</title></head><body>
<p>Checking your browser.</p></body></html>"#;

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
        .route("/drm", get(|| async { Html(DRM_PAGE) }))
        .route("/probe", get(|| async { Html(PROBE_PAGE) }))
        .route("/clip", get(|| async { Html(VIDEO_TAG_PAGE) }))
        .route("/text", get(|| async { Html(TEXT_PAGE) }))
        .route("/slash/", get(|| async { Html(TEXT_PAGE) }))
        .route(
            "/slash",
            get(|| async { (axum::http::StatusCode::MOVED_PERMANENTLY, [(axum::http::header::LOCATION, "/slash/")]) }),
        )
        .route(
            "/moved",
            get(|| async { (axum::http::StatusCode::FOUND, [(axum::http::header::LOCATION, "/login")]) }),
        )
        .route("/login", get(|| async { Html(TEXT_PAGE) }))
        .route(
            "/blocked",
            get(|| async { (axum::http::StatusCode::FORBIDDEN, Html(BLOCKED_PAGE)) }),
        )
        .route("/v/clip.mp4", get(|| async { ([(CONTENT_TYPE, "video/mp4")], vec![0u8; 4096]) }))
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

#[tokio::test]
async fn a_page_that_sets_media_keys_is_protected() {
    if !browser_installed() {
        eprintln!("WARNING: skipping evidence test -- no Chrome/Edge installed");
        return;
    }
    let adblock = tempfile::tempdir().unwrap();
    UnifiedAdBlocker::init(adblock.path().to_path_buf());
    let addr = serve().await;

    let (snap, diag) = StreamSniffer::diagnose(&format!("http://{addr}/drm"), 40).await;

    assert!(!diag.set_keys.is_empty(), "{diag:?}");
    let e = classify(&snap, &[], Some(&diag), StreamSniffer::is_ad_or_tracking);
    assert_eq!(e.class, EvidenceClass::Protected, "{:?}", e.signals);
    assert!(e.diagnosed);
    assert_eq!(snap.title.as_deref(), Some("Fixture protected"));
}

#[tokio::test]
async fn a_page_with_a_playing_video_tag_is_strong() {
    if !browser_installed() {
        eprintln!("WARNING: skipping evidence test -- no Chrome/Edge installed");
        return;
    }
    let adblock = tempfile::tempdir().unwrap();
    UnifiedAdBlocker::init(adblock.path().to_path_buf());
    let addr = serve().await;

    let (snap, diag) = StreamSniffer::diagnose(&format!("http://{addr}/clip"), 40).await;

    let seen_in_play = diag.played_srcs.iter().any(|s| s.ends_with("/v/clip.mp4"));
    let seen_on_wire = snap.network.iter().any(|n| n.url.ends_with("/v/clip.mp4") && n.status == Some(200));
    assert!(seen_in_play || seen_on_wire, "{diag:?} {:?}", snap.network);
    let e = classify(&snap, &[], Some(&diag), StreamSniffer::is_ad_or_tracking);
    assert_eq!(e.class, EvidenceClass::Strong, "{:?}", e.signals);
}

#[tokio::test]
async fn a_text_only_page_is_none_after_the_diagnostic_run() {
    if !browser_installed() {
        eprintln!("WARNING: skipping evidence test -- no Chrome/Edge installed");
        return;
    }
    let adblock = tempfile::tempdir().unwrap();
    UnifiedAdBlocker::init(adblock.path().to_path_buf());
    let addr = serve().await;

    let (snap, diag) = StreamSniffer::diagnose(&format!("http://{addr}/text"), 40).await;

    assert_eq!(snap.title.as_deref(), Some("Fixture text"), "the page facts are read");
    let e = classify(&snap, &[], Some(&diag), StreamSniffer::is_ad_or_tracking);
    assert_eq!(e.class, EvidenceClass::None, "{:?}", e.signals);
    assert!(e.diagnosed);
}

#[tokio::test]
async fn a_page_that_only_probes_widevine_is_not_protected() {
    if !browser_installed() {
        eprintln!("WARNING: skipping evidence test -- no Chrome/Edge installed");
        return;
    }
    let adblock = tempfile::tempdir().unwrap();
    UnifiedAdBlocker::init(adblock.path().to_path_buf());
    let addr = serve().await;

    let (snap, diag) = StreamSniffer::diagnose(&format!("http://{addr}/probe"), 40).await;

    assert!(diag.key_systems.iter().any(|k| k == "com.widevine.alpha"), "the probe is seen: {diag:?}");
    assert!(diag.set_keys.is_empty() && diag.gen_requests.is_empty() && diag.encrypted.is_empty(), "{diag:?}");
    let e = classify(&snap, &[], Some(&diag), StreamSniffer::is_ad_or_tracking);
    assert_ne!(e.class, EvidenceClass::Protected, "{:?}", e.signals);
    assert_eq!(e.class, EvidenceClass::None, "{:?}", e.signals);
}

#[tokio::test]
async fn a_blocked_page_is_not_a_second_look_at_a_page_without_video() {
    if !browser_installed() {
        eprintln!("WARNING: skipping evidence test -- no Chrome/Edge installed");
        return;
    }
    let adblock = tempfile::tempdir().unwrap();
    UnifiedAdBlocker::init(adblock.path().to_path_buf());
    let addr = serve().await;

    let (snap, diag) = StreamSniffer::diagnose(&format!("http://{addr}/blocked"), 40).await;

    assert!(!diag.complete, "a 403 document is not a page that was looked at");
    let e = classify(&snap, &[], Some(&diag), StreamSniffer::is_ad_or_tracking);
    assert!(!e.diagnosed, "{:?}", e.signals);
}

#[tokio::test]
async fn a_run_cut_short_before_the_facts_are_read_is_incomplete() {
    if !browser_installed() {
        eprintln!("WARNING: skipping evidence test -- no Chrome/Edge installed");
        return;
    }
    let adblock = tempfile::tempdir().unwrap();
    UnifiedAdBlocker::init(adblock.path().to_path_buf());
    let addr = serve().await;

    // the run waits 15 s before reading the page; a 3 s budget cuts it off
    let (snap, diag) = StreamSniffer::diagnose(&format!("http://{addr}/text"), 3).await;

    assert!(!diag.complete);
    let e = classify(&snap, &[], Some(&diag), StreamSniffer::is_ad_or_tracking);
    assert!(!e.diagnosed);
    assert_eq!(e.class, EvidenceClass::None);
}

#[tokio::test]
async fn a_page_that_ends_up_on_a_login_is_incomplete_but_a_trailing_slash_is_not() {
    if !browser_installed() {
        eprintln!("WARNING: skipping evidence test -- no Chrome/Edge installed");
        return;
    }
    let adblock = tempfile::tempdir().unwrap();
    UnifiedAdBlocker::init(adblock.path().to_path_buf());
    let addr = serve().await;

    let (snap, diag) = StreamSniffer::diagnose(&format!("http://{addr}/moved"), 40).await;
    assert!(snap.final_url.as_deref().is_some_and(|u| u.ends_with("/login")), "{:?}", snap.final_url);
    assert!(!diag.complete, "the page asked for is not the page that was read");
    assert!(!classify(&snap, &[], Some(&diag), StreamSniffer::is_ad_or_tracking).diagnosed);

    let (snap, diag) = StreamSniffer::diagnose(&format!("http://{addr}/slash"), 40).await;
    assert!(snap.final_url.as_deref().is_some_and(|u| u.ends_with("/slash/")), "{:?}", snap.final_url);
    assert!(diag.complete, "a redirect to the same page with a slash is the same page");
}
