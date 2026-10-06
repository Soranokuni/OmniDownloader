use anyhow::{anyhow, Context, Result};
use chromiumoxide::browser::Browser;
use chromiumoxide::cdp::browser_protocol::network::{EventRequestWillBeSent, SetBlockedUrLsParams};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::adblock::UnifiedAdBlocker;
use crate::agent::BrowserError;
use crate::browser::HeadlessBrowserManager;
use crate::glomex;

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

/// StreamSniffer uses headless Chrome via CDP to navigate complex news websites,
/// bypass cookie consent banners, detect embedded platform players (Twitter/X, Glomex,
/// YouTube, Dailymotion, TikTok), filter out ad networks via UnifiedAdBlocker (HaGeZi + Greek),
/// trigger programmatic lazy hydration, and extract broadcast-grade video streams.
pub struct StreamSniffer;

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
        let result = Self::sniff(&session.browser, PAGE, 10).await;
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
        info!("StreamSniffer: Starting browser extraction for {}", target_url);

        let mut session = HeadlessBrowserManager::launch().await?;
        let result = Self::sniff(&session.browser, target_url, timeout_secs).await;
        session.shutdown().await;
        result
    }

    async fn sniff(browser: &Browser, target_url: &str, timeout_secs: u64) -> Result<DiscoveredMedia> {
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

        tokio::spawn(async move {
            while let Some(event) = request_events.next().await {
                if stop_flag_clone.load(Ordering::Relaxed) {
                    break;
                }
                let req_url = event.request.url.clone();
                let score = Self::score_stream_url(&req_url);

                if score > 0 {
                    info!(
                        "StreamSniffer: Discovered candidate (score: {}): {}",
                        score, req_url
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
        let cookie_bypass_js = r#"
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
                            best_score, best_url
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

        if let Some((score, best_url)) = sorted_candidates.first() {
            info!(
                "StreamSniffer: Discovered {} media streams total. Selected primary (score: {}): {}",
                sorted_candidates.len(),
                score,
                best_url
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
            Err(BrowserError::NoStreamFound(target_url.to_string()).into())
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
                        info!("StreamSniffer: glomex player {raw} has no video for this page; ignored");
                        continue;
                    }
                    Err(e) => {
                        warn!("StreamSniffer: could not ask glomex which video {raw} plays: {e:#}");
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
                    info!("StreamSniffer: Identified DOM embedded media [{embed_type}] (score: {score}): {url}");
                    list.push((score, url));
                }
            }
        }
    }
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
