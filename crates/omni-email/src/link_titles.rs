//! A link's own title, as context for its keyword (plan P4.34).
//!
//! "τρέιλερ 9-10" and four YouTube links: the mail cannot name the films,
//! the videos' titles can ("Dune: Part Three | Official Trailer"). These
//! are cheap to ask for: the platforms' oEmbed endpoints answer with the
//! title without a key or a browser, and any other page has a `<title>`.
//! Only asked for jobs whose keyword the meter scores low, and only with
//! the LLM assist on (the same "may this mail reach the network" switch).
//! A slow or failed lookup is no title, never an error.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

use regex::Regex;
use reqwest::Url;
use serde_json::Value;

/// Where titles come from: the web, or (tests) a fixed table.
#[derive(Debug, Clone, Default)]
pub enum TitleSource {
    #[default]
    Web,
    Fixed(HashMap<String, String>),
}

/// Per lookup; the lookups run side by side.
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(6);
/// A page's title is in its head; nothing past this is read.
const MAX_PAGE_BYTES: usize = 512 * 1024;
const MAX_TITLE_CHARS: usize = 200;

const UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

impl TitleSource {
    /// The titles found for `urls`, by URL. Missing: none found in time.
    pub async fn titles(&self, urls: &[String]) -> HashMap<String, String> {
        match self {
            Self::Fixed(t) => urls.iter().filter_map(|u| t.get(u).map(|v| (u.clone(), v.clone()))).collect(),
            Self::Web => {
                let Ok(client) = reqwest::Client::builder().timeout(LOOKUP_TIMEOUT).user_agent(UA).build() else {
                    return HashMap::new();
                };
                let mut set = tokio::task::JoinSet::new();
                for u in urls.iter().take(20).cloned() {
                    let client = client.clone();
                    set.spawn(async move {
                        let t = tokio::time::timeout(LOOKUP_TIMEOUT, title_of(&client, &u)).await.ok().flatten();
                        (u, t)
                    });
                }
                let mut out = HashMap::new();
                while let Some(Ok((u, t))) = set.join_next().await {
                    if let Some(t) = t {
                        out.insert(u, t);
                    }
                }
                out
            }
        }
    }
}

async fn title_of(client: &reqwest::Client, url: &str) -> Option<String> {
    if let Some(endpoint) = oembed_url(url) {
        let v: Value = client.get(endpoint).send().await.ok()?.error_for_status().ok()?.json().await.ok()?;
        return title_from_oembed(&v);
    }
    let resp = client.get(url).send().await.ok()?.error_for_status().ok()?;
    let mut body = Vec::new();
    let mut resp = resp;
    while let Ok(Some(chunk)) = resp.chunk().await {
        body.extend_from_slice(&chunk);
        if body.len() >= MAX_PAGE_BYTES {
            break;
        }
    }
    title_from_html(&String::from_utf8_lossy(&body))
}

fn host_is(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// The oEmbed request for a link on a platform that has one.
pub fn oembed_url(url: &str) -> Option<String> {
    let u = Url::parse(url).ok()?;
    let host = u.host_str()?.to_ascii_lowercase();
    let base = if host_is(&host, "youtube.com") || host_is(&host, "youtu.be") {
        "https://www.youtube.com/oembed?format=json"
    } else if host_is(&host, "vimeo.com") {
        "https://vimeo.com/api/oembed.json?"
    } else if host_is(&host, "x.com") || host_is(&host, "twitter.com") {
        "https://publish.twitter.com/oembed?omit_script=1"
    } else if host_is(&host, "tiktok.com") {
        "https://www.tiktok.com/oembed?"
    } else if host_is(&host, "dailymotion.com") || host_is(&host, "dai.ly") {
        "https://www.dailymotion.com/services/oembed?"
    } else {
        return None;
    };
    let mut e = Url::parse(base).ok()?;
    e.query_pairs_mut().append_pair("url", url);
    Some(e.to_string())
}

static RE_TAG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]*>").unwrap());
static RE_P: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<p[^>]*>(.*?)</p>").unwrap());

