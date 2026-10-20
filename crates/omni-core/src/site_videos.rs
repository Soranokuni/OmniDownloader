//! Site furniture (P3.16): a video the browser sees on two or more different
//! articles of one site within a week is the site's own (a promo, a show, a
//! recommendation), not the article's. It is never a job's video and never
//! queued; MCR is only offered it.
//!
//! The memory holds hashes only: a stream address may carry a token, so no
//! address is ever stored. Pure helpers here; the SQL is in `repository.rs`.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};

use crate::dependencies::sha256_hex;
use crate::urlnorm;

/// A video seen on this many different articles (the one being asked about
/// included) is site furniture.
pub const SITE_WIDE_MIN_ARTICLES: usize = 2;
/// How far back a sighting counts.
pub const SITE_WIDE_WINDOW_DAYS: i64 = 7;
/// How long retention keeps a sighting (twice the window).
pub const SITE_VIDEOS_KEEP_DAYS: i64 = 14;

/// The video address as it is compared: scheme and host in lower case, tracking
/// parameters (`utm_*`, `fbclid`, `gclid`) dropped, and nothing else touched. Two
/// clips that differ only in `source=` or a `#id=` fragment stay two videos; the
/// dedup normaliser, which drops those, is too coarse here.
pub fn video_key(raw: &str) -> String {
    let raw = raw.trim();
    if let Some(post) = platform_post(raw) {
        return post;
    }
    let Ok(mut url) = reqwest::Url::parse(raw) else { return raw.to_string() };
    if url.query().is_some() {
        let kept: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(k, _)| !(k.starts_with("utm_") || k == "fbclid" || k == "gclid"))
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if kept.is_empty() {
            url.set_query(None);
        } else {
            url.query_pairs_mut().clear().extend_pairs(kept);
        }
    }
    url.to_string()
}

/// A video platform's post in its one canonical form, so the address yt-dlp's page scan gives
/// (`youtube.com/embed/ID?si=...`) and the one the browser records (`watch?v=ID`) are one video:
/// YouTube (`watch`, `embed`, `shorts`, `youtu.be`, `m.`, `-nocookie`) as `https://www.youtube.com/watch?v=ID`, an X
/// or Twitter post as `https://x.com/i/status/ID`, Streamable as `https://streamable.com/ID`.
fn platform_post(raw: &str) -> Option<String> {
    let norm = urlnorm::normalize(raw);
    let rest = norm.split("://").nth(1)?;
    let (host, path) = rest.split_once('/').map(|(h, p)| (h, format!("/{p}"))).unwrap_or((rest, String::new()));
    let path_only = path.split(['?', '#']).next().unwrap_or("");
    match host {
        // Only a real video id: `embed/videoseries?list=…` and `embed/live_stream?channel=…`
        // normalise to `watch?v=videoseries` / `watch?v=live_stream`, which would make every
        // playlist (or every channel's live embed) one video. Those keep the narrow key.
        "www.youtube.com" if path_only == "/watch" => {
            let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
            let id = query.split('&').find_map(|kv| kv.strip_prefix("v="))?;
            is_youtube_id(id).then(|| format!("https://www.youtube.com/watch?v={id}"))
        }
        "x.com" if path_only.starts_with("/i/status/") => Some(norm.clone()),
        "www.youtube-nocookie.com" | "youtube-nocookie.com" => {
            let id = path_only.strip_prefix("/embed/")?.split('/').next()?;
            is_youtube_id(id).then(|| format!("https://www.youtube.com/watch?v={id}"))
        }
        "streamable.com" | "www.streamable.com" => {
            let mut parts = path_only.split('/').filter(|p| !p.is_empty());
            let first = parts.next()?;
            let id = if matches!(first, "e" | "o" | "s") { parts.next()? } else { first };
            id.chars().all(|c| c.is_ascii_alphanumeric()).then(|| format!("https://streamable.com/{id}"))
        }
        _ => None,
    }
}

/// A YouTube video id: eleven characters of `[A-Za-z0-9_-]`. "videoseries" (a playlist
/// embed) and "live_stream" (a channel's live embed) are eleven characters too, but name
/// no single video.
fn is_youtube_id(id: &str) -> bool {
    id.len() == 11
        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !matches!(id, "videoseries" | "live_stream")
}

/// Lower-case hex SHA-256 of the video address ([`video_key`]).
pub fn video_hash(url: &str) -> String {
    sha256_hex(video_key(url).as_bytes())
}

