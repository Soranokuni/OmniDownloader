//! The recipe `Fetch` step's network side (P10.11a): the address rule, the
//! redirect limit, the body cap. A local server stands in for the web; a made-up
//! public name is pinned to it, because the real rule refuses 127.0.0.1.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use omni_browser::http_fetcher::{check_target, MAX_BODY_BYTES, MAX_IMAGE_BYTES};
use omni_browser::{Fetcher, HttpFetcher};

const FAKE: &str = "player.example.test";

#[derive(Default)]
struct Hits {
    /// Requests that reached `/secret` (must stay 0).
    secret: AtomicUsize,
    /// Cookie headers seen.
    cookies: AtomicUsize,
    last_ua: std::sync::Mutex<String>,
}

async fn handler(State((hits, port)): State<(Arc<Hits>, u16)>, Path(name): Path<String>, headers: HeaderMap) -> Response {
    if headers.contains_key(header::COOKIE) {
        hits.cookies.fetch_add(1, Ordering::SeqCst);
    }
    *hits.last_ua.lock().unwrap() = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let to = |p: &str| (StatusCode::FOUND, [(header::LOCATION, format!("http://{FAKE}:{port}/{p}"))]).into_response();
    match name.as_str() {
        "r0" => to("r1"),
        "loop" => to("loop"),
        "r1" => to("r2"),
        "r2" => to("r3"),
        "r3" => to("ok"),
        "ok" => "{\"file\":\"ok\"}".into_response(),
        "to-private" => (StatusCode::FOUND, [(header::LOCATION, format!("http://127.0.0.1:{port}/secret"))]).into_response(),
        "secret" => {
            hits.secret.fetch_add(1, Ordering::SeqCst);
            "secret".into_response()
        }
        "big" => vec![b'a'; MAX_BODY_BYTES + 1].into_response(),
        "edge" => vec![b'a'; MAX_BODY_BYTES].into_response(),
        "imgbig" => vec![b'a'; MAX_IMAGE_BYTES + 1].into_response(),
        "imgedge" => vec![b'a'; MAX_IMAGE_BYTES].into_response(),
        "slow1" => {
            tokio::time::sleep(Duration::from_secs(6)).await;
            to("hang")
        }
        "hang" => {
            tokio::time::sleep(Duration::from_secs(120)).await;
            "late".into_response()
        }
        "gone" => StatusCode::NOT_FOUND.into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn server() -> (SocketAddr, Arc<Hits>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let hits = Arc::new(Hits::default());
    let app = Router::new().route("/:name", get(handler)).with_state((hits.clone(), addr.port()));
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (addr, hits)
}

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

#[test]
fn the_target_rule_names_the_host_and_refuses_what_is_not_public() {
    let (u, host, port) = check_target("https://cdn.example.gr/p/1.json?x=1").unwrap();
    assert_eq!((host.as_str(), port, u.path()), ("cdn.example.gr", 443, "/p/1.json"));
    assert_eq!(check_target("http://cdn.example.gr:8080/a").unwrap().2, 8080);
    for bad in [
        "ftp://cdn.example.gr/a",
        "file:///c:/windows/win.ini",
        "https://user:pw@cdn.example.gr/a",
        "http://localhost/a",
        "http://x.localhost/a",
        "http://127.0.0.1/a",
        "http://10.1.2.3/a",
        "http://192.168.0.9:8080/a",
        "http://169.254.169.254/latest/meta-data",
        "http://[::1]/a",
        "http://[fd00::1]/a",
        "https://cdn.page.gr./x",
        "http://cdn.page.gr.:8080/x",
        "not a url",
    ] {
        let e = check_target(bad).expect_err(bad);
        assert!(!e.contains("pw"), "the refusal must not repeat a credential: {e}");
    }
}

#[tokio::test]
async fn a_private_address_is_refused_before_anything_connects() {
    let (addr, hits) = server().await;
    let f = HttpFetcher::new();
    for target in [format!("http://127.0.0.1:{}/secret", addr.port()), format!("http://localhost:{}/secret", addr.port())] {
        assert!(f.get(&target, soon()).await.is_err(), "{target}");
    }
    assert_eq!(hits.secret.load(Ordering::SeqCst), 0, "the server must never have been asked");
}

#[tokio::test]
async fn up_to_three_redirects_are_followed_and_a_fourth_is_refused() {
    let (addr, _hits) = server().await;
    let f = HttpFetcher::pinned_for_tests(FAKE, addr);
    let base = format!("http://{FAKE}:{}", addr.port());
    // r3 -> ok is one hop; r1 -> r2 -> r3 -> ok is three.
    assert_eq!(f.get(&format!("{base}/r3"), soon()).await.unwrap(), "{\"file\":\"ok\"}");
    assert_eq!(f.get(&format!("{base}/r1"), soon()).await.unwrap(), "{\"file\":\"ok\"}");
    // r0 -> r1 -> r2 -> r3 -> ok is four hops: refused, even though it would have ended well.
    let e = f.get(&format!("{base}/r0"), soon()).await.expect_err("four redirects");
    assert!(e.to_string().contains("too many redirects"), "{e}");
}

#[tokio::test]
async fn a_redirect_to_a_private_address_is_refused_at_the_hop() {
    let (addr, hits) = server().await;
    let f = HttpFetcher::pinned_for_tests(FAKE, addr);
    let e = f.get(&format!("http://{FAKE}:{}/to-private", addr.port()), soon()).await.expect_err("private hop");
    assert!(e.to_string().contains("refused"), "{e}");
    assert_eq!(hits.secret.load(Ordering::SeqCst), 0, "the private hop must never be requested");
}

#[tokio::test]
async fn the_body_is_capped_at_one_mebibyte_and_errors_are_errors() {
    let (addr, _hits) = server().await;
    let f = HttpFetcher::pinned_for_tests(FAKE, addr);
    let base = format!("http://{FAKE}:{}", addr.port());
    assert_eq!(f.get(&format!("{base}/edge"), soon()).await.unwrap().len(), MAX_BODY_BYTES);
    assert!(f.get(&format!("{base}/big"), soon()).await.is_err(), "one byte over the cap");
    assert!(f.get(&format!("{base}/gone"), soon()).await.is_err(), "a 404 is not a body");
}

#[tokio::test]
async fn no_cookies_a_browser_user_agent_and_no_time_means_no_request() {
    let (addr, hits) = server().await;
    let f = HttpFetcher::pinned_for_tests(FAKE, addr);
    let base = format!("http://{FAKE}:{}", addr.port());
    f.get(&format!("{base}/r3"), soon()).await.unwrap();
    assert_eq!(hits.cookies.load(Ordering::SeqCst), 0);
    assert!(hits.last_ua.lock().unwrap().starts_with("Mozilla/5.0"), "{}", hits.last_ua.lock().unwrap());
    assert!(f.get(&format!("{base}/ok"), Instant::now()).await.is_err(), "a spent deadline");
}

#[tokio::test]
async fn an_article_picture_may_be_two_mib_and_not_a_byte_more_while_a_recipe_fetch_stays_at_one() {
    let (addr, _hits) = server().await;
    let f = HttpFetcher::pinned_for_tests(FAKE, addr);
    let url = |n: &str| format!("http://{FAKE}:{}/{n}", addr.port());
    assert_eq!(f.get_image_bytes(&url("imgedge"), soon()).await.unwrap().len(), MAX_IMAGE_BYTES);
    let err = f.get_image_bytes(&url("imgbig"), soon()).await.unwrap_err().to_string();
    assert!(err.contains("larger than"), "{err}");
    // The same body is far over the recipe cap.
    assert!(MAX_IMAGE_BYTES > MAX_BODY_BYTES);
    assert!(f.get(&url("imgedge"), soon()).await.is_err(), "the recipe path keeps its 1 MiB cap");
    // A private address is refused before anything connects.
    assert!(f.get_image_bytes(&format!("http://127.0.0.1:{}/imgedge", addr.port()), soon()).await.is_err());
}

/// The query of a signed picture address is a secret: no error text may carry it.
#[tokio::test]
async fn a_failed_picture_fetch_never_puts_the_address_query_in_the_error() {
    let (addr, _hits) = server().await;
    let f = HttpFetcher::pinned_for_tests(FAKE, addr);
    // Connection refused: a port nobody listens on.
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dead_addr = dead.local_addr().unwrap();
    drop(dead);
    let f_dead = HttpFetcher::pinned_for_tests(FAKE, dead_addr);
    let url = format!("http://{FAKE}:{}/p.jpg?X-Amz-Signature=SECRET&token=SECRET", dead_addr.port());
    let err = format!("{:#}", f_dead.get_image_bytes(&url, soon()).await.unwrap_err());
    assert!(!err.contains("SECRET") && !err.contains("token="), "refused: {err}");
    assert!(err.contains(FAKE), "the host is still named: {err}");
    // A recipe fetch too.
    let err = format!("{:#}", f_dead.get(&url, soon()).await.unwrap_err());
    assert!(!err.contains("SECRET") && !err.contains("token="), "recipe fetch: {err}");
    // A timeout: the server never answers.
    let url = format!("http://{FAKE}:{}/hang?token=SECRET", addr.port());
    let started = Instant::now();
    let err = format!("{:#}", f.get_image_bytes(&url, Instant::now() + Duration::from_millis(800)).await.unwrap_err());
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(!err.contains("SECRET") && !err.contains("token="), "timeout: {err}");
}

/// One slow picture may take at most 10 s of the card's budget, however much
/// is left, so the next candidate is still tried.
#[tokio::test]
async fn a_slow_name_server_counts_against_the_deadline() {
    // Review finding (P10.11b): the lookup ran on its own 3 s clock after
    // the time left was measured, so a fetch could end ~3 s past its deadline.
    let (addr, _hits) = server().await;
    let f = HttpFetcher::pinned_for_tests(FAKE, addr).with_resolve_delay_for_tests(Duration::from_secs(3));
    let started = Instant::now();
    let r = f.get_image_bytes(&format!("http://{FAKE}:{}/p.png", addr.port()), Instant::now() + Duration::from_millis(500)).await;
    let took = started.elapsed();
    assert!(r.is_err(), "the lookup outlived the deadline, so nothing may be fetched");
    assert!(took < Duration::from_millis(1500), "took {took:?}");
}

#[tokio::test]
async fn a_slow_picture_takes_ten_seconds_at_most_whatever_deadline_is_left() {
    let (addr, _hits) = server().await;
    let f = HttpFetcher::pinned_for_tests(FAKE, addr);
    let started = Instant::now();
    // 6 s on the first hop, then a hop that never answers: the limit is for the whole fetch.
    let r = f.get_image_bytes(&format!("http://{FAKE}:{}/slow1", addr.port()), Instant::now() + Duration::from_secs(60)).await;
    let took = started.elapsed();
    assert!(r.is_err());
    assert!(took >= Duration::from_secs(8) && took < Duration::from_secs(12), "took {took:?}");
}

#[tokio::test]
async fn the_final_address_follows_redirects_past_the_fetch_limit_and_never_reaches_a_private_hop() {
    let (addr, hits) = server().await;
    let f = HttpFetcher::pinned_for_tests(FAKE, addr);
    let base = format!("http://{FAKE}:{}", addr.port());
    // Four hops (the fetch limit is three), ending on /ok.
    assert_eq!(f.final_url(&format!("{base}/r0")).await.unwrap(), format!("{base}/ok"));
    // A page that answers 404 is still where the chain ended.
    assert_eq!(f.final_url(&format!("{base}/gone")).await.unwrap(), format!("{base}/gone"));
    assert!(f.final_url(&format!("{base}/to-private")).await.is_err());
    assert_eq!(hits.secret.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_redirect_loop_fails_at_the_eleventh_hop_and_a_slow_chain_ends_within_the_time_limit() {
    let (addr, _hits) = server().await;
    let f = HttpFetcher::pinned_for_tests(FAKE, addr);
    let base = format!("http://{FAKE}:{}", addr.port());
    let e = f.final_url(&format!("{base}/loop")).await.expect_err("a loop never ends");
    assert!(e.to_string().contains("too many redirects"), "{e}");
    // slow1 answers after 6 s with a redirect to a page that never answers.
    let started = Instant::now();
    assert!(f.final_url(&format!("{base}/slow1")).await.is_err());
    assert!(started.elapsed() < Duration::from_secs(12), "took {:?}", started.elapsed());
}
