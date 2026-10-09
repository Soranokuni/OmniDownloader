//! Glomex player embeds (neakriti.gr and other Greek portals).
//!
//! A portal embeds the player as `<glomex-integration integration-id=…
//! playlist-id=…>`. yt-dlp's `glomex:embed` extractor needs a concrete clip
//! id (`v-…`) in the `iframe-player.html` form; it does not know the
//! `integration.html` form, and it cannot resolve `playlist-id="auto"`.
//!
//! `auto` is a contextual player: the player asks glomex which clip matches
//! the article, and hides itself when none does. neakriti.gr puts one on
//! every article, with or without a video, so handing `auto` to yt-dlp
//! failed every job with "HTTP Error 400", and an article whose real video
//! was a TikTok or a second, explicit glomex player lost it to the empty
//! `auto` one. Here `auto` is asked the same question the player asks: a
//! clip becomes an ordinary embed, no clip means the page has no glomex video.

use anyhow::{Context, Result};
use reqwest::Url;
use serde_json::Value;
use std::time::Duration;

/// The endpoint the glomex player itself calls for a contextual playlist.
const CONTEXTUAL_API: &str = "https://integration-cloudfront-eu-west-1.mes.glomex.cloud/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GlomexEmbed {
    pub integration_id: Option<String>,
    pub playlist_id: String,
}

impl GlomexEmbed {
    /// Chosen by the player from the page, not named in the markup.
    pub fn is_contextual(&self) -> bool {
        self.playlist_id.eq_ignore_ascii_case("auto")
    }

    /// The form yt-dlp's `glomex:embed` extractor accepts.
    pub fn embed_url(&self) -> String {
        match &self.integration_id {
            Some(i) => format!(
                "https://player.glomex.com/integration/1/iframe-player.html?integrationId={i}&playlistId={}",
                self.playlist_id
            ),
            None => format!("https://player.glomex.com/integration/1/iframe-player.html?playlistId={}", self.playlist_id),
        }
    }
}

fn is_glomex_player_host(host: &str) -> bool {
    host == "player.glomex.com" || host.ends_with(".player.glomex.com")
}

/// The integration and playlist of a glomex player URL, in any of its forms
/// (`iframe-player.html`, `integration.html`, versioned paths, either key
/// order). `None` for anything that is not a glomex player page.
pub fn parse_embed(url: &str) -> Option<GlomexEmbed> {
    let u = Url::parse(url).ok()?;
    if !is_glomex_player_host(u.host_str()?) {
        return None;
    }
    let mut integration_id = None;
    let mut playlist_id = None;
    for (k, v) in u.query_pairs() {
        let v = v.trim().to_string();
        if v.is_empty() || !v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            continue;
        }
        match k.to_ascii_lowercase().as_str() {
            "integrationid" | "integration_id" => integration_id = Some(v),
            "playlistid" | "playlist_id" => playlist_id = Some(v),
            _ => {}
        }
    }
    Some(GlomexEmbed { integration_id, playlist_id: playlist_id? })
}

/// The request the player makes to learn which clip matches `page_url`.
pub fn contextual_api_url(integration_id: &str, page_url: &str) -> String {
    let mut u = Url::parse(CONTEXTUAL_API).expect("constant URL");
    u.query_pairs_mut()
        .append_pair("integration_id", integration_id)
        .append_pair("current_url", page_url);
    u.to_string()
}

/// The clip a contextual playlist answer starts with. `None` when glomex
/// found nothing for the page (`"status":"hidden"`, the player stays empty).
pub fn first_clip(answer: &Value) -> Option<String> {
    if answer.get("status").and_then(Value::as_str) != Some("ok") {
        return None;
    }
    answer
        .get("videos")?
        .as_array()?
        .iter()
        .filter_map(|v| v.get("clip_id").and_then(Value::as_str))
        .find(|id| id.starts_with("v-") && id[2..].chars().all(|c| c.is_ascii_alphanumeric()))
        .map(str::to_string)
}

/// The clip id in a glomex CDN stream URL
/// (`…mes.glomex.cloud/v2/{tenant}/v-…/{token}/stream.mp4`): the network
/// sees the stream of a player that is also found as an embed.
pub fn clip_id_of_stream(url: &str) -> Option<String> {
    let u = Url::parse(url).ok()?;
    if !u.host_str()?.ends_with("glomex.cloud") {
        return None;
    }
    let found = u.path_segments()?
        .find(|s| s.len() > 2 && s.starts_with("v-") && s[2..].chars().all(|c| c.is_ascii_alphanumeric()))
        .map(str::to_string);
    found
}

/// What to do with a glomex player found on `page_url`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// Download this (the canonical embed URL of a concrete clip).
    Embed(String),
    /// Not a video: a contextual player glomex has nothing for.
    Empty,
}

