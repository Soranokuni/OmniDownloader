//! P11.15b: the file-locker capture in real Chrome/Edge, against invented
//! locker pages a local axum server serves on 127.0.0.1. Nothing leaves the
//! machine: the browser runs offline (it resolves no host name but 127.0.0.1),
//! the egress rule's resolver refuses every lookup, and the fixtures' hosts are
//! address literals.
//!
//! Skipped with a visible warning when no browser is installed.

use std::path::Path;
use std::time::Duration;

use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;

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

fn page(body: &str) -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], format!("<!doctype html><html><body>{body}</body></html>")).into_response()
}

fn attachment(bytes: Vec<u8>) -> Response {
    ([(header::CONTENT_TYPE, "application/octet-stream"), (header::CONTENT_DISPOSITION, "attachment; filename=\"clip.mp4\"")], bytes).into_response()
}

fn respond(path: &str) -> Response {
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
        // A download button that points at an address the rule refuses (127.0.0.2 is
        // outside the allowed 127.0.0.1/32 and nothing listens there, so even a
        // broken rule reaches nothing).
        "/internal" => page("<button onclick=\"location.href='http://127.0.0.2:'+location.port+'/file'\">Download</button>"),
        // The same file, in four pieces a second apart.
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

async fn serve() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().fallback(|uri: Uri| async move { respond(uri.path()) });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    base
}

/// The egress resolver of these tests: no name is looked up.
async fn refuse(_host: String, _port: u16) -> std::io::Result<Vec<std::net::SocketAddr>> {
    Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no lookups in tests"))
}

fn options(budget_secs: u64, max_bytes: u64) -> CaptureOptions {
    CaptureOptions {
        limits: CaptureLimits { budget: Duration::from_secs(budget_secs), max_bytes },
        // The fixture server is on 127.0.0.1, which the rule otherwise refuses.
        allow: CidrSet::parse(&["127.0.0.1/32".to_string()]).unwrap(),
        offline: true,
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

#[tokio::test]
async fn the_capture_presses_agree_then_download_and_keeps_the_one_file() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    let base = serve().await;
    let job = tempfile::tempdir().unwrap();
    let dir = job.path().join("capture");
    let file = capture_with(&format!("{base}/locker"), &dir, &options(60, 1 << 20), refuse).await.unwrap_or_else(|e| panic!("no file: {e}"));
    assert_eq!(std::fs::read(&file).unwrap(), FILE);
    assert!(file.starts_with(&dir), "inside the capture folder: {file:?}");
    assert_eq!(left(&dir), vec!["files/capture.mp4".to_string()], "only the file is left (no profile, no partial file)");
}

#[tokio::test]
async fn a_file_over_the_limit_is_cancelled_and_nothing_is_left() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    let base = serve().await;
    let job = tempfile::tempdir().unwrap();
    let dir = job.path().join("capture");
    let why = capture_with(&format!("{base}/big-locker"), &dir, &options(60, 1000), refuse).await.unwrap_err();
    assert!(why.contains("larger than"), "{why}");
    assert!(left(&dir).is_empty(), "{:?}", left(&dir));
}

#[tokio::test]
async fn a_page_without_a_download_ends_within_its_budget() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    let base = serve().await;
    let job = tempfile::tempdir().unwrap();
    let dir = job.path().join("capture");
    let started = std::time::Instant::now();
    let why = capture_with(&format!("{base}/empty"), &dir, &options(8, 1 << 20), refuse).await.unwrap_err();
    assert!(why.contains("no download started"), "{why}");
    assert!(started.elapsed() < Duration::from_secs(40), "took {:?}", started.elapsed());
    assert!(left(&dir).is_empty(), "{:?}", left(&dir));
}

#[tokio::test]
async fn an_internal_address_is_refused_by_the_egress_rule() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    let base = serve().await;
    let job = tempfile::tempdir().unwrap();
    let dir = job.path().join("capture");
    let why = capture_with(&format!("{base}/internal"), &dir, &options(10, 1 << 20), refuse).await.unwrap_err();
    assert!(why.contains("not allowed"), "the page's request to 127.0.0.2 is refused: {why}");
    assert!(left(&dir).is_empty(), "{:?}", left(&dir));
}

#[tokio::test]
async fn of_two_downloads_only_the_first_is_kept() {
    need_browser!();
    let _turn = ONE_BROWSER.lock().await;
    let base = serve().await;
    let job = tempfile::tempdir().unwrap();
    let dir = job.path().join("capture");
    let file = capture_with(&format!("{base}/two"), &dir, &options(60, 1 << 20), refuse).await.unwrap_or_else(|e| panic!("no file: {e}"));
    assert_eq!(std::fs::read(&file).unwrap(), FILE, "the first download");
    // The second was cancelled; at most its empty start may linger until the job's workspace goes.
    let files: Vec<String> = left(&dir).into_iter().filter(|f| !f.ends_with(".crdownload")).collect();
    assert_eq!(files, vec!["files/capture.mp4".to_string()], "one file");
}
