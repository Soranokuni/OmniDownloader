use anyhow::Result;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock, RwLock};
use tracing::{info, warn};

/// High-impact default rules embedded at compile-time to guarantee immediate
/// ad & tracker filtering even prior to initial network synchronization.
const EMBEDDED_BASELINE_RULES: &[&str] = &[
    // Major Greek Ad Networks & Trackers
    "exitbee.com",
    "cdn.exitbee.com",
    "orangeclickmedia.com",
    "cdn.orangeclickmedia.com",
    "e-go.gr",
    "ads.e-go.gr",
    "adman.gr",
    "project-agora.com",
    "projectagora.com",
    "liquidmedia.gr",
    "astrack.mediacdn.com",
    "2810.gr",
    "grxchange.gr",
    "x.grxchange.gr",
    "adform.net",
    "adform.com",
    "yieldlove.com",
    "seedtag.com",
    "connatix.com",
    "aniview.com",
    "brid.tv",
    // International Video Ad Networks & Trackers
    "doubleclick.net",
    "securepubads.g.doubleclick.net",
    "googleads.g.doubleclick.net",
    "googlesyndication.com",
    "2mdn.net",
    "gcdn.2mdn.net",
    "teads.tv",
    "teads.com",
    "smartadserver.com",
    "criteo.com",
    "criteo.net",
    "pubmatic.com",
    "rubiconproject.com",
    "taboola.com",
    "outbrain.com",
    "adnxs.com",
    "openx.net",
    "spotxchange.com",
    "spotx.tv",
    "appnexus.com",
    "inmobi.com",
    "advertising.com",
    "amazon-adsystem.com",
    "bidswitch.net",
    "casalemedia.com",
    "revcontent.com",
    "mgid.com",
    "trafficfactory.biz",
    "ad-delivery.net",
    "quantserve.com",
    "scorecardresearch.com",
    "moatads.com",
    "adtech.de",
    "kwikmotion.com",
    "stat.kwikmotion.com",
    "hotjar.com",
    "clarity.ms",
];

pub struct AdBlockStats {
    pub total_domains: usize,
    pub hagezi_count: usize,
    pub greek_count: usize,
    pub last_updated: Option<chrono::DateTime<chrono::Local>>,
}

/// UnifiedAdBlocker merges HaGeZi DNS Blocklist (~39,500 domains) with
/// the authoritative Greek AdBlock Filter (kargig/void-gr-filters, ~1,260 domains)
/// into an ultra-fast O(1) in-memory domain suffix trie/hashset.
pub struct UnifiedAdBlocker {
    blocked_domains: RwLock<HashSet<String>>,
    blocked_path_patterns: RwLock<Vec<String>>,
    cache_dir: PathBuf,
}

static GLOBAL_BLOCKER: OnceLock<Arc<UnifiedAdBlocker>> = OnceLock::new();

impl UnifiedAdBlocker {
    pub fn new(cache_dir: PathBuf) -> Self {
        let mut domains = HashSet::new();
        for &d in EMBEDDED_BASELINE_RULES {
            domains.insert(d.to_lowercase());
        }

        let blocker = Self {
            blocked_domains: RwLock::new(domains),
            blocked_path_patterns: RwLock::new(vec![
                "/ads/".to_string(),
                "/ad/".to_string(),
                "/vast".to_string(),
                "/vpaid".to_string(),
                "preroll".to_string(),
                "midroll".to_string(),
                "postroll".to_string(),
                "creative".to_string(),
                "user-template-uploads".to_string(),
                "web_video_ads".to_string(),
                "/beacon".to_string(),
                "/telemetry".to_string(),
                "/metrics".to_string(),
                "/ping".to_string(),
            ]),
            cache_dir,
        };

        // Try loading existing cached files from disk if available
        blocker.load_cached_lists_from_disk();
        blocker
    }

    /// Returns or initializes the global blocker instance.
    pub fn global() -> Arc<Self> {
        GLOBAL_BLOCKER
            .get_or_init(|| {
                let cache_dir = PathBuf::from("data/adblock");
                Arc::new(Self::new(cache_dir))
            })
            .clone()
    }

    /// Checks whether an arbitrary URL is classified as an advertisement, tracker, or telemetry beacon.
    pub fn is_blocked(&self, url: &str) -> bool {
        let lower = url.to_lowercase();

        // 1. Check non-video asset extensions
        const NON_MEDIA_EXTS: &[&str] = &[
            ".png", ".jpg", ".jpeg", ".gif", ".webp", ".svg", ".css", ".js", ".json", ".woff",
            ".woff2", ".ttf", ".ico", ".otf",
        ];
        if let Some(clean_path) = lower.split('?').next() {
            for ext in NON_MEDIA_EXTS {
                if clean_path.ends_with(ext) {
                    return true;
                }
            }
        }

        // 2. Extract host and check against domain set (including parent domains)
        if let Some(host) = extract_host(&lower) {
            let domains = self.blocked_domains.read().unwrap();
            let parts: Vec<&str> = host.split('.').collect();
            for i in 0..parts.len().saturating_sub(1) {
                let candidate = parts[i..].join(".");
                if domains.contains(&candidate) {
                    return true;
                }
            }
        }

        // 3. Check path and query patterns
        let patterns = self.blocked_path_patterns.read().unwrap();
        for p in patterns.iter() {
            if lower.contains(p) {
                return true;
            }
        }

        false
    }

