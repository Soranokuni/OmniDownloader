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
    fn default_ports_compare_equal() {
        assert!(same_host("mcr.local:80", "mcr.local"));
        assert!(same_host("mcr.local", "mcr.local:443"));
        assert!(same_host("mcr.local:8080", "mcr.local:8080"));
        assert!(!same_host("mcr.local:8080", "mcr.local:9090"));
        assert!(!same_host("mcr.local", "evil.example"));
        assert!(!same_host("mcr.local", "null"));
    }
}
