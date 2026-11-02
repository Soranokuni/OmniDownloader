//! CSRF defence and security headers (plan P2.2, W-07).

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::auth::{ApiError, CSRF_HEADER};

/// Content Security Policy for the panels.
///
/// `script-src 'self'` is only honest once the CDN `<script>` tags are gone
/// (P2.5) — with them the policy would break the UI rather than protect it, so
/// the two land together. `'unsafe-inline'` remains for styles only; the
/// panels carry no inline `<script>`.
const CSP: &str = "default-src 'self'; \
     img-src 'self' data:; \
     style-src 'self' 'unsafe-inline'; \
     script-src 'self'; \
     connect-src 'self'; \
     font-src 'self'; \
     object-src 'none'; \
     base-uri 'none'; \
     form-action 'self'; \
     frame-ancestors 'none'";

/// Reject cross-site state-changing requests.
///
/// Two independent checks, because each covers the other's gap:
///
/// * A custom header (`X-Omni-Request`) that a cross-site HTML form cannot set.
///   A `fetch` that sets it becomes preflighted, and with the CORS layer gone
///   the preflight has no allowing response.
/// * An `Origin`/`Referer` host that matches the `Host` we were asked on. This
///   catches the same-header-different-site case and costs nothing.
///
/// A request with *neither* `Origin` nor `Referer` is allowed **if** it carries
/// the header: some corporate proxies strip both, and a station whose MCR desk
/// silently stops being able to retry jobs is a worse outcome than the residual
/// risk, which the custom header already covers.
pub async fn csrf_guard(req: Request<Body>, next: Next) -> Response {
    let method = req.method().clone();
    let is_state_changing = !matches!(method, Method::GET | Method::HEAD | Method::OPTIONS);

    if !is_state_changing {
        return next.run(req).await;
    }

    let headers = req.headers();

    let has_marker = headers
        .get(CSRF_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);

    if !has_marker {
        return csrf_refusal("missing X-Omni-Request header");
    }

    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let origin_host = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .and_then(url_host)
        .or_else(|| {
            headers
                .get(header::REFERER)
                .and_then(|v| v.to_str().ok())
                .and_then(url_host)
        });

    if let (Some(host), Some(origin_host)) = (host.as_deref(), origin_host.as_deref()) {
        if !same_host(host, origin_host) {
            return csrf_refusal("Origin does not match Host");
        }
    }

    next.run(req).await
}

fn csrf_refusal(reason: &'static str) -> Response {
    tracing::warn!(reason, "Rejected a state-changing request as cross-site");
    ApiError::new(
        StatusCode::FORBIDDEN,
        "CSRF_REJECTED",
        "This request did not come from the OmniDownloader panel.",
    )
    .into_response()
}

/// Host (and port, when present) of an absolute URL, without pulling in a URL
/// parser for a header we only ever compare against `Host`.
fn url_host(value: &str) -> Option<String> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("null") {
        // A sandboxed iframe or a `file://` page. Never our own panel.
        return Some("null".to_string());
    }
    let rest = value
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(value);
    let authority = rest.split(['/', '?', '#']).next()?;
    let authority = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    if authority.is_empty() {
        None
    } else {
        Some(authority.to_ascii_lowercase())
    }
}

/// Compare an `Origin` authority with a `Host` header.
///
/// Both carry `host[:port]`, so a plain comparison is right, except that the
/// default port may be written on one side and not the other.
fn same_host(host: &str, origin: &str) -> bool {
    let host = host.to_ascii_lowercase();
    if host == origin {
        return true;
    }
    let strip_default = |s: &str| -> String {
        s.strip_suffix(":80")
            .or_else(|| s.strip_suffix(":443"))
            .unwrap_or(s)
            .to_string()
    };
    strip_default(&host) == strip_default(origin)
}

/// Where the current interface addresses come from; injectable for tests.
pub type IpSource = std::sync::Arc<dyn Fn() -> Vec<std::net::IpAddr> + Send + Sync>;