    /// Generates top wildcard domain patterns for Chrome CDP `Network.setBlockedURLs`
    /// to abort ad requests before network socket transmission.
    pub fn generate_cdp_blocked_patterns(&self) -> Vec<String> {
        let mut patterns = Vec::new();
        // High-impact ad and telemetry wildcard patterns
        let high_impact = [
            "*exitbee.com*",
            "*orangeclickmedia.com*",
            "*doubleclick.net*",
            "*2mdn.net*",
            "*teads.tv*",
            "*teads.com*",
            "*smartadserver.com*",
            "*criteo.com*",
            "*pubmatic.com*",
            "*rubiconproject.com*",
            "*taboola.com*",
            "*outbrain.com*",
            "*adnxs.com*",
            "*openx.net*",
            "*spotxchange.com*",
            "*appnexus.com*",
            "*inmobi.com*",
            "*grxchange.gr*",
            "*adman.gr*",
            "*ads.e-go.gr*",
            "*project-agora.com*",
            "*astrack.mediacdn.com*",
            "*connatix.com*",
            "*aniview.com*",
            "*brid.tv*",
            "*kwikmotion.com*",
            "*/ads/*",
            "*/vast*",
            "*/vpaid*",
            "*user-template-uploads*",
            "*web_video_ads*",
        ];

        for &p in &high_impact {
            patterns.push(p.to_string());
        }

        patterns
    }

    /// Loads cached HaGeZi and Greek filter files from disk if present.
    fn load_cached_lists_from_disk(&self) {
        let hagezi_file = self.cache_dir.join("hagezi_light.txt");
        let greek_file = self.cache_dir.join("greek_adblock.txt");

        if hagezi_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&hagezi_file) {
                self.parse_and_insert_rules(&content);
                info!("Loaded cached HaGeZi blocklist from {:?}", hagezi_file);
            }
        }

        if greek_file.exists() {
            if let Ok(content) = std::fs::read_to_string(&greek_file) {
                self.parse_and_insert_rules(&content);
                info!("Loaded cached Greek AdBlock list from {:?}", greek_file);
            }
        }
    }

    /// Parses Adblock Plus / uBlock formatted rules (e.g. `||domain.com^` or `domain.com`)
    /// and populates the in-memory domain and pattern sets.
    pub fn parse_and_insert_rules(&self, content: &str) -> usize {
        let mut count = 0;
        let mut new_domains = Vec::new();
        let mut new_patterns = Vec::new();

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty()
                || trimmed.starts_with('!')
                || trimmed.starts_with('#')
                || trimmed.starts_with('[')
            {
                continue;
            }

            // Standard Adblock syntax: ||domain.com^
            if let Some(rest) = trimmed.strip_prefix("||") {
                let domain_part = rest
                    .trim_end_matches('^')
                    .split('/')
                    .next()
                    .unwrap_or("")
                    .trim();
                if !domain_part.is_empty() {
                    new_domains.push(domain_part.to_lowercase());
                    count += 1;
                }
            } else if let Some(rest) = trimmed.strip_prefix("*.") {
                // Wildcard domain syntax: *.domain.com
                new_domains.push(rest.to_lowercase());
                count += 1;
            } else if !trimmed.contains('/') && !trimmed.contains(' ') && trimmed.contains('.') {
                // Raw domain
                new_domains.push(trimmed.to_lowercase());
                count += 1;
            } else if trimmed.contains("banner") || trimmed.contains("adserver") {
                new_patterns.push(trimmed.to_lowercase());
            }
        }

        {
            let mut domains = self.blocked_domains.write().unwrap();
            for d in new_domains {
                domains.insert(d);
            }
        }
        {
            let mut patterns = self.blocked_path_patterns.write().unwrap();
            for p in new_patterns {
                patterns.push(p);
            }
        }

        count
    }

    /// Downloads the latest HaGeZi Light and Greek AdBlock filter lists from upstream repositories,
    /// saves them to local cache, and updates the in-memory engine.
    pub async fn update_blocklists(&self) -> Result<AdBlockStats> {
        info!("UnifiedAdBlocker: Initiating blocklist synchronization...");
        tokio::fs::create_dir_all(&self.cache_dir).await?;

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("OmniDownloader-IngestEngine/1.0")
            .build()?;

        // 1. Download HaGeZi Light (~39,500 rules)
        let hagezi_url = "https://raw.githubusercontent.com/hagezi/dns-blocklists/main/adblock/light.txt";
        let mut hagezi_count = 0;
        match client.get(hagezi_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(text) = resp.text().await {
                    let dest = self.cache_dir.join("hagezi_light.txt");
                    let _ = tokio::fs::write(&dest, &text).await;
                    hagezi_count = self.parse_and_insert_rules(&text);
                    info!(
                        "UnifiedAdBlocker: HaGeZi Light updated successfully ({} rules)",
                        hagezi_count
                    );
                }
            }
            Ok(resp) => warn!("UnifiedAdBlocker: HaGeZi upstream returned HTTP {}", resp.status()),
            Err(e) => warn!("UnifiedAdBlocker: HaGeZi download failed: {}", e),
        }

        // 2. Download Greek AdBlock Filter (void-gr-filters by kargig, ~1,260 rules)
        let greek_url = "https://www.void.gr/kargig/void-gr-filters.txt";
        let mut greek_count = 0;
        match client.get(greek_url).send().await {
            Ok(resp) if resp.status().is_success() => {
                if let Ok(text) = resp.text().await {
                    let dest = self.cache_dir.join("greek_adblock.txt");
                    let _ = tokio::fs::write(&dest, &text).await;
                    greek_count = self.parse_and_insert_rules(&text);
                    info!(
                        "UnifiedAdBlocker: Greek AdBlock updated successfully ({} rules)",
                        greek_count
                    );
                }
            }
            Ok(resp) => warn!("UnifiedAdBlocker: Greek AdBlock upstream returned HTTP {}", resp.status()),
            Err(e) => warn!("UnifiedAdBlocker: Greek AdBlock download failed: {}", e),
        }

        let total = self.blocked_domains.read().unwrap().len();
        info!(
            "UnifiedAdBlocker: Engine active with {} unique blocked domains across global & Greek lists.",
            total
        );

        Ok(AdBlockStats {
            total_domains: total,
            hagezi_count,
            greek_count,
            last_updated: Some(chrono::Local::now()),
        })
    }

    pub fn stats(&self) -> AdBlockStats {
        let total = self.blocked_domains.read().unwrap().len();
        AdBlockStats {
            total_domains: total,
            hagezi_count: 0,
            greek_count: 0,
            last_updated: None,
        }
    }
}

