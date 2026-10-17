//! The stream sniffer's DOM coverage (P3.4): AMP embeds, consent dialogs in
//! frames, the canonical retry and per-element lazy scroll, against local
//! pages with invented content in real Chrome/Edge.
//!
//! Skipped with a visible warning when no browser is installed.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::response::Html;
use axum::routing::get;
use axum::Router;

use omni_browser::adblock::UnifiedAdBlocker;
use omni_browser::sniffer::StreamSniffer;

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

/// Start `app` on a free local port and return that port.
async fn serve(app: Router) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    port
}

fn init_adblock() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    UnifiedAdBlocker::init(dir.path().to_path_buf());
    dir
}

const LONG_ID: &str = "ZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZZ";

#[tokio::test]
async fn amp_embeds_are_found_and_hostile_ids_are_ignored() {
    if !browser_installed() {
        eprintln!("WARNING: skipping AMP sniffer test -- no Chrome/Edge installed");
        return;
    }
    let _adblock = init_adblock();
    let page = format!(
        r#"<!doctype html><html amp><head><meta charset="utf-8"><title>AMP</title></head><body>
<amp-video width="640" height="360" src="/media/amp-clip.mp4"></amp-video>
<amp-youtube data-videoid="AAAAAAAAAAA" width="480" height="270"></amp-youtube>
<amp-youtube data-videoid="x&quot;&gt;&lt;script&gt;alert(1)&lt;/script&gt;"></amp-youtube>
<amp-youtube data-videoid="{LONG_ID}"></amp-youtube>
<amp-vimeo data-videoid="123456789"></amp-vimeo>
<amp-dailymotion data-videoid="x8abcde"></amp-dailymotion>
<amp-twitter data-tweetid="1900000000000000077"></amp-twitter>
<amp-facebook data-href="https://www.facebook.com/NewsDeskTest/videos/1234567890/"></amp-facebook>
<amp-facebook data-href="https://www.facebook.com/NewsDeskTest/posts/5550001"></amp-facebook>
<amp-facebook data-embed-as="video" data-href="https://www.facebook.com/NewsDeskTest/posts/5550002"></amp-facebook>
<amp-iframe src="https://www.youtube.com/embed/CCCCCCCCCCC" width="480" height="270"></amp-iframe>
</body></html>"#
    );
    let app = Router::new().route("/amp", get(move || async move { Html(page.clone()) }));
    let port = serve(app).await;

    let media = StreamSniffer::extract_media_bundle(&format!("http://127.0.0.1:{port}/amp"), 15)
        .await
        .expect("the AMP embeds should be found");
    let all = media.all_streams;
    for want in [
        format!("http://127.0.0.1:{port}/media/amp-clip.mp4"),
        "https://www.youtube.com/watch?v=AAAAAAAAAAA".to_string(),
        "https://www.youtube.com/watch?v=CCCCCCCCCCC".to_string(),
        "https://vimeo.com/123456789".to_string(),
        "https://www.dailymotion.com/video/x8abcde".to_string(),
        "https://x.com/i/status/1900000000000000077".to_string(),
        "https://www.facebook.com/NewsDeskTest/videos/1234567890/".to_string(),
    ] {
        assert!(all.contains(&want), "missing {want}; all streams: {all:?}");
    }
    assert!(
        !all.iter().any(|u| u.contains("script") || u.contains("ZZZZ") || u.contains('"')),
        "a hostile id reached an address: {all:?}"
    );
    assert!(!all.iter().any(|u| u.contains("/posts/5550001")), "a plain Facebook post is not a video: {all:?}");
    assert!(all.iter().any(|u| u.contains("/posts/5550002")), "a post declared as video is: {all:?}");
    let youtube = all.iter().filter(|u| u.contains("youtube.com")).count();
    assert_eq!(youtube, 2, "only the two valid ids: {all:?}");
}

/// Top page whose player only appears once the consent dialog's "accepted"
/// message arrives. `frame` is the dialog markup placed in the page.
fn gated_page(frame: &str) -> String {
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>Gated</title></head><body>
<h1>Δοκιμαστικό άρθρο</h1>
<div id="slot"></div>
{frame}
<script>
function reveal() {{
  if (document.getElementById('shown')) return;
  const f = document.createElement('iframe');
  f.id = 'shown';
  f.src = 'https://www.youtube.com/embed/DDDDDDDDDDD';
  document.getElementById('slot').appendChild(f);
}}
window.addEventListener('message', (e) => {{ if (e.data === 'consent-accepted') reveal(); }});
</script></body></html>"#
    )
}

