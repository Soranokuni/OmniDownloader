use anyhow::{anyhow, Context, Result};
use chromiumoxide::browser::Browser;
use chromiumoxide::cdp::browser_protocol::network::{
    EventRequestWillBeSent, EventResponseReceived, GetResponseBodyParams, RequestId, ResourceType, SetBlockedUrLsParams,
};
use chromiumoxide::cdp::browser_protocol::browser::{SetDownloadBehaviorBehavior, SetDownloadBehaviorParams};
use chromiumoxide::cdp::browser_protocol::page::AddScriptToEvaluateOnNewDocumentParams;
use chromiumoxide::cdp::js_protocol::runtime::{CallArgument, CallFunctionOnParams, EvaluateParams, EventConsoleApiCalled};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::adblock::UnifiedAdBlocker;
use crate::agent::BrowserError;
use crate::browser::HeadlessBrowserManager;
use crate::evidence::{vast_candidate, vast_media_files, Diagnostic, NetworkEntry, NetworkLog, PageSnapshot, DIAG_JS, PAGE_FACTS_JS};
use crate::glomex;
use crate::recipe::LiveAction;

/// Click a cookie-consent button, if the page shows one. Fixed text.
const COOKIE_BYPASS_JS: &str = r#"
            (() => {
                const selectors = [
                    '#didomi-notice-agree-button',
                    '.didomi-components-button--color',
                    '.fc-cta-consent',
                    '#onetrust-accept-btn-handler',
                    '#onetrust-reject-all-handler',
                    '.ot-sdk-button',
                    'button[id*="accept"]',
                    'button[class*="accept"]',
                    'button[id*="agree"]',
                    'button[class*="agree"]',
                    'button[aria-label*="accept"]',
                    'button[aria-label*="agree"]',
                    '#CybotCookiebotDialogBodyLevelButtonLevelOptinAllowAll'
                ];
                for (const sel of selectors) {
                    const el = document.querySelector(sel);
                    if (el) {
                        try { el.click(); return sel; } catch (e) {}
                    }
                }
                // Search buttons by Greek or English text
                const buttons = Array.from(document.querySelectorAll('button, a'));
                for (const b of buttons) {
                    const txt = (b.textContent || '').trim().toUpperCase();
                    if (txt.includes('ΣΥΜΦΩΝΩ') || txt.includes('ΑΠΟΔΟΧΗ') || txt.includes('ACCEPT') || txt.includes('AGREE')) {
                        try { b.click(); return 'button-text:' + txt; } catch (e) {}
                    }
                }
                return null;
            })()
        "#;

/// Live recipe actions (P10.11c). Fixed text: the recipe's selector reaches the
/// page only as a call argument (`Runtime.callFunctionOn`), never inside this
/// script, so a selector cannot become code.
const CLICK_FN: &str = "function (css) { try { const el = document.querySelector(css); if (!el) return 'missing'; el.scrollIntoView({block: 'center'}); el.click(); return 'clicked'; } catch (e) { return 'error'; } }";
const SCROLL_FN: &str = "function (css) { try { const el = document.querySelector(css); if (!el) return 'missing'; el.scrollIntoView({block: 'center'}); return 'scrolled'; } catch (e) { return 'error'; } }";

/// The whole live run is cut here whatever the caller's budget.
const LIVE_RUN_CAP_SECS: u64 = 40;
/// A recipe has at most this many live steps, and a wait at most this long
/// (the recipe validator's limits, held again here).
const LIVE_MAX_ACTIONS: usize = 2;
const LIVE_MAX_WAIT_MS: u32 = 8000;

/// Media session metadata and extracted video streams.
/// Captures browser context (cookies, user-agent, referer) to forward to downstream
/// media tools (yt-dlp/FFmpeg) to prevent HTTP 403 Forbidden errors on origin-bound CDNs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredMedia {
    pub primary_stream: String,
    pub all_streams: Vec<String>,
    pub user_agent: String,
    pub referer: String,
    pub cookies: Option<String>,
}

/// A sniff's outcome together with what the browser saw (plan P10.1). The
/// snapshot's page facts are only read when no stream was found.
pub struct Sniffed {
    pub media: Result<DiscoveredMedia>,
    pub snapshot: PageSnapshot,
}

/// Shared between the diagnostic run's listeners and its caller, so a run
/// cut short by its budget still hands back what it saw.
#[derive(Default)]
struct DiagState {
    net_log: std::sync::Mutex<NetworkLog>,
    diag: std::sync::Mutex<Diagnostic>,
    xml: std::sync::Mutex<Vec<RequestId>>,
    facts: std::sync::Mutex<DiagFacts>,
    main_doc: std::sync::Mutex<MainDoc>,
}

/// The page's own document: the first document request, and how it was answered.
#[derive(Default)]
struct MainDoc {
    frame_id: Option<chromiumoxide::cdp::browser_protocol::page::FrameId>,
    status: Option<i64>,
}

#[derive(Default)]
struct DiagFacts {
    facts: Option<Value>,
    final_url: Option<String>,
}

/// StreamSniffer uses headless Chrome via CDP to navigate complex news websites,
/// bypass cookie consent banners, detect embedded platform players (Twitter/X, Glomex,
/// YouTube, Dailymotion, TikTok), filter out ad networks via UnifiedAdBlocker (HaGeZi + Greek),
/// trigger programmatic lazy hydration, and extract broadcast-grade video streams.
pub struct StreamSniffer;

/// An address as the log may show it: host and path, never the query or fragment
/// (a media address often carries a token there).
pub fn log_addr(url: &str) -> String {
    let full = if url.starts_with("//") { format!("https:{url}") } else { url.to_string() };
    match reqwest::Url::parse(&full) {
        Ok(u) => format!("{}{}", u.host_str().unwrap_or("?"), u.path()),
        Err(_) => "(not an address)".to_string(),
    }
}

impl StreamSniffer {
    /// Determines whether a URL belongs to known ad networks, trackers, or non-media assets.
    pub fn is_ad_or_tracking(url: &str) -> bool {
        UnifiedAdBlocker::global().is_blocked(url)
    }

    /// Evaluates candidate stream URL and assigns a priority score (0-100).
    /// Higher scores represent higher broadcast quality and reliability.
    pub fn score_stream_url(url: &str) -> u32 {
        if Self::is_ad_or_tracking(url) {
            return 0;
        }

        let lower = url.to_lowercase();
        let url_path = lower.split('?').next().unwrap_or("");

        // Highest priority: HLS (.m3u8) adaptive stream in URL path
        if url_path.contains(".m3u8") {
            return 100;
        }

        // MPEG-DASH (.mpd) stream in URL path
        if url_path.contains(".mpd") {
            return 90;
        }

        // Dedicated video CDNs
        if lower.contains("video.twimg.com") {
            return 85;
        }

        if lower.contains("glomex") && (url_path.contains(".mp4") || url_path.contains(".m3u8")) {
            return 80;
        }

        // Generic direct MP4 stream in URL path (lower priority to avoid interstitial ad video clips)
        if url_path.contains(".mp4") {
            return 40;
        }

        // Fallback for manifest in query parameters (only if not an analytics beacon)
        if lower.contains(".m3u8") {
            return 30;
        }

        0
    }

