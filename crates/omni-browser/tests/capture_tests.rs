//! P11.15b: the file-locker capture in real Chrome/Edge, against invented
//! locker pages a local axum server serves on 127.0.0.1. Nothing leaves the
//! machine, even with a guard broken: the egress rule's resolver answers no
//! real lookup (one test name is 127.0.0.1), the browser's own resolver maps
//! every name to nothing but that one (to 127.0.0.2), and the fixtures' hosts
//! are loopback addresses.
//!
//! The canary (P11.15b2) is a listener on 127.0.0.2, which the rule refuses
//! (only 127.0.0.1/32 is admitted). It counts every connection it accepts, so
//! a popup, a tab, a worker, a WebSocket or a preconnect that slipped past the
//! gate shows up there, even when no request follows.
//!
//! Skipped with a visible warning when no browser is installed.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use omni_browser::capture::{capture_with, CaptureOptions};
use omni_core::locker::CaptureLimits;
use omni_core::net::CidrSet;

fn browser_installed() -> bool {
    [
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
    ]
    .iter()
    .any(|p| Path::new(p).exists())
}

macro_rules! need_browser {
    () => {
        if !browser_installed() {
            eprintln!("WARNING: skipping locker capture test -- no Chrome/Edge installed");
            return;
        }
    };
}

/// One browser at a time.
static ONE_BROWSER: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The file the fixture locker hands out.
const FILE: &[u8] = b"invented clip bytes, 40 of them........";

/// The one name both resolvers know, and disagree on: the rule's says the
/// fixture, the browser's own says the canary (a rebinding name).
const REBIND: &str = "rebind.test";

fn page(body: &str) -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], format!("<!doctype html><html><body>{body}</body></html>")).into_response()
}

fn attachment(bytes: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, "application/octet-stream"), (header::CONTENT_DISPOSITION, "attachment; filename=\"clip.mp4\"")], bytes).into_response()
}

