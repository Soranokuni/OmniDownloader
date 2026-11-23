//! URL normalization for idempotent enqueue (plan P1.2).
//!
//! Two journalists forwarding the same story send the same video under
//! different URLs: `youtu.be/ID`, `m.youtube.com/watch?v=ID&feature=share`,
//! and `youtube.com/watch?v=ID&utm_source=newsletter` are one asset. Without a
//! normalized form the queue transcodes it three times and drops three files
//! into the watchfolder — three copies of the same clip for the operator to
//! reconcile under deadline.
//!
//! The normalized string is a **dedup key only**. The original URL is what gets
//! downloaded, because some sites are fussy about the exact form and a
//! normalized URL that 404s would be a much worse failure than a duplicate.

use std::collections::BTreeMap;

/// The version of [`normalize`]'s rules (P14.8b). Bump it with every change to
/// what `normalize` returns: at the next start the stored keys of the rows a
/// duplicate check can still meet are computed again
/// (`Repository::renormalize_if_needed`), so a link sent before the change and
/// again after it is still one job.
///
/// 1: the rules before P14.8. 2: P14.8 (YouTube playlist embeds, Facebook,
/// Instagram and TikTok keys). 3: P11.15a2 (file-locker shares: Dropbox, Google Drive,
/// OneDrive, SharePoint). 4: P11.15a3 (OneDrive personal shares by item id, Dropbox
/// without the file name, Drive with its resourcekey).
pub const NORMALIZE_VERSION: i64 = 4;

/// Instagram path words that are pages, not shortcodes (P14.8b).
const INSTAGRAM_PATH_WORDS: &[&str] = &["audio", "tagged", "saved", "explore", "stories", "accounts", "direct", "about", "legal"];

/// Query parameters that never identify the asset: analytics, share tracking,
/// and playback state. Removing them is what makes two forwarded links equal.
///
/// `t` (start time) is dropped deliberately: `?t=50s` marks where a journalist
/// wants the cut to start, but it is the same source video, and the pipeline
/// always ingests the whole clip.
const STRIP_PARAMS: &[&str] = &[
    "fbclid",
    "gclid",
    "igshid",
    "feature",
    "si",
    "t",
    "ref_src",
    "ref_url",
    "ref",
    "source",
    "spm",
    "mc_cid",
    "mc_eid",
    "_ga",
    "yclid",
    "msclkid",
];

/// Normalize a URL into a stable dedup key.
///
/// Never fails: an unparseable string is lowercased and trimmed so that at
/// least exact repeats still deduplicate.
pub fn normalize(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches(['.', ',', ';', ')', '>', '»', '"', '\'']);
    if trimmed.is_empty() {
        return String::new();
    }

    let (scheme, rest) = match trimmed.split_once("://") {
        Some((s, r)) => (s.to_ascii_lowercase(), r),
        // A bare `www.example.gr/x` from an email body; assume https.
        None => ("https".to_string(), trimmed),
    };
    if scheme != "http" && scheme != "https" {
        // attachment://, file://, mailto: — leave the shape alone, just case-fold.
        return trimmed.to_ascii_lowercase();
    }

    // Split host[:port] / path ? query # fragment. The fragment is always
    // dropped: it never reaches the server.
    let rest = rest.split('#').next().unwrap_or("");
    let (authority, path_and_query) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, q),
        None => (path_and_query, ""),
    };

    // Strip userinfo and default ports.
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    let mut host = authority.to_ascii_lowercase();
    for default_port in [":80", ":443"] {
        if let Some(stripped) = host.strip_suffix(default_port) {
            host = stripped.to_string();
        }
    }

    let mut path = path.to_string();
    let mut params = parse_query(query);

    apply_host_rules(&mut host, &mut path, &mut params);

    // AMP variants are the same article; the sniffer retries the canonical URL
    // anyway (plan P3.4), so they must not enqueue twice.
    if let Some(stripped) = path.strip_suffix("/amp") {
        path = stripped.to_string();
    }
    if let Some(stripped) = path.strip_suffix("/amp/") {
        path = format!("{stripped}/");
    }
    params.remove("amp");
    params.remove("outputType");
    params.remove("output");

    for key in STRIP_PARAMS {
        params.remove(*key);
    }
    params.retain(|k, _| !k.to_ascii_lowercase().starts_with("utm_"));

    // Trailing slash carries no meaning for these sites, but keep a bare root.
    if path.len() > 1 {
        while path.ends_with('/') {
            path.pop();
        }
    }
    if path.is_empty() {
        path.push('/');
    }

    let mut out = format!("{scheme}://{host}{path}");
    if !params.is_empty() {
        // BTreeMap iterates in key order, so parameter order stops mattering.
        let q: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
        out.push('?');
        out.push_str(&q.join("&"));
    }
    out
}

