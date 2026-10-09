//! What the browser saw on a news page that gave no video: network requests
//! and responses plus a few page facts, kept bounded so a failed job can be
//! looked at later (plan P10.1).
//!
//! Nothing here stores request or response headers, cookies or POST bodies;
//! there is no field that could hold them.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

pub const MAX_NETWORK: usize = 300;
pub const MAX_META: usize = 100;
pub const MAX_LD_JSON: usize = 8;
pub const MAX_LD_JSON_BYTES: usize = 32 * 1024;
pub const MAX_PLAYERS: usize = 20;
pub const MAX_PLAYER_HTML_CHARS: usize = 4096;
pub const MAX_SCRIPTS: usize = 20;
pub const MAX_SCRIPT_CHARS: usize = 2048;
pub const MAX_POSTERS: usize = 20;
pub const MAX_HTML_BYTES: usize = 2 * 1024 * 1024;
const MAX_URL_CHARS: usize = 2048;

/// One request the page made, with the response's status and type when seen.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NetworkEntry {
    pub url: String,
    pub method: Option<String>,
    pub status: Option<i64>,
    pub mime: Option<String>,
    pub resource_type: Option<String>,
    pub frame_id: Option<String>,
}

impl NetworkEntry {
    /// Looks like a video or a manifest, by response type or by address.
    pub fn is_media_like(&self) -> bool {
        if let Some(m) = &self.mime {
            let m = m.to_ascii_lowercase();
            if m.starts_with("video/")
                || m == "application/vnd.apple.mpegurl"
                || m == "application/x-mpegurl"
                || m == "application/dash+xml"
            {
                return true;
            }
        }
        let lower = self.url.to_ascii_lowercase();
        let path = lower.split(['?', '#']).next().unwrap_or("");
        [".m3u8", ".mpd", ".mp4", ".webm", ".mov", ".ts"].iter().any(|e| path.ends_with(e))
    }
}

/// A player-looking element of the page.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlayerBlock {
    pub tag: String,
    /// Outer HTML, at most 4096 characters.
    pub html: String,
    /// Position among the page's player candidates, document order.
    pub index: usize,
    pub in_article: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PageSnapshot {
    pub page_url: String,
    pub final_url: Option<String>,
    pub title: Option<String>,
    pub canonical: Option<String>,
    pub meta: Vec<(String, String)>,
    pub ld_json: Vec<String>,
    pub network: Vec<NetworkEntry>,
    pub players: Vec<PlayerBlock>,
    pub scripts: Vec<String>,
    pub posters: Vec<String>,
    pub html: Option<String>,
    /// RFC 3339.
    pub captured_at: String,
}

/// Cut `s` to at most `max` bytes on a char boundary.
fn truncate_bytes(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut cut = max;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
}

fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Add to a capped list: when full, a media-like item evicts the oldest
/// non-media one; anything else is ignored.
fn push_capped<T>(list: &mut Vec<T>, item: T, is_media: impl Fn(&T) -> bool) -> bool {
    if list.len() < MAX_NETWORK {
        list.push(item);
        return true;
    }
    if !is_media(&item) {
        return false;
    }
    match list.iter().position(|x| !is_media(x)) {
        Some(i) => {
            list.remove(i);
            list.push(item);
            true
        }
        None => false,
    }
}

impl PageSnapshot {
    pub fn new(page_url: &str) -> Self {
        Self {
            page_url: page_url.to_string(),
            captured_at: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        }
    }

    /// Add a network entry under the cap.
    pub fn push_network(&mut self, mut entry: NetworkEntry) {
        entry.url = truncate_chars(&entry.url, MAX_URL_CHARS);
        push_capped(&mut self.network, entry, NetworkEntry::is_media_like);
    }