/// The fixture locker's pages; `canary` is the canary's port on 127.0.0.2.
fn respond(path: &str, canary: u16, udp: u16) -> Response {
    let refused = format!("http://127.0.0.2:{canary}");
    match path {
        // A WeTransfer-like page: terms first, then the download button.
        "/locker" => page(
            "<p>Files ready</p>\
             <button id=agree onclick=\"document.getElementById('dl').style.display='inline'\">I agree</button>\
             <button id=dl style=display:none onclick=\"location.href='/file'\">Download</button>",
        ),
        "/file" => attachment(FILE.to_vec()),
        "/big-locker" => page("<button onclick=\"location.href='/big'\">Download</button>"),
        "/big" => attachment(vec![7u8; 4096]),
        "/empty" => page("<p>Nothing to press here</p>"),
        // A download button that points at the canary.
        "/internal" => page(&format!("<button onclick=\"location.href='{refused}/file'\">Download</button>")),
        // A popup, and a link that opens a tab, to the canary.
        "/popup" => page(&format!("<button onclick=\"window.open('{refused}/file')\">Download</button>")),
        "/tab" => page(&format!("<a href=\"{refused}/file\" target=\"_blank\">Download</a>")),
        // A button that waits, inside its own press, for its request to the canary.
        "/sync" => page(&format!(
            "<button onclick=\"var x=new XMLHttpRequest();x.open('GET','{refused}/file',false);try{{x.send();}}catch(e){{}}\">Download</button>"
        )),
        // Ways around a page's own requests: a WebSocket, a worker, and hints.
        "/socket" => page(&format!("<script>try {{ new WebSocket('ws://127.0.0.2:{canary}/socket'); }} catch (e) {{}}</script>")),
        "/worker" => page(&format!(
            "<script>new Worker(URL.createObjectURL(new Blob([\"fetch('{refused}/worker').catch(function(){{}})\"], {{type: 'text/javascript'}})));</script>"
        )),
        // WebRTC: a STUN server and a remote candidate on the UDP canary.
        "/webrtc" => page(&format!(
            "<script>(async () => {{\
               const pc = new RTCPeerConnection({{iceServers: [{{urls: 'stun:127.0.0.2:{udp}'}}]}});\
               pc.createDataChannel('x');\
               await pc.setLocalDescription(await pc.createOffer());\
               const peer = new RTCPeerConnection();\
               await peer.setRemoteDescription(pc.localDescription);\
               await peer.setLocalDescription(await peer.createAnswer());\
               await pc.setRemoteDescription(peer.localDescription);\
               const mid = (pc.localDescription.sdp.match(/a=mid:(\\S+)/) || [null, '0'])[1];\
               await pc.addIceCandidate({{candidate: 'candidate:1 1 udp 2122260223 127.0.0.2 {udp} typ host', sdpMid: mid, sdpMLineIndex: 0}});\
             }})().catch(() => {{}});</script><p>Nothing to press here</p>"
        )),
        "/hints" => page(&format!(
            "<link rel=preconnect href=\"{refused}\"><link rel=dns-prefetch href=\"//{REBIND}\"><link rel=prefetch href=\"{refused}/hint\">\
             <p>Nothing to press here</p>"
        )),
        // The same file in four pieces a second apart.
        "/slow" => {
            let pieces = futures::stream::unfold(0usize, |i| async move {
                if i >= 4 {
                    return None;
                }
                if i > 0 {
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                let piece = FILE.chunks(FILE.len().div_ceil(4)).nth(i).unwrap_or_default().to_vec();
                Some((Ok::<_, std::io::Error>(piece), i + 1))
            });
            (
                [(header::CONTENT_TYPE, "application/octet-stream"), (header::CONTENT_DISPOSITION, "attachment; filename=\"clip.mp4\"")],
                axum::body::Body::from_stream(pieces),
            )
                .into_response()
        }
        // One press, two files: the second begins while the first is still
        // arriving, and only the first is kept.
        "/two" => page(
            "<button onclick=\"var a=document.createElement('a');a.href='/slow';a.download='a.mp4';document.body.appendChild(a);a.click();\
             setTimeout(function(){var b=document.createElement('a');b.href='/big';b.download='b.mp4';document.body.appendChild(b);b.click();},1500);\">Download all</button>",
        ),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

/// A canary listener: counts every connection; answers `/page` with a page
/// and everything else with the file.
fn canary_on(listener: tokio::net::TcpListener, hits: Arc<AtomicUsize>) {
    tokio::spawn(async move {
        while let Ok((mut conn, _)) = listener.accept().await {
            hits.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut head = vec![0u8; 4096];
                let n = tokio::time::timeout(Duration::from_secs(5), conn.read(&mut head)).await.ok().and_then(Result::ok).unwrap_or(0);
                let line = String::from_utf8_lossy(&head[..n]).lines().next().unwrap_or_default().to_string();
                let (kind, extra, body): (&str, &str, &[u8]) = if line.contains(" /page ") {
                    ("text/html", "", b"<!doctype html><p>hello</p>")
                } else {
                    ("application/octet-stream", "Content-Disposition: attachment; filename=\"clip.mp4\"\r\n", FILE)
                };
                let reply = format!("HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = conn.write_all(reply.as_bytes()).await;
                let _ = conn.write_all(body).await;
            });
        }
    });
}

/// The fixture server on 127.0.0.1 and the canary on 127.0.0.2: (base,
/// canary port, canary hits). The canary also listens on the fixture's own
/// port, so that a browser sent to 127.0.0.2 instead of 127.0.0.1 (by its own
/// resolver) lands there.
async fn serve() -> (String, u16, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let canary = tokio::net::TcpListener::bind("127.0.0.2:0").await.expect("127.0.0.2 is a loopback address");
    let cport = canary.local_addr().unwrap().port();
    canary_on(canary, hits.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fport = listener.local_addr().unwrap().port();
    let twin = tokio::net::TcpListener::bind(("127.0.0.2", fport)).await.expect("the canary takes the fixture's port on 127.0.0.2");
    canary_on(twin, hits.clone());
    // And for UDP: every datagram counts.
    let udp = tokio::net::UdpSocket::bind("127.0.0.2:0").await.expect("a UDP canary on 127.0.0.2");
    let uport = udp.local_addr().unwrap().port();
    let datagrams = hits.clone();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while udp.recv_from(&mut buf).await.is_ok() {
            datagrams.fetch_add(1, Ordering::SeqCst);
        }
    });
    let base = format!("http://127.0.0.1:{fport}");
    let app = Router::new().fallback(move |uri: Uri| async move { respond(uri.path(), cport, uport) });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, cport, hits)
}

/// The egress resolver of these tests: the rebinding name is the fixture on
/// 127.0.0.1, which the rule admits; every other name fails. The browser's
/// own resolver sends the same name to the canary (`host_rules`).
async fn resolve(host: String, port: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
    if host == REBIND {
        return Ok(vec![std::net::SocketAddr::from(([127, 0, 0, 1], port))]);
    }
    Err(std::io::Error::new(std::io::ErrorKind::NotFound, "no lookups in tests"))
}