/// The `Host` values this server answers to (P2.17).
///
/// A DNS-rebinding page makes the browser send our own address to a name the
/// attacker controls; the `Host` header is then the attacker's name. Refusing
/// any `Host` that does not name this machine closes that, whatever the peer
/// address says. Names are fixed at start-up; interface addresses are re-read
/// when an IP-literal `Host` misses (at most once per refresh interval),
/// because an address can appear later (DHCP, a VPN) and an IP literal cannot
/// be a rebinding vector.
pub struct AllowedHosts {
    keys: std::collections::HashSet<String>,
    ip_source: Option<IpSource>,
    refresh_every: std::time::Duration,
    /// Interface addresses seen at the last refresh, and when it was.
    live: std::sync::Mutex<(Option<std::time::Instant>, std::collections::HashSet<String>)>,
    /// Hosts already logged, with when: at most one warn per host per minute.
    logged: std::sync::Mutex<std::collections::HashMap<String, std::time::Instant>>,
}

impl std::fmt::Debug for AllowedHosts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AllowedHosts").field("names", &self.keys.len()).finish()
    }
}

impl AllowedHosts {
    /// An explicit set (tests, and the base of [`Self::discover`]). Entries may
    /// carry a port or brackets; they are normalised like a request's `Host`.
    pub fn new<I, S>(hosts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self {
            keys: hosts.into_iter().filter_map(|h| host_key(h.as_ref())).collect(),
            ip_source: None,
            refresh_every: std::time::Duration::from_secs(60),
            live: Default::default(),
            logged: Default::default(),
        }
    }

    /// Re-read interface addresses from `source` when an IP-literal host misses.
    pub fn with_ip_source(mut self, source: IpSource) -> Self {
        self.ip_source = Some(source);
        self
    }

    /// Minimum time between two address re-reads.
    pub fn with_refresh_every(mut self, every: std::time::Duration) -> Self {
        self.refresh_every = every;
        self
    }

    /// The real set: loopback, this machine's names and interface addresses,
    /// `web_host` when it names one, and `security.allowed_hosts`.
    pub fn discover(web_host: &str, configured: &[String]) -> Self {
        let mut hosts: Vec<String> = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
        hosts.extend(omni_core::net::local_host_names());
        let bind = web_host.trim();
        if !bind.is_empty() && bind != "0.0.0.0" && bind != "::" && bind != "[::]" {
            hosts.push(bind.to_string());
        }
        hosts.extend(configured.iter().cloned());
        let this = Self::new(hosts).with_ip_source(std::sync::Arc::new(omni_core::net::local_ip_addrs));
        this.refresh_ips();
        this
    }

    /// Re-read the interface addresses now, unless that was done recently.
    fn refresh_ips(&self) -> bool {
        let Some(source) = &self.ip_source else {
            return false;
        };
        {
            let mut live = self.live.lock().unwrap_or_else(|e| e.into_inner());
            if live.0.is_some_and(|t| t.elapsed() < self.refresh_every) {
                return false;
            }
            // Stamped before the read, so concurrent misses do not all read.
            live.0 = Some(std::time::Instant::now());
        }
        let ips: std::collections::HashSet<String> = source()
            .into_iter()
            .map(|ip| omni_core::net::unmap_v4(ip).to_string())
            .collect();
        self.live.lock().unwrap_or_else(|e| e.into_inner()).1 = ips;
        true
    }

    fn knows_ip(&self, key: &str) -> bool {
        self.live.lock().unwrap_or_else(|e| e.into_inner()).1.contains(key)
    }

    /// True when a raw `Host` (or `X-Forwarded-Host` entry) is in the set.
    pub fn allows(&self, raw_host: &str) -> bool {
        let Some(key) = host_key(raw_host) else {
            return false;
        };
        if self.keys.contains(&key) || self.knows_ip(&key) {
            return true;
        }
        if key.parse::<std::net::IpAddr>().is_ok() && self.refresh_ips() {
            return self.knows_ip(&key);
        }
        false
    }

    /// True when this host has not been logged in the last minute (and records it).
    fn should_log(&self, key: &str) -> bool {
        const EVERY: std::time::Duration = std::time::Duration::from_secs(60);
        const MAX_TRACKED: usize = 256;
        let now = std::time::Instant::now();
        let mut logged = self.logged.lock().unwrap_or_else(|e| e.into_inner());
        if logged.len() >= MAX_TRACKED {
            logged.retain(|_, t| now.duration_since(*t) < EVERY);
            if logged.len() >= MAX_TRACKED {
                // A scanner cycling names: say nothing rather than grow.
                return false;
            }
        }
        match logged.get(key) {
            Some(t) if now.duration_since(*t) < EVERY => false,
            _ => {
                logged.insert(key.to_string(), now);
                true
            }
        }
    }
}