    /// Hosts of the page, its final address and every request.
    pub fn observed_hosts(&self) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let urls = std::iter::once(self.page_url.as_str())
            .chain(self.final_url.as_deref())
            .chain(self.network.iter().map(|n| n.url.as_str()));
        for u in urls {
            if let Ok(parsed) = reqwest::Url::parse(u) {
                if let Some(h) = parsed.host_str() {
                    out.insert(h.to_ascii_lowercase());
                }
            }
        }
        out
    }

    /// Fill the page facts from the result of the in-page read. Anything
    /// missing or of the wrong shape stays empty.
    pub fn apply_page_facts(&mut self, v: &Value) {
        let Some(obj) = v.as_object() else { return };
        let text = |k: &str| obj.get(k).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string);
        self.title = text("title").map(|s| truncate_chars(&s, 1024));
        self.canonical = text("canonical").map(|s| truncate_chars(&s, MAX_URL_CHARS));
        let list = |k: &str| obj.get(k).and_then(Value::as_array).cloned().unwrap_or_default();

        self.meta = list("meta")
            .iter()
            .filter_map(|p| {
                let a = p.as_array()?;
                Some((
                    truncate_chars(a.first()?.as_str()?, 256),
                    truncate_chars(a.get(1)?.as_str()?, 1024),
                ))
            })
            .take(MAX_META)
            .collect();
        self.ld_json = list("ld_json")
            .iter()
            .filter_map(Value::as_str)
            .take(MAX_LD_JSON)
            .map(|s| {
                let mut s = s.to_string();
                truncate_bytes(&mut s, MAX_LD_JSON_BYTES);
                s
            })
            .collect();
        self.players = list("players")
            .iter()
            .filter_map(Value::as_object)
            .take(MAX_PLAYERS)
            .map(|o| PlayerBlock {
                tag: o.get("tag").and_then(Value::as_str).unwrap_or("").chars().take(32).collect(),
                html: truncate_chars(o.get("html").and_then(Value::as_str).unwrap_or(""), MAX_PLAYER_HTML_CHARS),
                index: o.get("index").and_then(Value::as_u64).unwrap_or(0) as usize,
                in_article: o.get("in_article").and_then(Value::as_bool).unwrap_or(false),
            })
            .collect();
        self.scripts = list("scripts")
            .iter()
            .filter_map(Value::as_str)
            .take(MAX_SCRIPTS)
            .map(|s| truncate_chars(s, MAX_SCRIPT_CHARS))
            .collect();
        self.posters = list("posters")
            .iter()
            .filter_map(Value::as_str)
            .take(MAX_POSTERS)
            .map(|s| truncate_chars(s, MAX_URL_CHARS))
            .collect();
        self.html = text("html").map(|mut h| {
            truncate_bytes(&mut h, MAX_HTML_BYTES);
            h
        });
    }

    /// Valid JSON never above `max_bytes`. Trims html, scripts, players,
    /// ld_json, meta, then non-media network entries (newest first); media-like
    /// requests go last.
    pub fn to_capped_json(&self, max_bytes: usize) -> String {
        let mut s = self.clone();
        let size = |s: &PageSnapshot| serde_json::to_string(s).map(|j| j.len()).unwrap_or(usize::MAX);

        let mut cur = size(&s);
        if cur <= max_bytes {
            return serde_json::to_string(&s).unwrap_or_else(|_| "{}".into());
        }
        if let Some(html) = s.html.as_mut() {
            // every cut byte saves at least one serialised byte
            let new_len = html.len().saturating_sub(cur - max_bytes + 64);
            if new_len == 0 {
                s.html = None;
            } else {
                truncate_bytes(html, new_len);
            }
            cur = size(&s);
            if cur > max_bytes {
                s.html = None;
            }
        }
        let steps: [fn(&mut PageSnapshot); 4] = [
            |s| s.scripts.clear(),
            |s| s.players.clear(),
            |s| s.ld_json.clear(),
            |s| s.meta.clear(),
        ];
        for step in steps {
            if size(&s) <= max_bytes {
                break;
            }
            step(&mut s);
        }
        while size(&s) > max_bytes {
            match s.network.iter().rposition(|n| !n.is_media_like()) {
                Some(i) => {
                    s.network.remove(i);
                }
                None => break,
            }
        }
        if size(&s) > max_bytes {
            s.posters.clear();
            s.title = None;
            s.canonical = None;
        }
        while size(&s) > max_bytes && s.network.pop().is_some() {}
        if size(&s) > max_bytes {
            s = PageSnapshot { page_url: truncate_chars(&s.page_url, 256), captured_at: s.captured_at.clone(), ..Default::default() };
        }
        match serde_json::to_string(&s) {
            Ok(j) if j.len() <= max_bytes => j,
            _ => "{}".into(),
        }
    }
}

/// Requests seen so far, keyed by the browser's request id so a response can
/// fill in the status of its request.
#[derive(Debug, Default)]
pub struct NetworkLog {
    entries: Vec<(String, NetworkEntry)>,
}

impl NetworkLog {
    pub fn on_request(&mut self, id: &str, mut entry: NetworkEntry) {
        entry.url = truncate_chars(&entry.url, MAX_URL_CHARS);
        push_capped(&mut self.entries, (id.to_string(), entry), |(_, e)| e.is_media_like());
    }

    pub fn on_response(&mut self, id: &str, status: i64, mime: Option<String>, resource_type: Option<String>) {
        if let Some((_, e)) = self.entries.iter_mut().rev().find(|(i, _)| i == id) {
            e.status = Some(status);
            e.mime = mime.filter(|m| !m.is_empty());
            if resource_type.is_some() {
                e.resource_type = resource_type;
            }
        }
    }

    pub fn into_entries(self) -> Vec<NetworkEntry> {
        self.entries.into_iter().map(|(_, e)| e).collect()
    }