    /// Convenience wrapper returning only the highest quality stream URL.
    pub async fn extract_video_stream(target_url: &str, timeout_secs: u64) -> Result<String> {
        let media = Self::extract_media_bundle(target_url, timeout_secs).await?;
        Ok(media.primary_stream)
    }

    /// Launch the browser and find the video on a built-in test page (an X
    /// post as portals embed it; nothing is fetched from the network).
    /// Returns the browser's own version string ("Chrome/141.0.7390.55").
    ///
    /// This is the daily "does the browser still work?" check (plan P6.7):
    /// a Chrome/Edge update that the browser library no longer understands
    /// otherwise shows up as news-article jobs failing one by one.
    pub async fn self_test() -> Result<String> {
        const PAGE: &str = "data:text/html;charset=utf-8,%3C!doctype%20html%3E%3Cmeta%20charset%3Dutf-8%3E\
            %3Ch1%3EOmniDownloader%20self-test%3C%2Fh1%3E%3Cblockquote%20class%3D%22twitter-tweet%22%3E\
            %3Ca%20href%3D%22https%3A%2F%2Fx.com%2FSelfTest%2Fstatus%2F1900000000000000042%22%3Epost%3C%2Fa%3E\
            %3C%2Fblockquote%3E";
        const EXPECTED: &str = "https://x.com/SelfTest/status/1900000000000000042";

        let mut session = HeadlessBrowserManager::launch().await?;
        let version = session
            .browser
            .version()
            .await
            .map(|v| v.product.replace("HeadlessChrome", "Chrome"))
            .unwrap_or_else(|_| "unknown version".into());
        let result = Self::sniff(&session.browser, PAGE, 10).await.media;
        session.shutdown().await;
        let media = result.context("the browser could not read its test page")?;
        if !media.all_streams.iter().any(|u| u == EXPECTED) {
            anyhow::bail!("the browser opened its test page but did not find the video on it");
        }
        Ok(version)
    }

    /// Full media extraction pipeline returning primary stream, all discovered streams,
    /// active cookies, User-Agent, and Referer.
    pub async fn extract_media_bundle(target_url: &str, timeout_secs: u64) -> Result<DiscoveredMedia> {
        Self::extract_with_evidence(target_url, timeout_secs).await.media
    }

    /// The same extraction, also returning what the browser saw.
    pub async fn extract_with_evidence(target_url: &str, timeout_secs: u64) -> Sniffed {
        info!("StreamSniffer: Starting browser extraction for {}", log_addr(target_url));

        let mut session = match HeadlessBrowserManager::launch().await {
            Ok(s) => s,
            Err(e) => return Sniffed { media: Err(e), snapshot: PageSnapshot::new(target_url) },
        };
        let result = Self::sniff(&session.browser, target_url, timeout_secs).await;
        session.shutdown().await;
        result
    }

    /// A second, instrumented look at a page the normal sniff got nothing
    /// from (plan P10.2): the page is opened with recorders for Media Source
    /// and DRM requests in place before its own scripts run, every player is
    /// scrolled into view and started muted, and what the page then shows is
    /// read. Never used by the normal sniff; the page is only observed.
    ///
    /// Bounded by `budget_secs`; on a timeout or a launch failure it returns
    /// what it has (possibly nothing) rather than an error.
    pub async fn diagnose(target_url: &str, budget_secs: u64) -> (PageSnapshot, Diagnostic) {
        Self::diagnose_with(HeadlessBrowserManager::launch().await, target_url, budget_secs, &[]).await
    }

    /// `diagnose` that also performs a recipe's live actions (click, wait,
    /// scroll) after the normal settle, on this one page load (P10.11c).
    /// Downloads are denied for the run. The result is `complete` only when
    /// every click and scroll found its element and the page stayed the page
    /// asked for. Cut at `min(budget_secs, 40)` seconds.
    pub async fn diagnose_live(target_url: &str, budget_secs: u64, actions: &[LiveAction]) -> (PageSnapshot, Diagnostic) {
        Self::diagnose_with(HeadlessBrowserManager::launch().await, target_url, budget_secs, actions).await
    }