/// Normalise a `Host` value for comparison: port and IPv6 brackets stripped,
/// lower-cased, trailing dot dropped, IP literals in canonical form. `None` for
/// an empty or malformed value.
fn host_key(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let host = if let Some(rest) = raw.strip_prefix('[') {
        let (inner, tail) = rest.split_once(']')?;
        if !(tail.is_empty() || tail.starts_with(':')) {
            return None;
        }
        inner
    } else if raw.matches(':').count() > 1 {
        // A bare IPv6 literal, as written in config.
        raw
    } else {
        raw.split_once(':').map(|(h, _)| h).unwrap_or(raw)
    };
    let host = host.trim_end_matches('.');
    if host.is_empty() {
        return None;
    }
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        return Some(omni_core::net::unmap_v4(ip).to_string());
    }
    Some(host.to_ascii_lowercase())
}

/// Refuse every request whose `Host` does not name this server, with a 421.
///
/// Runs before auth and CSRF, on every route and method. The name asked for is
/// the URI's authority (absolute-form and HTTP/2 requests) or else the `Host`
/// header; when both are present they must agree. That name must always be
/// allowed. Behind a trusted proxy (`security.trust_proxy_header` on and the
/// socket peer in `security.trusted_proxies`) every entry of every
/// `X-Forwarded-Host` line must be allowed as well: a rebound page can set that
/// header itself, and a proxy that keeps the client's `Host` adds none, so the
/// header can only ever narrow what is accepted, never widen it. From any other
/// peer `X-Forwarded-Host` is ignored.
pub async fn host_guard(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // Not set only where a test harness builds the router directly; the
    // daemon's `WebServer::run` always installs one.
    let Some(allowed) = state.allowed_hosts.clone() else {
        return next.run(req).await;
    };

    let (parts, body) = req.into_parts();
    let via_proxy = {
        let cfg = state.config.read().await;
        cfg.security.trust_proxy_header
            && crate::auth::peer_addr(&parts)
                .map(|a| omni_core::net::unmap_v4(a.ip()))
                .is_some_and(|ip| cfg.security.trusted_proxy_networks().contains(ip))
    };

    let header_host = parts.headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let uri_host = parts.uri.authority().map(|a| a.as_str());
    let (host, consistent) = match (uri_host, header_host) {
        (Some(u), Some(h)) => (Some(u), host_key(u) == host_key(h)),
        (Some(h), None) | (None, Some(h)) => (Some(h), true),
        (None, None) => (None, true),
    };

    let mut ok = consistent && host.is_some_and(|h| allowed.allows(h));
    if ok && via_proxy {
        for value in parts.headers.get_all("x-forwarded-host") {
            match value.to_str() {
                Ok(line) => ok &= line.split(',').all(|e| allowed.allows(e.trim())),
                Err(_) => ok = false,
            }
        }
    }
    if ok {
        return next.run(Request::from_parts(parts, body)).await;
    }

    let shown: String = host.unwrap_or_default().chars().take(80).collect();
    if allowed.should_log(&shown.to_ascii_lowercase()) {
        tracing::warn!(host = ?shown, "Refused a request whose Host does not name this server");
    }
    (
        StatusCode::MISDIRECTED_REQUEST,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "421 Misdirected Request: the Host of this request is not one this server answers to. \
         If the panel is reached through a DNS alias or a reverse-proxy name, add that name to \
         security.allowed_hosts in config.json and restart the service.\n",
    )
        .into_response()
}

