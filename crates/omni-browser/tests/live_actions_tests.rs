//! Live recipe actions (P10.11c) against a local page in real Chrome/Edge.
//!
//! Skipped with a visible warning when no browser is installed.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::response::Html;
use axum::routing::get;
use axum::Router;

use omni_browser::adblock::UnifiedAdBlocker;
use omni_browser::recipe::LiveAction;
use omni_browser::sniffer::{classify_live_run, LiveRun, StreamSniffer};

/// A page whose video only loads after a click on #load. The timer reports
/// back when the page's own global `__pwned` was ever set, and that it ran.
const PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Fixture lazy video</title></head><body>
<h1>Fixture lazy video</h1>
<button id="load" onclick="fetch('/v/master.m3u8').catch(() => {})">Load video</button>
<a id="go" href="/other">elsewhere</a>
<script>
setInterval(() => {
  fetch('/tick').catch(() => {});
  if (window.__pwned) fetch('/pwned').catch(() => {});
}, 250);
</script>
</body></html>"#;

const OTHER: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Fixture other</title></head><body>
<h1>Another page</h1></body></html>"#;

/// A page that freezes its own main thread a second after load, so every
/// evaluate sent to it hangs.
const FROZEN: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Fixture frozen</title></head><body>
<button id="x">x</button>
<script>setTimeout(() => { while (true) {} }, 1000);</script>
</body></html>"#;

#[derive(Default)]
struct Hits {
    ticks: AtomicUsize,
    pwned: AtomicUsize,
}

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