/// The oldest sighting that still counts at `now`.
pub fn window_start(now: DateTime<Utc>) -> DateTime<Utc> {
    now - Duration::days(SITE_WIDE_WINDOW_DAYS)
}

fn page_hash_of(url: &str) -> String {
    sha256_hex(urlnorm::normalize(url).as_bytes())
}

/// Which article of which site a sighting belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageKey {
    /// The site the page really is on: where the browser ended when that is a page, else the address asked.
    pub domain: String,
    /// What a sighting is stored under: the address asked for (the job's or the canary's), normalised.
    /// It does not move between runs, as the browser's final address and the canonical do.
    pub page_hash: String,
    /// Every address this article is known by (asked, ended on, canonical), hashed. When counting
    /// other articles none of these is another article.
    pub identities: Vec<String>,
}

/// The key of the article asked for at `page_url`; `final_url` is where the browser ended and
/// `canonical` the page's own canonical address, when known.
pub fn page_key(page_url: &str, final_url: Option<&str>, canonical: Option<&str>) -> Option<PageKey> {
    let usable = |u: &str| u.trim().starts_with("http://") || u.trim().starts_with("https://");
    let ended = final_url.filter(|u| usable(u));
    let domain = urlnorm::registrable_domain(ended.unwrap_or(page_url))?.to_lowercase();
    let page_hash = page_hash_of(page_url);
    let mut identities = vec![page_hash.clone()];
    for u in [ended, canonical.filter(|u| usable(u))].into_iter().flatten() {
        let h = page_hash_of(u);
        if !identities.contains(&h) {
            identities.push(h);
        }
    }
    Some(PageKey { domain, page_hash, identities })
}

/// One video the site shows on other articles too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Furniture {
    pub url: String,
    /// On how many other articles of the site it was seen this week.
    pub other_articles: usize,
}

/// `urls` without the site-wide ones (`wide`: video hash to the number of
/// distinct articles it was seen on, the current one included), and those
/// dropped. Order kept; one address named twice is kept or dropped as one.
pub fn split_site_wide(urls: &[String], wide: &HashMap<String, usize>) -> (Vec<String>, Vec<Furniture>) {
    let mut kept = Vec::new();
    let mut dropped: Vec<Furniture> = Vec::new();
    for u in urls {
        match wide.get(&video_hash(u)) {
            Some(pages) => {
                if !dropped.iter().any(|f| f.url == *u) {
                    dropped.push(Furniture { url: u.clone(), other_articles: pages.saturating_sub(1).max(1) });
                }
            }
            None => kept.push(u.clone()),
        }
    }
    (kept, dropped)
}

