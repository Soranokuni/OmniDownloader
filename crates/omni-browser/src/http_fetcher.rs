//! The network side of a recipe's `Fetch` step (plan P10.11a): one small GET
//! for a page the recipe names (a player's JSON), under the same egress rule
//! as the repair gate. No cookies, a browser user agent, 5 seconds, 1 MiB,
//! at most 3 redirects, and every hop is checked and resolved again before
//! anything connects. The connection is pinned to the addresses that were
//! checked, so a second DNS answer cannot move it.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use reqwest::header::LOCATION;
use reqwest::redirect::Policy;
use reqwest::Url;
use tracing::debug;

use crate::recipe::Fetcher;

const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(3);
/// Largest body read; a bigger answer is refused, not cut.
pub const MAX_BODY_BYTES: usize = 1 << 20;
/// Largest article picture read (P10.11b), and the time one fetch of it may take.
pub const MAX_IMAGE_BYTES: usize = 2 << 20;
const IMAGE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_REDIRECTS: usize = 3;
const FINAL_URL_REDIRECTS: usize = 10;
const FINAL_URL_TIMEOUT: Duration = Duration::from_secs(10);
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// The address rule for one URL: http(s), no userinfo, never `localhost` or a
/// private literal. Returns the parsed URL (use it, not the raw text), the
/// host and the port that will be connected to. The error names the host only.
pub fn check_target(raw: &str) -> Result<(Url, String, u16), String> {
    let (url, host) = omni_core::net::parse_public_url(raw)?;
    // The connection is pinned under the name without its dot, but the client
    // connects to the name as written: a rooted name could miss the pin.
    if url.host_str().is_some_and(|h| h.ends_with('.')) {
        return Err(format!("host {host} ends with a dot"));
    }
    let port = url.port_or_known_default().ok_or_else(|| "port".to_string())?;
    Ok((url, host, port))
}

pub struct HttpFetcher {
    /// Test only: one fake public name that resolves to a local server.
    pinned_for_tests: Option<(String, SocketAddr)>,
    /// Test only: how long resolving the pinned name takes.
    resolve_delay_for_tests: Duration,
}

impl HttpFetcher {
    pub fn new() -> Self {
        Self { pinned_for_tests: None, resolve_delay_for_tests: Duration::ZERO }
    }

    /// For tests that serve from 127.0.0.1: `host` (a made-up public name)
    /// resolves to `addr`; every other name goes through the real rule.
    /// Not reachable from configuration.
    #[doc(hidden)]
    pub fn pinned_for_tests(host: &str, addr: SocketAddr) -> Self {
        Self { pinned_for_tests: Some((host.to_ascii_lowercase(), addr)), resolve_delay_for_tests: Duration::ZERO }
    }

    /// Test only: a slow name server for the pinned name.
    #[doc(hidden)]
    pub fn with_resolve_delay_for_tests(mut self, delay: Duration) -> Self {
        self.resolve_delay_for_tests = delay;
        self
    }

    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
        if let Some((name, addr)) = &self.pinned_for_tests {
            if name == host {
                if !self.resolve_delay_for_tests.is_zero() {
                    tokio::time::sleep(self.resolve_delay_for_tests).await;
                }
                return Ok(vec![*addr]);
            }
        }
        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            // A literal passed `check_target`, so it is public.
            return Ok(vec![SocketAddr::new(ip, port)]);
        }
        omni_core::net::resolve_public(host, port, RESOLVE_TIMEOUT)
            .await
            .map_err(|e| anyhow!("{host}: {e}"))
    }
}

