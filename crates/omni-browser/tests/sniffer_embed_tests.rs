//! The stream sniffer against a local article page in real Chrome/Edge.
//!
//! Skipped with a visible warning when no browser is installed.

use axum::response::Html;
use axum::routing::get;
use axum::Router;

use omni_browser::adblock::UnifiedAdBlocker;
use omni_browser::sniffer::StreamSniffer;

/// Three X embeds the way a Greek portal ships them before widgets.js runs
/// (all ids invented). The same post also appears as a plain link, which
/// must not count twice.
const ARTICLE: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Άρθρο</title></head><body>
<h1>Δοκιμαστικό άρθρο</h1>
<p>Πρώτο βίντεο:</p>
<blockquote class="twitter-tweet"><p>one</p>&mdash; A (@a)
  <a href="https://twitter.com/NewsDeskOne/status/1900000000000000001?ref_src=twsrc%5Etfw">September 24, 2026</a></blockquote>
<p>Δεύτερο βίντεο:</p>
<blockquote class="twitter-tweet"><p>two</p>&mdash; B (@b)
  <a href="https://x.com/NewsDeskTwo/status/1900000000000000002">September 24, 2026</a></blockquote>
<p>Τρίτο βίντεο:</p>
<blockquote class="twitter-tweet"><p>three</p>&mdash; C (@c)
  <a href="https://x.com/NewsDeskThree/status/1900000000000000003">September 24, 2026</a></blockquote>
<p>Δείτε ξανά το <a href="https://x.com/NewsDeskTwo/status/1900000000000000002">δεύτερο</a>.</p>
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

#[tokio::test]
async fn every_x_embed_in_an_article_is_found_once() {
    if !browser_installed() {
        eprintln!("WARNING: skipping sniffer embed test -- no Chrome/Edge installed");
        return;
    }
    let adblock = tempfile::tempdir().unwrap();
    UnifiedAdBlocker::init(adblock.path().to_path_buf());

    let app = Router::new().route("/article", get(|| async { Html(ARTICLE) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let media = StreamSniffer::extract_media_bundle(&format!("http://{addr}/article"), 20)
        .await
        .expect("sniffer should find the embeds");

    let mut posts: Vec<&str> = media
        .all_streams
        .iter()
        .map(String::as_str)
        .filter(|u| u.contains("/status/"))
        .collect();
    posts.sort();
    assert_eq!(
        posts,
        vec![
            "https://x.com/NewsDeskOne/status/1900000000000000001",
            "https://x.com/NewsDeskThree/status/1900000000000000003",
            "https://x.com/NewsDeskTwo/status/1900000000000000002",
        ],
        "all streams: {:?}",
        media.all_streams
    );
}

/// Two workers sniffing at once, as in the 2026-09-25 trial where the second
/// failed "Failed launching Chromium via CDP": both launches shared one
/// profile, and Chrome runs one process per profile.
#[tokio::test]
async fn two_sniffs_at_once_both_run() {
    if !browser_installed() {
        eprintln!("WARNING: skipping concurrent sniffer test -- no Chrome/Edge installed");
        return;
    }
    let adblock = tempfile::tempdir().unwrap();
    UnifiedAdBlocker::init(adblock.path().to_path_buf());

    let app = Router::new().route("/article", get(|| async { Html(ARTICLE) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let url = format!("http://{addr}/article");
    let (a, b) = tokio::join!(
        StreamSniffer::extract_media_bundle(&url, 20),
        StreamSniffer::extract_media_bundle(&url, 20),
    );
    assert!(a.is_ok(), "first sniff: {:#}", a.unwrap_err());
    assert!(b.is_ok(), "second sniff: {:#}", b.unwrap_err());
}

/// iefimerida.gr, 2026-10-05: an Instagram reel behind the portal's own lazy
/// oEmbed proxy, and Streamable players, were never found (ids invented).
/// The official Instagram blockquote is covered too.
const PROXIED_EMBEDS: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Άρθρο</title></head><body>
<h1>Δοκιμαστικό άρθρο</h1>
<iframe class="portal-embed lazyload" data-src="/oembed?url=https%3A%2F%2Fwww.instagram.com%2Freel%2FFixReel01%2F%3Futm_source%3Dig_embed%26amp%3Butm_campaign%3Dloading&amp;provider=default"></iframe>
<figure><iframe class="lazyload" data-src="https://streamable.com/e/fix0001?"></iframe></figure>
<figure><iframe class="lazyload" data-src="https://streamable.com/e/fix0002?"></iframe></figure>
<blockquote class="instagram-media" data-instgrm-permalink="https://www.instagram.com/p/FixPost02/?utm_source=ig_embed"><a href="https://www.instagram.com/p/FixPost02/">post</a></blockquote>
</body></html>"#;

#[tokio::test]
async fn proxied_instagram_and_streamable_embeds_are_found() {
    if !browser_installed() {
        eprintln!("WARNING: skipping sniffer embed test -- no Chrome/Edge installed");
        return;
    }
    let adblock = tempfile::tempdir().unwrap();
    UnifiedAdBlocker::init(adblock.path().to_path_buf());

    let app = Router::new().route("/article", get(|| async { Html(PROXIED_EMBEDS) }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let media = StreamSniffer::extract_media_bundle(&format!("http://{addr}/article"), 20)
        .await
        .expect("sniffer should find the embeds");
    let mut found = media.all_streams.clone();
    found.sort();
    assert_eq!(
        found,
        vec![
            "https://streamable.com/fix0001",
            "https://streamable.com/fix0002",
            "https://www.instagram.com/p/FixPost02/",
            "https://www.instagram.com/reel/FixReel01/",
        ]
    );
}

/// The daily browser check (plan P6.7) passes on a working browser and says
/// which one it drove.
#[tokio::test]
async fn the_browser_self_test_finds_its_test_video() {
    if !browser_installed() {
        eprintln!("WARNING: skipping browser self-test -- no Chrome/Edge installed");
        return;
    }
    let adblock = tempfile::tempdir().unwrap();
    UnifiedAdBlocker::init(adblock.path().to_path_buf());
    let version = StreamSniffer::self_test().await.expect("self-test");
    assert!(version.contains("Chrome/") || version.contains("Edg"), "{version}");
    assert!(!version.contains("Headless"), "{version}");
}