/// Add the standard security headers to every response.
///
/// `Cache-Control: no-store` is applied to API responses only: the panels'
/// static CSS/JS are content-addressed and want to be cached, while a cached
/// `/api/jobs` on a shared MCR workstation is both stale and a small
/// information leak.
pub async fn security_headers(req: Request<Body>, next: Next) -> Response {
    let is_api = req.uri().path().starts_with("/api/");
    let mut response = next.run(req).await;
    let headers = response.headers_mut();

    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        axum::http::HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );

    if is_api {
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        );
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_host_extracts_the_authority() {
        assert_eq!(url_host("http://mcr.local:8080"), Some("mcr.local:8080".into()));
        assert_eq!(
            url_host("https://mcr.local/mcr?x=1"),
            Some("mcr.local".into())
        );
        assert_eq!(url_host("null"), Some("null".into()));
        assert_eq!(url_host("http://MCR.LOCAL"), Some("mcr.local".into()));
    }

    #[test]
    fn host_key_strips_port_brackets_and_case() {
        assert_eq!(host_key("Mcr.Local:8080").as_deref(), Some("mcr.local"));
        assert_eq!(host_key("[::1]:8080").as_deref(), Some("::1"));
        assert_eq!(host_key("[::1]").as_deref(), Some("::1"));
        assert_eq!(host_key("::1").as_deref(), Some("::1"));
        assert_eq!(host_key("127.0.0.1:80").as_deref(), Some("127.0.0.1"));
        assert_eq!(host_key("[::ffff:127.0.0.1]:1").as_deref(), Some("127.0.0.1"));
        assert_eq!(host_key("host."), Some("host".into()));
        assert_eq!(host_key(""), None);
        assert_eq!(host_key(":80"), None);
        assert_eq!(host_key("[::1"), None);
        assert_eq!(host_key("[::1]x"), None);
    }

    #[test]
    fn the_refusal_log_is_once_per_host_per_minute() {
        let a = AllowedHosts::new(["localhost"]);
        assert!(a.should_log("evil.example"));
        assert!(!a.should_log("evil.example"));
        assert!(a.should_log("other.example"));
    }

    #[test]
    fn discover_always_has_loopback_the_bind_host_and_configured_aliases() {
        let a = AllowedHosts::discover("192.0.2.7", &["Alias.Station.Test:8080".to_string()]);
        for h in ["localhost", "127.0.0.1", "[::1]:8080", "192.0.2.7:8080", "alias.station.test"] {
            assert!(a.allows(h), "{h}");
        }
        assert!(!a.allows("evil.example"));
        // A wildcard bind host names nothing.
        assert!(!AllowedHosts::discover("0.0.0.0", &[]).allows("0.0.0.0"));
    }

    #[test]
    fn a_new_interface_address_is_picked_up_on_a_miss_at_most_once_per_interval() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};
        let addrs: Arc<Mutex<Vec<std::net::IpAddr>>> = Arc::new(Mutex::new(Vec::new()));
        let reads = Arc::new(AtomicUsize::new(0));
        let (a2, r2) = (addrs.clone(), reads.clone());
        let source: IpSource = Arc::new(move || {
            r2.fetch_add(1, Ordering::SeqCst);
            a2.lock().unwrap().clone()
        });
        let hosts = AllowedHosts::new(["localhost"]).with_ip_source(source);

        // A name never triggers a read: it cannot be a new interface.
        assert!(!hosts.allows("evil.example"));
        assert_eq!(reads.load(Ordering::SeqCst), 0);

        // First IP miss reads (nothing there yet) ...
        assert!(!hosts.allows("192.0.2.50:8080"));
        assert_eq!(reads.load(Ordering::SeqCst), 1);
        // ... the address appears, but a second miss inside the interval does not re-read.
        addrs.lock().unwrap().push("192.0.2.50".parse().unwrap());
        assert!(!hosts.allows("192.0.2.50"));
        assert!(!hosts.allows("192.0.2.51"));
        assert_eq!(reads.load(Ordering::SeqCst), 1, "rate-limited, not per request");

        // Once the interval has passed, the next miss re-reads and finds it.
        let hosts = AllowedHosts::new(["localhost"])
            .with_ip_source({
                let a = addrs.clone();
                Arc::new(move || a.lock().unwrap().clone())
            })
            .with_refresh_every(std::time::Duration::ZERO);
        assert!(hosts.allows("192.0.2.50"));
        assert!(hosts.allows("192.0.2.50:8080"), "now remembered");
        assert!(!hosts.allows("192.0.2.99"));
    }

    #[test]
    fn default_ports_compare_equal() {
        assert!(same_host("mcr.local:80", "mcr.local"));
        assert!(same_host("mcr.local", "mcr.local:443"));
        assert!(same_host("mcr.local:8080", "mcr.local:8080"));
        assert!(!same_host("mcr.local:8080", "mcr.local:9090"));
        assert!(!same_host("mcr.local", "evil.example"));
        assert!(!same_host("mcr.local", "null"));
    }
}