fn options(budget_secs: u64, max_bytes: u64) -> CaptureOptions {
    CaptureOptions {
        limits: CaptureLimits { budget: Duration::from_secs(budget_secs), max_bytes },
        // The fixture server is on 127.0.0.1, which the rule otherwise refuses.
        allow: CidrSet::parse(&["127.0.0.1/32".to_string()]).unwrap(),
        host_rules: Some(format!("MAP {REBIND} 127.0.0.2, MAP * ~NOTFOUND, EXCLUDE 127.0.0.1, EXCLUDE 127.0.0.2")),
    }
}

/// What is left under the capture folder, as relative paths.
fn left(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&d) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(p.strip_prefix(dir).unwrap().to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
    out.sort();
    out
}

/// Run the capture of `path` on the fixture server: (result, what is left, canary hits).
async fn capture(path: &str, budget_secs: u64, max_bytes: u64) -> (Result<std::path::PathBuf, String>, Vec<String>, usize, tempfile::TempDir) {
    let (base, _, hits) = serve().await;
    let url = if path.starts_with("http") { path.to_string() } else { format!("{base}{path}") };
    capture_url(&url, budget_secs, max_bytes, hits).await
}

async fn capture_url(url: &str, budget_secs: u64, max_bytes: u64, hits: Arc<AtomicUsize>) -> (Result<std::path::PathBuf, String>, Vec<String>, usize, tempfile::TempDir) {
    let job = tempfile::tempdir().unwrap();
    let dir = job.path().join("capture");
    // P15.7: a capture that stalls fails here in about a minute rather than
    // holding the whole test run (and CI) until its job limit.
    let limit = Duration::from_secs(budget_secs + 60);
    let got = tokio::time::timeout(limit, capture_with(url, &dir, &options(budget_secs, max_bytes), resolve))
        .await
        .unwrap_or_else(|_| {
            let test = std::thread::current().name().unwrap_or("a capture test").to_string();
            panic!("{test}: the capture of {url} did not end within {limit:?}");
        });
    let left = left(&dir);
    // A connection the page opened late still counts.
    tokio::time::sleep(Duration::from_millis(300)).await;
    (got, left, hits.load(Ordering::SeqCst), job)
}

#[tokio::test]
async fn the_capture_presses_agree_then_download_and_keeps_the_one_file() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    let (got, left, hits, _job) = capture("/locker", 60, 1 << 20).await;
    let file = got.unwrap_or_else(|e| panic!("no file: {e}"));
    assert_eq!(std::fs::read(&file).unwrap(), FILE);
    assert!(file.ends_with("files/capture.mp4") || file.ends_with("files\\capture.mp4"), "{file:?}");
    assert_eq!(left, vec!["files/capture.mp4".to_string()], "only the file is left (no profile, no partial file)");
    assert_eq!(hits, 0);
}

#[tokio::test]
async fn a_file_over_the_limit_is_cancelled_and_nothing_is_left() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    let (got, left, _, _job) = capture("/big-locker", 60, 1000).await;
    let why = got.unwrap_err();
    assert!(why.contains("larger than"), "{why}");
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test]
async fn a_page_without_a_download_ends_within_its_budget() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    let started = std::time::Instant::now();
    let (got, left, _, _job) = capture("/empty", 8, 1 << 20).await;
    let why = got.unwrap_err();
    // The browser's own background traffic, refused by the proxy here, may be named.
    assert!(why.starts_with("no download started"), "{why}");
    assert!(started.elapsed() < Duration::from_secs(40), "took {:?}", started.elapsed());
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test]
async fn an_internal_address_is_refused_by_the_egress_rule() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    let (got, left, hits, _job) = capture("/internal", 10, 1 << 20).await;
    let why = got.unwrap_err();
    assert!(why.contains("not allowed (127.0.0.2)"), "the page's request to the canary is refused, named by its site only: {why}");
    assert_eq!(hits, 0, "nothing reached the canary");
    assert!(left.is_empty(), "{left:?}");
}

/// P11.15b2: the rule fails closed. A name that does not resolve is refused,
/// not handed to the browser.
#[tokio::test]
async fn a_name_that_does_not_resolve_is_refused() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    let (got, left, _, _job) = capture("http://nowhere.test/locker", 10, 1 << 20).await;
    let why = got.unwrap_err();
    assert!(why.contains("not allowed (nowhere.test)"), "{why}");
    assert!(left.is_empty(), "{left:?}");
}