/// Start the fixture server; `None` (with a warning printed) when there is no browser.
async fn serve() -> Option<(String, Arc<Hits>)> {
    if !browser_installed() {
        eprintln!("WARNING: skipping live action test -- no Chrome/Edge installed");
        return None;
    }
    let adblock = tempfile::tempdir().unwrap();
    UnifiedAdBlocker::init(adblock.path().to_path_buf());
    std::mem::forget(adblock);

    let hits = Arc::new(Hits::default());
    let app = Router::new()
        .route("/page", get(|| async { Html(PAGE) }))
        .route("/block", get(|| async { Html(FROZEN) }))
        .route("/other", get(|| async { Html(OTHER) }))
        .route("/v/master.m3u8", get(|| async { "#EXTM3U\n" }))
        .route("/tick", get(|State(h): State<Arc<Hits>>| async move { h.ticks.fetch_add(1, Ordering::SeqCst); "" }))
        .route("/pwned", get(|State(h): State<Arc<Hits>>| async move { h.pwned.fetch_add(1, Ordering::SeqCst); "" }))
        .with_state(hits.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Some((format!("http://{addr}/page"), hits))
}

fn has_stream(snap: &omni_browser::PageSnapshot) -> bool {
    snap.network.iter().any(|n| n.url.ends_with("/v/master.m3u8"))
}

#[tokio::test]
async fn a_click_that_starts_the_video_shows_it_in_the_network_and_the_run_is_complete() {
    let Some((url, _)) = serve().await else { return };
    let started = Instant::now();
    let (snap, diag) = StreamSniffer::diagnose_live(&url, 40, &[LiveAction::Click("#load".into())]).await;
    eprintln!("live run with one click took {:?}", started.elapsed());
    assert_eq!(diag.live_actions, vec!["clicked".to_string()]);
    assert!(has_stream(&snap), "network: {:?}", snap.network.iter().map(|n| &n.url).collect::<Vec<_>>());
    assert!(diag.complete);
}

#[tokio::test]
async fn without_the_click_the_video_never_loads() {
    // the control for the test above: it must be the click that starts it
    let Some((url, _)) = serve().await else { return };
    let (snap, diag) = StreamSniffer::diagnose(&url, 40).await;
    assert!(!has_stream(&snap));
    assert!(diag.live_actions.is_empty());
    assert!(diag.complete, "a normal diagnose is unchanged");
}

#[tokio::test]
async fn a_click_on_a_missing_element_is_an_incomplete_run() {
    let Some((url, _)) = serve().await else { return };
    let (snap, diag) = StreamSniffer::diagnose_live(&url, 40, &[LiveAction::Click("#missing".into())]).await;
    assert_eq!(diag.live_actions, vec!["missing".to_string()]);
    assert!(!diag.complete);
    assert_eq!(classify_live_run(&[LiveAction::Click("#missing".into())], &diag), LiveRun::Missing, "a clean page without the target is a conclusive miss");
    assert!(!has_stream(&snap));
}

#[tokio::test]
async fn a_selector_is_data_and_never_becomes_script() {
    let Some((url, hits)) = serve().await else { return };
    let actions = [
        LiveAction::Click("#x'); window.__pwned=1; ('".into()),
        LiveAction::ScrollTo("x\"]; window.__pwned=1; [\"".into()),
    ];
    let (_snap, diag) = StreamSniffer::diagnose_live(&url, 40, &actions).await;
    assert_eq!(diag.live_actions.len(), 2);
    for o in &diag.live_actions {
        assert!(o == "error" || o == "missing", "outcome {o}");
    }
    assert!(!diag.complete);
    // the page's own timer was running, and never saw the global set
    assert!(hits.ticks.load(Ordering::SeqCst) > 0, "the page timer did not run");
    assert_eq!(hits.pwned.load(Ordering::SeqCst), 0, "a selector was executed as script");
}

#[tokio::test]
async fn a_click_that_leaves_the_page_is_an_incomplete_run() {
    let Some((url, _)) = serve().await else { return };
    let (_snap, diag) = StreamSniffer::diagnose_live(&url, 40, &[LiveAction::Click("#go".into())]).await;
    assert_eq!(diag.live_actions, vec!["clicked".to_string()]);
    assert!(!diag.complete, "the page ended up somewhere else");
    assert_eq!(classify_live_run(&[LiveAction::Click("#go".into())], &diag), LiveRun::Inconclusive);
}

#[tokio::test]
async fn two_long_waits_stay_inside_the_cap_whatever_the_budget() {
    let Some((url, _)) = serve().await else { return };
    let started = Instant::now();
    let (_snap, diag) = StreamSniffer::diagnose_live(&url, 600, &[LiveAction::Wait(8000), LiveAction::Wait(8000)]).await;
    let took = started.elapsed();
    eprintln!("live run with two 8 s waits took {took:?}");
    assert_eq!(diag.live_actions, vec!["waited".to_string(), "waited".to_string()]);
    assert!(diag.complete);
    // 40 s cap for the run itself, plus closing the browser
    assert!(took < Duration::from_secs(55), "took {took:?}");
}

#[tokio::test]
async fn a_third_action_is_not_run_and_the_run_is_incomplete() {
    let Some((url, _)) = serve().await else { return };
    let actions = [LiveAction::Wait(1), LiveAction::Wait(1), LiveAction::Wait(1)];
    let (_snap, diag) = StreamSniffer::diagnose_live(&url, 40, &actions).await;
    assert_eq!(diag.live_actions, vec!["waited", "waited", "skipped"]);
    assert!(!diag.complete);
}

#[tokio::test]
async fn a_frozen_page_is_cut_at_the_run_cap_whatever_the_budget() {
    let Some((url, _)) = serve().await else { return };
    let url = url.replace("/page", "/block");
    let started = Instant::now();
    let (_snap, diag) = StreamSniffer::diagnose_live(&url, 600, &[LiveAction::Click("#x".into())]).await;
    let took = started.elapsed();
    eprintln!("live run on a frozen page took {took:?}");
    assert!(!diag.complete, "the page never answered the click");
    assert_eq!(classify_live_run(&[LiveAction::Click("#x".into())], &diag), LiveRun::Inconclusive);
    // the 40 s cap, plus closing a browser whose page is stuck
    // Without the cap the run would last the whole 600 s budget; the margin
    // above 40 s is browser shutdown and machine load.
    assert!(took < Duration::from_secs(120), "took {took:?}");
}

#[tokio::test]
async fn a_wait_longer_than_the_recipe_limit_is_clamped() {
    let Some((url, _)) = serve().await else { return };
    let started = Instant::now();
    let (_snap, diag) = StreamSniffer::diagnose_live(&url, 600, &[LiveAction::Wait(60_000)]).await;
    let took = started.elapsed();
    eprintln!("live run with a 60 s wait took {took:?}");
    assert_eq!(diag.live_actions, vec!["waited".to_string()]);
    // Without the clamp this fails on the ["waited"] outcome above: the 40 s run cap would
    // cut an unclamped wait and leave no outcome. The time bound is only a sanity check.
    assert!(took < Duration::from_secs(60), "the wait was not clamped to 8 s: {took:?}");
}