/// Turn a found glomex player into something yt-dlp can download.
///
/// A concrete clip only needs the canonical form. A contextual one is asked
/// of glomex; a failed request keeps the player out rather than handing
/// yt-dlp a URL it is known to reject.
pub async fn resolve(embed: &GlomexEmbed, page_url: &str) -> Result<Resolved> {
    if !embed.is_contextual() {
        return Ok(Resolved::Embed(embed.embed_url()));
    }
    let Some(integration_id) = embed.integration_id.as_deref() else {
        return Ok(Resolved::Empty);
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .context("building HTTP client")?;
    let answer: Value = client
        .get(contextual_api_url(integration_id, page_url))
        .header("Referer", page_url)
        .send()
        .await
        .context("glomex contextual playlist request")?
        .error_for_status()
        .context("glomex contextual playlist request")?
        .json()
        .await
        .context("glomex contextual playlist answer")?;
    Ok(match first_clip(&answer) {
        Some(clip) => Resolved::Embed(
            GlomexEmbed { integration_id: Some(integration_id.to_string()), playlist_id: clip }.embed_url(),
        ),
        None => Resolved::Empty,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_player_url_form_parses_to_integration_and_playlist() {
        let e = parse_embed("https://player.glomex.com/integration/1/iframe-player.html?integrationId=abc123&playlistId=auto")
            .unwrap();
        assert_eq!(e, GlomexEmbed { integration_id: Some("abc123".into()), playlist_id: "auto".into() });
        assert!(e.is_contextual());

        // integration.html and a versioned path, keys reversed: yt-dlp knows
        // neither form, so both come out canonical.
        let e = parse_embed("https://player.glomex.com/integration/1.1593.2/integration.html?playlistId=v-dlrkzeilzsqx&integrationId=4059a01hkr65z550")
            .unwrap();
        assert!(!e.is_contextual());
        assert_eq!(
            e.embed_url(),
            "https://player.glomex.com/integration/1/iframe-player.html?integrationId=4059a01hkr65z550&playlistId=v-dlrkzeilzsqx"
        );

        assert_eq!(parse_embed("https://player.glomex.com/integration/1/integration.js"), None);
        assert_eq!(parse_embed("https://www.neakriti.gr/?playlistId=v-1"), None);
        // Markup values go into a URL: nothing but an id passes.
        assert_eq!(parse_embed("https://player.glomex.com/x.html?playlistId=v-1%26a%3Db").map(|e| e.playlist_id), None);
    }

    #[test]
    fn the_contextual_request_carries_the_article_url() {
        let u = contextual_api_url("eexbs17mbtfi1y9", "https://www.neakriti.gr/life/2202602_a-b?x=1&y=2");
        let parsed = Url::parse(&u).unwrap();
        let q: Vec<(String, String)> = parsed.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
        assert_eq!(
            q,
            vec![
                ("integration_id".into(), "eexbs17mbtfi1y9".into()),
                ("current_url".into(), "https://www.neakriti.gr/life/2202602_a-b?x=1&y=2".into()),
            ]
        );
    }

    #[test]
    fn a_hidden_contextual_player_has_no_clip_and_an_ok_one_has_its_first() {
        // What glomex answered for neakriti articles without a video
        // (2026-10-05): the player hides itself.
        assert_eq!(first_clip(&json!({"status": "hidden"})), None);
        assert_eq!(first_clip(&json!({"status": "ok", "videos": []})), None);
        assert_eq!(
            first_clip(&json!({"status": "ok", "playlist_id": "tl-contextual",
                "videos": [{"clip_id": "v-dlso19wczcdl"}, {"clip_id": "v-other"}]})),
            Some("v-dlso19wczcdl".into())
        );
        assert_eq!(first_clip(&json!({"status": "ok", "videos": [{"clip_id": "v-1&x=2"}]})), None);
    }

    #[test]
    fn a_cdn_stream_names_its_clip() {
        assert_eq!(
            clip_id_of_stream("https://video-cdn-jwt-new.mes.glomex.cloud/v2/77a350/v-dlso19wczcdl/eyJ0eXAi.eyJleHAi.-wpsH9/stream.mp4"),
            Some("v-dlso19wczcdl".into())
        );
        assert_eq!(clip_id_of_stream("https://cdn.example.com/v2/77a350/v-dlso19wczcdl/stream.mp4"), None);
    }

    #[tokio::test]
    async fn a_concrete_clip_resolves_without_a_request() {
        let e = parse_embed("https://player.glomex.com/integration/1/integration.html?integrationId=i1&playlistId=v-abc").unwrap();
        // No network in tests: a request here would fail the test, not pass it.
        assert_eq!(
            resolve(&e, "https://unreachable.invalid/").await.unwrap(),
            Resolved::Embed("https://player.glomex.com/integration/1/iframe-player.html?integrationId=i1&playlistId=v-abc".into())
        );
        let no_integration = GlomexEmbed { integration_id: None, playlist_id: "auto".into() };
        assert_eq!(resolve(&no_integration, "https://unreachable.invalid/").await.unwrap(), Resolved::Empty);
    }
}
