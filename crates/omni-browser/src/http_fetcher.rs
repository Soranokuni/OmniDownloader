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
const MAX_REDIRECTS: usize = 3;
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
}

impl HttpFetcher {
    pub fn new() -> Self {
        Self { pinned_for_tests: None }
    }

    /// For tests that serve from 127.0.0.1: `host` (a made-up public name)
    /// resolves to `addr`; every other name goes through the real rule.
    /// Not reachable from configuration.
    #[doc(hidden)]
    pub fn pinned_for_tests(host: &str, addr: SocketAddr) -> Self {
        Self { pinned_for_tests: Some((host.to_ascii_lowercase(), addr)) }
    }

    async fn resolve(&self, host: &str, port: u16) -> Result<Vec<SocketAddr>> {
        if let Some((name, addr)) = &self.pinned_for_tests {
            if name == host {
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

#[async_trait]
impl Fetcher for HttpFetcher {
    async fn get(&self, url: &str, deadline: Instant) -> Result<String> {
        let mut current = url.to_string();
        for _hop in 0..=MAX_REDIRECTS {
            let left = deadline.saturating_duration_since(Instant::now()).min(FETCH_TIMEOUT);
            if left.is_zero() {
                bail!("no time left");
            }
            let (target, host, port) = check_target(&current).map_err(|e| anyhow!("fetch refused: {e}"))?;
            let addrs = self.resolve(&host, port).await?;
            let mut builder = reqwest::Client::builder().redirect(Policy::none()).timeout(left).user_agent(USER_AGENT);
            if host.parse::<std::net::IpAddr>().is_err() {
                builder = builder.resolve_to_addrs(&host, &addrs);
            }
            let mut resp = builder.build()?.get(target.clone()).send().await?;
            if resp.status().is_redirection() {
                let next = resp
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|l| target.join(l).ok())
                    .ok_or_else(|| anyhow!("redirect without a usable address"))?;
                debug!(host = %host, "recipe fetch redirected");
                current = next.to_string();
                continue;
            }
            if !resp.status().is_success() {
                bail!("{host} answered {}", resp.status().as_u16());
            }
            let mut body: Vec<u8> = Vec::new();
            while let Some(chunk) = resp.chunk().await? {
                if body.len() + chunk.len() > MAX_BODY_BYTES {
                    bail!("{host}: the answer is larger than {MAX_BODY_BYTES} bytes");
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(String::from_utf8_lossy(&body).into_owned());
        }
        bail!("too many redirects")
    }
}
