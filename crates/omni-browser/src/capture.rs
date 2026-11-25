//! The file-locker capture (P11.15b): a WeTransfer-like page opened in the
//! headless browser, which presses its «download» (and «I agree») buttons and
//! keeps the ONE file it hands out. Switched on by `locker.browser_capture`;
//! every failure ends at MCR (MANUAL_DOWNLOAD), never in a retry loop.
//!
//! Bounded and contained:
//! * one budget for the page and the download together, a size limit checked
//!   on every progress report and again at the end, one file (any second
//!   download is cancelled), at most [`MAX_CLICKS`] clicks;
//! * every connection the browser opens (a page, a popup or tab, a worker, a
//!   WebSocket, a preconnect) goes through the capture's own proxy
//!   ([`proxy`]), which applies the egress rule before it connects and then
//!   connects to the address it checked: the browser's own resolver is never
//!   used, so a name cannot answer public to the rule and internal to the
//!   browser (DNS rebinding) (P11.15b2);
//! * on top of that, every request of every target is held by the
//!   browser-wide `Fetch` under the same rule, which names what was refused;
//!   the rule fails closed (a name that does not resolve, or not in time, is
//!   refused), and only refusals are remembered;
//! * a response that came from an internal address other than the proxy ends
//!   the capture; the browser predicts and preconnects nothing;
//! * the download's own address is checked when the browser announces it,
//!   and cancelled when refused;
//! * the browser profile and the download both live in the capture's own
//!   directory, which the worker removes as a whole when the job ends.

use std::collections::HashSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chromiumoxide::cdp::browser_protocol::browser::{
    CancelDownloadParams, DownloadProgressState, EventDownloadProgress, EventDownloadWillBegin, SetDownloadBehaviorBehavior,
    SetDownloadBehaviorParams,
};
use chromiumoxide::cdp::browser_protocol::fetch::{
    AuthChallengeResponse, AuthChallengeResponseResponse, AuthChallengeSource, ContinueRequestParams, ContinueWithAuthParams, EnableParams as FetchEnable,
    EventAuthRequired, EventRequestPaused, FailRequestParams,
};
use chromiumoxide::cdp::browser_protocol::network::{ErrorReason, EventResponseReceived};
use chromiumoxide::cdp::browser_protocol::target::{EventTargetCreated, TargetId};
use chromiumoxide::Page;
use futures::StreamExt;
use serde_json::Value;
use tracing::{info, warn};

use omni_core::locker::{CaptureFuture, CaptureLimits, LockerCapture};
use omni_core::net::CidrSet;
use omni_core::urlnorm::site_for_log;

use crate::browser::HeadlessBrowserManager;

mod proxy;

/// Clicks on «download» / «agree» controls one capture may make.
pub const MAX_CLICKS: usize = 8;

/// Between two clicks, while no download has begun.
const CLICK_EVERY: Duration = Duration::from_secs(3);

/// How long one name lookup of the egress rule may take.
const LOOKUP: Duration = Duration::from_secs(5);

/// How long closing the capture's page may take.
const PAGE_CLOSE: Duration = Duration::from_secs(5);

/// How far past the budget the capture may run when a DevTools command does
/// not answer: a step in flight at the deadline (a lookup, at most
/// [`LOOKUP`]) and closing the page. Past it the capture is dropped (P15.7).
const RUN_OVERRUN: Duration = Duration::from_secs(12);

/// How long the browser's own shutdown may take; past it Chrome is killed.
const SHUTDOWN: Duration = Duration::from_secs(10);

/// How long killing Chrome may take, after a shutdown that did not end.
const KILL: Duration = Duration::from_secs(3);

/// A capture ends within its budget plus this, whatever the browser does
/// (or does not) answer (P15.7): [`RUN_OVERRUN`] + [`SHUTDOWN`] + [`KILL`].
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(25);

/// The browser predicts, prefetches and preconnects nothing: a preconnect
/// opens a connection no request (and so no egress check) goes with. WebRTC
/// sends no UDP the proxy does not carry, which is none (P11.15b3): STUN and
/// ICE would otherwise reach any address, inside the network too.
///
/// chromiumoxide puts its own `--disable-features=TranslateUI` before these;
/// Chrome keeps the last one, so this one names TranslateUI as well.
const QUIET_FLAGS: &[&str] = &[
    "--dns-prefetch-disable",
    "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
    "--disable-features=TranslateUI,NetworkPrediction,Prerender2,SpeculationRulesPrefetchProxy,PreloadingHeuristics,LoadingPredictorUseLocalPredictions",
];