    /// `diagnose` on an already attempted launch (a failed one is a run that
    /// saw nothing, never a clean page).
    async fn diagnose_with(
        launched: Result<crate::browser::BrowserSession>,
        target_url: &str,
        budget_secs: u64,
        actions: &[LiveAction],
    ) -> (PageSnapshot, Diagnostic) {
        let host = reqwest::Url::parse(target_url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
        let mut snapshot = PageSnapshot::new(target_url);
        let mut session = match launched {
            Ok(s) => s,
            Err(e) => {
                warn!("StreamSniffer: diagnostic run for {host} could not start the browser: {e:#}");
                return (snapshot, Diagnostic::default());
            }
        };
        let state = Arc::new(DiagState::default());
        let budget_secs = if actions.is_empty() { budget_secs } else { budget_secs.min(LIVE_RUN_CAP_SECS) };
        let run = Self::diagnose_page(&session.browser, target_url, &state, actions);
        if tokio::time::timeout(Duration::from_secs(budget_secs), run).await.is_err() {
            warn!("StreamSniffer: diagnostic run for {host} hit its {budget_secs} s budget");
        }
        session.shutdown().await;

        if let Ok(log) = state.net_log.lock() {
            snapshot.network = log.entries();
        }
        if let Ok(mut f) = state.facts.lock() {
            snapshot.final_url = f.final_url.take();
            if let Some(v) = f.facts.take() {
                snapshot.apply_page_facts(&v);
            }
        }
        let mut diag = state.diag.lock().map(|d| d.clone()).unwrap_or_default();
        for a in &diag.ad_media {
            snapshot.push_ad_media(a);
        }
        let main_ok = state.main_doc.lock().is_ok_and(|m| m.status.is_some_and(|s| (200..300).contains(&s)));
        // A page that ended up somewhere else (a login, a consent wall) was
        // not the one asked for.
        diag.complete = crate::evidence::run_was_complete(main_ok, &snapshot, target_url);
        if !actions.is_empty() {
            diag.live_page_ok = diag.complete;
            diag.complete = diag.complete && live_outcomes_ok(actions, &diag.live_actions);
        }
        (snapshot, diag)
    }

    async fn diagnose_page(browser: &Browser, target_url: &str, state: &Arc<DiagState>, actions: &[LiveAction]) {
        let page = match browser.new_page("about:blank").await {
            Ok(p) => p,
            Err(e) => return warn!("StreamSniffer: diagnostic run could not open a page: {e}"),
        };
        if let Ok(params) = SetBlockedUrLsParams::builder().urls(UnifiedAdBlocker::global().generate_cdp_blocked_patterns()).build() {
            let _ = page.execute(params).await;
        }

        if let Ok(mut events) = page.event_listener::<EventRequestWillBeSent>().await {
            let st = state.clone();
            tokio::spawn(async move {
                while let Some(event) = events.next().await {
                    if let Ok(mut log) = st.net_log.lock() {
                        log.on_request(
                            event.request_id.inner(),
                            NetworkEntry {
                                url: event.request.url.clone(),
                                method: Some(event.request.method.clone()),
                                status: None,
                                mime: None,
                                resource_type: event.r#type.as_ref().map(|t| format!("{:?}", t)),
                                frame_id: event.frame_id.as_ref().map(|f| f.inner().clone()),
                            },
                        );
                    }
                }
            });
        }
        if let Ok(mut events) = page.event_listener::<EventResponseReceived>().await {
            let st = state.clone();
            tokio::spawn(async move {
                while let Some(event) = events.next().await {
                    let mime = event.response.mime_type.clone();
                    if let Ok(mut log) = st.net_log.lock() {
                        log.on_response(
                            event.request_id.inner(),
                            event.response.status,
                            Some(mime.clone()),
                            Some(format!("{:?}", event.r#type)),
                        );
                    }
                    // The first document answered in the page's main frame.
                    if matches!(event.r#type, ResourceType::Document) {
                        if let Ok(mut m) = st.main_doc.lock() {
                            let ours = m.frame_id.is_none() || m.frame_id.as_ref() == event.frame_id.as_ref();
                            if m.status.is_none() && ours {
                                m.status = Some(event.response.status);
                            }
                        }
                    }
                    if vast_candidate(&mime, &format!("{:?}", event.r#type)) {
                        if let Ok(mut x) = st.xml.lock() {
                            if x.len() < 16 {
                                x.push(event.request_id.clone());
                            }
                        }
                    }
                }
            });
        }
        if let Ok(mut events) = page.event_listener::<EventConsoleApiCalled>().await {
            let st = state.clone();
            tokio::spawn(async move {
                while let Some(event) = events.next().await {
                    let first = event.args.first().and_then(|a| a.value.as_ref()).and_then(Value::as_str);
                    if first != Some("__omni_diag") {
                        continue;
                    }
                    if let Some(rec) = event.args.get(1).and_then(|a| a.value.as_ref()).and_then(Value::as_str) {
                        if let Ok(mut d) = st.diag.lock() {
                            d.record(rec);
                        }
                    }
                }
            });
        }

        if let Err(e) = page.execute(AddScriptToEvaluateOnNewDocumentParams::new(DIAG_JS)).await {
            warn!("StreamSniffer: diagnostic recorders could not be installed: {e}");
        }

        let main_frame = page.mainframe().await.ok().flatten();
        if let Ok(mut m) = state.main_doc.lock() {
            m.frame_id = main_frame;
        }

        if !actions.is_empty() {
            // A click must never save a file onto this machine.
            let denied = match SetDownloadBehaviorParams::builder().behavior(SetDownloadBehaviorBehavior::Deny).build() {
                Ok(p) => browser.execute(p).await.is_ok(),
                Err(_) => false,
            };
            if !denied {
                warn!("StreamSniffer: live actions refused, downloads could not be denied");
                if let Ok(mut d) = state.diag.lock() {
                    d.live_actions.push("refused".to_string());
                }
                return;
            }
        }

        let started = std::time::Instant::now();
        let settle = Duration::from_secs(15);
        // The load may never finish on a page with a live player; the wait
        // below is what bounds it.
        let _ = tokio::time::timeout(settle, page.goto(target_url)).await;
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let _ = page.evaluate(COOKIE_BYPASS_JS).await;

        let scroll_js = r#"
            (async () => {
                try {
                    const els = Array.from(document.querySelectorAll('video, iframe, [class*=player]')).slice(0, 15);
                    for (const el of els) {
                        try { el.scrollIntoView({ block: 'center' }); } catch (e) {}
                        await new Promise(r => setTimeout(r, 300));
                    }
                    return els.length;
                } catch (e) { return 0; }
            })()
        "#;
        let _ = page.evaluate(scroll_js).await;
        let play_js = r#"
            (() => {
                for (const v of Array.from(document.querySelectorAll('video'))) {
                    try {
                        v.muted = true;
                        const p = v.play();
                        if (p && p.catch) p.catch(() => {});
                    } catch (e) {}
                }
            })()
        "#;
        let _ = page.evaluate(play_js).await;

        if let Some(rest) = settle.checked_sub(started.elapsed()) {
            tokio::time::sleep(rest).await;
        }

        if !actions.is_empty() {
            // A fixed read, no selector involved: is the page still loading?
            let ready = tokio::time::timeout(Duration::from_secs(3), page.evaluate("document.readyState")).await;
            let ready = ready.ok().and_then(|r| r.ok()).and_then(|e| e.value().and_then(Value::as_str).map(str::to_string));
            if let Ok(mut d) = state.diag.lock() {
                d.live_ready_before = ready.as_deref().is_some_and(|s| s != "loading");
            }
            Self::run_live_actions(&page, actions, state).await;
            // What the actions started needs a moment to show.
            tokio::time::sleep(Duration::from_secs(3)).await;
        }

        if let Ok(Ok(eval)) = tokio::time::timeout(Duration::from_secs(3), page.evaluate(PAGE_FACTS_JS)).await {
            if let Ok(mut f) = state.facts.lock() {
                f.facts = eval.value().cloned();
            }
        }
        let final_url = page.url().await.ok().flatten();
        if let Ok(mut f) = state.facts.lock() {
            f.final_url = final_url;
        }

        // Ad servers' VAST replies name the ad's own media files, which must
        // not be mistaken for the page's video. Best effort.
        let ids: Vec<_> = state.xml.lock().map(|x| x.clone()).unwrap_or_default();
        for id in ids {
            let body = tokio::time::timeout(Duration::from_secs(2), page.execute(GetResponseBodyParams::new(id))).await;
            if let Ok(Ok(resp)) = body {
                if !resp.result.base64_encoded && resp.result.body.len() <= 65536 {
                    let files = vast_media_files(&resp.result.body);
                    if let Ok(mut d) = state.diag.lock() {
                        for f in files {
                            if d.ad_media.len() < 64 && !d.ad_media.contains(&f) {
                                d.ad_media.push(f);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Run a recipe's live actions in order, recording one outcome each.
    /// Selectors go to the page only as call arguments to the fixed functions.
    async fn run_live_actions(page: &chromiumoxide::Page, actions: &[LiveAction], state: &Arc<DiagState>) {
        let note = |s: &str| {
            if let Ok(mut d) = state.diag.lock() {
                d.live_actions.push(s.to_string());
            }
        };
        for (i, action) in actions.iter().enumerate() {
            if i >= LIVE_MAX_ACTIONS {
                note("skipped");
                continue;
            }
            let outcome = match action {
                LiveAction::Wait(ms) => {
                    tokio::time::sleep(Duration::from_millis((*ms).min(LIVE_MAX_WAIT_MS) as u64)).await;
                    "waited".to_string()
                }
                LiveAction::Click(css) => Self::call_on_document(page, CLICK_FN, css).await,
                LiveAction::ScrollTo(css) => Self::call_on_document(page, SCROLL_FN, css).await,
            };
            debug!(step = i, outcome = %outcome, "live action");
            note(&outcome);
        }
    }

    /// Call one of the fixed functions with `css` as its only argument, on the
    /// page's current `document` object. Returns the function's answer, or
    /// "error" / "timeout" when the call itself failed.
    async fn call_on_document(page: &chromiumoxide::Page, function: &'static str, css: &str) -> String {
        let call = async {
            let doc = EvaluateParams::builder().expression("document").build().ok()?;
            let doc = page.execute(doc).await.ok()?;
            let object_id = doc.result.result.object_id.clone()?;
            let arg = CallArgument::builder().value(Value::String(css.to_string())).build();
            let params = CallFunctionOnParams::builder()
                .function_declaration(function)
                .object_id(object_id)
                .argument(arg)
                .return_by_value(true)
                .user_gesture(true)
                .build()
                .ok()?;
            let out = page.execute(params).await.ok()?;
            out.result.result.value.as_ref().and_then(Value::as_str).map(str::to_string)
        };
        match tokio::time::timeout(Duration::from_secs(3), call).await {
            Ok(Some(s)) if matches!(s.as_str(), "clicked" | "scrolled" | "missing" | "error") => s,
            Ok(_) => "error".to_string(),
            Err(_) => "timeout".to_string(),
        }
    }

    async fn sniff(browser: &Browser, target_url: &str, timeout_secs: u64) -> Sniffed {
        let mut snapshot = PageSnapshot::new(target_url);
        let media = Self::sniff_page(browser, target_url, timeout_secs, &mut snapshot).await;
        Sniffed { media, snapshot }
    }

    async fn sniff_page(
        browser: &Browser,
        target_url: &str,
        timeout_secs: u64,
        snapshot: &mut PageSnapshot,
    ) -> Result<DiscoveredMedia> {
        let page = browser
            .new_page(target_url)
            .await
            .map_err(|e| anyhow!("Failed opening page: {}", e))?;

        // 1. Injected CDP Hardware Ad-Blocking: Instruct Chrome to drop ad sockets immediately
        let blocker = UnifiedAdBlocker::global();
        let blocked_patterns = blocker.generate_cdp_blocked_patterns();
        if let Ok(params) = SetBlockedUrLsParams::builder().urls(blocked_patterns).build() {
            let _ = page.execute(params).await;
        }

        // Thread-safe list of discovered stream candidates: (score, url)
        let candidates = Arc::new(Mutex::new(Vec::<(u32, String)>::new()));
        let candidates_clone = candidates.clone();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_flag_clone = stop_flag.clone();

        // 2. Register CDP Network request listener
        let mut request_events = page
            .event_listener::<EventRequestWillBeSent>()
            .await
            .map_err(|e| anyhow!("Failed registering network request listener: {}", e))?;

        // Passive record of what the page requested and got back (P10.1).
        let net_log = Arc::new(std::sync::Mutex::new(NetworkLog::default()));
        let net_log_req = net_log.clone();
        let stop_flag_resp = stop_flag.clone();
        if let Ok(mut response_events) = page.event_listener::<EventResponseReceived>().await {
            let net_log_resp = net_log.clone();
            tokio::spawn(async move {
                while let Some(event) = response_events.next().await {
                    if stop_flag_resp.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Ok(mut log) = net_log_resp.lock() {
                        log.on_response(
                            event.request_id.inner(),
                            event.response.status,
                            Some(event.response.mime_type.clone()),
                            Some(format!("{:?}", event.r#type)),
                        );
                    }
                }
            });
        }

        tokio::spawn(async move {
            while let Some(event) = request_events.next().await {
                if stop_flag_clone.load(Ordering::Relaxed) {
                    break;
                }
                if let Ok(mut log) = net_log_req.lock() {
                    log.on_request(
                        event.request_id.inner(),
                        NetworkEntry {
                            url: event.request.url.clone(),
                            method: Some(event.request.method.clone()),
                            status: None,
                            mime: None,
                            resource_type: event.r#type.as_ref().map(|t| format!("{:?}", t)),
                            frame_id: event.frame_id.as_ref().map(|f| f.inner().clone()),
                        },
                    );
                }
                let req_url = event.request.url.clone();
                let score = Self::score_stream_url(&req_url);

                if score > 0 {
                    info!(
                        "StreamSniffer: Discovered candidate (score: {}): {}",
                        score, log_addr(&req_url)
                    );
                    let mut list = candidates_clone.lock().await;
                    if !list.iter().any(|(_, u)| u == &req_url) {
                        list.push((score, req_url));
                    }
                }
            }
        });

        // 3. Initial wait for page scripts and DOM initialization
        tokio::time::sleep(Duration::from_millis(1500)).await;

        // 4. Bypass Cookie Consent (OneTrust, Didomi, Quantcast, Google Funding Choices, Greek portals)
        let cookie_bypass_js = COOKIE_BYPASS_JS;
        let _ = page.evaluate(cookie_bypass_js).await;

        // 5. Programmatic Lazy-Scroll (Operational Protocol from PDF Benchmark)
        // Triggers IntersectionObserver callbacks down long-form editorial articles (Iefimerida, Gazzetta, News247)
        let lazy_scroll_js = r#"
            (async () => {
                try {
                    const totalHeight = Math.min(document.body.scrollHeight, 4000);
                    const step = 800;
                    for (let y = 0; y < totalHeight; y += step) {
                        window.scrollTo(0, y);
                        await new Promise(r => setTimeout(r, 200));
                    }
                    window.scrollTo(0, 0);
                    return true;
                } catch(e) {
                    return false;
                }
            })()
        "#;
        let _ = page.evaluate(lazy_scroll_js).await;

        // 6. Inspect DOM for Embedded Media Platforms (Twitter/X, Glomex, YouTube, Dailymotion, TikTok, Inline Script HLS)
        let dom_embed_js = r#"
            (() => {
                const found = [];

                // Helper to get effective src from an element (handles data-src, data-lazy-src from WP Rocket, etc.)
                const getSrc = (el) => el.src || el.getAttribute('src') || el.getAttribute('data-src') || el.getAttribute('data-lazy-src') || '';

                // 1. Twitter / X tweet embeds
                const tweetNodes = Array.from(document.querySelectorAll(
                    'blockquote.twitter-tweet a, .twitter-tweet a, iframe[src*="twitter.com"], iframe[src*="x.com"], div[data-tweet-id], a[href*="/status/"]'
                ));
                // Every post, not just the first: an article routinely embeds
                // several (a newsbomb.gr story had three X videos and only the
                // first was ever seen). Keyed on the status id so the same
                // post found as a blockquote link and as a rendered iframe
                // counts once.
                const tweetIds = new Set();
                const addTweet = (id, url) => {
                    if (!tweetIds.has(id)) { tweetIds.add(id); found.push({ type: 'twitter', url }); }
                };
                for (const el of tweetNodes) {
                    const href = el.href || getSrc(el) || el.getAttribute('href') || '';
                    const m = href.match(/(https?:\/\/(?:twitter|x)\.com\/(?:#!\/)?[a-zA-Z0-9_]+\/status\/(\d+))/i);
                    if (m) { addTweet(m[2], m[1].replace(/\/\/twitter\.com\//i, '//x.com/')); continue; }
                    const idMatch = href.match(/id=(\d{15,})/);
                    if (idMatch) { addTweet(idMatch[1], 'https://x.com/i/status/' + idMatch[1]); continue; }
                    const dataId = el.getAttribute('data-tweet-id');
                    if (dataId) { addTweet(dataId, 'https://x.com/i/status/' + dataId); }
                }

                // 2. Glomex player embeds (web components, divs, iframes)
                const glomexNodes = Array.from(document.querySelectorAll(
                    'glomex-integration, glomex-player, .glomex-player, [data-integration-id], [integration-id], [data-playlist-id], [playlist-id]'
                ));
                for (const d of glomexNodes) {
                    const pId = d.getAttribute('playlist-id') || d.getAttribute('data-playlist-id');
                    const iId = d.getAttribute('integration-id') || d.getAttribute('data-integration-id');
                    if (pId && iId) {
                        found.push({ type: 'glomex', url: `https://player.glomex.com/integration/1/iframe-player.html?integrationId=${iId}&playlistId=${pId}` });
                    } else if (pId) {
                        found.push({ type: 'glomex', url: `https://player.glomex.com/integration/1/iframe-player.html?playlistId=${pId}` });
                    }
                }
                const glomexIframes = Array.from(document.querySelectorAll('iframe[src*="glomex.com"], iframe[data-src*="glomex.com"], iframe[data-lazy-src*="glomex.com"]'));
                for (const f of glomexIframes) {
                    const src = getSrc(f);
                    if (src) found.push({ type: 'glomex', url: src });
                }

                // 3. JWPlayer / JWPlatform embeds (Proto Thema, Antenna, Star, etc.)
                const jwIframes = Array.from(document.querySelectorAll(
                    'iframe[src*="jwplayer.com"], iframe[src*="jwplatform.com"], iframe[data-src*="jwplayer.com"], iframe[data-lazy-src*="jwplayer.com"]'
                ));
                for (const j of jwIframes) {
                    const src = getSrc(j);
                    if (src) found.push({ type: 'jwplayer', url: src });
                }
                const jwDivs = Array.from(document.querySelectorAll('[data-media-id], [data-jwplayer-id]'));
                for (const jd of jwDivs) {
                    const mId = jd.getAttribute('data-media-id') || jd.getAttribute('data-jwplayer-id');
                    if (mId) found.push({ type: 'jwplayer', url: `https://cdn.jwplayer.com/players/${mId}.html` });
                }

                // 4. ERT WebTV / ERTFLIX player embeds
                const ertIframes = Array.from(document.querySelectorAll(
                    'iframe[src*="ert.gr/webtv"], iframe[src*="ertflix.gr"], iframe[data-src*="ert.gr/webtv"], iframe[data-lazy-src*="ert.gr/webtv"]'
                ));
                for (const e of ertIframes) {
                    const src = getSrc(e);
                    const m = src.match(/[?&]f=([^&]+)/);
                    if (m) {
                        const relPath = decodeURIComponent(m[1]).replace(/^\/+/, '');
                        found.push({
                            type: 'ert_hls',
                            url: `https://mediastream.ert.gr/vodedge/_definst_/mp4:dvrorigin/${relPath}/playlist.m3u8`
                        });
                    }
                }

                // 5. Brightcove / OVP embeds (The Guardian, CNN)
                const bcIframes = Array.from(document.querySelectorAll(
                    'iframe[src*="players.brightcove.net"], iframe[src*="brightcove.com"], iframe[data-src*="players.brightcove.net"], iframe[data-lazy-src*="players.brightcove.net"]'
                ));
                for (const b of bcIframes) {
                    const src = getSrc(b);
                    if (src) found.push({ type: 'brightcove', url: src });
                }

                // 6. YouTube embeds (standard iframes, WP Rocket lazyload, ProtoThema custom plugins, explicit article links)
                // Only a real YouTube address yields an id: the old pattern took
                // any 11 characters after a "/", and read "ServiceLogi" out of a
                // sign-in link (2026-10-06).
                const ytId = (value) => {
                    const m = String(value || '').match(
                        /(?:youtube(?:-nocookie)?\.com\/(?:embed\/|shorts\/|live\/|v\/|watch\?(?:[^#]*&)?v=)|youtu\.be\/)([a-zA-Z0-9_-]{11})(?![a-zA-Z0-9_-])/i
                    );
                    return m ? m[1] : null;
                };
                const ytNodes = Array.from(document.querySelectorAll(
                    'iframe[src*="youtube.com"], iframe[src*="youtube-nocookie.com"], iframe[src*="youtu.be"],' +
                    'iframe[data-src*="youtube.com"], iframe[data-lazy-src*="youtube.com"],' +
                    '.rll-youtube-player, [data-plugin-youtube]'
                ));
                for (const y of ytNodes) {
                    let id = ytId(getSrc(y));
                    // WP Rocket's lazy player carries the bare id.
                    const dataId = y.getAttribute('data-id') || '';
                    if (!id && y.classList.contains('rll-youtube-player') && /^[a-zA-Z0-9_-]{11}$/.test(dataId)) id = dataId;
                    if (id) found.push({ type: 'youtube', url: 'https://www.youtube.com/watch?v=' + id });
                    const pluginAttr = y.getAttribute('data-plugin-youtube') || '';
                    const pm = pluginAttr.match(/"ID":\s*"([a-zA-Z0-9_-]{11})"/);
                    if (pm) {
                        found.push({ type: 'youtube', url: 'https://www.youtube.com/watch?v=' + pm[1] });
                    }
                }

                const ytLinks = Array.from(document.querySelectorAll('a[href*="youtube.com/watch"], a[href*="youtu.be/"]'));
                for (const a of ytLinks) {
                    const id = ytId(a.href);
                    if (id) {
                        found.push({ type: 'youtube', url: 'https://www.youtube.com/watch?v=' + id });
                        break;
                    }
                }

                // 7. Facebook video & reel embeds (ERT News, Iefimerida, Protothema)
                const fbIframes = Array.from(document.querySelectorAll(
                    'iframe[src*="facebook.com/plugins/video"], iframe[data-src*="facebook.com/plugins/video"], iframe[data-lazy-src*="facebook.com/plugins/video"]'
                ));
                for (const f of fbIframes) {
                    const src = getSrc(f);
                    const m = src.match(/[?&]href=([^&]+)/);
                    if (m) {
                        const directUrl = decodeURIComponent(m[1]);
                        found.push({ type: 'facebook', url: directUrl });
                    }
                }

                // 8. Dailymotion embeds
                const dmIframes = Array.from(document.querySelectorAll(
                    'iframe[src*="dailymotion.com"], iframe[data-src*="dailymotion.com"], iframe[data-lazy-src*="dailymotion.com"]'
                ));
                for (const d of dmIframes) {
                    const src = getSrc(d);
                    const m = src.match(/embed\/video\/([a-zA-Z0-9]+)/);
                    if (m) found.push({ type: 'dailymotion', url: 'https://www.dailymotion.com/video/' + m[1] });
                }

                // 9. Vimeo embeds
                const vimIframes = Array.from(document.querySelectorAll(
                    'iframe[src*="player.vimeo.com"], iframe[data-src*="player.vimeo.com"], iframe[data-lazy-src*="player.vimeo.com"]'
                ));
                for (const v of vimIframes) {
                    const src = getSrc(v);
                    if (src) found.push({ type: 'vimeo', url: src });
                }

                // 10. TikTok embeds
                const ttEmbeds = Array.from(document.querySelectorAll('blockquote.tiktok-embed'));
                for (const t of ttEmbeds) {
                    if (t.getAttribute('cite')) found.push({ type: 'tiktok', url: t.getAttribute('cite') });
                }

                // 10b. Instagram posts: the official blockquote, or the
                // embed iframe once it has loaded.
                const igCanon = (u) => {
                    const m = (u || '').match(/instagram\.com\/(p|reels?|tv)\/([A-Za-z0-9_-]+)/i);
                    if (!m) return null;
                    const kind = m[1].toLowerCase() === 'p' ? 'p' : (m[1].toLowerCase() === 'tv' ? 'tv' : 'reel');
                    return `https://www.instagram.com/${kind}/${m[2]}/`;
                };
                const igNodes = Array.from(document.querySelectorAll(
                    'blockquote.instagram-media, [data-instgrm-permalink], iframe[src*="instagram.com/"], iframe[data-src*="instagram.com/"]'
                ));
                for (const n of igNodes) {
                    const u = igCanon(n.getAttribute('data-instgrm-permalink') || getSrc(n) || (n.querySelector('a') || {}).href);
                    if (u) found.push({ type: 'instagram', url: u });
                }

                // 10c. Streamable players.
                for (const f of Array.from(document.querySelectorAll('iframe[src*="streamable.com"], iframe[data-src*="streamable.com"], iframe[data-lazy-src*="streamable.com"]'))) {
                    const m = getSrc(f).match(/streamable\.com\/(?:e|o|s)\/([A-Za-z0-9]+)/i);
                    if (m) found.push({ type: 'streamable', url: 'https://streamable.com/' + m[1] });
                }

                // 10d. A portal's own oEmbed proxy (iefimerida.gr:
                // <iframe data-src="/oembed?url=https%3A%2F%2Fwww.instagram.com%2Freel%2F…">),
                // lazy, so the post inside never loads in a headless visit.
                for (const f of Array.from(document.querySelectorAll('iframe[src*="oembed"], iframe[data-src*="oembed"], iframe[data-lazy-src*="oembed"]'))) {
                    let inner = null;
                    try { inner = new URL(getSrc(f), location.href).searchParams.get('url'); } catch (e) {}
                    if (!inner || !/^https?:\/\//i.test(inner)) continue;
                    const ig = igCanon(inner);
                    found.push({ type: 'oembed', url: ig || inner.replace(/[?&]utm_[^&#]*/g, '') });
                }

                // 11. Direct HTML5 <video> and <source> elements (In.gr, Star.gr, ERT)
                const videoElements = Array.from(document.querySelectorAll('video, video source, source'));
                for (const el of videoElements) {
                    const src = getSrc(el);
                    if (src && src.startsWith('http')) {
                        found.push({ type: 'source_src', url: src });
                    }
                }

                // 12. Full-page markup scan for .m3u8 and .mpd manifests (KwikMotion, JWPlayer, Plyr, VideoJS)
                try {
                    const html = document.documentElement.innerHTML || '';
                    const manifestMatches = html.match(/https?:\/\/[^"'<>\s\\]+\.(?:m3u8|mpd)[^"'<>\s\\]*/gi) || [];
                    for (const m of manifestMatches) {
                        const cleanUrl = m.replace(/&amp;/g, '&');
                        found.push({ type: 'script_hls', url: cleanUrl });
                    }
                } catch(e) {}

                return found;
            })()
        "#;

        // Where the page really is (after redirects): what a contextual glomex
        // player is matched against.
        let page_url = page.url().await.ok().flatten().unwrap_or_else(|| target_url.to_string());
        let mut handled: HashSet<String> = HashSet::new();
        match page.evaluate(dom_embed_js).await {
            Ok(eval) => {
                if let Some(arr) = eval.value().and_then(Value::as_array) {
                    info!("StreamSniffer: DOM embed inspection found {} raw elements", arr.len());
                    Self::add_dom_items(arr, &candidates, &mut handled, &page_url).await;
                }
            }
            Err(e) => {
                warn!("StreamSniffer: DOM embed script evaluation failed: {}", e);
            }
        }

        // 7. Trigger play on HTML5 video elements to induce stream requests
        let trigger_play_js = r#"
            (() => {
                const videos = document.querySelectorAll('video');
                videos.forEach(v => {
                    v.muted = true;
                    try { v.play(); } catch(e) {}
                });
            })()
        "#;
        let _ = page.evaluate(trigger_play_js).await;

        // 8. Poll candidates until timeout or confident high-priority stream is found
        let start = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);
        let mut last_dom_poll = std::time::Instant::now();

        while start.elapsed() < timeout {
            {
                let list = candidates.lock().await;
                if let Some((best_score, best_url)) = list.iter().max_by_key(|(s, _)| *s) {
                    if *best_score >= 95 && start.elapsed() > Duration::from_millis(3000) {
                        stop_flag.store(true, Ordering::Relaxed);
                        info!(
                            "StreamSniffer: Selected high-confidence stream (score: {}): {}",
                            best_score, log_addr(best_url)
                        );
                        break;
                    }
                }
            }

            // Periodically re-evaluate DOM embeds if no high-score candidate is found yet.
            // Handles delayed frame hydration, consent dismissals, and execution context recreation.
            if last_dom_poll.elapsed() >= Duration::from_millis(1500) {
                last_dom_poll = std::time::Instant::now();
                if let Ok(eval) = page.evaluate(dom_embed_js).await {
                    if let Some(arr) = eval.value().and_then(Value::as_array) {
                        Self::add_dom_items(arr, &candidates, &mut handled, &page_url).await;
                    }
                }
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        stop_flag.store(true, Ordering::Relaxed);

        // 9. Extract Browser Session Context (User-Agent, Referer, Cookies)
        let user_agent = page
            .evaluate("navigator.userAgent")
            .await
            .ok()
            .and_then(|v| v.into_value::<String>().ok())
            .unwrap_or_else(|| {
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36".to_string()
            });

        let cookies = page.get_cookies().await.ok().and_then(|c_list| {
            if c_list.is_empty() {
                None
            } else {
                Some(
                    c_list
                        .into_iter()
                        .map(|c| format!("{}={}", c.name, c.value))
                        .collect::<Vec<_>>()
                        .join("; "),
                )
            }
        });

        // 10. Compile all streams and primary stream
        let sorted_candidates = final_candidates(candidates.lock().await.clone());

        // What the browser saw goes in whether or not a stream was found: a
        // found stream can still fail downstream (HTTP 403).
        if let Ok(log) = net_log.lock() {
            snapshot.network = log.entries();
        }
        snapshot.final_url = Some(page_url.clone());

        if let Some((score, best_url)) = sorted_candidates.first() {
            info!(
                "StreamSniffer: Discovered {} media streams total. Selected primary (score: {}): {}",
                sorted_candidates.len(),
                score,
                log_addr(best_url)
            );

            let all_urls = sorted_candidates.iter().map(|(_, u)| u.clone()).collect();

            Ok(DiscoveredMedia {
                primary_stream: best_url.clone(),
                all_streams: all_urls,
                user_agent,
                referer: target_url.to_string(),
                cookies,
            })
        } else {
            // Nothing found: one read-only look at the page for the facts (1 s at most).
            // Never changes the outcome.
            match tokio::time::timeout(Duration::from_secs(1), page.evaluate(PAGE_FACTS_JS)).await {
                Ok(Ok(eval)) => {
                    if let Some(v) = eval.value() {
                        snapshot.apply_page_facts(v);
                    }
                }
                Ok(Err(e)) => warn!("StreamSniffer: page facts could not be read: {}", e),
                Err(_) => warn!("StreamSniffer: page facts read timed out"),
            }
            Err(BrowserError::NoStreamFound(log_addr(target_url)).into())
        }
    }

    /// One DOM inspection's findings into the candidate list. `handled` holds
    /// the URLs already looked at, so a glomex player is resolved once, not on
    /// every re-poll.
    async fn add_dom_items(
        items: &[Value],
        candidates: &Mutex<Vec<(u32, String)>>,
        handled: &mut HashSet<String>,
        page_url: &str,
    ) {
        for obj in items.iter().filter_map(Value::as_object) {
            let Some(raw) = obj.get("url").and_then(Value::as_str) else {
                continue;
            };
            if !handled.insert(raw.to_string()) {
                continue;
            }
            let embed_type = obj.get("type").and_then(Value::as_str).unwrap_or("embed");

            let (score, url) = if let Some(embed) = glomex::parse_embed(raw) {
                match glomex::resolve(&embed, page_url).await {
                    Ok(glomex::Resolved::Embed(u)) => (95, u),
                    Ok(glomex::Resolved::Empty) => {
                        info!("StreamSniffer: glomex player {} has no video for this page; ignored", log_addr(raw));
                        continue;
                    }
                    Err(e) => {
                        warn!("StreamSniffer: could not ask glomex which video {} plays: {e:#}", log_addr(raw));
                        continue;
                    }
                }
            } else if embed_type == "script_hls" || embed_type == "ert_hls" {
                (100, raw.to_string())
            } else if embed_type == "video_src" || embed_type == "source_src" {
                (Self::score_stream_url(raw), raw.to_string())
            } else {
                // Embedded platform URL (Twitter, JWPlayer, YouTube, Dailymotion, TikTok, Brightcove, Vimeo)
                (95, raw.to_string())
            };

            if score > 0 {
                let mut list = candidates.lock().await;
                if !list.iter().any(|(_, u)| *u == url) {
                    info!("StreamSniffer: Identified DOM embedded media [{embed_type}] (score: {score}): {}", log_addr(&url));
                    list.push((score, url));
                }
            }
        }
    }
}

/// What a live run can say about the recipe that asked for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveRun {
    /// Every action did its job on the page asked for.
    Complete,
    /// The page was fine and every action ran, but a click or scroll found no
    /// such element: the recipe's target is not on the page (a conclusive miss).
    Missing,
    /// A timeout, an error, a refusal, a skipped action, another page, a page
    /// that did not answer: nothing can be said about the recipe.
    Inconclusive,
}

/// Classify a finished live run (see [`LiveRun`]).
pub fn classify_live_run(actions: &[LiveAction], diag: &Diagnostic) -> LiveRun {
    if diag.complete {
        return LiveRun::Complete;
    }
    let o = &diag.live_actions;
    // A page still loading before the first action may simply not have its element yet.
    let ran_all = diag.live_page_ok && diag.live_ready_before && o.len() == actions.len();
    let only_missing = actions.iter().zip(o).all(|(a, o)| match a {
        LiveAction::Click(_) => o == "clicked" || o == "missing",
        LiveAction::ScrollTo(_) => o == "scrolled" || o == "missing",
        LiveAction::Wait(_) => o == "waited",
    });
    if ran_all && only_missing && o.iter().any(|x| x == "missing") {
        LiveRun::Missing
    } else {
        LiveRun::Inconclusive
    }
}

/// Every action ran and every click and scroll found its element.
fn live_outcomes_ok(actions: &[LiveAction], outcomes: &[String]) -> bool {
    actions.len() == outcomes.len()
        && actions.iter().zip(outcomes).all(|(a, o)| match a {
            LiveAction::Click(_) => o == "clicked",
            LiveAction::ScrollTo(_) => o == "scrolled",
            LiveAction::Wait(_) => o == "waited",
        })
}

/// The candidates, best first, without the network copy of a glomex clip
/// that is also found as its player: the player URL gives yt-dlp the clip's
/// best rendition and its title, and the copy would be offered to MCR as a
/// second video.
fn final_candidates(mut list: Vec<(u32, String)>) -> Vec<(u32, String)> {
    let embedded: HashSet<String> = list
        .iter()
        .filter_map(|(_, u)| glomex::parse_embed(u))
        .map(|e| e.playlist_id)
        .collect();
    list.retain(|(_, u)| glomex::clip_id_of_stream(u).map_or(true, |clip| !embedded.contains(&clip)));
    // Stable: among equal scores the one found first stays first.
    list.sort_by(|a, b| b.0.cmp(&a.0));
    list

}

#[cfg(test)]
mod tests {
    use super::*;

    /// Production source only (everything before the test module).
    fn production_source() -> &'static str {
        include_str!("sniffer.rs").split("#[cfg(test)]").next().unwrap()
    }

    #[test]
    fn the_log_shows_host_and_path_never_the_query() {
        assert_eq!(log_addr("https://cdn.example/v/a.m3u8?token=SECRET&x=1#frag"), "cdn.example/v/a.m3u8");
        assert_eq!(log_addr("//cdn.example/p?token=SECRET"), "cdn.example/p");
        assert!(!log_addr("not a url?token=SECRET").contains("SECRET"));
    }

    #[test]
    fn no_log_line_prints_a_raw_address() {
        let production = production_source();
        for mac in ["info!(", "warn!("] {
            let mut rest = production;
            while let Some(i) = rest.find(mac) {
                rest = &rest[i + mac.len()..];
                let end = [");", "),
", ")
"].iter().filter_map(|p| rest.find(p)).min().unwrap_or(rest.len());
                let call = &rest[..end];
                for raw in ["req_url", "best_url", "target_url", "{url}", "{raw}"] {
                    assert!(!call.contains(raw) || call.contains("log_addr("), "{mac} prints `{raw}` raw: {call}");
                }
            }
        }
    }

    #[test]
    fn a_selector_is_never_formatted_into_script_text() {
        let production = production_source();
        assert!(production.contains("CLICK_FN") && production.contains("SCROLL_FN"));
        let mut rest = production;
        while let Some(i) = rest.find("format!(") {
            rest = &rest[i + 8..];
            let call = rest.split(");").next().unwrap_or("");
            assert!(!call.contains("querySelector("), "a format! builds a selector lookup: {call}");
        }
        assert!(!CLICK_FN.contains("{}") && !SCROLL_FN.contains("{}"));
        // The fixed functions are never built from other text.
        for line in production.lines().filter(|l| l.contains("CLICK_FN") || l.contains("SCROLL_FN")) {
            for bad in ["format!", ".replace(", "concat!", " + ", "push_str", "&format"] {
                assert!(!line.contains(bad), "`{bad}` on a line with a fixed live function: {line}");
            }
        }
    }

    #[test]
    fn a_live_selector_reaches_the_browser_only_as_a_call_argument() {
        let production = production_source();
        let start = production.find("async fn run_live_actions").unwrap();
        let end = production.find("async fn sniff(").unwrap();
        let live = &production[start..end];
        // No script is evaluated in the live code: the only Runtime.evaluate is the fixed `document`.
        assert!(!live.contains(".evaluate("), "page.evaluate in the live code");
        assert_eq!(live.matches(".expression(").count(), 1);
        assert!(live.contains(".expression(\"document\")"));
        // The selector is handed over in exactly one place, as a call argument value.
        assert_eq!(live.matches("Value::String(css").count(), 1);
        assert!(live.contains("CallArgument::builder().value(Value::String(css.to_string()))"));
        assert!(live.contains("call_on_document(page, CLICK_FN, css)") && live.contains("call_on_document(page, SCROLL_FN, css)"));
        // `function_declaration` takes only the fixed `&'static str` parameter.
        assert!(live.contains("function: &'static str") && live.contains(".function_declaration(function)"));
    }

    #[test]
    fn a_live_run_is_complete_only_when_every_action_did_its_job() {
        use LiveAction::*;
        let acts = [Click("a".into()), Wait(1), ScrollTo("b".into())];
        let ok = |o: &[&str]| live_outcomes_ok(&acts, &o.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert!(ok(&["clicked", "waited", "scrolled"]));
        assert!(!ok(&["missing", "waited", "scrolled"]));
        assert!(!ok(&["clicked", "waited", "error"]));
        assert!(!ok(&["clicked", "waited"]), "cut short");
        assert!(!ok(&["clicked", "waited", "scrolled", "x"]));
        assert!(!ok(&["scrolled", "waited", "clicked"]));
    }

    #[test]
    fn only_a_missing_target_on_a_clean_page_is_a_conclusive_miss() {
        use LiveAction::*;
        let acts = [Click("a".into()), Wait(1)];
        let run = |o: &[&str], page_ok: bool| {
            let outcomes: Vec<String> = o.iter().map(|s| s.to_string()).collect();
            let complete = page_ok && live_outcomes_ok(&acts, &outcomes);
            classify_live_run(&acts, &Diagnostic { live_actions: outcomes, complete, live_page_ok: page_ok, live_ready_before: true, ..Default::default() })
        };
        assert_eq!(run(&["clicked", "waited"], true), LiveRun::Complete);
        assert_eq!(run(&["missing", "waited"], true), LiveRun::Missing);
        assert_eq!(run(&["missing", "waited"], false), LiveRun::Inconclusive, "page not read properly / left");
        assert_eq!(run(&["timeout", "waited"], true), LiveRun::Inconclusive);
        assert_eq!(run(&["error", "waited"], true), LiveRun::Inconclusive);
        assert_eq!(run(&["refused"], true), LiveRun::Inconclusive);
        assert_eq!(run(&["missing"], true), LiveRun::Inconclusive, "cut short");
        assert_eq!(run(&["missing", "waited", "skipped"], true), LiveRun::Inconclusive);
        assert_eq!(run(&[], true), LiveRun::Inconclusive);
    }

    #[test]
    fn a_page_still_loading_before_the_first_action_is_never_a_conclusive_miss() {
        let acts = [LiveAction::Click("a".into())];
        let d = |ready: bool| Diagnostic {
            live_actions: vec!["missing".into()],
            live_page_ok: true,
            live_ready_before: ready,
            ..Default::default()
        };
        assert_eq!(classify_live_run(&acts, &d(true)), LiveRun::Missing, "interactive or complete");
        assert_eq!(classify_live_run(&acts, &d(false)), LiveRun::Inconclusive, "loading, or unknown");
    }

    #[tokio::test]
    async fn a_diagnostic_run_that_could_not_launch_is_incomplete() {
        let (snap, diag) = StreamSniffer::diagnose_with(Err(anyhow!("no browser here")), "https://news.example/a", 5, &[]).await;
        assert!(!diag.complete, "a run that saw nothing must not read as a clean page");
        assert!(snap.network.is_empty() && snap.html.is_none());
        let e = crate::evidence::classify(&snap, &[], Some(&diag), |_| false);
        assert!(!e.diagnosed);
    }

    /// The blocker is process-wide and no longer self-initializes from a
    /// CWD-relative path (plan P0.1). Point it at a scratch directory; with no
    /// cached lists present it falls back to the built-in seed domains, which
    /// is what these scoring tests exercise.
    fn init_blocker() {
        crate::adblock::UnifiedAdBlocker::init(
            std::env::temp_dir().join("omni-adblock-unit-tests"),
        );
    }

    #[test]
    fn test_is_ad_or_tracking() {
        init_blocker();
        assert!(StreamSniffer::is_ad_or_tracking(
            "https://cdn.exitbee.com/user-template-uploads/HOTB_Survive_16x9-1-_1789034729.mp4"
        ));
        assert!(StreamSniffer::is_ad_or_tracking(
            "https://securepubads.g.doubleclick.net/gampad/ads?env=vp&gdfp_req=1"
        ));
        assert!(StreamSniffer::is_ad_or_tracking(
            "https://s.teads.tv/media/vast.xml"
        ));
        assert!(StreamSniffer::is_ad_or_tracking(
            "https://ads.smartadserver.com/diff/123/vast.json"
        ));
        assert!(StreamSniffer::is_ad_or_tracking(
            "https://www.newsbomb.gr/static/images/logo.png"
        ));

        // Greek ad servers
        assert!(StreamSniffer::is_ad_or_tracking(
            "https://ads.e-go.gr/gbanner/sponsor.png"
        ));
        assert!(StreamSniffer::is_ad_or_tracking(
            "https://astrack.mediacdn.com/ipquery?siteorigin=sportfm"
        ));

        // Legitimate broadcast media streams
        assert!(!StreamSniffer::is_ad_or_tracking(
            "https://stream.star.gr/star/live/master.m3u8"
        ));
        assert!(!StreamSniffer::is_ad_or_tracking(
            "https://video.twimg.com/amplify_video/2100498150641283072/pl/avc1/master.m3u8"
        ));
        assert!(!StreamSniffer::is_ad_or_tracking(
            "https://videostream.protothema.gr/vod/hls/video.m3u8"
        ));
        assert!(!StreamSniffer::is_ad_or_tracking(
            "https://player.glomex.com/integration/1/iframe-player.html?playlistId=v-123"
        ));
    }

    #[test]
    fn test_score_stream_url() {
        init_blocker();
        assert_eq!(
            StreamSniffer::score_stream_url("https://stream.star.gr/star/live/master.m3u8"),
            100
        );
        assert_eq!(
            StreamSniffer::score_stream_url("https://dash.news.com/video/manifest.mpd"),
            90
        );
        assert_eq!(
            StreamSniffer::score_stream_url(
                "https://video.twimg.com/amplify_video/2100498150641283072/vid/avc1/video.mp4"
            ),
            85
        );
        assert_eq!(
            StreamSniffer::score_stream_url("https://cdn.example.com/videos/sample.mp4"),
            40
        );
        // Ad should score 0
        assert_eq!(
            StreamSniffer::score_stream_url(
                "https://cdn.exitbee.com/user-template-uploads/HOTB_Survive_16x9-1-_1789034729.mp4"
            ),
            0
        );
    }

    /// neakriti.gr, 2026-10-05: the network saw the stream of the clip a
    /// contextual player played, and the same clip was found as its player.
    /// One video, not two, and the player URL is the one downloaded.
    #[test]
    fn a_glomex_clip_seen_as_player_and_as_stream_is_one_candidate() {
        let player = "https://player.glomex.com/integration/1/iframe-player.html?integrationId=eexbs17mbtfi1y9&playlistId=v-dlso19wczcdl";
        let stream = "https://video-cdn-jwt-new.mes.glomex.cloud/v2/77a350/v-dlso19wczcdl/eyJ0.eyJl.-wps/stream.mp4";
        let other = "https://video-cdn-jwt-new.mes.glomex.cloud/v2/77a350/v-another/eyJ0.eyJl.-wps/stream.mp4";
        let tiktok = "https://www.tiktok.com/@someone/video/7690626350159318305";
        let out = final_candidates(vec![(80, stream.into()), (95, player.into()), (80, other.into()), (95, tiktok.into())]);
        let urls: Vec<&str> = out.iter().map(|(_, u)| u.as_str()).collect();
        assert_eq!(urls, vec![player, tiktok, other]);
    }
}