/// Per-platform rewrites that collapse several URL shapes onto one canonical
/// form. Everything here is a shape the newsroom actually receives.
fn apply_host_rules(host: &mut String, path: &mut String, params: &mut BTreeMap<String, String>) {
    match host.as_str() {
        // youtu.be/ID -> youtube.com/watch?v=ID
        "youtu.be" => {
            let id = path.trim_start_matches('/').to_string();
            if !id.is_empty() {
                *host = "www.youtube.com".into();
                *path = "/watch".into();
                params.insert("v".into(), id);
            }
        }
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" => {
            *host = "www.youtube.com".into();
            // /shorts/ID and /live/ID and /embed/ID are all /watch?v=ID. Two embed
            // names are not videos (P14.8): `videoseries` is the playlist in `list`
            // (every playlist embed was one key, `watch?v=videoseries`), and
            // `live_stream` is a channel's live stream; neither is a `v`.
            for prefix in ["/shorts/", "/live/", "/embed/", "/v/"] {
                if let Some(id) = path.strip_prefix(prefix) {
                    let id = id.split('/').next().unwrap_or("").to_string();
                    if prefix == "/embed/" && id == "videoseries" {
                        if params.contains_key("list") {
                            *path = "/playlist".into();
                        }
                    } else if prefix == "/embed/" && id == "live_stream" {
                        // Left as it is: the channel parameter names the stream.
                    } else if !id.is_empty() {
                        *path = "/watch".into();
                        params.insert("v".into(), id);
                    }
                    break;
                }
            }
            // A playlist link for a single video is still that video.
            if path == "/watch" {
                params.retain(|k, _| k == "v");
            }
            // A playlist is its list.
            if path == "/playlist" {
                params.retain(|k, _| k == "list");
            }
        }
        "instagram.com" | "m.instagram.com" => *host = "www.instagram.com".into(),
        "tiktok.com" => *host = "www.tiktok.com".into(),
        // X and Twitter are one service; journalists send both.
        "twitter.com" | "www.twitter.com" | "mobile.twitter.com" | "www.x.com" | "mobile.x.com" => {
            *host = "x.com".into();
        }
        "m.facebook.com" | "web.facebook.com" | "facebook.com" => {
            *host = "www.facebook.com".into();
        }
        "vm.tiktok.com" | "m.tiktok.com" => {
            *host = "www.tiktok.com".into();
        }
        "m.vimeo.com" => *host = "vimeo.com".into(),
        "dropbox.com" => *host = "www.dropbox.com".into(),
        _ => {
            // Generic mobile subdomain folding for news portals.
            if let Some(rest) = host.strip_prefix("m.") {
                if rest.matches('.').count() >= 1 {
                    *host = format!("www.{rest}");
                }
            }
        }
    }

    // An X post is its status id; the handle in front of it is decoration X
    // ignores (`/i/status/N`, `/IOL/status/N` and `/iol/status/N` are one
    // post), and the share link's `?s=20&t=…` is tracking. The same post
    // under two handles was delivered twice (trial run 2026-09-25). What
    // follows the id stays: `/video/2` picks one video of a post with several.
    if host == "x.com" {
        let segs: Vec<&str> = path.trim_start_matches('/').split('/').collect();
        if let Some(i) = segs.iter().position(|s| *s == "status") {
            let id = segs.get(i + 1).copied().unwrap_or("");
            if (1..=2).contains(&i) && !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) {
                let rest = segs[i + 2..].join("/");
                *path = if rest.is_empty() { format!("/i/status/{id}") } else { format!("/i/status/{id}/{rest}") };
                params.clear();
            }
        }
    }

    // P14.8: one key per video on Facebook, Instagram and TikTok. Only shapes
    // that certainly name the same video are merged; the rest lose the share
    // link's tracking query and keep their path.
    let segs: Vec<String> = path.trim_matches('/').split('/').filter(|s| !s.is_empty()).map(str::to_string).collect();
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match host.as_str() {
        "www.facebook.com" => {
            // /watch?v=N, /watch/?v=N and /video.php?v=N are one video.
            if matches!(segs.as_slice(), [w] if w == "watch" || w == "video.php") && params.get("v").is_some_and(|v| digits(v)) {
                *path = "/watch".into();
                params.retain(|k, _| k == "v");
            // /{page}/videos/N and /{page}/videos/{title}/N are the same video N.
            } else if let Some(id) = segs.iter().position(|s| s == "videos").filter(|&i| i == 1).and_then(|_| segs.last().filter(|s| digits(s))) {
                *path = "/watch".into();
                params.clear();
                params.insert("v".into(), id.clone());
            } else if matches!(segs.first().map(String::as_str), Some("reel" | "share")) {
                params.clear();
            }
        }
        "www.instagram.com" => {
            // A post, a reel or an IGTV video by its shortcode, with or without
            // the account's name in front, is /p/{shortcode}. Only a code that
            // looks like a shortcode counts: `/reels/audio/{id}` is an audio page,
            // not a reel called «audio» (P14.8b).
            let shortcode = |c: &str| {
                c.len() >= 5
                    && c.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                    && !INSTAGRAM_PATH_WORDS.contains(&c.to_ascii_lowercase().as_str())
            };
            let kind = |k: &str| matches!(k, "p" | "reel" | "reels" | "tv");
            let at = match segs.as_slice() {
                [k, code, ..] if kind(k) && shortcode(code) => Some(code.clone()),
                [_, k, code, ..] if kind(k) && shortcode(code) => Some(code.clone()),
                _ => None,
            };
            if let Some(code) = at {
                *path = format!("/p/{code}");
                params.clear();
            }
        }
        "www.tiktok.com" => {
            // /@{account}/video/N: the account is decoration, as on X; the old
            // mobile /v/N.html is the same video.
            let id = match segs.as_slice() {
                [_, kind, id, ..] if kind == "video" && digits(id) => Some(id.clone()),
                [v, file] if v == "v" => file.strip_suffix(".html").filter(|s| digits(s)).map(str::to_string),
                _ => None,
            };
            if let Some(id) = id {
                *path = format!("/video/{id}");
                params.clear();
            }
        }
        // File-locker shares (P11.15a2): since P11.15a they download by
        // themselves, so every form of one share must be one key, or the same
        // file goes on air twice.
        "www.dropbox.com" => {
            // A shared file is its key (`/s/{key}`) or id (`/scl/fi/{id}`) and its
            // `rlkey`; the file name after it (raw or percent-encoded) and `dl`/`raw`
            // only say how to show or serve it (P11.15a3).
            match segs.as_slice() {
                [s, key, ..] if s == "s" => *path = format!("/s/{key}"),
                [scl, fi, id, ..] if scl == "scl" && fi == "fi" => *path = format!("/scl/fi/{id}"),
                _ => {}
            }
            params.retain(|k, _| k == "rlkey");
        }
        "drive.google.com" => {
            // A file is its id: /file/d/{id}/view?usp=…, open?id=, uc?id=&export=…,
            // and its `resourcekey` when the share has one (older shared files, P11.15a3).
            let id = match segs.as_slice() {
                [f, d, id, ..] if f == "file" && d == "d" => Some(id.clone()),
                [one] if one == "open" || one == "uc" => params.get("id").cloned(),
                _ => None,
            };
            if let Some(id) = id.filter(|id| !id.is_empty()) {
                *path = format!("/file/d/{id}");
                params.retain(|k, _| k == "resourcekey");
            }
        }
        // A OneDrive short link, or a SharePoint share: the path is the share;
        // `e`, `usp`, `download` and the like only say how to show it.
        "1drv.ms" => params.clear(),
        h if h.ends_with(".sharepoint.com") && segs.first().is_some_and(|s| s.len() == 3 && s.starts_with(':') && s.ends_with(':')) => {
            params.clear();
        }
        "onedrive.live.com" => {
            // A personal share is its item id, whatever page opens it (`/redir`,
            // `/embed`, `/`) and whichever parameter carries it (`resid` or `id`),
            // `!` written as is or as `%21` (P11.15a3). `cid`, `authkey`,
            // `ithint` and the like only say how to show it.
            let item = params.get("resid").or_else(|| params.get("id")).cloned();
            if let Some(item) = item.filter(|v| !v.is_empty()) {
                let item = item.replace("%21", "!");
                *path = "/".into();
                params.clear();
                params.insert("resid".into(), item);
            } else {
                params.retain(|k, _| k == "redeem");
            }
        }
        _ => {}
    }
}

