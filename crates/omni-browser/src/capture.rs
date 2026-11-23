//! The file-locker capture (P11.15b): a WeTransfer-like page opened in the
//! headless browser, which presses its «download» (and «I agree») buttons and
//! keeps the ONE file it hands out. Switched on by `locker.browser_capture`;
//! every failure ends at MCR (MANUAL_DOWNLOAD), never in a retry loop.
//!
//! Bounded and contained:
//! * one budget for the page and the download together, a size limit checked
//!   on every progress report and again at the end, one file (any second
//!   download is cancelled), at most [`MAX_CLICKS`] clicks;
//! * every request of the page is held (`Fetch`) and let through only under
//!   the egress rule, the host checked once and remembered; the download's
//!   own address is checked when the browser announces it, and cancelled when
//!   refused;
//! * the browser profile and the download both live in the job's own
//!   directory, which the pipeline removes as a whole.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chromiumoxide::cdp::browser_protocol::browser::{
    CancelDownloadParams, DownloadProgressState, EventDownloadProgress, EventDownloadWillBegin, SetDownloadBehaviorBehavior,
    SetDownloadBehaviorParams,
};
use chromiumoxide::cdp::browser_protocol::fetch::{ContinueRequestParams, EnableParams as FetchEnable, EventRequestPaused, FailRequestParams};
use chromiumoxide::cdp::browser_protocol::network::ErrorReason;
use futures::StreamExt;
use serde_json::Value;
use tracing::{info, warn};

use omni_core::locker::{CaptureFuture, CaptureLimits, LockerCapture};
use omni_core::net::{CidrSet, EgressRefusal};

use crate::browser::HeadlessBrowserManager;

/// Clicks on «download» / «agree» controls one capture may make.
pub const MAX_CLICKS: usize = 8;

/// Between two clicks, while no download has begun.
const CLICK_EVERY: Duration = Duration::from_secs(3);

/// How long one name lookup of the egress rule may take.
const LOOKUP: Duration = Duration::from_secs(5);

/// Finds the first visible control that says «download» (or «agree»,
/// «accept», in English and Greek) and was not clicked yet, marks it and
/// clicks it. Returns its label, or null.
const CLICK_JS: &str = r#"(() => {
  const words = /(download|λήψη|κατέβασμα|κατεβάστε|i agree|agree|accept|συμφωνώ|αποδοχή|αποδέχομαι)/i;
  const nodes = document.querySelectorAll('button, a, [role=button], input[type=submit], input[type=button]');
  for (const n of nodes) {
    if (n.dataset && n.dataset.omniClicked) continue;
    const label = ((n.innerText || '') + ' ' + (n.value || '') + ' ' + (n.getAttribute('aria-label') || '')).trim();
    if (!words.test(label)) continue;
    const r = n.getBoundingClientRect();
    if (r.width === 0 || r.height === 0) continue;
    n.dataset.omniClicked = '1';
    n.click();
    return label.slice(0, 60);
  }
  return null;
})()"#;

/// How one capture runs.
#[derive(Debug, Clone)]
pub struct CaptureOptions {
    pub limits: CaptureLimits,
    /// Internal networks the egress rule admits (`security.egress_allow`).
    pub allow: CidrSet,
    /// Tests: the browser resolves no host name but 127.0.0.1, so a page that
    /// points elsewhere reaches nothing.
    pub offline: bool,
}

/// The daemon's [`LockerCapture`]: the headless browser, under the egress rule
/// with the system resolver.
pub struct BrowserCapture {
    pub allow: CidrSet,
}

impl LockerCapture for BrowserCapture {
    fn capture<'a>(&'a self, url: &'a str, dir: &'a Path, limits: CaptureLimits) -> CaptureFuture<'a> {
        Box::pin(async move {
            let options = CaptureOptions { limits, allow: self.allow.clone(), offline: false };
            capture_with(url, dir, &options, omni_core::net::system_lookup).await
        })
    }
}