fn extract_host(url: &str) -> Option<String> {
    let clean = if let Some(stripped) = url.strip_prefix("https://") {
        stripped
    } else if let Some(stripped) = url.strip_prefix("http://") {
        stripped
    } else {
        url
    };

    let host = clean.split('/').next()?.split(':').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_unified_adblocker_baseline_rules() {
        let blocker = UnifiedAdBlocker::new(PathBuf::from("temp/test_adblock"));

        // Global ad domains
        assert!(blocker.is_blocked("https://securepubads.g.doubleclick.net/gampad/ads?env=vp"));
        assert!(blocker.is_blocked("https://s.teads.tv/media/vast.xml"));
        assert!(blocker.is_blocked("https://gcdn.2mdn.net/videoplayback/file.mp4"));

        // Greek-specific ad domains & servers
        assert!(blocker.is_blocked("https://cdn.exitbee.com/user-template-uploads/ad.mp4"));
        assert!(blocker.is_blocked("https://cdn.orangeclickmedia.com/videos/ovp.mp4"));
        assert!(blocker.is_blocked("https://ads.e-go.gr/gbanner/sponsor.png"));
        assert!(blocker.is_blocked("https://astrack.mediacdn.com/ipquery?siteorigin=sportfm"));
        assert!(blocker.is_blocked("https://x.grxchange.gr/videoad/30291?vast=4.2"));

        // Legitimate broadcast CDNs & media hosts
        assert!(!blocker.is_blocked("https://starmotionvod.siliconweb.com/starvod/index.m3u8"));
        assert!(!blocker.is_blocked("https://video.twimg.com/amplify_video/123/pl/master.m3u8"));
        assert!(!blocker.is_blocked("https://videostream.protothema.gr/vod/hls/video.m3u8"));
        assert!(!blocker.is_blocked("https://www.youtube.com/watch?v=sample123"));
        assert!(!blocker.is_blocked("https://player.glomex.com/integration/1/iframe-player.html"));
    }

    #[test]
    fn test_parse_adblock_syntax() {
        let blocker = UnifiedAdBlocker::new(PathBuf::from("temp/test_adblock_parse"));
        let sample = r#"
        ! Title: Sample Adblock
        ||bad-ad-server.com^
        ||greek-tracker.gr/api
        *.wildcard-ad.net
        clean-banner-domain.org
        "#;

        let count = blocker.parse_and_insert_rules(sample);
        assert_eq!(count, 4);

        assert!(blocker.is_blocked("https://sub.bad-ad-server.com/banner.js"));
        assert!(blocker.is_blocked("https://greek-tracker.gr/api/collect"));
        assert!(blocker.is_blocked("https://sub.wildcard-ad.net/pixel.gif"));
        assert!(blocker.is_blocked("https://clean-banner-domain.org/ad"));
    }
}
