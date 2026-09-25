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