/// Open `url` in the headless browser and keep the one file it downloads, in
/// `dir/files/`. `resolve` is the egress rule's resolver. On any failure
/// nothing is left in `dir` and the error says why, in a few words.
pub async fn capture_with<R, Fut>(url: &str, dir: &Path, options: &CaptureOptions, resolve: R) -> Result<PathBuf, String>
where
    R: Fn(String, u16) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>> + Send,
{
    let files = dir.join("files");
    let profile = dir.join("profile");
    if let Err(e) = std::fs::create_dir_all(&files) {
        return Err(format!("the capture folder could not be made: {e}"));
    }
    let deadline = Instant::now() + options.limits.budget;
    let extra = if options.offline { vec!["--host-resolver-rules=MAP * ~NOTFOUND , EXCLUDE 127.0.0.1".to_string()] } else { Vec::new() };
    let mut session = match HeadlessBrowserManager::launch_with(profile.clone(), extra).await {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_dir_all(dir);
            return Err(format!("the browser did not start: {e:#}"));
        }
    };
    let result = run(&session.browser, url, &files, options, deadline, resolve).await;
    session.shutdown().await;
    let _ = std::fs::remove_dir_all(&profile);
    match result {
        Ok(path) => Ok(path),
        Err(why) => {
            let _ = std::fs::remove_dir_all(&files);
            Err(why)
        }
    }
}

/// The egress rule for one address, with the answers of this capture remembered per host.
async fn allowed<R, Fut>(url: &str, allow: &CidrSet, resolve: R, seen: &Mutex<HashMap<String, bool>>) -> bool
where
    R: Fn(String, u16) -> Fut + Clone,
    Fut: std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>>,
{
    let Some(host) = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)) else {
        // data:, blob:, about: carry no host and reach no network.
        return true;
    };
    if let Some(known) = seen.lock().ok().and_then(|s| s.get(&host).copied()) {
        return known;
    }
    let ok = match omni_core::net::check_public_with(url, allow, LOOKUP, resolve).await {
        Ok(_) => true,
        // A name that does not resolve reaches nothing either; the browser fails it itself.
        Err(r) if r.is_unknown() => true,
        Err(EgressRefusal::NonPublic(_)) | Err(_) => false,
    };
    if let Ok(mut s) = seen.lock() {
        s.insert(host, ok);
    }
    ok
}

