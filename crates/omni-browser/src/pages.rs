//! Pages whose video is one cheap HTTP request away (plan P3.8).
//!
//! Some portals are single-page apps: the HTML has no player, the app asks
//! the site's own API which video to show and builds the embed in the
//! browser. yt-dlp's generic extractor sees nothing ("Unsupported URL") and
//! the headless browser has to render the whole app (~7 s, and only while
//! the site's bot shield lets Chrome through) to find an embed the API
//! names directly.
//!
//! Asking that API the same question the app asks is faster and does not
//! depend on Chrome. When the request fails or the answer has no video, the
//! job goes the usual way (yt-dlp, then the sniffer), so a site changing its
//! API costs speed, never a job.
//!
//! ΑΠΕ-ΜΠΕ (amna.gr): `/home/videos/{id}/{title}` (and the other sections'
//! `/…/videos/{id}/…` routes) is an Angular app; the article comes from
//! `/feeds/getarticle.php?id={id}&infolevel=ADVANCED`, whose `hyperlink` is
//! the YouTube id the page embeds.

use anyhow::{Context, Result};
use reqwest::Url;
use serde_json::Value;
use std::time::Duration;

fn host_is(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// The article id of an amna.gr video page; `None` for anything else.
pub fn amna_video_id(url: &str) -> Option<String> {
    let u = Url::parse(url).ok()?;
    if !host_is(&u.host_str()?.to_ascii_lowercase(), "amna.gr") {
        return None;
    }
    let segments: Vec<&str> = u.path_segments()?.collect();
    segments
        .windows(2)
        .find(|w| w[0].eq_ignore_ascii_case("videos") && !w[1].is_empty() && w[1].chars().all(|c| c.is_ascii_digit()))
        .map(|w| w[1].to_string())
}

/// The request the amna.gr app makes for article `id`.
pub fn amna_api_url(id: &str) -> String {
    format!("https://www.amna.gr/feeds/getarticle.php?id={id}&infolevel=ADVANCED")
}

fn is_youtube_id(s: &str) -> bool {
    s.len() == 11 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The video an amna.gr article answer points at: its YouTube embed, or a
/// file of its own. `None` when it has neither (a text or photo article).
pub fn amna_video_from_answer(answer: &Value) -> Option<String> {
    let field = |k: &str| answer.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    if let Some(id) = field("hyperlink").filter(|h| is_youtube_id(h)) {
        return Some(format!("https://www.youtube.com/watch?v={id}"));
    }
    let file = field("file_url")?;
    let base = Url::parse("https://www.amna.gr/").expect("constant URL");
    let abs = base.join(file).ok()?;
    let path = abs.path().to_ascii_lowercase();
    [".mp4", ".mov", ".m3u8", ".mxf", ".m4v", ".webm"]
        .iter()
        .any(|e| path.ends_with(e))
        .then(|| abs.to_string())
}

/// The video behind `page_url` when it is a page this module knows, found
/// without a browser. `Ok(None)`: not such a page, or the page has no video.
pub async fn resolve(page_url: &str) -> Result<Option<String>> {
    let Some(id) = amna_video_id(page_url) else {
        return Ok(None);
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("building HTTP client")?;
    let answer: Value = client
        .get(amna_api_url(&id))
        .header("Referer", page_url)
        .header("Accept", "application/json")
        .send()
        .await
        .context("amna.gr article request")?
        .error_for_status()
        .context("amna.gr article request")?
        .json()
        .await
        .context("amna.gr article answer")?;
    Ok(amna_video_from_answer(&answer))
}

/// The address a decorated dead link was meant to be (plan P3.10).
///
/// Only when the site itself confirms it: `url` answers 404 or 410, and one
/// of [`omni_core::urlnorm::undecorated_candidates`] answers with success.
/// Anything else (the page exists, a bot wall answers 403 to everything, no
/// network) is `None` and the job goes on with the address it has. Costs
/// nothing for an address that does not look decorated.
pub async fn repair_dead_link(url: &str) -> Option<String> {
    let candidates = omni_core::urlnorm::undecorated_candidates(url);
    if candidates.is_empty() {
        return None;
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(PAGE_CHECK_UA)
        .build()
        .ok()?;
    let status = |u: String| {
        let client = client.clone();
        async move { client.get(u).send().await.ok().map(|r| r.status()) }
    };
    let gone = status(url.to_string()).await.is_some_and(|s| s.as_u16() == 404 || s.as_u16() == 410);
    if !gone {
        return None;
    }
    for c in candidates {
        if status(c.clone()).await.is_some_and(|s| s.is_success()) {
            return Some(c);
        }
    }
    None
}

/// A desktop browser's identity: some portals answer a bare HTTP client
/// with 403 whatever the address.
const PAGE_CHECK_UA: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn amna_video_pages_are_recognised_in_every_section() {
        let real = "https://www.amna.gr/home/videos/1028434/Proores-ekloges-stin-Ispania-stis-29-Noembriou--anakoinose-o-P-Santseth-%CE%92%CE%99%CE%9D%CE%A4%CE%95%CE%9F";
        assert_eq!(amna_video_id(real).as_deref(), Some("1028434"));
        assert_eq!(amna_video_id("https://amna.gr/abnaconferenceEn/videos/77/x").as_deref(), Some("77"));
        assert_eq!(amna_video_id("https://www.amna.gr/home/article/1028434/x"), None);
        assert_eq!(amna_video_id("https://www.amna.gr/home/videos/abc/x"), None);
        // Not fooled by a lookalike host or another site's /videos/ path.
        assert_eq!(amna_video_id("https://notamna.gr/home/videos/1/x"), None);
        assert_eq!(amna_video_id("https://example.org/home/videos/1/x"), None);
    }

    #[test]
    fn the_answer_gives_the_youtube_embed() {
        // Shape of the real answer for article 1028434 (2026-10-07), trimmed.
        let a = json!({"id":"1028434","kind":"videos","hyperlink":"7iVA2gPKPNo","file_url":"","photos":[]});
        assert_eq!(amna_video_from_answer(&a).as_deref(), Some("https://www.youtube.com/watch?v=7iVA2gPKPNo"));
    }

    #[test]
    fn a_hosted_file_is_used_when_there_is_no_embed() {
        let a = json!({"hyperlink":"","file_url":"../files/202610/clip.mp4"});
        assert_eq!(amna_video_from_answer(&a).as_deref(), Some("https://www.amna.gr/files/202610/clip.mp4"));
    }

    #[test]
    fn an_answer_without_a_video_is_none() {
        assert_eq!(amna_video_from_answer(&json!({"hyperlink":"","file_url":""})), None);
        // A hyperlink that is not a YouTube id (a web address, junk) is not guessed at.
        assert_eq!(amna_video_from_answer(&json!({"hyperlink":"https://example.org/x"})), None);
        assert_eq!(amna_video_from_answer(&json!({"file_url":"../files/report.pdf"})), None);
        assert_eq!(amna_video_from_answer(&json!({})), None);
    }
}