/// The same, in the profile: `network_prediction_options` 2 is «never». The
/// WebRTC policy here is the one that holds (measured: with the flag above
/// alone, ICE still sent UDP; with this alone, none); the flag stays as well.
const QUIET_PREFERENCES: &str = r#"{"net":{"network_prediction_options":2},"webrtc":{"ip_handling_policy":"disable_non_proxied_udp"}}"#;

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
    /// Tests: Chrome's own `--host-resolver-rules`, so that the browser
    /// resolves no name but the ones a test maps. `None` in the daemon.
    pub host_rules: Option<String>,
}

/// The daemon's [`LockerCapture`]: the headless browser, under the egress rule
/// with the system resolver.
pub struct BrowserCapture {
    pub allow: CidrSet,
}

impl LockerCapture for BrowserCapture {
    fn capture<'a>(&'a self, url: &'a str, dir: &'a Path, limits: CaptureLimits) -> CaptureFuture<'a> {
        Box::pin(async move {
            let options = CaptureOptions { limits, allow: self.allow.clone(), host_rules: None };
            capture_with(url, dir, &options, omni_core::net::system_lookup).await
        })
    }
}

/// Open `url` in the headless browser and keep the one file it downloads, in
/// `dir/files/`. `resolve` is the egress rule's resolver. On any failure
/// nothing is left in `dir` and the error says why, in a few words, naming a
/// site at most, never an address. Dropping the future kills the browser.
/// It returns within the budget plus [`SHUTDOWN_GRACE`], whatever the browser
/// does (after a launch chromiumoxide bounds itself).
pub async fn capture_with<R, Fut>(url: &str, dir: &Path, options: &CaptureOptions, resolve: R) -> Result<PathBuf, String>
where
    R: Fn(String, u16) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>> + Send + 'static,
{
    let files = dir.join("files");
    let profile = dir.join("profile");
    let made = std::fs::create_dir_all(&files)
        .and_then(|_| std::fs::create_dir_all(profile.join("Default")))
        .and_then(|_| std::fs::write(profile.join("Default").join("Preferences"), QUIET_PREFERENCES));
    if let Err(e) = made {
        let _ = std::fs::remove_dir_all(dir);
        return Err(format!("the capture folder could not be made: {e}"));
    }
    let deadline = Instant::now() + options.limits.budget;
    // The sent link's own port, when it is not 80 or 443, is the one other port the proxy reaches.
    let own_port = reqwest::Url::parse(url).ok().and_then(|u| u.port_or_known_default());
    let mut proxy = match proxy::start(options.allow.clone(), resolve.clone(), LOOKUP, own_port).await {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_dir_all(dir);
            return Err(format!("the capture's proxy did not start: {e}"));
        }
    };
    let mut extra: Vec<String> = QUIET_FLAGS.iter().map(|f| f.to_string()).collect();
    // Every connection through the proxy, loopback included (Chrome skips the
    // proxy for loopback unless told otherwise).
    extra.push(format!("--proxy-server=http://{}", proxy.addr));
    extra.push("--proxy-bypass-list=<-loopback>".to_string());
    if let Some(rules) = &options.host_rules {
        extra.push(format!("--host-resolver-rules={rules}"));
    }
    let mut session = match HeadlessBrowserManager::launch_with(profile.clone(), extra).await {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::remove_dir_all(dir);
            return Err(format!("the browser did not start: {e:#}"));
        }
    };
    // A DevTools command that never answers must not hold the capture (P15.7):
    // past the deadline and its overrun the run is dropped, and the browser is
    // shut down within its own bound or killed.
    let seen = Mutex::new(Seen::default());
    let result = until(
        tokio::time::Instant::from_std(deadline + RUN_OVERRUN),
        run(&session.browser, url, &files, options, deadline, resolve, &mut proxy, &seen),
        || {
            warn!("Locker capture: the browser stopped answering ({})", site_for_log(url));
            Err(seen.lock().map(|s| s.reason_at_deadline()).unwrap_or_else(|_| "no download started".to_string()))
        },
    )
    .await;
    if !bounded(SHUTDOWN, session.shutdown()).await {
        warn!("Locker capture: the browser did not close in time; killing it");
        let _ = tokio::time::timeout(KILL, session.browser.kill()).await;
    }
    // Dropping the session kills Chrome if it still runs (kill on drop).
    drop(session);
    drop(proxy);
    let _ = std::fs::remove_dir_all(&profile);
    match result {
        Ok(path) => Ok(path),
        Err(why) => {
            let _ = std::fs::remove_dir_all(&files);
            Err(why)
        }
    }
}