async fn run<R, Fut>(
    browser: &chromiumoxide::Browser,
    url: &str,
    files: &Path,
    options: &CaptureOptions,
    deadline: Instant,
    resolve: R,
) -> Result<PathBuf, String>
where
    R: Fn(String, u16) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>> + Send,
{
    let behavior = SetDownloadBehaviorParams::builder()
        .behavior(SetDownloadBehaviorBehavior::AllowAndName)
        .download_path(files.to_string_lossy().to_string())
        .events_enabled(true)
        .build()
        .map_err(|e| format!("downloads could not be set up: {e}"))?;
    browser.execute(behavior).await.map_err(|e| format!("downloads could not be set up: {e}"))?;
    let mut will_begin = browser.event_listener::<EventDownloadWillBegin>().await.map_err(|e| format!("no download events: {e}"))?;
    let mut progress = browser.event_listener::<EventDownloadProgress>().await.map_err(|e| format!("no download events: {e}"))?;

    let page = browser.new_page("about:blank").await.map_err(|e| format!("no page: {e}"))?;
    // Every request of the page waits for the egress rule.
    let mut paused = page.event_listener::<EventRequestPaused>().await.map_err(|e| format!("no request control: {e}"))?;
    page.execute(FetchEnable::default()).await.map_err(|e| format!("no request control: {e}"))?;
    let seen = Arc::new(Mutex::new(HashMap::<String, bool>::new()));
    let refused_any = Arc::new(Mutex::new(None::<String>));
    let gate = {
        let (page, allow, resolve, seen, refused_any) = (page.clone(), options.allow.clone(), resolve.clone(), seen.clone(), refused_any.clone());
        tokio::spawn(async move {
            while let Some(ev) = paused.next().await {
                let id = ev.request_id.clone();
                if allowed(&ev.request.url, &allow, resolve.clone(), &seen).await {
                    let _ = page.execute(ContinueRequestParams::new(id)).await;
                } else {
                    if let Ok(mut r) = refused_any.lock() {
                        r.get_or_insert_with(|| omni_core::cookies::host_and_path(&ev.request.url));
                    }
                    let _ = page.execute(FailRequestParams::new(id, ErrorReason::BlockedByClient)).await;
                }
            }
        })
    };

    let result = async {
        // A page that starts its download at once never finishes loading: the wait below bounds it.
        let _ = tokio::time::timeout(Duration::from_secs(30).min(deadline.saturating_duration_since(Instant::now())), page.goto(url)).await;
        let mut ours: Option<(String, String)> = None;
        let mut clicks = 0usize;
        let mut last_click = Instant::now();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            if Instant::now() >= deadline {
                return Err(match &ours {
                    Some(_) => "the download did not finish in time".to_string(),
                    None => "no download started".to_string(),
                });
            }
            tokio::select! {
                Some(ev) = will_begin.next() => {
                    if ours.is_some() {
                        // One file only.
                        let _ = browser.execute(CancelDownloadParams::new(ev.guid.clone())).await;
                        continue;
                    }
                    if !allowed(&ev.url, &options.allow, resolve.clone(), &seen).await {
                        let _ = browser.execute(CancelDownloadParams::new(ev.guid.clone())).await;
                        return Err(format!("the download's address is not allowed ({})", omni_core::cookies::host_and_path(&ev.url)));
                    }
                    info!("Locker capture: a download began ({})", omni_core::urlnorm::site_for_log(&ev.url));
                    ours = Some((ev.guid.clone(), ev.suggested_filename.clone()));
                }
                Some(ev) = progress.next() => {
                    let Some((guid, name)) = ours.clone() else { continue };
                    if ev.guid != guid {
                        continue;
                    }
                    let too_big = ev.received_bytes > options.limits.max_bytes as f64 || ev.total_bytes > options.limits.max_bytes as f64;
                    if too_big {
                        let _ = browser.execute(CancelDownloadParams::new(guid)).await;
                        return Err(format!("the file is larger than {} MB", options.limits.max_bytes >> 20));
                    }
                    match ev.state {
                        DownloadProgressState::Completed => return finished(files, &guid, &name, options.limits.max_bytes),
                        DownloadProgressState::Canceled => return Err("the download was cancelled".to_string()),
                        DownloadProgressState::InProgress => {}
                    }
                }
                _ = tick.tick() => {
                    if ours.is_none() && clicks < MAX_CLICKS && last_click.elapsed() >= CLICK_EVERY {
                        last_click = Instant::now();
                        let clicked = page.evaluate(CLICK_JS).await.ok().and_then(|r| r.value().and_then(Value::as_str).map(str::to_string));
                        if let Some(label) = clicked {
                            clicks += 1;
                            info!("Locker capture: pressed «{label}»");
                        }
                    }
                }
            }
        }
    }
    .await;
    gate.abort();
    let _ = page.close().await;
    match result {
        Err(why) if why == "no download started" => match refused_any.lock().ok().and_then(|r| r.clone()) {
            Some(host) => Err(format!("no download started; the page asked for an address that is not allowed ({host})")),
            None => Err(why),
        },
        other => other,
    }
}

/// The finished download `guid` (saved under its guid by `AllowAndName`),
/// renamed with the extension its suggested name has, after the size check.
fn finished(files: &Path, guid: &str, suggested: &str, max_bytes: u64) -> Result<PathBuf, String> {
    let saved = files.join(guid);
    let size = std::fs::metadata(&saved).map_err(|_| "the downloaded file is missing".to_string())?.len();
    if size == 0 {
        return Err("the downloaded file is empty".to_string());
    }
    if size > max_bytes {
        return Err(format!("the file is larger than {} MB", max_bytes >> 20));
    }
    let ext = Path::new(suggested)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty() && e.len() <= 5 && e.bytes().all(|b| b.is_ascii_alphanumeric()))
        .map(|e| e.to_ascii_lowercase());
    let target = match ext {
        Some(ext) => files.join(format!("capture.{ext}")),
        None => files.join("capture"),
    };
    if let Err(e) = std::fs::rename(&saved, &target) {
        warn!("Locker capture: the file could not be renamed ({e}); kept as it was saved");
        return Ok(saved);
    }
    Ok(target)
}
