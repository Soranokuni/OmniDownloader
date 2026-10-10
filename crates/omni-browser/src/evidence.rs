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
pub const MAX_AD_MEDIA: usize = 64;
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
    /// `document.readyState` when the facts were read.
    #[serde(default)]
    pub ready_state: Option<String>,
    /// Media files an ad server's VAST reply named (capped, `MAX_AD_MEDIA`),
    /// so later checks have them from the saved snapshot alone.
    #[serde(default)]
    pub ad_media: Vec<String>,
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

    /// Add an ad media URL under the cap; false when it was not added.
    pub fn push_ad_media(&mut self, url: &str) -> bool {
        let url = truncate_chars(url, MAX_URL_CHARS);
        if self.ad_media.len() >= MAX_AD_MEDIA || self.ad_media.contains(&url) {
            return false;
        }
        self.ad_media.push(url);
        true
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
        self.ready_state = text("ready_state").map(|s| truncate_chars(&s, 32));
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
        let steps: [fn(&mut PageSnapshot); 5] = [
            |s| s.scripts.clear(),
            |s| s.players.clear(),
            |s| s.ld_json.clear(),
            |s| s.meta.clear(),
            |s| s.ad_media.clear(),
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
    /// Responses whose request has not been seen yet (the two come from
    /// separate listeners, in no fixed order).
    pending: Vec<(String, i64, Option<String>, Option<String>)>,
}

const MAX_PENDING_RESPONSES: usize = 64;

impl NetworkLog {
    pub fn on_request(&mut self, id: &str, mut entry: NetworkEntry) {
        entry.url = truncate_chars(&entry.url, MAX_URL_CHARS);
        let kept = push_capped(&mut self.entries, (id.to_string(), entry), |(_, e)| e.is_media_like());
        if let Some(i) = self.pending.iter().position(|(p, ..)| p == id) {
            let (_, status, mime, rt) = self.pending.remove(i);
            if kept {
                self.on_response(id, status, mime, rt);
            }
        }
    }

    pub fn on_response(&mut self, id: &str, status: i64, mime: Option<String>, resource_type: Option<String>) {
        if let Some((_, e)) = self.entries.iter_mut().rev().find(|(i, _)| i == id) {
            e.status = Some(status);
            e.mime = mime.filter(|m| !m.is_empty());
            if resource_type.is_some() {
                e.resource_type = resource_type;
            }
        } else if self.pending.len() < MAX_PENDING_RESPONSES {
            self.pending.push((id.to_string(), status, mime, resource_type));
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
    const out = { title: null, canonical: null, meta: [], ld_json: [], players: [], scripts: [], posters: [], html: null, ready_state: null };
    try { out.ready_state = document.readyState || null; } catch (e) {}
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

/// What the diagnostic re-sniff saw that a passive look cannot (plan P10.2).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Container types the page's player handed to Media Source Extensions.
    pub mse_types: Vec<String>,
    /// Key systems the page asked about. A probe alone proves nothing: players
    /// and embeds ask at load on clear content too.
    pub key_systems: Vec<String>,
    /// `setMediaKeys` calls with real keys (key system when readable, else "1").
    pub set_keys: Vec<String>,
    /// `generateRequest` calls (the init data type).
    pub gen_requests: Vec<String>,
    /// `encrypted` events: the media itself is encrypted (the init data type).
    pub encrypted: Vec<String>,
    /// Sources of the media elements that were started (http, https, blob).
    pub played_srcs: Vec<String>,
    /// Media files named by ad responses (VAST); they are not the page's video.
    pub ad_media: Vec<String>,
    /// The run really looked: the browser started, the page's own document
    /// answered 2xx and the page facts were read. Without it, an empty result
    /// says nothing about the page.
    pub complete: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EvidenceClass {
    /// The media is encrypted and the page set keys or asked for a licence.
    Protected,
    /// A media request was refused for want of a login, or the page is a paywall.
    LoginWall,
    /// A media request was refused for the region (451).
    GeoBlocked,
    /// The page really has a video.
    Strong,
    /// Something player-shaped, nothing that proves a video.
    Weak,
    /// Nothing video-like at all.
    None,
}

impl EvidenceClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Protected => "protected",
            Self::LoginWall => "login-wall",
            Self::GeoBlocked => "geo-blocked",
            Self::Strong => "strong",
            Self::Weak => "weak",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    pub class: EvidenceClass,
    /// Short English tokens (hosts, never full addresses), strongest first, at most 8.
    pub signals: Vec<String>,
    /// The diagnostic re-sniff ran, so `None` means "looked twice".
    pub diagnosed: bool,
}

const MAX_SIGNALS: usize = 8;

fn host_of(url: &str) -> Option<String> {
    let u = if url.starts_with("//") { format!("https:{url}") } else { url.to_string() };
    reqwest::Url::parse(&u).ok()?.host_str().map(|h| h.to_ascii_lowercase())
}

fn is_http(url: &str) -> bool {
    let l = url.trim_start().to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

/// A key system or media type as a short safe token.
fn token(s: &str, max: usize) -> String {
    s.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/' | '+'))
        .take(max)
        .collect()
}

fn regex_src_attr() -> &'static regex::Regex {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    R.get_or_init(|| regex::Regex::new(r#"(?is)\s(?:data-)?src\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>]+))"#).unwrap())
}

/// The first `src` (or `data-src`) attribute of an HTML start tag.
fn src_of_tag(tag: &str) -> Option<String> {
    let c = regex_src_attr().captures(tag)?;
    let v = c.get(1).or_else(|| c.get(2)).or_else(|| c.get(3))?.as_str().trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// The start tags named `name` in `html`.
fn start_tags<'a>(html: &'a str, name: &'static str) -> impl Iterator<Item = &'a str> {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = R.get_or_init(|| regex::Regex::new(r#"(?is)<(?:video|source|iframe|amp-video|amp-iframe)\b[^>]*>"#).unwrap());
    re.find_iter(html).map(|m| m.as_str()).filter(move |t| {
        let n: String = t[1..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-').collect();
        n.eq_ignore_ascii_case(name)
    })
}

/// A known video player's address, as a short name.
fn embed_name(src: &str) -> Option<&'static str> {
    let u = if src.starts_with("//") { format!("https:{src}") } else { src.to_string() };
    let url = reqwest::Url::parse(&u).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let path = url.path();
    let seg_after = |prefix: &str| path.strip_prefix(prefix).map(|r| r.trim_matches('/')).filter(|r| !r.is_empty());
    let on = |d: &str| host == d || host.ends_with(&format!(".{d}"));
    if (on("youtube.com") || on("youtube-nocookie.com")) && seg_after("/embed/").is_some() {
        return Some("youtube");
    }
    if host == "player.vimeo.com" {
        if let Some(id) = seg_after("/video/") {
            if id.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                return Some("vimeo");
            }
        }
    }
    if on("dailymotion.com") && seg_after("/embed/video/").is_some() {
        return Some("dailymotion");
    }
    if host == "players.brightcove.net" {
        return Some("brightcove");
    }
    if (host == "cdn.jwplayer.com" && path.starts_with("/players")) || on("jwplatform.com") {
        return Some("jwplayer");
    }
    if host == "player.glomex.com" {
        return Some("glomex");
    }
    if on("ert.gr") && path.starts_with("/webtv") {
        return Some("ert");
    }
    None
}

fn is_false_free(ld: &str) -> bool {
    let flat: String = ld.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_ascii_lowercase();
    flat.contains("\"isaccessibleforfree\":false") || flat.contains("\"isaccessibleforfree\":\"false\"")
}

fn push_signal(list: &mut Vec<String>, s: String) {
    if !s.is_empty() && !list.contains(&s) {
        list.push(s);
    }
}

/// Whether `html` carries attribute `name` with a non-empty value.
fn has_attr(html: &str, name: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    lower.match_indices(name).any(|(i, _)| {
        let prev = if i > 0 { lower.as_bytes()[i - 1] } else { b'x' };
        let before_ok = !(prev.is_ascii_alphanumeric() || prev == b'-' || prev == b'_');
        let rest = lower[i + name.len()..].trim_start();
        before_ok
            && rest.strip_prefix('=').is_some_and(|v| {
                let v = v.trim_start().trim_start_matches(['"', '\'']);
                v.chars().next().is_some_and(|c| !c.is_whitespace() && c != '>')
            })
    })
}

/// Decide from what the browser saw whether the page has a video.
///
/// Pure: the caller supplies the ad check (`is_ad`, the ad-block lists in the
/// daemon). Order matters and the first class that matches wins: DRM, login,
/// region, then a real video, then something player-shaped. An ad's media
/// file never counts as the page's video.
pub fn classify(
    snap: &PageSnapshot,
    sniffed_streams: &[String],
    diag: Option<&Diagnostic>,
    is_ad: impl Fn(&str) -> bool,
) -> Evidence {
    let diagnosed = diag.is_some_and(|d| d.complete);
    let empty = Diagnostic::default();
    let d = diag.unwrap_or(&empty);
    let media: Vec<&NetworkEntry> = snap.network.iter().filter(|n| n.is_media_like()).collect();
    let is_real_ok = |n: &NetworkEntry| {
        n.status.is_some_and(|s| (200..300).contains(&s)) && !is_ad(&n.url) && !d.ad_media.iter().any(|a| a == &n.url)
    };

    // Only use of DRM counts: an encrypted-media event, keys set on an
    // element, or a licence request. A key-system probe is a signal only.
    let mut protected: Vec<String> = Vec::new();
    let in_use = !(d.set_keys.is_empty() && d.gen_requests.is_empty() && d.encrypted.is_empty());
    let mut probes: Vec<String> = Vec::new();
    if in_use {
        for k in d.key_systems.iter().chain(d.set_keys.iter()).filter(|k| k.contains('.')) {
            push_signal(&mut protected, format!("eme:{}", token(k, 64)));
        }
        if !d.set_keys.is_empty() {
            push_signal(&mut protected, "drm:setkeys".into());
        }
        for t in &d.gen_requests {
            push_signal(&mut protected, format!("drm:genreq:{}", token(t, 32)));
        }
        for t in &d.encrypted {
            push_signal(&mut protected, format!("drm:encrypted:{}", token(t, 32)));
        }
    } else {
        for k in &d.key_systems {
            push_signal(&mut probes, format!("eme-probe:{}", token(k, 64)));
        }
    }

    let mut login: Vec<String> = Vec::new();
    for n in media.iter().filter(|n| n.status == Some(401)) {
        push_signal(&mut login, format!("http-401 {}", host_of(&n.url).unwrap_or_default()));
    }
    let any_media_ok = media.iter().any(|n| is_real_ok(n));
    if !any_media_ok && snap.ld_json.iter().any(|l| is_false_free(l)) {
        push_signal(&mut login, "ld+json isAccessibleForFree=false".into());
    }

    let mut geo: Vec<String> = Vec::new();
    for n in media.iter().filter(|n| n.status == Some(451)) {
        push_signal(&mut geo, format!("http-451 {}", host_of(&n.url).unwrap_or_default()));
    }

    let mut strong: Vec<String> = Vec::new();
    if !sniffed_streams.is_empty() {
        push_signal(&mut strong, format!("sniffer:{} streams", sniffed_streams.len()));
    }
    for n in media.iter().filter(|n| is_real_ok(n)) {
        let kind = n.mime.as_deref().map(|m| token(m, 64)).filter(|m| !m.is_empty()).unwrap_or_else(|| "media".into());
        push_signal(&mut strong, format!("network:{kind} {}", host_of(&n.url).unwrap_or_default()));
    }
    for t in d.mse_types.iter().filter(|t| t.trim().to_ascii_lowercase().starts_with("video/")) {
        push_signal(&mut strong, format!("mse:{}", token(t, 64)));
    }
    let played_ok = |s: &&String| {
        if is_http(s) {
            !is_ad(s) && !d.ad_media.iter().any(|a| a == *s)
        } else {
            s.trim_start().to_ascii_lowercase().starts_with("blob:")
        }
    };
    for s in d.played_srcs.iter().filter(played_ok) {
        push_signal(&mut strong, format!("played:{}", if is_http(s) { host_of(s).unwrap_or_default() } else { "blob".into() }));
    }
    for p in &snap.players {
        if start_tags(&p.html, "video").chain(start_tags(&p.html, "source")).any(|t| src_of_tag(t).is_some()) {
            push_signal(&mut strong, "video-src".into());
        }
        for t in start_tags(&p.html, "iframe").chain(start_tags(&p.html, "amp-iframe")) {
            if let Some(name) = src_of_tag(t).as_deref().and_then(embed_name) {
                push_signal(&mut strong, format!("embed:{name}"));
            }
        }
        for a in ["data-video-id", "data-media-id", "data-jwplayer-id", "mediaid"] {
            if has_attr(&p.html, a) {
                push_signal(&mut strong, format!("attr:{a}"));
            }
        }
    }
    if snap.ld_json.iter().any(|l| l.contains("\"VideoObject\"") && (l.contains("\"contentUrl\"") || l.contains("\"embedUrl\""))) {
        push_signal(&mut strong, "ld+json VideoObject".into());
    }
    for (k, v) in &snap.meta {
        let k = k.to_ascii_lowercase();
        if matches!(k.as_str(), "og:video" | "og:video:url" | "og:video:secure_url" | "twitter:player") && !v.trim().is_empty() {
            push_signal(&mut strong, k);
        }
    }
    static MANIFEST: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let manifest = MANIFEST.get_or_init(|| regex::Regex::new(r#"(?i)https?://[^\s"'<>\\]+?\.(m3u8|mpd)\b"#).unwrap());
    for w in &snap.scripts {
        let w = w.replace("\\/", "/");
        if let Some(m) = manifest.find_iter(&w).find(|m| !is_ad(m.as_str())) {
            let ext = if m.as_str().to_ascii_lowercase().contains(".m3u8") { "m3u8" } else { "mpd" };
            push_signal(&mut strong, format!("script:{ext} {}", host_of(m.as_str()).unwrap_or_default()));
        }
    }

    let mut weak: Vec<String> = Vec::new();
    if snap.players.iter().any(|p| {
        let open = p.html.split('>').next().unwrap_or("").to_ascii_lowercase();
        matches!(p.tag.as_str(), "video" | "iframe") || open.contains("player")
    }) {
        push_signal(&mut weak, "player-container".into());
    }
    let path_has_video = [Some(snap.page_url.as_str()), snap.final_url.as_deref()].into_iter().flatten().any(|u| {
        reqwest::Url::parse(u).is_ok_and(|u| {
            let p = u.path().to_ascii_lowercase();
            p.contains("/video/") || p.contains("/videos/")
        })
    });
    if path_has_video {
        push_signal(&mut weak, "url:/video/".into());
    }
    if snap.meta.iter().any(|(k, v)| k.eq_ignore_ascii_case("og:type") && v.trim().to_ascii_lowercase().starts_with("video")) {
        push_signal(&mut weak, "og:type video".into());
    }

    let (class, mut signals) = if !protected.is_empty() {
        (EvidenceClass::Protected, protected)
    } else if !login.is_empty() {
        (EvidenceClass::LoginWall, login)
    } else if !geo.is_empty() {
        (EvidenceClass::GeoBlocked, geo)
    } else if !strong.is_empty() {
        (EvidenceClass::Strong, Vec::new())
    } else if !weak.is_empty() {
        (EvidenceClass::Weak, Vec::new())
    } else {
        (EvidenceClass::None, Vec::new())
    };
    // A refusal or DRM is explained by what else the page showed.
    signals.extend(strong);
    signals.extend(probes);
    if class == EvidenceClass::Weak {
        signals.extend(weak);
    }
    let mut seen = std::collections::HashSet::new();
    signals.retain(|s| seen.insert(s.clone()));
    signals.truncate(MAX_SIGNALS);
    Evidence { class, signals, diagnosed }
}

/// Whether two addresses are the same page: scheme, a leading `www.`, a
/// trailing slash, the fragment, tracking parameters (`utm_*`, `fbclid`,
/// `gclid`, `mc_cid`, `mc_eid`, `ref`) and the order of the other parameters
/// do not count. Other query parameters do: `index.php?id=1` and
/// `index.php?option=com_users` are different pages.
pub fn same_page(a: &str, b: &str) -> bool {
    fn key(u: &str) -> Option<(String, Option<u16>, String, Vec<(String, String)>)> {
        let u = reqwest::Url::parse(u).ok()?;
        let host = u.host_str()?.to_ascii_lowercase();
        let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
        let path = u.path().trim_end_matches('/').to_string();
        let mut query: Vec<(String, String)> = u
            .query_pairs()
            .map(|(k, v)| (k.to_ascii_lowercase(), v.to_string()))
            .filter(|(k, _)| !(k.starts_with("utm_") || matches!(k.as_str(), "fbclid" | "gclid" | "mc_cid" | "mc_eid" | "ref")))
            .collect();
        query.sort();
        Some((host, u.port(), path, query))
    }
    matches!((key(a), key(b)), (Some(x), Some(y)) if x == y)
}

/// Whether a diagnostic run really looked at the page asked for: its own
/// document answered 2xx, the facts were read after the page had loaded, and
/// it did not end up on another page.
pub fn run_was_complete(main_doc_ok: bool, snap: &PageSnapshot, target_url: &str) -> bool {
    main_doc_ok
        && snap.html.is_some()
        && snap.ready_state.as_deref() != Some("loading")
        && snap.final_url.as_deref().is_some_and(|f| same_page(target_url, f))
}

fn class_rank(c: EvidenceClass) -> u8 {
    match c {
        EvidenceClass::Protected => 5,
        EvidenceClass::LoginWall => 4,
        EvidenceClass::GeoBlocked => 3,
        EvidenceClass::Strong => 2,
        EvidenceClass::Weak => 1,
        EvidenceClass::None => 0,
    }
}

/// The verdict of a passive look and a diagnostic run together. Protected,
/// login and region verdicts from either side win; otherwise the stronger of
/// None < Weak < Strong, so a diagnostic that found less can never cancel what
/// the passive look saw. Signals of both are kept, the winner's first.
pub fn combine(passive: &Evidence, diagnostic: &Evidence) -> Evidence {
    let (win, other) = if class_rank(diagnostic.class) >= class_rank(passive.class) {
        (diagnostic, passive)
    } else {
        (passive, diagnostic)
    };
    let mut seen = std::collections::HashSet::new();
    let mut signals: Vec<String> = win.signals.iter().chain(other.signals.iter()).filter(|s| seen.insert((*s).clone())).cloned().collect();
    signals.truncate(MAX_SIGNALS);
    Evidence { class: win.class, signals, diagnosed: diagnostic.diagnosed }
}

/// Longest and shortest video length a page statement is believed for.
const MIN_EXPECTED_SECS: f64 = 1.0;
const MAX_EXPECTED_SECS: f64 = 6.0 * 3600.0;

/// ISO 8601 duration (`PT2M34S`, `PT1H2M3.5S`, `P0DT1H`) in seconds.
fn parse_iso8601_duration(s: &str) -> Option<f64> {
    let s = s.trim().to_ascii_uppercase();
    let mut rest = s.strip_prefix('P')?;
    let mut total = 0.0f64;
    let mut in_time = false;
    let mut any = false;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix('T') {
            if in_time {
                return None;
            }
            in_time = true;
            rest = r;
            continue;
        }
        let end = rest.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
        let n: f64 = rest[..end].parse().ok()?;
        let unit = rest[end..].chars().next()?;
        let secs = match (in_time, unit) {
            (false, 'D') => 86400.0,
            (true, 'H') => 3600.0,
            (true, 'M') => 60.0,
            (true, 'S') => 1.0,
            _ => return None,
        };
        total += n * secs;
        any = true;
        rest = &rest[end + 1..];
    }
    (any && total.is_finite()).then_some(total)
}

fn plausible_secs(v: f64) -> Option<f64> {
    (v.is_finite() && v > MIN_EXPECTED_SECS && v <= MAX_EXPECTED_SECS).then_some(v)
}

fn is_video_object(v: &Value) -> bool {
    match v.get("@type") {
        Some(Value::String(t)) => t == "VideoObject",
        Some(Value::Array(a)) => a.iter().any(|t| t.as_str() == Some("VideoObject")),
        _ => false,
    }
}

/// First VideoObject duration in a JSON-LD value, through arrays, `@graph` and
/// nested objects (bounded depth).
fn ld_video_duration(v: &Value, depth: usize) -> Option<f64> {
    if depth > 6 {
        return None;
    }
    match v {
        Value::Array(a) => a.iter().find_map(|x| ld_video_duration(x, depth + 1)),
        Value::Object(o) => {
            if is_video_object(v) {
                if let Some(d) = o.get("duration").and_then(Value::as_str).and_then(parse_iso8601_duration).and_then(plausible_secs) {
                    return Some(d);
                }
            }
            o.values().find_map(|x| ld_video_duration(x, depth + 1))
        }
        _ => None,
    }
}

/// The length the page itself states for its video, in seconds: a JSON-LD
/// VideoObject `duration`, then `og:video:duration` / `video:duration`
/// (seconds), then `itemprop=duration` (ISO 8601). Values of 1 s or less, or
/// over 6 h, are ignored as junk.
pub fn expected_duration_secs(snap: &PageSnapshot) -> Option<f64> {
    for raw in &snap.ld_json {
        if let Ok(v) = serde_json::from_str::<Value>(raw) {
            if let Some(d) = ld_video_duration(&v, 0) {
                return Some(d);
            }
        }
    }
    fn meta<'a>(snap: &'a PageSnapshot, names: &'a [&str]) -> impl Iterator<Item = &'a str> {
        snap.meta.iter().filter(move |(k, _)| names.iter().any(|n| k.eq_ignore_ascii_case(n))).map(|(_, v)| v.trim())
    }
    for v in meta(snap, &["og:video:duration", "video:duration"]) {
        if let Some(d) = v.parse::<f64>().ok().and_then(plausible_secs) {
            return Some(d);
        }
    }
    for v in meta(snap, &["duration"]) {
        if let Some(d) = parse_iso8601_duration(v).and_then(plausible_secs) {
            return Some(d);
        }
    }
    None
}

/// Whether a response may be an ad server's VAST reply worth reading: an XML
/// answer to a script's request (not a page, a stylesheet or an image).
pub fn vast_candidate(mime: &str, resource_type: &str) -> bool {
    let m = mime.to_ascii_lowercase();
    m.contains("xml")
        && !m.contains("dash")
        && !m.contains("svg")
        && !m.contains("xhtml")
        && matches!(resource_type, "Xhr" | "Fetch" | "Other")
}

/// The media files an ad server's VAST reply names (`<MediaFile>`, CDATA
/// allowed). Best effort: anything else in the text is ignored.
pub fn vast_media_files(xml: &str) -> Vec<String> {
    static R: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = R.get_or_init(|| {
        regex::Regex::new(r#"(?is)<MediaFile\b[^>]*>\s*(?:<!\[CDATA\[(.*?)\]\]>|([^<]*))\s*</MediaFile>"#).unwrap()
    });
    re.captures_iter(xml)
        .filter_map(|c| c.get(1).or_else(|| c.get(2)).map(|m| m.as_str().trim().to_string()))
        .filter(|u| !u.is_empty())
        .take(64)
        .collect()
}

/// Installed before any page script runs by the diagnostic re-sniff. Fixed
/// text; the records go out through `console.debug` and nothing is changed
/// for the page: every wrapper calls the original.
pub const DIAG_JS: &str = r#"
(() => {
    try {
        if (window.__omniDiagOn) return;
        Object.defineProperty(window, '__omniDiagOn', { value: true });
        const counts = { mse: 0, eme: 0, play: 0, setkeys: 0, genreq: 0, encrypted: 0 };
        const rec = (k, v) => {
            try {
                if (counts[k] >= 32) return;
                counts[k]++;
                console.debug('__omni_diag', JSON.stringify({ k: k, v: String(v).slice(0, 512) }));
            } catch (e) {}
        };
        try {
            const o = MediaSource.prototype.addSourceBuffer;
            MediaSource.prototype.addSourceBuffer = function (t) { rec('mse', t); return o.apply(this, arguments); };
        } catch (e) {}
        try {
            const o = Navigator.prototype.requestMediaKeySystemAccess;
            if (o) Navigator.prototype.requestMediaKeySystemAccess = function (ks) { rec('eme', ks); return o.apply(this, arguments); };
        } catch (e) {}
        try {
            const o = HTMLMediaElement.prototype.setMediaKeys;
            if (o) HTMLMediaElement.prototype.setMediaKeys = function (m) {
                try { if (m) rec('setkeys', (m && m.keySystem) || '1'); } catch (e) {}
                return o.apply(this, arguments);
            };
        } catch (e) {}
        try {
            const o = MediaKeySession.prototype.generateRequest;
            if (o) MediaKeySession.prototype.generateRequest = function (t) { rec('genreq', t); return o.apply(this, arguments); };
        } catch (e) {}
        try {
            window.addEventListener('encrypted', (e) => { rec('encrypted', (e && e.initDataType) || '1'); }, true);
        } catch (e) {}
        try {
            const o = HTMLMediaElement.prototype.play;
            HTMLMediaElement.prototype.play = function () { rec('play', this.currentSrc || this.src || ''); return o.apply(this, arguments); };
        } catch (e) {}
    } catch (e) {}
})();
"#;

impl Diagnostic {
    /// One `__omni_diag` record from the page: `{"k": "mse"|"eme"|"play", "v": "..."}`.
    /// Anything else is ignored. Each list stays small.
    pub fn record(&mut self, json: &str) {
        let Ok(v) = serde_json::from_str::<Value>(json) else { return };
        let (Some(k), Some(val)) = (v.get("k").and_then(Value::as_str), v.get("v").and_then(Value::as_str)) else { return };
        let list = match k {
            "mse" => &mut self.mse_types,
            "eme" => &mut self.key_systems,
            "setkeys" => &mut self.set_keys,
            "genreq" => &mut self.gen_requests,
            "encrypted" => &mut self.encrypted,
            "play" => &mut self.played_srcs,
            _ => return,
        };
        let val = val.trim();
        if val.is_empty() || list.len() >= 32 || list.iter().any(|x| x == val) {
            return;
        }
        list.push(val.chars().take(512).collect());
    }
}

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

    // ---- classifier (P10.2) ----

    fn snap() -> PageSnapshot {
        PageSnapshot::new("https://news.example/story?utm=1")
    }

    fn net_st(url: &str, mime: Option<&str>, status: i64) -> NetworkEntry {
        NetworkEntry { status: Some(status), ..net(url, mime) }
    }

    fn player(tag: &str, html: &str) -> PlayerBlock {
        PlayerBlock { tag: tag.into(), html: html.into(), index: 0, in_article: true }
    }

    fn no_ads(_: &str) -> bool {
        false
    }

    fn class_of(s: &PageSnapshot) -> EvidenceClass {
        classify(s, &[], None, no_ads).class
    }

    #[test]
    fn an_empty_snapshot_is_none() {
        let e = classify(&snap(), &[], None, no_ads);
        assert_eq!(e.class, EvidenceClass::None);
        assert!(e.signals.is_empty());
        assert!(!e.diagnosed);
        assert!(!classify(&snap(), &[], Some(&Diagnostic::default()), no_ads).diagnosed, "an incomplete run proves nothing");
        let done = Diagnostic { complete: true, ..Default::default() };
        assert!(classify(&snap(), &[], Some(&done), no_ads).diagnosed);
    }

    #[test]
    fn an_incomplete_diagnostic_never_says_the_page_was_looked_at_twice() {
        // blocked, failed or cut short: nothing found is not "no video"
        let part = Diagnostic { complete: false, ..Default::default() };
        let e = classify(&snap(), &[], Some(&part), no_ads);
        assert_eq!(e.class, EvidenceClass::None);
        assert!(!e.diagnosed);
        // what it did see still counts
        let seen = Diagnostic { complete: false, mse_types: vec!["video/mp4".into()], ..Default::default() };
        assert_eq!(classify(&snap(), &[], Some(&seen), no_ads).class, EvidenceClass::Strong);
    }

    fn ev(class: EvidenceClass, signals: &[&str], diagnosed: bool) -> Evidence {
        Evidence { class, signals: signals.iter().map(|s| s.to_string()).collect(), diagnosed }
    }

    #[test]
    fn combine_keeps_the_stronger_side() {
        use EvidenceClass as C;
        let all = [C::None, C::Weak, C::Strong, C::GeoBlocked, C::LoginWall, C::Protected];
        for p in all {
            for d in all {
                let got = combine(&ev(p, &["p"], false), &ev(d, &["d"], true));
                let want = if class_rank(p) > class_rank(d) { p } else { d };
                assert_eq!(got.class, want, "passive {p:?} diagnostic {d:?}");
                assert!(got.diagnosed);
            }
        }
        // a diagnostic None never overrides a passive Weak
        let w = combine(&ev(C::Weak, &["player-container"], false), &ev(C::None, &[], true));
        assert_eq!(w.class, C::Weak);
        assert_eq!(w.signals, vec!["player-container"]);
        // winner's signals first, merged, capped
        let m = combine(&ev(C::Weak, &["a", "b"], false), &ev(C::Strong, &["b", "c"], true));
        assert_eq!(m.signals, vec!["b", "c", "a"]);
        let many: Vec<String> = (0..20).map(|i| format!("s{i}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        assert_eq!(combine(&ev(C::Strong, &refs, false), &ev(C::Strong, &refs, true)).signals.len(), 8);
    }

    #[test]
    fn only_script_made_xml_answers_are_read_as_vast() {
        assert!(vast_candidate("text/xml", "Xhr"));
        assert!(vast_candidate("application/xml", "Fetch"));
        assert!(vast_candidate("application/xml", "Other"));
        assert!(!vast_candidate("image/svg+xml", "Other"));
        assert!(!vast_candidate("application/xhtml+xml", "Xhr"));
        assert!(!vast_candidate("application/dash+xml", "Xhr"));
        assert!(!vast_candidate("text/xml", "Document"));
        assert!(!vast_candidate("text/xml", "Stylesheet"));
        assert!(!vast_candidate("application/json", "Xhr"));
    }

    #[test]
    fn drm_is_protected() {
        let d = Diagnostic {
            key_systems: vec!["com.widevine.alpha".into()],
            encrypted: vec!["cenc".into()],
            ..Default::default()
        };
        let e = classify(&snap(), &[], Some(&d), no_ads);
        assert_eq!(e.class, EvidenceClass::Protected);
        assert_eq!(e.signals[0], "eme:com.widevine.alpha");
        let k = Diagnostic { set_keys: vec!["org.w3.clearkey".into()], ..Default::default() };
        let e = classify(&snap(), &[], Some(&k), no_ads);
        assert_eq!(e.class, EvidenceClass::Protected);
        assert!(e.signals.iter().any(|s| s == "drm:setkeys"), "{:?}", e.signals);
        let g = Diagnostic { gen_requests: vec!["cenc".into()], ..Default::default() };
        assert_eq!(classify(&snap(), &[], Some(&g), no_ads).class, EvidenceClass::Protected);
        let bare = Diagnostic { set_keys: vec!["1".into()], ..Default::default() };
        assert_eq!(classify(&snap(), &[], Some(&bare), no_ads).class, EvidenceClass::Protected);
    }

    #[test]
    fn a_key_system_probe_alone_is_only_a_signal() {
        let mut s = snap();
        s.push_network(net_st("https://cdn.example/v/clip.mp4", Some("video/mp4"), 200));
        let d = Diagnostic { key_systems: vec!["com.widevine.alpha".into()], ..Default::default() };
        let e = classify(&s, &[], Some(&d), no_ads);
        assert_eq!(e.class, EvidenceClass::Strong, "{:?}", e.signals);
        assert!(e.signals.iter().any(|x| x == "eme-probe:com.widevine.alpha"), "{:?}", e.signals);
        // and with nothing else on the page, still not a class of its own
        assert_eq!(classify(&snap(), &[], Some(&d), no_ads).class, EvidenceClass::None);
    }

    #[test]
    fn a_media_401_is_a_login_wall_and_a_451_is_geo() {
        let mut s = snap();
        s.push_network(net_st("https://cdn.example/v/master.m3u8?token=abc", Some("application/vnd.apple.mpegurl"), 401));
        assert_eq!(class_of(&s), EvidenceClass::LoginWall);
        let mut g = snap();
        g.push_network(net_st("https://cdn.example/v/clip.mp4", Some("video/mp4"), 451));
        assert_eq!(class_of(&g), EvidenceClass::GeoBlocked);
    }

    #[test]
    fn a_401_on_something_that_is_not_media_is_not_a_login_wall() {
        let mut s = snap();
        s.push_network(net_st("https://api.example/session.json", Some("application/json"), 401));
        assert_eq!(class_of(&s), EvidenceClass::None);
    }

    #[test]
    fn a_paywall_page_without_media_is_a_login_wall_but_with_a_media_response_it_is_strong() {
        let mut s = snap();
        s.ld_json = vec![r#"{"@type":"NewsArticle", "isAccessibleForFree" : "False"}"#.into()];
        assert_eq!(class_of(&s), EvidenceClass::LoginWall);
        s.ld_json = vec![r#"{"isAccessibleForFree": false}"#.into()];
        assert_eq!(class_of(&s), EvidenceClass::LoginWall);
        s.push_network(net_st("https://cdn.example/v/clip.mp4", Some("video/mp4"), 206));
        assert_eq!(class_of(&s), EvidenceClass::Strong);
        let mut free = snap();
        free.ld_json = vec![r#"{"isAccessibleForFree": true}"#.into()];
        assert_eq!(class_of(&free), EvidenceClass::None);
    }

    #[test]
    fn precedence_drm_over_media_and_login_over_og_video() {
        let mut s = snap();
        s.push_network(net_st("https://cdn.example/v/clip.mp4", Some("video/mp4"), 200));
        let d = Diagnostic {
            key_systems: vec!["com.microsoft.playready".into()],
            gen_requests: vec!["cenc".into()],
            ..Default::default()
        };
        let e = classify(&s, &[], Some(&d), no_ads);
        assert_eq!(e.class, EvidenceClass::Protected);
        assert!(e.signals.iter().any(|x| x.starts_with("network:")), "{:?}", e.signals);

        let mut l = snap();
        l.meta = vec![("og:video".into(), "https://cdn.example/v.mp4".into())];
        l.push_network(net_st("https://cdn.example/v/clip.mp4", Some("video/mp4"), 401));
        assert_eq!(class_of(&l), EvidenceClass::LoginWall);
    }

    #[test]
    fn every_strong_signal_alone_is_strong() {
        let mut cases: Vec<(&str, PageSnapshot, Option<Diagnostic>, Vec<String>)> = Vec::new();
        cases.push(("sniffer", snap(), None, vec!["https://x.example/a.m3u8".into()]));
        let mut s = snap();
        s.push_network(net_st("https://cdn.example/seg/a.mp4", None, 200));
        cases.push(("network", s, None, vec![]));
        cases.push(("mse", snap(), Some(Diagnostic { mse_types: vec!["video/mp4; codecs=\"avc1.42E01E\"".into()], ..Default::default() }), vec![]));
        cases.push(("played http", snap(), Some(Diagnostic { played_srcs: vec!["https://cdn.example/a.mp4".into()], ..Default::default() }), vec![]));
        cases.push(("played blob", snap(), Some(Diagnostic { played_srcs: vec!["blob:https://news.example/1-2".into()], ..Default::default() }), vec![]));
        for (html, tag) in [
            (r#"<video muted src="/v/a.mp4"></video>"#, "video"),
            (r#"<video><source type="video/mp4" src='https://cdn.example/a.mp4'></video>"#, "video"),
            (r#"<iframe src="https://www.youtube.com/embed/abc123_-X"></iframe>"#, "iframe"),
            (r#"<iframe src="https://www.youtube-nocookie.com/embed/abc123"></iframe>"#, "iframe"),
            (r#"<iframe data-src="https://player.vimeo.com/video/12345"></iframe>"#, "iframe"),
            (r#"<iframe src="https://www.dailymotion.com/embed/video/x8abc"></iframe>"#, "iframe"),
            (r#"<iframe src="https://players.brightcove.net/1/default/index.html?videoId=2"></iframe>"#, "iframe"),
            (r#"<iframe src="https://cdn.jwplayer.com/players/abc-def.html"></iframe>"#, "iframe"),
            (r#"<iframe src="https://content.jwplatform.com/players/x.html"></iframe>"#, "iframe"),
            (r#"<iframe src="//player.glomex.com/integration/1/iframe-player.html"></iframe>"#, "iframe"),
            (r#"<iframe src="https://www.ert.gr/webtv/live-event/x"></iframe>"#, "iframe"),
            (r#"<div class="box" data-video-id="12345"></div>"#, "div"),
            (r#"<div class="box" data-media-id='9'></div>"#, "div"),
            (r#"<div id="p" data-jwplayer-id="Ab12"></div>"#, "div"),
            (r#"<div class="player" mediaid="77"></div>"#, "div"),
        ] {
            let mut s = snap();
            s.players = vec![player(tag, html)];
            cases.push((html, s, None, vec![]));
        }
        let mut ld = snap();
        ld.ld_json = vec![r#"{"@type":"VideoObject","embedUrl":"https://x.example/e"}"#.into()];
        cases.push(("ld video", ld, None, vec![]));
        for key in ["og:video", "og:video:url", "og:video:secure_url", "twitter:player"] {
            let mut s = snap();
            s.meta = vec![(key.into(), "https://x.example/v".into())];
            cases.push((key, s, None, vec![]));
        }
        for text in [
            r#"var cfg = {"file":"https://cdn.example/p/master.m3u8?x=1"};"#,
            r#"src: "https:\/\/cdn.example\/p\/manifest.mpd""#,
        ] {
            let mut s = snap();
            s.scripts = vec![text.into()];
            cases.push((text, s, None, vec![]));
        }
        for (what, s, d, streams) in cases {
            let e = classify(&s, &streams, d.as_ref(), no_ads);
            assert_eq!(e.class, EvidenceClass::Strong, "{what}: {:?}", e.signals);
            assert!(!e.signals.is_empty(), "{what}");
        }
    }

    #[test]
    fn look_alikes_are_not_strong() {
        let mut s = snap();
        s.players = vec![
            player("video", r#"<video controls><img src="/poster.jpg"></video>"#),
            player("iframe", r#"<iframe src="https://www.youtube.com/watch?v=abc"></iframe>"#),
            player("iframe", r#"<iframe src="https://www.youtube.com/embed/"></iframe>"#),
            player("iframe", r#"<iframe src="https://player.vimeo.com/video/abc"></iframe>"#),
            player("div", r#"<div class="player" data-video-id="">"#),
            player("div", r#"<div class="player" xdata-video-id="7">"#),
        ];
        s.ld_json = vec![r#"{"@type":"VideoObject","name":"x"}"#.into()];
        s.meta = vec![("og:video".into(), "  ".into())];
        s.scripts = vec!["see manifest and playlist docs, https://docs.example/m3u8-guide".into()];
        let e = classify(&s, &[], None, no_ads);
        assert_eq!(e.class, EvidenceClass::Weak, "{:?}", e.signals);
    }

    #[test]
    fn weak_signals() {
        let mut p = snap();
        p.players = vec![player("div", r#"<div class="article-player"></div>"#)];
        assert_eq!(class_of(&p), EvidenceClass::Weak);
        let mut u = PageSnapshot::new("https://news.example/videos/story-1");
        assert_eq!(class_of(&u), EvidenceClass::Weak);
        u = PageSnapshot::new("https://news.example/news/x");
        u.meta = vec![("og:type".into(), "video.other".into())];
        assert_eq!(class_of(&u), EvidenceClass::Weak);
        let mut plain = PageSnapshot::new("https://news.example/news/x");
        plain.meta = vec![("og:type".into(), "article".into())];
        plain.players = vec![player("div", r#"<div class="share-bar"></div>"#)];
        assert_eq!(class_of(&plain), EvidenceClass::None);
    }

    #[test]
    fn an_ad_response_alone_is_not_strong() {
        let mut s = snap();
        s.push_network(net_st("https://ads.tracker.example/spot.mp4", Some("video/mp4"), 200));
        assert_eq!(classify(&s, &[], None, |u| u.contains("ads.tracker.example")).class, EvidenceClass::None);
        assert_eq!(classify(&s, &[], None, no_ads).class, EvidenceClass::Strong);
        // and an ad's media does not hide a paywall
        s.ld_json = vec![r#"{"isAccessibleForFree": false}"#.into()];
        assert_eq!(classify(&s, &[], None, |u| u.contains("ads.tracker.example")).class, EvidenceClass::LoginWall);
    }

    #[test]
    fn a_playing_ad_alone_is_not_strong() {
        let by_list = Diagnostic {
            played_srcs: vec!["https://ads.tracker.example/preroll.mp4".into()],
            complete: true,
            ..Default::default()
        };
        assert_eq!(classify(&snap(), &[], Some(&by_list), |u| u.contains("ads.tracker.example")).class, EvidenceClass::None);
        assert_eq!(classify(&snap(), &[], Some(&by_list), no_ads).class, EvidenceClass::Strong);
        let by_vast = Diagnostic {
            played_srcs: vec!["https://cdn.adserver.example/spot.mp4".into()],
            ad_media: vec!["https://cdn.adserver.example/spot.mp4".into()],
            ..Default::default()
        };
        assert_eq!(classify(&snap(), &[], Some(&by_vast), no_ads).class, EvidenceClass::None);
    }

    #[test]
    fn a_response_before_its_request_is_still_matched() {
        let mut log = NetworkLog::default();
        log.on_response("7", 403, Some("text/html".into()), Some("Document".into()));
        log.on_request("7", net("https://a/page", None));
        log.on_request("8", net("https://a/x.js", None));
        let e = log.entries();
        assert_eq!(e[0].status, Some(403));
        assert_eq!(e[0].mime.as_deref(), Some("text/html"));
        assert_eq!(e[0].resource_type.as_deref(), Some("Document"));
        assert_eq!(e[1].status, Some(200), "an unrelated request is not touched: net() sets 200");
        // the pending list is bounded
        let mut log = NetworkLog::default();
        for i in 0..500 {
            log.on_response(&format!("r{i}"), 200, None, None);
        }
        assert!(log.pending.len() <= MAX_PENDING_RESPONSES);
    }

    #[test]
    fn a_log_that_dropped_a_request_drops_its_early_response() {
        let mut log = NetworkLog::default();
        for i in 0..MAX_NETWORK {
            log.on_request(&format!("a{i}"), net(&format!("https://a/{i}.js"), Some("text/javascript")));
        }
        log.on_response("z", 200, Some("text/css".into()), None);
        assert_eq!(log.pending.len(), 1);
        log.on_request("z", net("https://a/z.css", Some("text/css")));
        assert!(log.pending.is_empty(), "the response of a request that was not kept is not replayed");
        assert_eq!(log.entries().len(), MAX_NETWORK);
    }

    #[test]
    fn a_run_read_while_the_page_is_loading_is_incomplete() {
        let mut s = PageSnapshot::new("https://news.example/a");
        s.final_url = Some("https://news.example/a".into());
        s.apply_page_facts(&serde_json::json!({"html": "<html></html>", "ready_state": "loading"}));
        assert_eq!(s.ready_state.as_deref(), Some("loading"));
        assert!(!run_was_complete(true, &s, "https://news.example/a"));
        s.apply_page_facts(&serde_json::json!({"html": "<html></html>", "ready_state": "complete"}));
        assert!(run_was_complete(true, &s, "https://news.example/a"));
        assert!(!run_was_complete(false, &s, "https://news.example/a"), "main document not 2xx");
        assert!(!run_was_complete(true, &s, "https://news.example/other"), "ended elsewhere");
        s.apply_page_facts(&serde_json::json!({"html": "<html></html>", "ready_state": "interactive"}));
        assert!(run_was_complete(true, &s, "https://news.example/a"));
    }

    #[test]
    fn the_same_page_ignores_scheme_www_slash_and_query() {
        assert!(same_page("https://news.example/a/b#f", "http://www.News.example/a/b/"));
        assert!(same_page("https://news.example/a?utm_source=x&id=2&fbclid=z", "https://news.example/a?id=2"));
        assert!(same_page("https://news.example/a?utm_source=x", "https://news.example/a"));
        assert!(same_page("https://news.example/a?b=1&c=2", "https://news.example/a?c=2&b=1"));
        assert!(!same_page("https://news.example/index.php?id=123", "https://news.example/index.php?option=com_users&view=login"));
        assert!(!same_page("https://news.example/a?id=1", "https://news.example/a?id=2"));
        assert!(same_page("https://news.example/a", "https://news.example/a/"));
        assert!(!same_page("https://news.example/a", "https://news.example/login"));
        assert!(!same_page("https://news.example/a", "https://other.example/a"));
        assert!(!same_page("https://news.example/a", "not a url"));
    }

    #[test]
    fn a_vast_media_file_alone_is_not_strong() {
        let mut s = snap();
        s.push_network(net_st("https://cdn.adserver.example/creative/spot.mp4", Some("video/mp4"), 200));
        let d = Diagnostic { ad_media: vec!["https://cdn.adserver.example/creative/spot.mp4".into()], ..Default::default() };
        assert_eq!(classify(&s, &[], Some(&d), no_ads).class, EvidenceClass::None);
    }

    #[test]
    fn signals_are_short_hosts_only() {
        let mut s = PageSnapshot::new("https://news.example/videos/a?secret=1");
        s.push_network(net_st("https://cdn.example/v/master.m3u8?token=SECRET", Some("application/vnd.apple.mpegurl"), 200));
        s.scripts = vec![r#"x "https://cdn2.example/p/a.mpd?sig=SECRET""#.into()];
        s.players = vec![player("iframe", r#"<iframe src="https://www.youtube.com/embed/abc?key=SECRET"></iframe>"#)];
        s.meta = vec![("og:video".into(), "https://cdn3.example/a.mp4?k=SECRET".into())];
        s.ld_json = vec![r#"{"@type":"VideoObject","contentUrl":"https://cdn4.example/a.mp4?k=SECRET"}"#.into()];
        let d = Diagnostic {
            played_srcs: vec!["https://cdn5.example/a.mp4?k=SECRET".into()],
            mse_types: vec!["video/mp4; codecs=\"avc1.42E01E\"".into()],
            key_systems: vec!["com.widevine.alpha".into()],
            encrypted: vec!["cenc".into()],
            ..Default::default()
        };
        let e = classify(&s, &["https://cdn.example/v/master.m3u8?token=SECRET".into()], Some(&d), no_ads);
        assert!(e.signals.len() <= 8);
        assert!(!e.signals.is_empty());
        for sig in &e.signals {
            assert!(!sig.contains('?') && !sig.contains("://") && !sig.contains("SECRET") && sig.len() < 100, "{sig}");
        }
        assert!(e.signals.iter().any(|x| x == "network:application/vnd.apple.mpegurl cdn.example"), "{:?}", e.signals);
    }

    #[test]
    fn vast_media_files_are_read_with_or_without_cdata() {
        let xml = r#"<VAST><Ad><InLine><Creatives><Creative><Linear><MediaFiles>
            <MediaFile delivery="progressive" type="video/mp4"><![CDATA[ https://cdn.adserver.example/a.mp4?x=1 ]]></MediaFile>
            <MediaFile type="video/webm">https://cdn.adserver.example/b.webm</MediaFile>
            <MediaFile></MediaFile>
        </MediaFiles></Linear></Creative></Creatives></InLine></Ad></VAST>"#;
        assert_eq!(
            vast_media_files(xml),
            vec!["https://cdn.adserver.example/a.mp4?x=1".to_string(), "https://cdn.adserver.example/b.webm".to_string()]
        );
        assert!(vast_media_files("<html>not vast</html>").is_empty());
    }

    #[test]
    fn diagnostic_records_are_filtered_and_capped() {
        let mut d = Diagnostic::default();
        d.record(r#"{"k":"eme","v":"com.widevine.alpha"}"#);
        d.record(r#"{"k":"eme","v":"com.widevine.alpha"}"#);
        d.record(r#"{"k":"mse","v":"video/mp4"}"#);
        d.record(r#"{"k":"play","v":"https://a.example/x.mp4"}"#);
        d.record(r#"{"k":"setkeys","v":"1"}"#);
        d.record(r#"{"k":"genreq","v":"cenc"}"#);
        d.record(r#"{"k":"encrypted","v":"cenc"}"#);
        assert_eq!((d.set_keys.len(), d.gen_requests.len(), d.encrypted.len()), (1, 1, 1));
        d.record(r#"{"k":"other","v":"x"}"#);
        d.record(r#"{"k":"mse","v":""}"#);
        d.record("not json");
        assert_eq!(d.key_systems, vec!["com.widevine.alpha"]);
        assert_eq!(d.mse_types, vec!["video/mp4"]);
        assert_eq!(d.played_srcs.len(), 1);
        for i in 0..100 {
            d.record(&format!(r#"{{"k":"play","v":"https://a.example/{i}.mp4"}}"#));
        }
        assert_eq!(d.played_srcs.len(), 32);
    }

    fn snap_with(ld: &[&str], meta: &[(&str, &str)]) -> PageSnapshot {
        let mut s = PageSnapshot::new("https://news.example/a");
        s.ld_json = ld.iter().map(|x| x.to_string()).collect();
        s.meta = meta.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        s
    }

    #[test]
    fn expected_duration_table() {
        let vo = |d: &str| format!(r#"{{"@type":"VideoObject","duration":"{d}"}}"#);
        let cases: Vec<(PageSnapshot, Option<f64>)> = vec![
            (snap_with(&[&vo("PT2M34S")], &[]), Some(154.0)),
            (snap_with(&[&vo("PT1H2M3.5S")], &[]), Some(3723.5)),
            (snap_with(&[r#"{"@graph":[{"@type":"WebPage"},{"@type":["Thing","VideoObject"],"duration":"PT45S"}]}"#], &[]), Some(45.0)),
            (snap_with(&[&format!("[{}]", vo("PT1M"))], &[]), Some(60.0)),
            (snap_with(&[], &[("og:video:duration", "154")]), Some(154.0)),
            (snap_with(&[], &[("video:duration", "30.5")]), Some(30.5)),
            (snap_with(&[], &[("duration", "PT3M")]), Some(180.0)),
            (snap_with(&[&vo("PT2M")], &[("og:video:duration", "999")]), Some(120.0)),
            (snap_with(&[&vo("junk")], &[]), None),
            (snap_with(&["not json"], &[("og:video:duration", "abc")]), None),
            (snap_with(&[&vo("PT0S")], &[]), None),
            (snap_with(&[&vo("PT1S")], &[]), None),
            (snap_with(&[&vo("PT10H")], &[]), None),
            (snap_with(&[], &[("og:video:duration", "0")]), None),
            (snap_with(&[], &[("og:video:duration", "36000")]), None),
            (snap_with(&[r#"{"@type":"NewsArticle","duration":"PT2M"}"#], &[]), None),
            (snap_with(&[&vo("PT")], &[]), None),
            (PageSnapshot::new("https://x.example/"), None),
        ];
        for (i, (s, want)) in cases.iter().enumerate() {
            assert_eq!(expected_duration_secs(s), *want, "case {i}");
        }
    }

    #[test]
    fn ad_media_is_capped_and_deduplicated_in_the_snapshot() {
        let mut s = PageSnapshot::new("https://news.example/a");
        assert!(s.push_ad_media("https://ads.example/a.mp4"));
        assert!(!s.push_ad_media("https://ads.example/a.mp4"));
        for i in 0..100 {
            s.push_ad_media(&format!("https://ads.example/{i}.mp4"));
        }
        assert_eq!(s.ad_media.len(), MAX_AD_MEDIA);
        let back: PageSnapshot = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.ad_media.len(), MAX_AD_MEDIA);
        // An older saved snapshot without the field still loads.
        let old: PageSnapshot = serde_json::from_str(r#"{"page_url":"https://x.example/","final_url":null,"title":null,"canonical":null,"meta":[],"ld_json":[],"network":[],"players":[],"scripts":[],"posters":[],"html":null,"captured_at":"t"}"#).unwrap();
        assert!(old.ad_media.is_empty());
    }

    #[test]
    fn capping_drops_ad_media_before_any_network_entry() {
        let mut s = PageSnapshot::new("https://news.example/a");
        for i in 0..20 {
            s.push_network(NetworkEntry { url: format!("https://news.example/r/{i}"), ..Default::default() });
        }
        for i in 0..64 {
            s.push_ad_media(&format!("https://ads.example/{i}/{}", "x".repeat(400)));
        }
        let cap = serde_json::to_string(&s).unwrap().len() - 1000;
        let back: PageSnapshot = serde_json::from_str(&s.to_capped_json(cap)).unwrap();
        assert_eq!(back.network.len(), 20, "network entries survive while ad_media can still go");
        assert!(back.ad_media.is_empty());
    }
}