/// Run `work` until `deadline`; past it, `work` is dropped and `late` answers.
async fn until<T>(deadline: tokio::time::Instant, work: impl std::future::Future<Output = T>, late: impl FnOnce() -> T) -> T {
    match tokio::time::timeout_at(deadline, work).await {
        Ok(v) => v,
        Err(_) => late(),
    }
}

/// Await `step` for at most `grace`. False when it did not end in time (it is
/// dropped then).
async fn bounded(grace: Duration, step: impl std::future::Future<Output = ()>) -> bool {
    tokio::time::timeout(grace, step).await.is_ok()
}

/// What a capture saw, readable after its run was dropped at the deadline.
#[derive(Debug, Default)]
struct Seen {
    /// A download of ours began.
    began: bool,
    /// The first address a request of the page (`Fetch`) was refused.
    refused_by_page: Option<String>,
    /// The first connection only the proxy saw and refused (a WebSocket, a
    /// preconnect, the browser's own traffic).
    refused_by_proxy: Option<String>,
}

impl Seen {
    /// The reason when the budget ran out.
    fn reason_at_deadline(&self) -> String {
        if self.began {
            return "the download did not finish in time".to_string();
        }
        match self.refused_by_page.as_ref().or(self.refused_by_proxy.as_ref()) {
            Some(site) => format!("no download started; the page asked for an address that is not allowed ({site})"),
            None => "no download started".to_string(),
        }
    }
}

/// The egress rule for one address. Fails closed: a name that does not
/// resolve, or not within [`LOOKUP`], is refused. A refused host is
/// remembered for this capture; an allowed one never is, so the next request
/// to it is judged on a fresh lookup.
async fn allowed<R, Fut>(url: &str, allow: &CidrSet, resolve: R, refused: &Mutex<HashSet<String>>) -> bool
where
    R: Fn(String, u16) -> Fut + Clone,
    Fut: std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>>,
{
    let Some(host) = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase)) else {
        // data:, blob:, about: carry no host and reach no network.
        return true;
    };
    if refused.lock().map(|r| r.contains(&host)).unwrap_or(true) {
        return false;
    }
    let ok = omni_core::net::check_public_with(url, allow, LOOKUP, resolve).await.is_ok();
    if !ok {
        if let Ok(mut r) = refused.lock() {
            r.insert(host);
        }
    }
    ok
}

/// The address a response came from, as Chrome reports it (`remoteIPAddress`,
/// an IPv6 one possibly in brackets).
fn remote_ip(raw: &str) -> Option<IpAddr> {
    raw.trim().trim_start_matches('[').trim_end_matches(']').parse().ok()
}

/// True when `ip` is internal (loopback, private, link-local, ULA, an IPv4
/// address inside IPv6, …) and not admitted by `allow`.
fn reached_internal(ip: IpAddr, allow: &CidrSet) -> bool {
    omni_core::net::is_non_public(ip) && !allow.contains(omni_core::net::unmap_v4(ip))
}

/// Watch one page of the capture: the site of any response that came from an
/// internal address other than the capture's proxy (`proxy`), so by a way
/// round it, is sent on `reached`.
async fn watch(page: &Page, allow: &CidrSet, proxy: std::net::SocketAddr, reached: &tokio::sync::mpsc::UnboundedSender<String>) {
    let mut responses = match page.event_listener::<EventResponseReceived>().await {
        Ok(r) => r,
        Err(e) => {
            // Without the check the capture must not go on.
            let _ = reached.send(format!("unchecked page: {e}"));
            return;
        }
    };
    let (allow, reached) = (allow.clone(), reached.clone());
    tokio::spawn(async move {
        while let Some(ev) = responses.next().await {
            if let Some(ip) = ev.response.remote_ip_address.as_deref().and_then(remote_ip) {
                let via_proxy = ip == proxy.ip() && ev.response.remote_port == Some(i64::from(proxy.port()));
                if !via_proxy && reached_internal(ip, &allow) {
                    let _ = reached.send(site_for_log(&ev.response.url));
                }
            }
        }
    });
}