/// P11.15b2: the browser goes to the address the rule checked, never to the
/// one its own resolver would give (DNS rebinding). The rule's resolver says
/// the fixture (127.0.0.1); the browser's own says the canary (127.0.0.2, on
/// the same port). The file comes from the fixture and the canary sees nothing.
#[tokio::test]
async fn the_browser_goes_where_the_rule_looked_not_where_its_own_resolver_points() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    let (base, _, hits) = serve().await;
    let port = base.rsplit(':').next().unwrap().to_string();
    let (got, left, hits, _job) = capture_url(&format!("http://{REBIND}:{port}/locker"), 60, 1 << 20, hits).await;
    let file = got.unwrap_or_else(|e| panic!("no file: {e}"));
    assert_eq!(std::fs::read(&file).unwrap(), FILE);
    assert_eq!(hits, 0, "the browser's own answer (the canary) was never used");
    assert_eq!(left, vec!["files/capture.mp4".to_string()]);
}

/// P11.15b2: a popup is gated like the page that opened it.
#[tokio::test]
async fn a_popup_to_a_refused_address_is_refused() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    let (got, left, hits, _job) = capture("/popup", 10, 1 << 20).await;
    let why = got.unwrap_err();
    assert!(why.contains("not allowed (127.0.0.2)"), "{why}");
    assert_eq!(hits, 0, "nothing reached the canary");
    assert!(left.is_empty(), "{left:?}");
}

/// P11.15b2: a download link that opens a tab is gated too.
#[tokio::test]
async fn a_download_link_that_opens_a_tab_is_still_gated() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    let (got, left, hits, _job) = capture("/tab", 10, 1 << 20).await;
    let why = got.unwrap_err();
    assert!(why.contains("not allowed (127.0.0.2)"), "{why}");
    assert_eq!(hits, 0, "nothing reached the canary");
    assert!(left.is_empty(), "{left:?}");
}

/// A request of the page is answered while the capture waits on something
/// else: here the press itself, which does not return until its own request
/// to the canary is decided (a synchronous request). Were the gate to wait
/// for the press, neither would ever end and the refusal would go unseen
/// (PR #29's CI: a tab's request went unanswered past the deadline).
#[tokio::test]
async fn a_request_is_gated_while_the_press_that_made_it_is_still_running() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    let started = std::time::Instant::now();
    let (got, left, hits, _job) = capture("/sync", 10, 1 << 20).await;
    let why = got.unwrap_err();
    assert!(why.contains("not allowed (127.0.0.2)"), "the press's own request is refused and named, after {:?}: {why}", started.elapsed());
    assert_eq!(hits, 0, "nothing reached the canary");
    assert!(left.is_empty(), "{left:?}");
}

/// The page at `path` gets no file, reaches nothing and leaves nothing.
async fn reaches_nothing(path: &str) {
    let (got, left, hits, _job) = capture(path, 6, 1 << 20).await;
    assert!(got.is_err(), "{path}: no file");
    assert_eq!(hits, 0, "{path}: nothing reached the canary");
    assert!(left.is_empty(), "{path}: {left:?}");
}

/// P11.15b2: a WebSocket reaches nothing (`Fetch` never sees one; the proxy does).
#[tokio::test]
async fn a_websocket_reaches_nothing() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    reaches_nothing("/socket").await;
}

/// P11.15b2: nor does a worker's fetch.
#[tokio::test]
async fn a_workers_request_reaches_nothing() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    reaches_nothing("/worker").await;
}

/// P11.15b2: nor do the page's preconnect, DNS-prefetch and prefetch hints.
#[tokio::test]
async fn the_pages_hints_reach_nothing() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    reaches_nothing("/hints").await;
}

#[tokio::test]
async fn of_two_downloads_only_the_first_is_kept() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    let (got, left, _, _job) = capture("/two", 60, 1 << 20).await;
    let file = got.unwrap_or_else(|e| panic!("no file: {e}"));
    assert_eq!(std::fs::read(&file).unwrap(), FILE, "the first download");
    // The second was cancelled; at most its empty start may linger until the capture's folder goes.
    let files: Vec<String> = left.into_iter().filter(|f| !f.ends_with(".crdownload")).collect();
    assert_eq!(files, vec!["files/capture.mp4".to_string()], "one file");
}

/// P11.15b3: WebRTC sends no UDP: neither to a STUN server nor to a remote
/// candidate, both on the UDP canary.
#[tokio::test]
async fn webrtc_sends_nothing() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    let (got, left, hits, _job) = capture("/webrtc", 8, 1 << 20).await;
    assert!(got.is_err(), "no file");
    assert_eq!(hits, 0, "no datagram reached the canary");
    assert!(left.is_empty(), "{left:?}");
}
