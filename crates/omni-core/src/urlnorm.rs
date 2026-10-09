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
            // /shorts/ID and /live/ID and /embed/ID are all /watch?v=ID.
            for prefix in ["/shorts/", "/live/", "/embed/", "/v/"] {
                if let Some(id) = path.strip_prefix(prefix) {
                    let id = id.split('/').next().unwrap_or("").to_string();
                    if !id.is_empty() {
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
        }
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

/// Registrable-ish domain for per-domain routing, stats and cookie jars.
///
/// Handles the two-label public suffixes this newsroom actually meets
/// (`co.uk`, `com.gr`, …); it is not a full Public Suffix List, and does not
/// need to be — a wrong answer costs one misfiled statistic, not a bad file.
pub fn registrable_domain(raw: &str) -> Option<String> {
    let norm = normalize(raw);
    let rest = norm.split("://").nth(1)?;
    let host = rest.split('/').next()?.split(':').next()?;
    if host.is_empty() {
        return None;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() <= 2 {
        return Some(host.to_string());
    }
    const TWO_LABEL_SUFFIXES: &[&str] = &[
        "co.uk", "org.uk", "ac.uk", "com.gr", "net.gr", "org.gr", "edu.gr", "gov.gr",
        "com.au", "co.jp", "com.br", "co.nz", "com.tr", "com.cy",
    ];
    let last_two = labels[labels.len() - 2..].join(".");
    if TWO_LABEL_SUFFIXES.contains(&last_two.as_str()) && labels.len() >= 3 {
        Some(labels[labels.len() - 3..].join("."))
    } else {
        Some(last_two)
    }
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

            // --- Facebook / TikTok ---
            ("https://m.facebook.com/watch?v=99", "https://www.facebook.com/watch?v=99"),
            ("https://vm.tiktok.com/ZMabc/", "https://www.tiktok.com/ZMabc"),

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
    }

    #[test]
    fn normalization_is_idempotent() {
        for url in [
            "https://youtu.be/mOMyiwJCX6I?si=abc",
            "https://m.youtube.com/shorts/xyz?feature=share",
            "https://twitter.com/u/status/1?ref_src=x",
            "https://www.iefimerida.gr/news/story/amp",
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
    }
}