async fn run<R, Fut>(
    browser: &chromiumoxide::Browser,
    url: &str,
    files: &Path,
    options: &CaptureOptions,
    deadline: Instant,
    resolve: R,
    proxy: &mut proxy::EgressProxy,
    seen: &Mutex<Seen>,
) -> Result<PathBuf, String>
where
    R: Fn(String, u16) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = std::io::Result<Vec<std::net::SocketAddr>>> + Send + 'static,
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

    // Every request of every target waits for the egress rule: `Fetch` on the
    // browser itself holds the requests of each page, popup, tab and worker
    // before it runs, so none is missed while it starts (P11.15b2).
    let mut paused = browser.event_listener::<EventRequestPaused>().await.map_err(|e| format!("no request control: {e}"))?;
    let mut created = browser.event_listener::<EventTargetCreated>().await.map_err(|e| format!("no request control: {e}"))?;
    // The proxy asks for this capture's credentials (P11.15b3): they are given
    // to the proxy's own challenge only, never to a site's.
    let mut challenged = browser.event_listener::<EventAuthRequired>().await.map_err(|e| format!("no request control: {e}"))?;
    let proxy_origin = format!("http://{}", proxy.addr);
    let fetch = FetchEnable::builder().handle_auth_requests(true).build();
    browser.execute(fetch).await.map_err(|e| format!("no request control: {e}"))?;

    let (reached_tx, mut reached) = tokio::sync::mpsc::unbounded_channel::<String>();
    let page = browser.new_page("about:blank").await.map_err(|e| format!("no page: {e}"))?;
    watch(&page, &options.allow, proxy.addr, &reached_tx).await;
    let mut watched: HashSet<TargetId> = HashSet::from([page.target_id().clone()]);
    // Popups and tabs seen, not yet watched (their page is still starting).
    let mut unwatched: Vec<TargetId> = Vec::new();

    let refused = Mutex::new(HashSet::<String>::new());
    // The navigation runs on its own: the requests it makes are answered
    // below. It ends with the run, also when the run is dropped (P15.7).
    let navigation = AbortOnDrop({
        let (page, url) = (page.clone(), url.to_string());
        tokio::spawn(async move {
            let _ = page.goto(url).await;
        })
    });

    let result = async {
        let mut ours: Option<(String, String)> = None;
        let mut clicks = 0usize;
        // The first click waits for the page to load.
        let mut last_click = Instant::now();
        let mut tick = tokio::time::interval(Duration::from_millis(500));
        loop {
            if Instant::now() >= deadline {
                return Err(seen.lock().map(|s| s.reason_at_deadline()).unwrap_or_else(|_| "no download started".to_string()));
            }
            tokio::select! {
                // A download's start before its progress (P11.15b2): a progress
                // report is never read before the download it belongs to.
                biased;
                Some(ev) = will_begin.next() => {
                    if ours.is_some() {
                        // One file only.
                        let _ = browser.execute(CancelDownloadParams::new(ev.guid.clone())).await;
                        continue;
                    }
                    if !allowed(&ev.url, &options.allow, resolve.clone(), &refused).await {
                        let _ = browser.execute(CancelDownloadParams::new(ev.guid.clone())).await;
                        return Err(format!("the download's address is not allowed ({})", site_for_log(&ev.url)));
                    }
                    info!("Locker capture: a download began ({})", site_for_log(&ev.url));
                    ours = Some((ev.guid.clone(), ev.suggested_filename.clone()));
                    if let Ok(mut s) = seen.lock() {
                        s.began = true;
                    }
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
                Some(site) = reached.recv() => {
                    if let Some((guid, _)) = &ours {
                        let _ = browser.execute(CancelDownloadParams::new(guid.clone())).await;
                    }
                    return Err(format!("the page reached an internal address ({site})"));
                }
                // A connection the proxy refused (a WebSocket, a preconnect, …).
                Some(site) = proxy.refused.recv() => {
                    if let Ok(mut s) = seen.lock() {
                        s.refused_by_proxy.get_or_insert(site);
                    }
                }
                Some(ev) = challenged.next() => {
                    let challenge = &ev.auth_challenge;
                    let ours = challenge.source == Some(AuthChallengeSource::Proxy)
                        && challenge.origin.trim_end_matches('/').eq_ignore_ascii_case(&proxy_origin);
                    let answer = if ours {
                        AuthChallengeResponse {
                            response: AuthChallengeResponseResponse::ProvideCredentials,
                            username: Some(proxy::PROXY_USER.to_string()),
                            password: Some(proxy.password.clone()),
                        }
                    } else {
                        AuthChallengeResponse { response: AuthChallengeResponseResponse::CancelAuth, username: None, password: None }
                    };
                    let _ = browser.execute(ContinueWithAuthParams::new(ev.request_id.clone(), answer)).await;
                }
                Some(ev) = paused.next() => {
                    let id = ev.request_id.clone();
                    if allowed(&ev.request.url, &options.allow, resolve.clone(), &refused).await {
                        let _ = browser.execute(ContinueRequestParams::new(id)).await;
                    } else {
                        if let Ok(mut s) = seen.lock() {
                            s.refused_by_page.get_or_insert_with(|| site_for_log(&ev.request.url));
                        }
                        let _ = browser.execute(FailRequestParams::new(id, ErrorReason::BlockedByClient)).await;
                    }
                }
                Some(ev) = created.next() => {
                    let info = &ev.target_info;
                    if info.r#type == "page" && !watched.contains(&info.target_id) {
                        unwatched.push(info.target_id.clone());
                    }
                }
                _ = tick.tick() => {
                    // A popup's page is ready a moment after it is announced.
                    let mut still = Vec::new();
                    for id in std::mem::take(&mut unwatched) {
                        match browser.get_page(id.clone()).await {
                            Ok(p) => {
                                watch(&p, &options.allow, proxy.addr, &reached_tx).await;
                                watched.insert(id);
                            }
                            Err(_) => still.push(id),
                        }
                    }
                    unwatched = still;
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
    drop(navigation);
    let _ = tokio::time::timeout(PAGE_CLOSE, page.close()).await;
    result
}

/// A spawned task that is aborted when this is dropped.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn none_allowed() -> CidrSet {
        CidrSet::parse(&[]).unwrap()
    }

    #[test]
    fn a_response_from_an_internal_address_is_caught_in_every_form() {
        let allow = CidrSet::parse(&["127.0.0.1/32".to_string()]).unwrap();
        for raw in ["127.0.0.2", "10.1.2.3", "192.168.0.9", "172.16.5.5", "169.254.169.254", "[::1]", "::1", "fd00::5", "fe80::1", "::ffff:10.0.0.1", "[::ffff:192.168.1.1]"] {
            let ip = remote_ip(raw).unwrap_or_else(|| panic!("{raw} not read"));
            assert!(reached_internal(ip, &allow), "{raw} counts as internal");
        }
        for raw in ["93.184.216.34", "2606:4700::1111", "127.0.0.1", "::ffff:127.0.0.1"] {
            let ip = remote_ip(raw).unwrap_or_else(|| panic!("{raw} not read"));
            assert!(!reached_internal(ip, &allow), "{raw} is public or admitted");
        }
        assert_eq!(remote_ip(""), None);
    }

    #[tokio::test]
    async fn a_name_that_does_not_resolve_is_refused_and_remembered() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resolve = {
            let calls = calls.clone();
            move |_h: String, _p: u16| {
                calls.fetch_add(1, Ordering::SeqCst);
                async { Err::<Vec<std::net::SocketAddr>, _>(std::io::Error::new(std::io::ErrorKind::NotFound, "no such name")) }
            }
        };
        let refused = Mutex::new(HashSet::new());
        assert!(!allowed("https://nowhere.example/x", &none_allowed(), resolve.clone(), &refused).await, "fails closed");
        assert!(!allowed("https://nowhere.example/y", &none_allowed(), resolve, &refused).await);
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the refusal is remembered");
    }

    #[tokio::test]
    async fn a_lookup_that_does_not_answer_in_time_is_refused() {
        let resolve = |_h: String, _p: u16| async {
            std::future::pending::<()>().await;
            Ok::<Vec<std::net::SocketAddr>, std::io::Error>(Vec::new())
        };
        let refused = Mutex::new(HashSet::new());
        assert!(!allowed("https://slow.example/x", &none_allowed(), resolve, &refused).await);
    }

    #[tokio::test]
    async fn an_allowed_host_is_looked_up_again_on_every_request() {
        let calls = Arc::new(AtomicUsize::new(0));
        let resolve = {
            let calls = calls.clone();
            move |_h: String, port: u16| {
                calls.fetch_add(1, Ordering::SeqCst);
                async move { Ok::<_, std::io::Error>(vec![std::net::SocketAddr::from(([93, 184, 216, 34], port))]) }
            }
        };
        let refused = Mutex::new(HashSet::new());
        assert!(allowed("https://files.example/a", &none_allowed(), resolve.clone(), &refused).await);
        assert!(allowed("https://files.example/b", &none_allowed(), resolve, &refused).await);
        assert_eq!(calls.load(Ordering::SeqCst), 2, "nothing is cached as allowed");
    }

    /// P15.7: a run that never ends (a DevTools command that never answers)
    /// is dropped at its deadline and answered by `late`. Paused time: instant.
    #[tokio::test(start_paused = true)]
    async fn a_run_that_never_ends_is_dropped_at_its_deadline() {
        let start = tokio::time::Instant::now();
        let deadline = start + Duration::from_secs(900);
        let work = std::future::pending::<Result<PathBuf, String>>();
        let got = tokio::time::timeout_at(deadline + Duration::from_secs(60), until(deadline, work, || Err("late".to_string())))
            .await
            .expect("the run must end at its deadline, not hang");
        assert_eq!(got, Err("late".to_string()));
        assert_eq!(start.elapsed(), Duration::from_secs(900), "ended exactly at the deadline");
    }

    #[tokio::test(start_paused = true)]
    async fn a_run_that_ends_in_time_keeps_its_result() {
        let start = tokio::time::Instant::now();
        let work = async {
            tokio::time::sleep(Duration::from_secs(10)).await;
            Ok::<_, String>(PathBuf::from("capture.mp4"))
        };
        let got = until(start + Duration::from_secs(900), work, || Err("late".to_string())).await;
        assert_eq!(got, Ok(PathBuf::from("capture.mp4")));
        assert_eq!(start.elapsed(), Duration::from_secs(10));
    }

    /// P15.7: a shutdown that never ends is given up after its grace.
    #[tokio::test(start_paused = true)]
    async fn a_shutdown_that_never_ends_is_given_up_after_its_grace() {
        let start = tokio::time::Instant::now();
        let ended = tokio::time::timeout(Duration::from_secs(60), bounded(SHUTDOWN, std::future::pending::<()>()))
            .await
            .expect("the shutdown must be given up, not hang");
        assert!(!ended);
        assert_eq!(start.elapsed(), SHUTDOWN);
        assert!(bounded(SHUTDOWN, async {}).await, "a shutdown that ends is waited for");
    }

    #[test]
    fn the_grace_covers_every_bounded_step() {
        assert!(SHUTDOWN_GRACE >= RUN_OVERRUN + SHUTDOWN + KILL);
        assert!(RUN_OVERRUN >= LOOKUP + PAGE_CLOSE);
    }

    #[test]
    fn the_reason_at_the_deadline_says_what_was_seen() {
        assert_eq!(Seen::default().reason_at_deadline(), "no download started");
        let began = Seen { began: true, refused_by_page: Some("a.example".into()), ..Seen::default() };
        assert_eq!(began.reason_at_deadline(), "the download did not finish in time");
        let both = Seen { began: false, refused_by_page: Some("page.example".into()), refused_by_proxy: Some("proxy.example".into()) };
        assert_eq!(both.reason_at_deadline(), "no download started; the page asked for an address that is not allowed (page.example)");
        let proxy = Seen { refused_by_proxy: Some("proxy.example".into()), ..Seen::default() };
        assert_eq!(proxy.reason_at_deadline(), "no download started; the page asked for an address that is not allowed (proxy.example)");
    }

    #[tokio::test]
    async fn websocket_and_other_schemes_are_refused_and_blank_pages_pass() {
        let resolve = |_h: String, port: u16| async move { Ok::<_, std::io::Error>(vec![std::net::SocketAddr::from(([93, 184, 216, 34], port))]) };
        let refused = Mutex::new(HashSet::new());
        assert!(!allowed("wss://files.example/socket", &none_allowed(), resolve.clone(), &refused).await);
        assert!(!allowed("ws://other.example/socket", &none_allowed(), resolve.clone(), &refused).await);
        assert!(allowed("about:blank", &none_allowed(), resolve.clone(), &refused).await);
        assert!(allowed("data:text/plain,hi", &none_allowed(), resolve, &refused).await);
    }
}