fn parse_query(query: &str) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        match pair.split_once('=') {
            Some((k, v)) => {
                map.insert(k.to_string(), v.to_string());
            }
            None => {
                map.insert(pair.to_string(), String::new());
            }
        }
    }
    map
}

/// What a log or audit line may say about a link (P14.6): its site (the
/// registrable domain), never the address, whose path and query can carry a
/// token. A link that is not http(s) (an attachment) is «a non-web link».
pub fn site_for_log(raw: &str) -> String {
    let norm = normalize(raw);
    if !(norm.starts_with("http://") || norm.starts_with("https://")) {
        return "a non-web link".to_string();
    }
    registrable_domain(&norm).unwrap_or_else(|| "a link with no host".to_string())
}

/// Registrable domain for per-domain routing, stats and cookie jars: the
/// host's public suffix by the embedded Public Suffix List (ICANN section,
/// P14.6, [`crate::psl`]) and one label more. An IP address is its own.
pub fn registrable_domain(raw: &str) -> Option<String> {
    let norm = normalize(raw);
    let rest = norm.split("://").nth(1)?;
    let host = rest.split('/').next()?.split(':').next()?;
    if host.is_empty() {
        return None;
    }
    Some(crate::psl::registrable(host))
}

/// What MCR's «Λάθος βίντεο» remembers about a video's address (P10.13): the
/// lowercased host (with a non-default port) and the path, no scheme, query
/// or fragment, so a changed token does not bring the same wrong video back.
/// `None` for anything that is not http(s) with a host.
pub fn skip_pattern(raw: &str) -> Option<String> {
    let url = reqwest::Url::parse(raw.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host.is_empty() {
        return None;
    }
    Some(match url.port() {
        Some(port) => format!("{host}:{port}{}", url.path()),
        None => format!("{host}{}", url.path()),
    })
}

/// Addresses `url` may have been before someone typed a word onto its end
/// (plan P3.10): "…/arthro/-ΑΠΟΚΛΕΙΣΤΙΚΟ", "…-sismos-ΤΩΡΑ", "…-NEW".
///
/// The parser already removes the words it knows (P3.9). These are
/// guesses for the ones it does not, so they are never used on their
/// own: the caller tries one only when the site says the address as
/// given does not exist (404/410) and the guess does. The last token
/// (after the last `-`, `_` or `/`) is a candidate for removal when it
/// has a non-Latin letter, or is Latin capitals only: the sites this
/// newsroom reads write their slugs in lowercase. Most likely first.
pub fn undecorated_candidates(url: &str) -> Vec<String> {
    let Some(scheme_end) = url.find("://").map(|i| i + 3) else {
        return Vec::new();
    };
    let Some(path_start) = url[scheme_end..].find('/').map(|i| scheme_end + i) else {
        return Vec::new();
    };
    let tail_start = url[path_start..].rfind(['-', '_', '/']).map(|i| path_start + i);
    let Some(sep) = tail_start else {
        return Vec::new();
    };
    let token = percent_decode(&url[sep + 1..]);
    let letters: Vec<char> = token.chars().filter(|c| c.is_alphabetic()).collect();
    let decorated = !letters.is_empty()
        && (letters.iter().any(|c| !c.is_ascii()) || letters.iter().all(|c| c.is_ascii_uppercase()) && letters.len() >= 3);
    if !decorated {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    let mut push = |c: String| {
        let has_path = c.len() > path_start + 1 || c.contains('?');
        if c != url && has_path && !out.contains(&c) {
            out.push(c);
        }
    };
    // "…/arthro/-WORD" and "…/arthro/WORD": back to "…/arthro/", then without the slash.
    let before = &url[..sep];
    if url[sep..].starts_with('/') || before.ends_with('/') {
        let base = before.trim_end_matches('/');
        push(format!("{base}/"));
        push(base.to_string());
    } else {
        push(before.to_string());
        push(format!("{before}/"));
    }
    out
}

/// `%CE%92` → `Β`; anything that does not decode stays as it was.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Some(b) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_skip_pattern_is_host_port_and_path_only() {
        assert_eq!(skip_pattern("https://CDN.News.Example/V/a.mp4?token=1#t=5").as_deref(), Some("cdn.news.example/V/a.mp4"));
        assert_eq!(
            skip_pattern("http://cdn.news.example/V/a.mp4?token=2"),
            skip_pattern("https://cdn.news.example/V/a.mp4"),
            "the scheme and the query are not part of it"
        );
        assert_eq!(skip_pattern("https://cdn.news.example:8443/a.mp4").as_deref(), Some("cdn.news.example:8443/a.mp4"));
        assert_eq!(skip_pattern("https://cdn.news.example:443/a.mp4").as_deref(), Some("cdn.news.example/a.mp4"), "the default port is dropped");
        assert_ne!(skip_pattern("https://cdn.news.example/a.mp4"), skip_pattern("https://cdn.news.example/b.mp4"));
        assert_ne!(skip_pattern("https://cdn.news.example/a.mp4"), skip_pattern("https://other.example/a.mp4"));
        assert_eq!(skip_pattern("ftp://cdn.news.example/a.mp4"), None);
        assert_eq!(skip_pattern("attachment://x/1"), None);
        assert_eq!(skip_pattern("not a url"), None);
    }

    #[test]
    fn a_word_typed_onto_a_link_gives_the_address_without_it() {
        // P3.10: guesses, each confirmed by the site before use.
        assert_eq!(
            undecorated_candidates("https://www.news247.gr/kosmos/arthro/-ΑΠΟΚΛΕΙΣΤΙΚΟ"),
            vec!["https://www.news247.gr/kosmos/arthro/", "https://www.news247.gr/kosmos/arthro"]
        );
        assert_eq!(
            undecorated_candidates("https://site.gr/a/sismos-krites-ΤΩΡΑ"),
            vec!["https://site.gr/a/sismos-krites", "https://site.gr/a/sismos-krites/"]
        );
        assert_eq!(
            undecorated_candidates("https://site.gr/a/sismos-%CE%A4%CE%A9%CE%A1%CE%91")[0],
            "https://site.gr/a/sismos"
        );
        assert_eq!(undecorated_candidates("https://site.gr/a/sismos-NEW")[0], "https://site.gr/a/sismos");
        // An address that looks like a site's own is not second-guessed.
        assert!(undecorated_candidates("https://site.gr/a/sismos-sta-xania/").is_empty());
        assert!(undecorated_candidates("https://site.gr/a/sismos-sta-xania").is_empty());
        assert!(undecorated_candidates("https://site.gr/a/article-12345").is_empty());
        assert!(undecorated_candidates("https://site.gr/a/clip-HD").is_empty(), "two capitals are a format, not a word");
        assert!(undecorated_candidates("https://site.gr/ΤΩΡΑ").is_empty(), "nothing but the front page would be left");
        assert!(undecorated_candidates("not a url").is_empty());
    }

    /// Table of real shapes the newsroom receives, and what each must collapse to.
    #[test]
    fn normalization_table() {
        let cases: &[(&str, &str)] = &[
            // --- YouTube: every shape is one video ---
            ("https://youtu.be/mOMyiwJCX6I", "https://www.youtube.com/watch?v=mOMyiwJCX6I"),
            ("https://www.youtube.com/watch?v=mOMyiwJCX6I", "https://www.youtube.com/watch?v=mOMyiwJCX6I"),
            ("https://m.youtube.com/watch?v=mOMyiwJCX6I&feature=share", "https://www.youtube.com/watch?v=mOMyiwJCX6I"),
            ("https://www.youtube.com/shorts/aO8YWYaNoew", "https://www.youtube.com/watch?v=aO8YWYaNoew"),
            ("https://www.youtube.com/embed/aO8YWYaNoew", "https://www.youtube.com/watch?v=aO8YWYaNoew"),
            // ?t= is a cut point, not a different asset.
            ("https://www.youtube.com/watch?v=aO8YWYaNoew&t=50s", "https://www.youtube.com/watch?v=aO8YWYaNoew"),
            ("https://youtu.be/aO8YWYaNoew?si=XYZ123", "https://www.youtube.com/watch?v=aO8YWYaNoew"),

            // --- X / Twitter ---
            ("https://twitter.com/user/status/123", "https://x.com/i/status/123"),
            ("https://x.com/user/status/123?ref_src=twsrc%5Etfw", "https://x.com/i/status/123"),
            ("https://mobile.twitter.com/user/status/123", "https://x.com/i/status/123"),
            // One post, whichever handle (or none) is in front of the id.
            ("https://x.com/i/status/123", "https://x.com/i/status/123"),
            ("https://x.com/NewsDeskOne/status/123/", "https://x.com/i/status/123"),
            ("https://x.com/i/web/status/123", "https://x.com/i/status/123"),
            ("https://x.com/NewsDeskOne/status/123?s=20&t=AbCdEf", "https://x.com/i/status/123"),
            // The second video of a post with several is its own asset.
            ("https://x.com/NewsDeskOne/status/123/video/2", "https://x.com/i/status/123/video/2"),
            // Not a post: left alone.
            ("https://x.com/NewsDeskOne", "https://x.com/NewsDeskOne"),

            // --- YouTube embeds that are not one video (P14.8) ---
            ("https://www.youtube.com/embed/videoseries?list=PLabc123&index=2", "https://www.youtube.com/playlist?list=PLabc123"),
            ("https://www.youtube.com/playlist?list=PLabc123&si=x", "https://www.youtube.com/playlist?list=PLabc123"),
            ("https://www.youtube.com/embed/live_stream?channel=UCxyz", "https://www.youtube.com/embed/live_stream?channel=UCxyz"),

            // --- Facebook / TikTok ---
            ("https://m.facebook.com/watch?v=99", "https://www.facebook.com/watch?v=99"),
            ("https://vm.tiktok.com/ZMabc/", "https://www.tiktok.com/ZMabc"),
            // One Facebook video, every shape (P14.8).
            ("https://www.facebook.com/watch/?v=99&mibextid=abc", "https://www.facebook.com/watch?v=99"),
            ("https://web.facebook.com/video.php?v=99", "https://www.facebook.com/watch?v=99"),
            ("https://www.facebook.com/NewsPage/videos/99/?__cft__=x&__tn__=y", "https://www.facebook.com/watch?v=99"),
            ("https://facebook.com/NewsPage/videos/storm-in-the-port/99/", "https://www.facebook.com/watch?v=99"),
            ("https://www.facebook.com/reel/123?mibextid=abc&rdid=x", "https://www.facebook.com/reel/123"),
            ("https://www.facebook.com/share/v/AbC12/?mibextid=xyz", "https://www.facebook.com/share/v/AbC12"),
            // Instagram: post, reel and IGTV by shortcode, with or without the account.
            ("https://www.instagram.com/reel/CxYz123/?igsh=abc", "https://www.instagram.com/p/CxYz123"),
            ("https://instagram.com/p/CxYz123/?img_index=1", "https://www.instagram.com/p/CxYz123"),
            ("https://m.instagram.com/newsdesk/reel/CxYz123/", "https://www.instagram.com/p/CxYz123"),
            ("https://www.instagram.com/tv/CxYz123", "https://www.instagram.com/p/CxYz123"),
            ("https://www.instagram.com/newsdesk/", "https://www.instagram.com/newsdesk"),
            // TikTok: the account in front of the id is decoration.
            ("https://www.tiktok.com/@newsdesk/video/7301234567890?is_from_webapp=1&sender_device=pc", "https://www.tiktok.com/video/7301234567890"),
            ("https://tiktok.com/@other.account/video/7301234567890", "https://www.tiktok.com/video/7301234567890"),
            ("https://m.tiktok.com/v/7301234567890.html", "https://www.tiktok.com/video/7301234567890"),

            // --- News portals: tracking and AMP ---
            ("https://www.lifo.gr/now/sport/story?utm_source=newsletter&utm_medium=email",
             "https://www.lifo.gr/now/sport/story"),
            ("https://www.iefimerida.gr/news/story/amp", "https://www.iefimerida.gr/news/story"),
            ("https://www.iefimerida.gr/news/story?amp", "https://www.iefimerida.gr/news/story"),
            ("https://www.protothema.gr/story/?fbclid=IwAR123", "https://www.protothema.gr/story"),

            // --- Case, ports, fragments, trailing punctuation from email text ---
            ("HTTPS://WWW.Gazzetta.GR/Football/Story#comments", "https://www.gazzetta.gr/Football/Story"),
            ("https://www.in.gr:443/2026/09/19/story/", "https://www.in.gr/2026/09/19/story"),
            ("https://www.newsit.gr/story).", "https://www.newsit.gr/story"),

            // --- Parameter order must not create a duplicate ---
            ("https://example.gr/v?b=2&a=1", "https://example.gr/v?a=1&b=2"),
            ("https://example.gr/v?a=1&b=2", "https://example.gr/v?a=1&b=2"),

            // --- Bare host and scheme-less text ---
            ("www.example.gr/story", "https://www.example.gr/story"),
            ("https://example.gr", "https://example.gr/"),
        ];

        for (input, expected) in cases {
            assert_eq!(
                normalize(input),
                *expected,
                "normalizing {input}"
            );
        }
    }

    #[test]
    fn different_videos_never_collapse_together() {
        // The dangerous failure direction: two distinct assets deduplicating
        // into one means a story silently never reaches air.
        let a = normalize("https://www.youtube.com/watch?v=AAAAAAAAAAA");
        let b = normalize("https://www.youtube.com/watch?v=BBBBBBBBBBB");
        assert_ne!(a, b);

        let c = normalize("https://www.lifo.gr/now/story-one");
        let d = normalize("https://www.lifo.gr/now/story-two");
        assert_ne!(c, d);

        // Two posts by one account, and two videos of one post.
        assert_ne!(normalize("https://x.com/NewsDeskOne/status/1"), normalize("https://x.com/NewsDeskOne/status/2"));
        assert_ne!(
            normalize("https://x.com/NewsDeskOne/status/1/video/1"),
            normalize("https://x.com/NewsDeskOne/status/1/video/2")
        );

        // Same path on different hosts stays distinct.
        assert_ne!(
            normalize("https://www.lifo.gr/now/story"),
            normalize("https://www.newsit.gr/now/story")
        );

        // P14.8: two playlists, two Facebook videos, two reels, two posts, two TikToks.
        assert_ne!(
            normalize("https://www.youtube.com/embed/videoseries?list=PLone"),
            normalize("https://www.youtube.com/embed/videoseries?list=PLtwo")
        );
        assert_ne!(normalize("https://www.facebook.com/watch/?v=1"), normalize("https://www.facebook.com/watch/?v=2"));
        assert_ne!(normalize("https://www.facebook.com/Page/videos/1"), normalize("https://www.facebook.com/Page/videos/2"));
        assert_ne!(normalize("https://www.facebook.com/reel/1"), normalize("https://www.facebook.com/reel/2"));
        assert_ne!(normalize("https://www.instagram.com/reel/AAAAAAA"), normalize("https://www.instagram.com/reel/BBBBBBB"));
        // Two audio pages are not one «reel» called audio (P14.8b).
        assert_ne!(
            normalize("https://www.instagram.com/reels/audio/111111111/"),
            normalize("https://www.instagram.com/reels/audio/222222222/")
        );
        assert_ne!(normalize("https://www.tiktok.com/@a/video/1"), normalize("https://www.tiktok.com/@a/video/2"));
        // A Facebook page's video list is not one of its videos.
        assert_ne!(normalize("https://www.facebook.com/Page/videos"), normalize("https://www.facebook.com/watch?v=1"));
    }

    #[test]
    fn every_form_of_one_locker_share_is_one_key_and_two_files_are_two() {
        // P11.15a2: a share that downloads by itself, sent twice in two forms, is one job.
        let one = |forms: &[&str]| {
            let keys: Vec<String> = forms.iter().map(|u| normalize(u)).collect();
            assert!(keys.windows(2).all(|w| w[0] == w[1]), "{forms:?} -> {keys:?}");
            keys[0].clone()
        };
        let drive = one(&[
            "https://drive.google.com/file/d/1AbC_dEf-GhIjKlMn/view?usp=sharing",
            "https://drive.google.com/file/d/1AbC_dEf-GhIjKlMn/view?usp=drive_link",
            "https://drive.google.com/file/d/1AbC_dEf-GhIjKlMn/",
            "https://drive.google.com/open?id=1AbC_dEf-GhIjKlMn",
            "https://drive.google.com/uc?id=1AbC_dEf-GhIjKlMn&export=download",
        ]);
        assert_eq!(drive, "https://drive.google.com/file/d/1AbC_dEf-GhIjKlMn");
        let dropbox_old = one(&[
            "https://www.dropbox.com/s/abc123/clip.mp4?dl=0",
            "https://www.dropbox.com/s/abc123/clip.mp4?dl=1",
            "https://dropbox.com/s/abc123/clip.mp4?raw=1",
            "https://www.dropbox.com/s/abc123/clip.mp4",
        ]);
        let dropbox_new = one(&[
            "https://www.dropbox.com/scl/fi/x1y2/clip.mov?rlkey=k9k9&dl=0",
            "https://www.dropbox.com/scl/fi/x1y2/clip.mov?dl=1&rlkey=k9k9&st=abc",
        ]);
        assert!(dropbox_new.contains("rlkey=k9k9"), "the rlkey is part of the share: {dropbox_new}");
        let onedrive = one(&["https://1drv.ms/v/s!AkExample123?e=Xy1", "https://1drv.ms/v/s!AkExample123"]);
        let sharepoint = one(&[
            "https://station-my.sharepoint.com/:v:/g/personal/desk_station_example/EaBcD?e=Xy1",
            "https://station-my.sharepoint.com/:v:/g/personal/desk_station_example/EaBcD?e=Zz9&download=1",
        ]);
        // P11.15a3: a OneDrive personal share by its item id, whichever page and parameter carry it.
        let live = one(&[
            "https://onedrive.live.com/?cid=ABC&resid=ABC%21123&authkey=x",
            "https://onedrive.live.com/?resid=ABC%21123&cid=ABC&ithint=video",
            "https://onedrive.live.com/redir?resid=ABC!123&authkey=!AbCdE",
            "https://onedrive.live.com/embed?resid=ABC!123",
            "https://onedrive.live.com/?cid=ABC&id=ABC%21123",
        ]);
        assert_eq!(live, "https://onedrive.live.com/?resid=ABC!123");
        // P11.15a3: a Dropbox file is its key and rlkey, its name raw or percent-encoded.
        let dropbox_named = one(&[
            "https://www.dropbox.com/scl/fi/x1y2/%CE%B2%CE%AF%CE%BD%CF%84%CE%B5%CE%BF.mov?rlkey=k9k9&dl=0",
            "https://www.dropbox.com/scl/fi/x1y2/βίντεο.mov?rlkey=k9k9",
            "https://www.dropbox.com/scl/fi/x1y2?rlkey=k9k9",
        ]);
        assert_eq!(dropbox_named, "https://www.dropbox.com/scl/fi/x1y2?rlkey=k9k9");
        assert_eq!(dropbox_old, "https://www.dropbox.com/s/abc123");
        // P11.15a3: a Drive file's resourcekey is part of its identity.
        let drive_keyed = one(&[
            "https://drive.google.com/file/d/1AbC_dEf-GhIjKlMn/view?usp=sharing&resourcekey=0-AbCdE",
            "https://drive.google.com/open?id=1AbC_dEf-GhIjKlMn&resourcekey=0-AbCdE",
        ]);
        assert_eq!(drive_keyed, "https://drive.google.com/file/d/1AbC_dEf-GhIjKlMn?resourcekey=0-AbCdE");

        // Two different files stay two.
        assert_ne!(drive, normalize("https://drive.google.com/file/d/2ZzZ_other-file00/view"));
        assert_ne!(dropbox_old, normalize("https://www.dropbox.com/s/def456/clip.mp4?dl=0"));
        assert_ne!(dropbox_new, normalize("https://www.dropbox.com/scl/fi/x1y2/clip.mov?rlkey=other1"));
        assert_ne!(onedrive, normalize("https://1drv.ms/v/s!AkOther456"));
        assert_ne!(sharepoint, normalize("https://station-my.sharepoint.com/:v:/g/personal/desk_station_example/EfGhI?e=Xy1"));
        assert_ne!(live, normalize("https://onedrive.live.com/?cid=ABC&resid=ABC%21124"));
        assert_ne!(live, normalize("https://onedrive.live.com/redir?resid=ABC!125"));
        assert_ne!(dropbox_named, normalize("https://www.dropbox.com/scl/fi/x1y3/clip.mov?rlkey=k9k9"));
        assert_ne!(drive_keyed, normalize("https://drive.google.com/file/d/1AbC_dEf-GhIjKlMn/view?resourcekey=0-Other"));
        for u in [&drive, &dropbox_old, &dropbox_new, &dropbox_named, &drive_keyed, &onedrive, &sharepoint, &live] {
            assert_eq!(normalize(u), *u, "idempotent: {u}");
        }
    }

    #[test]
    fn normalization_is_idempotent() {
        for url in [
            "https://youtu.be/mOMyiwJCX6I?si=abc",
            "https://m.youtube.com/shorts/xyz?feature=share",
            "https://twitter.com/u/status/1?ref_src=x",
            "https://www.iefimerida.gr/news/story/amp",
            // The P14.8 keys.
            "https://www.youtube.com/embed/videoseries?list=PLabc123&index=2",
            "https://www.youtube.com/embed/live_stream?channel=UCxyz",
            "https://www.facebook.com/NewsPage/videos/storm/99/?__cft__=x",
            "https://www.facebook.com/reel/123?mibextid=abc",
            "https://m.instagram.com/newsdesk/reel/CxYz123/?igsh=abc",
            "https://www.instagram.com/reels/audio/123456789/",
            "https://www.tiktok.com/@newsdesk/video/7301234567890?is_from_webapp=1",
            "https://m.tiktok.com/v/7301234567890.html",
        ] {
            let once = normalize(url);
            let twice = normalize(&once);
            assert_eq!(once, twice, "normalize is not idempotent for {url}");
        }
    }

    #[test]
    fn non_http_schemes_pass_through() {
        // Email attachments are enqueued as attachment:// sources (plan P4.6)
        // and must still produce a stable, unique dedup key.
        let a = normalize("attachment://AAMkAGI2/att-1");
        let b = normalize("attachment://AAMkAGI2/att-2");
        assert_ne!(a, b);
        assert!(a.starts_with("attachment://"));
    }

    #[test]
    fn empty_and_garbage_input_is_safe() {
        assert_eq!(normalize(""), "");
        assert_eq!(normalize("   "), "");
        // Not a URL, but must not panic and must still self-dedup.
        let g = normalize("not a url at all");
        assert_eq!(g, normalize("not a url at all"));
    }

    #[test]
    fn registrable_domain_for_routing_and_cookie_jars() {
        assert_eq!(registrable_domain("https://www.youtube.com/watch?v=1").as_deref(), Some("youtube.com"));
        assert_eq!(registrable_domain("https://youtu.be/1").as_deref(), Some("youtube.com"));
        assert_eq!(registrable_domain("https://twitter.com/u/status/1").as_deref(), Some("x.com"));
        assert_eq!(registrable_domain("https://www.in.gr/story").as_deref(), Some("in.gr"));
        assert_eq!(registrable_domain("https://sport.protothema.gr/x").as_deref(), Some("protothema.gr"));
        // Two-label public suffix.
        assert_eq!(registrable_domain("https://www.theguardian.co.uk/x").as_deref(), Some("theguardian.co.uk"));
        assert_eq!(registrable_domain("").is_none(), true);
        // The whole Public Suffix List (P14.6): deeper suffixes, and an address is its own.
        assert_eq!(registrable_domain("https://news.site.com.gr/a").as_deref(), Some("site.com.gr"));
        assert_eq!(registrable_domain("https://www.ypes.gov.gr/a").as_deref(), Some("ypes.gov.gr"));
        assert_eq!(registrable_domain("https://www.pio.gov.cy/a").as_deref(), Some("pio.gov.cy"), "a suffix the old table lacked");
        assert_eq!(registrable_domain("http://192.168.10.4:8080/x").as_deref(), Some("192.168.10.4"));
    }

    /// P14.6: a log line names the site, never the address with its path and query.
    #[test]
    fn a_log_line_names_the_site_never_the_address() {
        assert_eq!(site_for_log("https://www.news.example.gr/story/1?token=abc#t"), "example.gr");
        assert_eq!(site_for_log("https://x.com/someone/status/1?s=20"), "x.com");
        assert_eq!(site_for_log("attachment://clip.mp4"), "a non-web link");
        assert!(!site_for_log("https://cdn.news.example/v.mp4?sig=SECRET").contains("SECRET"));
    }
}