/// The title in an oEmbed answer. X answers with the post as HTML and no
/// title: its text is the post's own words.
pub fn title_from_oembed(v: &Value) -> Option<String> {
    if let Some(t) = v.get("title").and_then(Value::as_str).map(tidy).filter(|t| !t.is_empty()) {
        return Some(t);
    }
    let html = v.get("html").and_then(Value::as_str)?;
    let p = RE_P.captures(html)?.get(1)?.as_str();
    Some(tidy(&RE_TAG.replace_all(p, " "))).filter(|t| !t.is_empty())
}

static RE_OG_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<meta[^>]+(?:property|name)\s*=\s*["'](?:og:title|twitter:title)["'][^>]*content\s*=\s*["']([^"']+)["']"#).unwrap()
});
static RE_OG_TITLE_REV: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<meta[^>]+content\s*=\s*["']([^"']+)["'][^>]*(?:property|name)\s*=\s*["'](?:og:title|twitter:title)["']"#).unwrap()
});
static RE_TITLE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap());

/// A page's title: `og:title` (the article's headline), else `<title>`.
pub fn title_from_html(html: &str) -> Option<String> {
    [&RE_OG_TITLE, &RE_OG_TITLE_REV, &RE_TITLE]
        .iter()
        .find_map(|re| re.captures(html).and_then(|c| c.get(1)).map(|m| tidy(m.as_str())))
        .filter(|t| !t.is_empty())
}

/// Entities decoded, whitespace collapsed, a site's " - YouTube" tail cut,
/// length capped.
fn tidy(raw: &str) -> String {
    let decoded = raw
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#039;", "'")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&nbsp;", " ")
        .replace("&mdash;", "—");
    let mut t = decoded.split_whitespace().collect::<Vec<_>>().join(" ");
    for tail in [" - YouTube", " | YouTube", " on Vimeo"] {
        if let Some(stripped) = t.strip_suffix(tail) {
            t = stripped.to_string();
        }
    }
    t.chars().take(MAX_TITLE_CHARS).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn platforms_are_asked_through_their_oembed_endpoint() {
        let yt = oembed_url("https://youtu.be/abcdefghijk").unwrap();
        assert!(yt.starts_with("https://www.youtube.com/oembed?format=json&url=https%3A%2F%2Fyoutu.be%2Fabcdefghijk"), "{yt}");
        assert!(oembed_url("https://x.com/someone/status/1").unwrap().starts_with("https://publish.twitter.com/oembed"));
        assert!(oembed_url("https://vimeo.com/123").unwrap().starts_with("https://vimeo.com/api/oembed.json"));
        assert_eq!(oembed_url("https://www.news247.gr/kosmos/arthro/"), None, "a portal page is read for its <title>");
        assert_eq!(oembed_url("https://notyoutube.com/watch?v=x"), None);
    }

    #[test]
    fn a_title_is_read_from_oembed_answers_and_pages() {
        assert_eq!(
            title_from_oembed(&json!({"title": "Dune: Part Three | Official Trailer", "author_name": "Studio"})).as_deref(),
            Some("Dune: Part Three | Official Trailer")
        );
        // X: no title, the post's words in its HTML.
        let x = json!({"html": "<blockquote class=\"twitter-tweet\"><p lang=\"el\" dir=\"ltr\">Διαγωνισμός παρκαρίσματος &amp; άλλα <a href=\"https://t.co/x\">pic.twitter.com/x</a></p>&mdash; Someone (@someone)</blockquote>"});
        assert_eq!(title_from_oembed(&x).as_deref(), Some("Διαγωνισμός παρκαρίσματος & άλλα pic.twitter.com/x"));
        assert_eq!(title_from_oembed(&json!({})), None);

        let page = r#"<html><head><title>Σεισμός στη Σητεία - Portal</title><meta property="og:title" content="Σεισμός 4,8 Ρίχτερ στη Σητεία"></head>"#;
        assert_eq!(title_from_html(page).as_deref(), Some("Σεισμός 4,8 Ρίχτερ στη Σητεία"), "og:title first");
        assert_eq!(title_from_html("<title>  Clip   name - YouTube </title>").as_deref(), Some("Clip name"));
        assert_eq!(title_from_html("<p>no title</p>"), None);
    }
}