    pub fn entries(&self) -> Vec<NetworkEntry> {
        self.entries.iter().map(|(_, e)| e.clone()).collect()
    }
}

/// The one read-only page read made when no stream was found. Fixed text;
/// no page data is ever spliced into it.
pub const PAGE_FACTS_JS: &str = r#"
(() => {
    const out = { title: null, canonical: null, meta: [], ld_json: [], players: [], scripts: [], posters: [], html: null };
    try { out.title = document.title || null; } catch (e) {}
    try { const c = document.querySelector('link[rel="canonical"]'); out.canonical = c ? c.href : null; } catch (e) {}
    try {
        for (const m of Array.from(document.querySelectorAll('meta')).slice(0, 100)) {
            const k = m.getAttribute('name') || m.getAttribute('property') || m.getAttribute('itemprop');
            const v = m.getAttribute('content');
            if (k && v !== null) out.meta.push([k, v]);
        }
    } catch (e) {}
    try {
        for (const s of Array.from(document.querySelectorAll('script[type="application/ld+json"]')).slice(0, 8)) {
            out.ld_json.push((s.textContent || '').slice(0, 32768));
        }
    } catch (e) {}
    try {
        const els = Array.from(document.querySelectorAll('video, iframe, amp-video, amp-iframe, [class*=player], [id*=player]'));
        els.slice(0, 20).forEach((el, i) => {
            out.players.push({
                tag: el.tagName.toLowerCase(),
                html: (el.outerHTML || '').slice(0, 4096),
                index: i,
                in_article: !!el.closest('article, main, .article-body, .entry-content')
            });
        });
    } catch (e) {}
    try {
        const re = /m3u8|mpd|mp4|manifest|videoId|mediaid|playlist|hls|dash/gi;
        for (const s of Array.from(document.querySelectorAll('script:not([src])'))) {
            if (out.scripts.length >= 20) break;
            const t = s.textContent || '';
            if (!t) continue;
            re.lastIndex = 0;
            const m = re.exec(t);
            if (m) out.scripts.push(t.slice(Math.max(0, m.index - 1000), m.index + 1000));
        }
    } catch (e) {}
    try {
        for (const v of Array.from(document.querySelectorAll('video[poster]'))) {
            if (v.poster) out.posters.push(v.poster);
        }
        for (const m of Array.from(document.querySelectorAll('meta[property="og:image"], meta[name="twitter:image"]'))) {
            const c = m.getAttribute('content');
            if (c) out.posters.push(c);
        }
        out.posters = out.posters.slice(0, 20);
    } catch (e) {}
    try { out.html = (document.documentElement.outerHTML || '').slice(0, 2097152); } catch (e) {}
    return out;
})()
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn net(url: &str, mime: Option<&str>) -> NetworkEntry {
        NetworkEntry {
            url: url.into(),
            method: Some("GET".into()),
            status: Some(200),
            mime: mime.map(str::to_string),
            resource_type: Some("Other".into()),
            frame_id: Some("F1".into()),
        }
    }

    fn big_snapshot() -> PageSnapshot {
        let mut s = PageSnapshot::new("https://news.example/story");
        s.html = Some("<p>x\u{3b1}</p>".repeat(600_000)); // ~6 MiB
        s.scripts = vec!["s".repeat(2048); 20];
        s.players = (0..20)
            .map(|i| PlayerBlock { tag: "iframe".into(), html: "p".repeat(4096), index: i, in_article: true })
            .collect();
        s.ld_json = vec!["{}".repeat(16 * 1024); 8];
        s.meta = (0..100).map(|i| (format!("k{i}"), "v".repeat(500))).collect();
        for i in 0..200 {
            s.push_network(net(&format!("https://cdn.example/a{i}.js?{}", "q".repeat(300)), Some("text/javascript")));
        }
        s.push_network(net("https://cdn.example/v/master.m3u8", Some("application/vnd.apple.mpegurl")));
        s.push_network(net("https://cdn.example/v/clip.mp4", None));
        s
    }

    #[test]
    fn capped_json_fits_parses_and_keeps_media_requests() {
        let s = big_snapshot();
        assert!(serde_json::to_string(&s).unwrap().len() > 5 * 1024 * 1024);
        let cap = 512 * 1024;
        let json = s.to_capped_json(cap);
        assert!(json.len() <= cap, "{} bytes", json.len());
        let back: PageSnapshot = serde_json::from_str(&json).unwrap();
        assert!(back.network.iter().any(|n| n.url.ends_with("master.m3u8")));
        assert!(back.network.iter().any(|n| n.url.ends_with("clip.mp4")));
        assert_eq!(back.page_url, "https://news.example/story");
    }

    #[test]
    fn capped_json_untouched_when_small() {
        let mut s = PageSnapshot::new("https://news.example/a");
        s.html = Some("<p>hi</p>".into());
        let back: PageSnapshot = serde_json::from_str(&s.to_capped_json(512 * 1024)).unwrap();
        assert_eq!(back.html.as_deref(), Some("<p>hi</p>"));
    }

    #[test]
    fn capped_json_drops_non_media_before_media() {
        let mut s = PageSnapshot::new("https://news.example/a");
        s.push_network(net("https://cdn.example/v.m3u8", None));
        for i in 0..50 {
            s.push_network(net(&format!("https://cdn.example/{i}.js"), Some("text/javascript")));
        }
        let full = serde_json::to_string(&s).unwrap().len();
        let back: PageSnapshot = serde_json::from_str(&s.to_capped_json(full / 2)).unwrap();
        assert!(back.network.iter().any(|n| n.url.ends_with("v.m3u8")));
        assert!(back.network.len() < 51);
    }

    fn keys_clean(v: &Value) {
        match v {
            Value::Object(m) => {
                for (k, v) in m {
                    let k = k.to_ascii_lowercase();
                    assert!(!k.contains("cookie") && !k.contains("header"), "key {k}");
                    keys_clean(v);
                }
            }
            Value::Array(a) => a.iter().for_each(keys_clean),
            _ => {}
        }
    }

    #[test]
    fn snapshot_has_no_cookie_or_header_fields() {
        let mut s = big_snapshot();
        s.final_url = Some("https://news.example/final".into());
        s.title = Some("t".into());
        s.canonical = Some("https://news.example/c".into());
        s.posters = vec!["https://news.example/p.jpg".into()];
        keys_clean(&serde_json::to_value(&s).unwrap());
    }

    #[test]
    fn network_cap_keeps_every_media_entry() {
        let mut s = PageSnapshot::new("https://news.example/a");
        let mut media = 0;
        for i in 0..400 {
            if i % 4 == 0 {
                media += 1;
                s.push_network(net(&format!("https://cdn.example/seg{i}.ts"), None));
            } else {
                s.push_network(net(&format!("https://cdn.example/{i}.js"), Some("text/javascript")));
            }
        }
        assert!(s.network.len() <= MAX_NETWORK);
        assert_eq!(s.network.iter().filter(|n| n.is_media_like()).count(), media);
    }

    #[test]
    fn media_like_detection() {
        assert!(net("https://a/x", Some("video/mp4")).is_media_like());
        assert!(net("https://a/x", Some("application/x-mpegURL")).is_media_like());
        assert!(net("https://a/x", Some("application/dash+xml")).is_media_like());
        assert!(net("https://a/clip.webm?t=1", None).is_media_like());
        assert!(!net("https://a/app.js", Some("text/javascript")).is_media_like());
        assert!(!net("https://a/tsconfig", None).is_media_like());
    }

    #[test]
    fn network_log_matches_response_by_request_id() {
        let mut log = NetworkLog::default();
        log.on_request("1", net("https://a/x.json", None));
        log.on_request("2", net("https://a/y.json", None));
        log.on_response("2", 404, Some("application/json".into()), Some("Fetch".into()));
        let e = log.into_entries();
        assert!(e[0].mime.is_none());
        assert_eq!(e[1].status, Some(404));
        assert_eq!(e[1].mime.as_deref(), Some("application/json"));
    }

    #[test]
    fn observed_hosts_cover_page_final_and_requests() {
        let mut s = PageSnapshot::new("https://News.Example/a");
        s.final_url = Some("https://www.news.example/b".into());
        s.push_network(net("https://cdn.other.example:8443/x.m3u8", None));
        let h = s.observed_hosts();
        assert!(h.contains("news.example") && h.contains("www.news.example") && h.contains("cdn.other.example"));
    }

    #[test]
    fn page_facts_are_applied_with_caps() {
        let v = serde_json::json!({
            "title": "T", "canonical": "https://x/c",
            "meta": [["og:title", "Hello"], ["bad"]],
            "ld_json": ["{}"],
            "players": [{"tag": "iframe", "html": "h".repeat(9000), "index": 3, "in_article": true}],
            "scripts": ["a"], "posters": ["https://x/p.jpg"], "html": "<html></html>"
        });
        let mut s = PageSnapshot::new("https://x/a");
        s.apply_page_facts(&v);
        assert_eq!(s.title.as_deref(), Some("T"));
        assert_eq!(s.meta, vec![("og:title".to_string(), "Hello".to_string())]);
        assert_eq!(s.players[0].html.len(), MAX_PLAYER_HTML_CHARS);
        assert!(s.players[0].in_article);
        s.apply_page_facts(&Value::Null);
        assert!(s.title.is_some(), "a non-object read leaves facts alone");
    }
}