/// The hashes of `urls`, each once.
pub fn hashes_of(urls: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for u in urls {
        let h = video_hash(u);
        if !out.contains(&h) {
            out.push(h);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_is_stored_under_the_address_asked_and_known_by_every_address_it_ended_on() {
        let asked = "https://www.news.example.gr/a/story?utm_source=x";
        let plain = page_key("https://www.news.example.gr/a/story", None, None).unwrap();
        let moved = page_key(asked, Some("https://amp.news.example.gr/a/story"), Some("https://www.news.example.gr/a/story-full")).unwrap();
        assert_eq!(moved.page_hash, plain.page_hash, "the stored identity is the asked address, whatever the browser did");
        assert_eq!(moved.identities.len(), 3);
        assert!(moved.identities.contains(&plain.page_hash));
        assert_ne!(page_key("https://www.news.example.gr/a/other", None, None).unwrap().page_hash, plain.page_hash);
        assert_eq!(plain.domain, "example.gr");
        // A canonical that is not an address is ignored.
        assert_eq!(page_key("https://www.news.example.gr/a/story", None, Some("javascript:x")).unwrap(), plain);
        assert!(page_key("", None, None).is_none());
    }

    #[test]
    fn the_site_is_where_the_browser_ended_not_the_shortener_asked() {
        let k = page_key("https://t.co/AbCdEf", Some("https://www.news.example/a/story"), None).unwrap();
        assert_eq!(k.domain, "news.example");
        assert_eq!(page_key("https://t.co/AbCdEf", None, None).unwrap().domain, "t.co");
    }

    #[test]
    fn a_hash_never_contains_the_address() {
        let h = video_hash("https://cdn.example.test/v/clip.m3u8?token=SECRETVALUE");
        assert_eq!(h.len(), 64);
        assert!(!h.contains("SECRET") && !h.contains("cdn"));
        assert_eq!(h, video_hash("https://cdn.example.test/v/clip.m3u8?token=SECRETVALUE"));
        assert_ne!(h, video_hash("https://cdn.example.test/v/other.m3u8"));
    }

    #[test]
    fn clips_that_differ_only_in_a_query_parameter_or_a_fragment_are_two_videos() {
        let base = "https://player.example.test/clip.php";
        assert_ne!(video_hash(&format!("{base}?id=1&source=a")), video_hash(&format!("{base}?id=1&source=b")));
        assert_ne!(video_hash(&format!("{base}#id=1")), video_hash(&format!("{base}#id=2")));
        assert_ne!(video_hash(&format!("{base}?id=1&t=5")), video_hash(&format!("{base}?id=1")));
        // Only tracking parameters and the case of scheme and host are ignored.
        assert_eq!(video_hash(&format!("{base}?id=1&utm_source=x&fbclid=y")), video_hash("HTTPS://Player.Example.test/clip.php?id=1"));
    }

    #[test]
    fn a_platform_post_has_one_form_whatever_address_names_it() {
        let yt = video_hash("https://www.youtube.com/watch?v=ALa-5MJTLmM");
        for same in [
            "https://www.youtube.com/embed/ALa-5MJTLmM?si=rSW-LgNkTgWxoHmU",
            "https://www.youtube.com/embed/ALa-5MJTLmM?feature=oembed",
            "https://youtu.be/ALa-5MJTLmM?si=abc",
            "https://m.youtube.com/watch?v=ALa-5MJTLmM&feature=share",
            "https://www.youtube-nocookie.com/embed/ALa-5MJTLmM",
            "https://www.youtube.com/shorts/ALa-5MJTLmM",
        ] {
            assert_eq!(video_hash(same), yt, "{same}");
        }
        assert_ne!(video_hash("https://www.youtube.com/embed/OTHERidOTHR?si=x"), yt, "another id is another video");
        assert_eq!(video_hash("https://x.com/someone/status/1900"), video_hash("https://twitter.com/i/status/1900"));
        assert_eq!(video_hash("https://mobile.twitter.com/someone/status/1900?s=20"), video_hash("https://x.com/i/status/1900"));
        assert_ne!(video_hash("https://x.com/someone/status/1901"), video_hash("https://x.com/i/status/1900"));
        assert_eq!(video_hash("https://streamable.com/e/abc123"), video_hash("https://www.streamable.com/abc123"));
        assert_ne!(video_hash("https://streamable.com/abc124"), video_hash("https://streamable.com/abc123"));
    }

    /// Playlists and channel live embeds have no video id: each must stay its own video,
    /// not collapse into one key per kind ("watch?v=videoseries", "watch?v=live_stream").
    #[test]
    fn a_playlist_or_a_channels_live_embed_is_never_one_key_for_all() {
        assert_ne!(
            video_hash("https://www.youtube.com/embed/videoseries?list=PLaaaaaaaaaa"),
            video_hash("https://www.youtube.com/embed/videoseries?list=PLbbbbbbbbbb"),
        );
        assert_ne!(
            video_hash("https://www.youtube.com/embed/live_stream?channel=UCaaaaaaaaaa"),
            video_hash("https://www.youtube.com/embed/live_stream?channel=UCbbbbbbbbbb"),
        );
        assert_ne!(
            video_hash("https://www.youtube.com/watch?list=PLaaaaaaaaaa"),
            video_hash("https://www.youtube.com/watch?list=PLbbbbbbbbbb"),
        );
        // A real id with a playlist is still that one video.
        assert_eq!(
            video_hash("https://www.youtube.com/watch?v=ALa-5MJTLmM&list=PLaaaaaaaaaa"),
            video_hash("https://www.youtube.com/watch?v=ALa-5MJTLmM"),
        );
    }

    #[test]
    fn the_split_drops_what_is_site_wide_and_says_on_how_many_other_articles() {
        let (a, b) = ("https://cdn.example.test/a.mp4".to_string(), "https://cdn.example.test/b.mp4".to_string());
        let wide = HashMap::from([(video_hash(&a), 3usize)]);
        let (kept, dropped) = split_site_wide(&[a.clone(), b.clone()], &wide);
        assert_eq!(kept, vec![b.clone()]);
        assert_eq!(dropped, vec![Furniture { url: a.clone(), other_articles: 2 }]);
        // Nothing site-wide: nothing dropped.
        let (kept, dropped) = split_site_wide(&[a.clone(), b.clone()], &HashMap::new());
        assert_eq!((kept.len(), dropped.len()), (2, 0));
    }
}