#[tokio::test]
async fn a_sourcepoint_dialog_in_another_sites_frame_is_accepted() {
    if !browser_installed() {
        eprintln!("WARNING: skipping consent frame test -- no Chrome/Edge installed");
        return;
    }
    let _adblock = init_adblock();
    // The dialog is served from a different site (localhost vs 127.0.0.1), as a CMP's is.
    let dialog = r#"<!doctype html><html><body><p>We value your privacy</p>
<button title="Manage options">Manage</button>
<button title="Accept all" onclick="parent.postMessage('consent-accepted','*')">Accept all</button>
</body></html>"#;
    let cmp = serve(Router::new().route("/dialog", get(move || async move { Html(dialog) }))).await;
    let page = gated_page(&format!(
        r#"<iframe id="sp_message_iframe_900001" src="http://localhost:{cmp}/dialog" width="400" height="200"></iframe>"#
    ));
    let port = serve(Router::new().route("/article", get(move || async move { Html(page.clone()) }))).await;

    let media = StreamSniffer::extract_media_bundle(&format!("http://127.0.0.1:{port}/article"), 15)
        .await
        .expect("the player behind the consent dialog should be found");
    assert!(
        media.all_streams.contains(&"https://www.youtube.com/watch?v=DDDDDDDDDDD".to_string()),
        "all streams: {:?}",
        media.all_streams
    );
}

#[tokio::test]
async fn a_greek_accept_button_in_the_top_page_is_clicked() {
    if !browser_installed() {
        eprintln!("WARNING: skipping Greek consent test -- no Chrome/Edge installed");
        return;
    }
    let _adblock = init_adblock();
    // Mixed case and accents: the label must match with both normalised.
    let page = gated_page(
        r##"<div class="banner"><p>Χρησιμοποιούμε cookies</p><a href="#" onclick="return false">Ρυθμίσεις</a>
<button onclick="window.postMessage('consent-accepted','*')">Αποδοχή όλων</button></div>"##,
    );
    let port = serve(Router::new().route("/article", get(move || async move { Html(page.clone()) }))).await;

    let media = StreamSniffer::extract_media_bundle(&format!("http://127.0.0.1:{port}/article"), 15)
        .await
        .expect("the player behind the consent banner should be found");
    assert!(
        media.all_streams.contains(&"https://www.youtube.com/watch?v=DDDDDDDDDDD".to_string()),
        "all streams: {:?}",
        media.all_streams
    );
}

#[derive(Clone)]
struct Hits {
    own: Arc<AtomicUsize>,
    other: Arc<AtomicUsize>,
    root: Arc<AtomicUsize>,
}

const CANONICAL_VIDEO: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>B</title></head><body>
<iframe src="https://www.youtube.com/embed/EEEEEEEEEEE"></iframe></body></html>"#;