impl Default for HttpFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpFetcher {
    /// One GET under the address rule: at most `cap` bytes (a bigger answer is
    /// refused, not cut), each hop at most `timeout`, redirects checked again.
    async fn get_capped(&self, url: &str, deadline: Instant, cap: usize, timeout: Duration) -> Result<Vec<u8>> {
        let mut current = url.to_string();
        for _hop in 0..=MAX_REDIRECTS {
            let left = deadline.saturating_duration_since(Instant::now()).min(timeout);
            if left.is_zero() {
                bail!("no time left");
            }
            let (target, host, port) = check_target(&current).map_err(|e| anyhow!("fetch refused: {e}"))?;
            // The name lookup counts against the same deadline as the request.
            let addrs = tokio::time::timeout(left, self.resolve(&host, port))
                .await
                .map_err(|_| anyhow!("{host}: no time left to resolve"))??;
            let left = deadline.saturating_duration_since(Instant::now()).min(timeout);
            if left.is_zero() {
                bail!("no time left");
            }
            let mut builder = reqwest::Client::builder().redirect(Policy::none()).timeout(left).user_agent(USER_AGENT);
            if host.parse::<std::net::IpAddr>().is_err() {
                builder = builder.resolve_to_addrs(&host, &addrs);
            }
            // A reqwest error prints the full address, query included; the
            // address of a signed picture must never reach a log.
            let strip = |e: reqwest::Error| anyhow!("{host}: {}", e.without_url());
            let mut resp = builder.build().map_err(strip)?.get(target.clone()).send().await.map_err(strip)?;
            if resp.status().is_redirection() {
                let next = resp
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|l| target.join(l).ok())
                    .ok_or_else(|| anyhow!("redirect without a usable address"))?;
                debug!(host = %host, "fetch redirected");
                current = next.to_string();
                continue;
            }
            if !resp.status().is_success() {
                bail!("{host} answered {}", resp.status().as_u16());
            }
            let mut body: Vec<u8> = Vec::new();
            while let Some(chunk) = resp.chunk().await.map_err(strip)? {
                if body.len() + chunk.len() > cap {
                    bail!("{host}: the answer is larger than {cap} bytes");
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(body);
        }
        bail!("too many redirects")
    }

    /// The raw bytes of an article picture (P10.11b): same rule as a recipe
    /// fetch, but up to 2 MiB, and 10 s for the whole fetch (every hop and
    /// resolve), so a slow picture leaves time for the next candidate. The
    /// caller decides whether the bytes decode.
    pub async fn get_image_bytes(&self, url: &str, deadline: Instant) -> Result<Vec<u8>> {
        let deadline = deadline.min(Instant::now() + IMAGE_TIMEOUT);
        self.get_capped(url, deadline, MAX_IMAGE_BYTES, IMAGE_TIMEOUT).await
    }

    /// Where `url` really is after its redirects (P3.15): one GET per hop under the address
    /// rule, at most 10 redirects and `FINAL_URL_TIMEOUT` in all, the body never read. The
    /// answer's status does not matter; only the address the chain ends on.
    pub async fn final_url(&self, url: &str) -> Result<String> {
        let deadline = Instant::now() + FINAL_URL_TIMEOUT;
        let mut current = url.to_string();
        for _hop in 0..=FINAL_URL_REDIRECTS {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("no time left");
            }
            let (target, host, port) = check_target(&current).map_err(|e| anyhow!("lookup refused: {e}"))?;
            let addrs = tokio::time::timeout(left, self.resolve(&host, port))
                .await
                .map_err(|_| anyhow!("{host}: no time left to resolve"))??;
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                bail!("no time left");
            }
            let mut builder = reqwest::Client::builder().redirect(Policy::none()).timeout(left).user_agent(USER_AGENT);
            if host.parse::<std::net::IpAddr>().is_err() {
                builder = builder.resolve_to_addrs(&host, &addrs);
            }
            let strip = |e: reqwest::Error| anyhow!("{host}: {}", e.without_url());
            let resp = builder.build().map_err(strip)?.get(target.clone()).send().await.map_err(strip)?;
            if !resp.status().is_redirection() {
                return Ok(target.to_string());
            }
            let next = resp
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .and_then(|l| target.join(l).ok())
                .ok_or_else(|| anyhow!("redirect without a usable address"))?;
            debug!(host = %host, "page lookup redirected");
            current = next.to_string();
        }
        bail!("too many redirects")
    }
}

#[async_trait]
impl Fetcher for HttpFetcher {
    async fn get(&self, url: &str, deadline: Instant) -> Result<String> {
        let body = self.get_capped(url, deadline, MAX_BODY_BYTES, FETCH_TIMEOUT).await?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }
}