async fn canonical_server() -> (u16, Hits) {
    let hits = Hits { own: Arc::new(AtomicUsize::new(0)), other: Arc::new(AtomicUsize::new(0)), root: Arc::new(AtomicUsize::new(0)) };
    // The "other site": the same machine under another host name.
    let other = serve(
        Router::new()
            .route(
                "/b",
                get(|State(h): State<Hits>| async move {
                    h.other.fetch_add(1, Ordering::SeqCst);
                    Html(CANONICAL_VIDEO)
                }),
            )
            .with_state(hits.clone()),
    )
    .await;
    let cross = format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><link rel="canonical" href="http://localhost:{other}/b"></head><body><p>no video</p></body></html>"#
    );
    let app = Router::new()
        .route(
            "/a",
            get(|| async {
                Html(r#"<!doctype html><html><head><meta charset="utf-8"><link rel="canonical" href="/b?utm_source=x"></head><body><p>no video</p></body></html>"#)
            }),
        )
        .route("/b", get(|| async { Html(CANONICAL_VIDEO) }))
        .route(
            "/",
            get(|State(h): State<Hits>| async move {
                h.root.fetch_add(1, Ordering::SeqCst);
                Html(CANONICAL_VIDEO)
            }),
        )
        .route(
            "/news/story",
            get(|| async {
                Html(r#"<!doctype html><html><head><meta charset="utf-8"><link rel="canonical" href="/"></head><body><p>no video</p></body></html>"#)
            }),
        )
        .route("/c", get(move || async move { Html(cross.clone()) }))
        .route(
            "/d",
            get(|State(h): State<Hits>| async move {
                h.own.fetch_add(1, Ordering::SeqCst);
                Html(r#"<!doctype html><html><head><meta charset="utf-8"><link id="c" rel="canonical" href=""></head><body><p>no video</p>
<script>document.getElementById('c').href = location.pathname + '?utm_source=x';</script></body></html>"#)
            }),
        )
        .with_state(hits.clone());
    let port = serve(app).await;
    (port, hits)
}

#[tokio::test]
async fn a_same_site_canonical_is_followed_when_the_page_shows_nothing() {
    if !browser_installed() {
        eprintln!("WARNING: skipping canonical test -- no Chrome/Edge installed");
        return;
    }
    let _adblock = init_adblock();
    let (port, _) = canonical_server().await;

    let media = StreamSniffer::extract_media_bundle(&format!("http://127.0.0.1:{port}/a"), 25)
        .await
        .expect("the video on the canonical page should be found");
    assert!(
        media.all_streams.contains(&"https://www.youtube.com/watch?v=EEEEEEEEEEE".to_string()),
        "all streams: {:?}",
        media.all_streams
    );
}

#[tokio::test]
async fn a_canonical_on_another_site_is_not_followed() {
    if !browser_installed() {
        eprintln!("WARNING: skipping canonical test -- no Chrome/Edge installed");
        return;
    }
    let _adblock = init_adblock();
    let (port, hits) = canonical_server().await;

    let r = StreamSniffer::extract_media_bundle(&format!("http://127.0.0.1:{port}/c"), 14).await;
    assert!(r.is_err(), "nothing should be found, got {:?}", r.map(|m| m.all_streams));
    assert_eq!(hits.other.load(Ordering::SeqCst), 0, "the other site was visited");
}

#[tokio::test]
async fn a_canonical_equal_to_the_page_causes_no_second_visit() {
    if !browser_installed() {
        eprintln!("WARNING: skipping canonical test -- no Chrome/Edge installed");
        return;
    }
    let _adblock = init_adblock();
    let (port, hits) = canonical_server().await;

    let r = StreamSniffer::extract_media_bundle(&format!("http://127.0.0.1:{port}/d"), 14).await;
    assert!(r.is_err(), "nothing should be found, got {:?}", r.map(|m| m.all_streams));
    assert_eq!(hits.own.load(Ordering::SeqCst), 1, "the page was loaded again");
}

#[tokio::test]
async fn a_player_inside_a_scroller_that_loads_only_when_seen_is_found() {
    if !browser_installed() {
        eprintln!("WARNING: skipping lazy scroll test -- no Chrome/Edge installed");
        return;
    }
    let _adblock = init_adblock();
    // The player is far down a nested scroller: scrolling the window never
    // brings it into view, only scrolling the element itself does.
    let page = r#"<!doctype html><html><head><meta charset="utf-8"><title>Lazy</title></head><body>
<div style="height:200px;overflow:auto"><div style="height:3000px"></div>
<div class="video-player" id="p" style="height:60px">player</div><div style="height:600px"></div></div>
<script>
new IntersectionObserver((entries, obs) => {
  for (const e of entries) if (e.isIntersecting) {
    const f = document.createElement('iframe');
    f.src = 'https://www.youtube.com/embed/FFFFFFFFFFF';
    document.body.appendChild(f);
    obs.disconnect();
  }
}).observe(document.getElementById('p'));
</script></body></html>"#;
    let port = serve(Router::new().route("/lazy", get(move || async move { Html(page) }))).await;

    let media = StreamSniffer::extract_media_bundle(&format!("http://127.0.0.1:{port}/lazy"), 15)
        .await
        .expect("the lazy player should be found");
    assert!(
        media.all_streams.contains(&"https://www.youtube.com/watch?v=FFFFFFFFFFF".to_string()),
        "all streams: {:?}",
        media.all_streams
    );
}

/// A player that asks for its playlist the instant the page loads, before any
/// delay: the sniffer's network listeners must already be in place when the
/// page starts loading. The address is built in script, so it is not in the markup.
#[tokio::test]
async fn a_playlist_requested_during_load_is_seen() {
    if !browser_installed() {
        eprintln!("WARNING: skipping early request test -- no Chrome/Edge installed");
        return;
    }
    let _adblock = init_adblock();
    let page = r#"<!doctype html><html><head><meta charset="utf-8"><title>Early</title></head><body>
<p>Player</p>
<script>new Image().src = '/v/master' + '.m3u8';</script></body></html>"#;
    let app = Router::new()
        .route("/early", get(move || async move { Html(page) }))
        .route("/v/master.m3u8", get(|| async { "#EXTM3U\n" }));
    let port = serve(app).await;

    let media = StreamSniffer::extract_media_bundle(&format!("http://127.0.0.1:{port}/early"), 12)
        .await
        .expect("the playlist requested during load should be seen");
    assert_eq!(media.primary_stream, format!("http://127.0.0.1:{port}/v/master.m3u8"));
}

#[tokio::test]
async fn a_canonical_that_is_the_site_root_is_not_followed() {
    if !browser_installed() {
        eprintln!("WARNING: skipping canonical test -- no Chrome/Edge installed");
        return;
    }
    let _adblock = init_adblock();
    let (port, hits) = canonical_server().await;

    let r = StreamSniffer::extract_media_bundle(&format!("http://127.0.0.1:{port}/news/story"), 14).await;
    assert!(r.is_err(), "nothing should be found, got {:?}", r.map(|m| m.all_streams));
    assert_eq!(hits.root.load(Ordering::SeqCst), 0, "the home page was visited");
}

/// A headline link whose text contains "αποδοχής", a link literally labelled
/// «Αποδοχή» that goes to another page, and a playlist button in a frame whose
/// label contains «συμφωνώ»: none is a consent button, none may be clicked.
#[tokio::test]
async fn ordinary_links_and_buttons_with_consent_words_are_never_clicked() {
    if !browser_installed() {
        eprintln!("WARNING: skipping consent safety test -- no Chrome/Edge installed");
        return;
    }
    let _adblock = init_adblock();
    let clicks = Arc::new(AtomicUsize::new(0));
    let c1 = clicks.clone();
    let c2 = clicks.clone();
    let c3 = clicks.clone();
    let app = Router::new()
        .route(
            "/article",
            get(|| async {
                Html(
                    r#"<!doctype html><html><head><meta charset="utf-8"></head><body>
<a href="/other-article?x=1">Δείτε: ποσοστά αποδοχής της κυβέρνησης στις δημοσκοπήσεις</a>
<a href="/other-nav">Αποδοχή</a>
<a href="/other-agrees">Η αντιπολίτευση συμφωνεί</a>
<iframe src="/playlist" width="300" height="100"></iframe>
</body></html>"#,
                )
            }),
        )
        .route(
            "/playlist",
            get(|| async {
                Html(
                    r#"<!doctype html><html><body>
<button onclick="fetch('/clicked-item')">Επόμενο: συμφωνώ με την πρόταση (4:12)</button>
</body></html>"#,
                )
            }),
        )
        .route("/clicked-item", get(move || async move { c1.fetch_add(1, Ordering::SeqCst); "ok" }))
        .route("/other-article", get(move || async move { c2.fetch_add(1, Ordering::SeqCst); Html("x") }))
        .route("/other-nav", get(move || async move { c3.fetch_add(1, Ordering::SeqCst); Html("x") }));
    let port = serve(app).await;

    let r = StreamSniffer::extract_media_bundle(&format!("http://127.0.0.1:{port}/article"), 8).await;
    assert!(r.is_err(), "nothing should be found, got {:?}", r.map(|m| m.all_streams));
    assert_eq!(clicks.load(Ordering::SeqCst), 0, "something that is not a consent button was clicked");
}

/// The ad-block patterns are in force before the page loads; a pattern that
/// matches the article's own path ("/vast...") must not stop the page loading.
#[tokio::test]
async fn an_article_whose_path_contains_vast_still_loads() {
    if !browser_installed() {
        eprintln!("WARNING: skipping blocked-path test -- no Chrome/Edge installed");
        return;
    }
    let _adblock = init_adblock();
    let app = Router::new().route("/news/vast-majority-of-voters", get(|| async { Html(CANONICAL_VIDEO) }));
    let port = serve(app).await;

    let media = StreamSniffer::extract_media_bundle(&format!("http://127.0.0.1:{port}/news/vast-majority-of-voters"), 12)
        .await
        .expect("the article should load and its video be found");
    assert!(media.all_streams.contains(&"https://www.youtube.com/watch?v=EEEEEEEEEEE".to_string()), "{:?}", media.all_streams);
}

/// A page with a player-like element near the top and a lazy sentinel far
/// below it, in no player-like element: the window itself must be scrolled too.
#[tokio::test]
async fn a_lazy_sentinel_far_down_the_page_is_reached_even_when_a_player_exists() {
    if !browser_installed() {
        eprintln!("WARNING: skipping stepped scroll test -- no Chrome/Edge installed");
        return;
    }
    let _adblock = init_adblock();
    let page = r#"<!doctype html><html><head><meta charset="utf-8"><title>Long</title></head><body>
<div class="player-shell" style="height:40px">player</div>
<div id="s" style="margin-top:2800px;height:60px">more</div><div style="height:1200px"></div>
<script>
new IntersectionObserver((entries, obs) => {
  for (const e of entries) if (e.isIntersecting) {
    const f = document.createElement('iframe');
    f.src = 'https://www.youtube.com/embed/GGGGGGGGGGG';
    document.body.appendChild(f);
    obs.disconnect();
  }
}).observe(document.getElementById('s'));
</script></body></html>"#;
    let port = serve(Router::new().route("/long", get(move || async move { Html(page) }))).await;

    let media = StreamSniffer::extract_media_bundle(&format!("http://127.0.0.1:{port}/long"), 15)
        .await
        .expect("the sentinel's video should be found");
    assert!(media.all_streams.contains(&"https://www.youtube.com/watch?v=GGGGGGGGGGG".to_string()), "{:?}", media.all_streams);
}

/// Labels made only of consent words and ordinary filler are accepted.
#[tokio::test]
async fn consent_labels_with_filler_words_are_clicked() {
    if !browser_installed() {
        eprintln!("WARNING: skipping consent label test -- no Chrome/Edge installed");
        return;
    }
    let _adblock = init_adblock();
    for label in ["Yes, I agree", "Accept and close", "ΑΠΟΔΟΧΗ ΟΛΩΝ ΤΩΝ COOKIES"] {
        let page = gated_page(&format!(
            r#"<div class="banner"><button onclick="window.postMessage('consent-accepted','*')">{label}</button></div>"#
        ));
        let port = serve(Router::new().route("/article", get(move || async move { Html(page.clone()) }))).await;
        let media = StreamSniffer::extract_media_bundle(&format!("http://127.0.0.1:{port}/article"), 12)
            .await
            .unwrap_or_else(|e| panic!("the button «{label}» was not clicked: {e}"));
        assert!(media.all_streams.contains(&"https://www.youtube.com/watch?v=DDDDDDDDDDD".to_string()), "{label}: {:?}", media.all_streams);
    }
}

/// A form's submit button that merely has "accept" in its class, hidden or
/// not, is not a consent button: clicking it would submit the form.
#[tokio::test]
async fn a_generic_accept_class_on_a_form_button_is_not_clicked() {
    if !browser_installed() {
        eprintln!("WARNING: skipping form button test -- no Chrome/Edge installed");
        return;
    }
    let _adblock = init_adblock();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let app = Router::new()
        .route(
            "/article",
            get(|| async {
                Html(
                    r#"<!doctype html><html><head><meta charset="utf-8"></head><body>
<form action="/submitted" method="get">
<button type="submit" class="btn-accept-terms" style="display:none">Send</button>
<button type="submit" class="agree-box">Send</button>
</form></body></html>"#,
                )
            }),
        )
        .route("/submitted", get(move || async move { h.fetch_add(1, Ordering::SeqCst); Html("x") }));
    let port = serve(app).await;

    let r = StreamSniffer::extract_media_bundle(&format!("http://127.0.0.1:{port}/article"), 8).await;
    assert!(r.is_err(), "nothing should be found, got {:?}", r.map(|m| m.all_streams));
    assert_eq!(hits.load(Ordering::SeqCst), 0, "a form was submitted by the consent pass");
}
