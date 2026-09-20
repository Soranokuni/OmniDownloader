//! Request authentication and the route access policy (plan P2.1, W-01/W-02).
//!
//! Before this module every route was reachable by anyone who could open a TCP
//! connection to the port: `POST /api/setup` could repoint the watchfolder,
//! `POST /api/system/test-email` was a credential oracle for the ingest
//! mailbox, and job override/retry/discard needed no session at all. The
//! pipeline hardening in Phases 0–1 guaranteed that a correct MXF was delivered
//! atomically to *whatever directory the last anonymous caller named*.
//!
//! Every request now resolves to a [`Principal`], and every route declares the
//! policy it needs through one of the extractors below. Adding a route without
//! an extractor is still possible — the compiler cannot force it — so
//! `tests/auth_matrix_tests.rs` walks the whole table and fails on anything
//! anonymous that should not be.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts};
use axum::http::header::COOKIE;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use omni_core::models::{User, UserRole};
use omni_core::net::{unmap_v4, CidrSet};
use omni_core::repository::Repository;

use crate::state::AppState;

/// Name of the session cookie. One constant, because a mismatch between the
/// setter and the reader is an outage that looks like "login does nothing".
pub const SESSION_COOKIE: &str = "omni_session";

/// Header the front-end sets on every state-changing request (plan P2.2).
/// A cross-site form post cannot set it, and a cross-site `fetch` that tries
/// becomes a preflighted request that the (now absent) CORS layer refuses.
pub const CSRF_HEADER: &str = "x-omni-request";

// ==========================================
// Principal
// ==========================================

/// Who is making this request.
#[derive(Debug, Clone)]
pub enum Principal {
    /// A logged-in account.
    User(Box<User>),
    /// No session, but the client is on an allowlisted network, so it gets MCR
    /// read access and may submit jobs. It can never reach admin routes: an
    /// allowlisted IP is a statement about a *room*, not about a person.
    OpenMcr(IpAddr),
}

impl Principal {
    pub fn user(&self) -> Option<&User> {
        match self {
            Principal::User(u) => Some(u),
            Principal::OpenMcr(_) => None,
        }
    }

    pub fn is_admin(&self) -> bool {
        matches!(self, Principal::User(u) if u.role == UserRole::Admin)
    }

    /// Admin, an MCR account, or an allowlisted client.
    pub fn is_mcr(&self) -> bool {
        match self {
            Principal::User(u) => matches!(u.role, UserRole::Admin | UserRole::OpenMcr),
            Principal::OpenMcr(_) => true,
        }
    }

    pub fn is_logged_in(&self) -> bool {
        matches!(self, Principal::User(_))
    }

    /// Label for audit rows — never a token, never a password.
    pub fn audit_label(&self) -> String {
        match self {
            Principal::User(u) => format!("{} (#{})", u.email, u.id),
            Principal::OpenMcr(ip) => format!("open-mcr {}", ip),
        }
    }
}

// ==========================================
// Client IP
// ==========================================

/// The client address, as trusted by policy.
///
/// `None` means the address could not be determined — a `oneshot` test request
/// with no `ConnectInfo`, or a transport that does not carry one. `None` is
/// treated as "not on any allowlist", never as loopback: a request whose origin
/// we cannot establish must not inherit the privileges of the machine we are
/// running on.
pub fn client_ip(parts: &Parts, trusted_proxies: &CidrSet, trust_proxy_header: bool) -> Option<IpAddr> {
    let peer = peer_addr(parts).map(|addr| unmap_v4(addr.ip()))?;

    if trust_proxy_header && trusted_proxies.contains(peer) {
        if let Some(forwarded) = forwarded_for(&parts.headers) {
            return Some(forwarded);
        }
    }
    Some(peer)
}

/// The socket peer, read the same way axum's `ConnectInfo` extractor reads it.
///
/// `axum::serve` inserts `ConnectInfo<SocketAddr>` when the app is served with
/// `into_make_service_with_connect_info`. A test harness instead installs
/// `MockConnectInfo<SocketAddr>`, which only the extractor unwraps — so a
/// direct `extensions.get::<ConnectInfo<_>>()` silently returns `None` under
/// test and every allowlist case looks denied. Both are checked here, so the
/// tests exercise the same code path production does.
fn peer_addr(parts: &Parts) -> Option<SocketAddr> {
    if let Some(ConnectInfo(addr)) = parts.extensions.get::<ConnectInfo<SocketAddr>>() {
        return Some(*addr);
    }
    parts
        .extensions
        .get::<axum::extract::connect_info::MockConnectInfo<SocketAddr>>()
        .map(|m| m.0)
}

/// Left-most entry of `X-Forwarded-For` — the original client, as written by
/// the proxy we just decided to believe.
fn forwarded_for(headers: &HeaderMap) -> Option<IpAddr> {
    let raw = headers.get("x-forwarded-for")?.to_str().ok()?;
    let first = raw.split(',').next()?.trim();
    // A proxy may write `ip:port` for IPv4 or `[v6]:port`.
    let candidate = first
        .strip_prefix('[')
        .and_then(|s| s.split(']').next())
        .unwrap_or_else(|| first.rsplit_once(':').map(|(h, _)| h).unwrap_or(first));
    candidate
        .parse::<IpAddr>()
        .or_else(|_| first.parse::<IpAddr>())
        .ok()
        .map(unmap_v4)
}

// ==========================================
// Cookies
// ==========================================

pub fn extract_session_token(headers: &HeaderMap) -> Option<String> {
    let cookie_str = headers.get(COOKIE)?.to_str().ok()?;
    for part in cookie_str.split(';') {
        let trimmed = part.trim();
        if let Some(stripped) = trimmed.strip_prefix(&format!("{}=", SESSION_COOKIE)) {
            if !stripped.is_empty() {
                return Some(stripped.to_string());
            }
        }
    }
    None
}

pub fn get_authenticated_user(headers: &HeaderMap, repo: &Repository) -> Option<User> {
    let token = extract_session_token(headers)?;
    repo.get_user_by_session_token(&token).ok().flatten()
}

/// Build the session cookie.
///
/// `Secure` is set only when the listener actually serves HTTPS: a `Secure`
/// cookie on a plain-HTTP LAN deployment is never sent back, which presents as
/// "login succeeds and then immediately logs out again".
pub fn build_session_cookie(token: &str, max_age_secs: i64, secure: bool) -> HeaderValue {
    let mut cookie = format!(
        "{}={}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        SESSION_COOKIE, token, max_age_secs
    );
    if secure {
        cookie.push_str("; Secure");
    }
    HeaderValue::from_str(&cookie).expect("session cookie is ASCII")
}

pub fn build_logout_cookie(secure: bool) -> HeaderValue {
    let mut cookie = format!(
        "{}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
        SESSION_COOKIE
    );
    if secure {
        cookie.push_str("; Secure");
    }
    HeaderValue::from_str(&cookie).expect("logout cookie is ASCII")
}

// ==========================================
// Resolution
// ==========================================

/// Resolve the principal for a request: session first, allowlist second.
pub async fn resolve_principal(parts: &Parts, state: &AppState) -> Option<Principal> {
    let (open_networks, trusted, trust_header, idle_hours) = {
        let cfg = state.config.read().await;
        (
            cfg.security.open_networks(),
            cfg.security.trusted_proxy_networks(),
            cfg.security.trust_proxy_header,
            cfg.security.session_idle_hours.max(1),
        )
    };

    if let Some(token) = extract_session_token(&parts.headers) {
        // Idle timeout, checked before the session is honoured. The absolute
        // lifetime alone does not protect an MCR browser left unlocked at the
        // end of a shift.
        let idle = chrono::Duration::hours(idle_hours);
        match state.repo.touch_session(&token, idle) {
            Ok(true) => {
                if let Ok(Some(user)) = state.repo.get_user_by_session_token(&token) {
                    return Some(Principal::User(Box::new(user)));
                }
            }
            Ok(false) => {
                tracing::debug!("Session rejected: idle timeout or unknown token");
            }
            Err(e) => {
                tracing::error!(error = ?e, "Session lookup failed");
            }
        }
    }

    let ip = client_ip(parts, &trusted, trust_header)?;
    if open_networks.contains(ip) {
        Some(Principal::OpenMcr(ip))
    } else {
        None
    }
}

/// True when the request arrives from the machine the daemon runs on.
///
/// Used for the first-run `/setup` window (P2.4). Deliberately independent of
/// `mcr_open_networks`: an operator who allowlists the whole newsroom subnet
/// must not thereby hand first-run configuration to the whole newsroom.
pub async fn is_loopback_client(parts: &Parts, state: &AppState) -> bool {
    let (trusted, trust_header) = {
        let cfg = state.config.read().await;
        (
            cfg.security.trusted_proxy_networks(),
            cfg.security.trust_proxy_header,
        )
    };
    matches!(client_ip(parts, &trusted, trust_header), Some(ip) if ip.is_loopback())
}

// ==========================================
// API errors
// ==========================================

/// `{ "error": { "code": ..., "message": ... } }` — the shape plan P7.5 fixes
/// for every endpoint, so the panels have one error path.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
            "Sign in to continue.",
        )
    }

    pub fn forbidden() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "FORBIDDEN",
            "This action requires a higher access level.",
        )
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "BAD_REQUEST", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "INTERNAL",
            message,
        )
    }

    pub fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "NOT_FOUND", "Not found.")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({
                "error": { "code": self.code, "message": self.message }
            })),
        )
            .into_response()
    }
}

/// Map an internal error into an API error without leaking its detail.
///
/// The message goes to the log; the client gets a stable code. An SQLite error
/// string can carry a file path, and a path is a fact about the server an
/// anonymous caller should not be given.
pub fn internal_error(context: &'static str) -> impl Fn(anyhow::Error) -> ApiError {
    move |e: anyhow::Error| {
        tracing::error!(error = ?e, "{}", context);
        ApiError::internal(context)
    }
}

// ==========================================
// Extractors
// ==========================================

/// Any principal: a logged-in account or an allowlisted client.
pub struct RequireMcr(pub Principal);

/// A logged-in account of any role.
pub struct RequireUser(pub User);

/// A logged-in administrator.
pub struct RequireAdmin(pub User);

/// The principal if there is one, without rejecting.
pub struct MaybePrincipal(pub Option<Principal>);

#[axum::async_trait]
impl FromRequestParts<AppState> for MaybePrincipal {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        Ok(MaybePrincipal(resolve_principal(parts, state).await))
    }
}

#[axum::async_trait]
impl FromRequestParts<AppState> for RequireMcr {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        match resolve_principal(parts, state).await {
            Some(p) if p.is_mcr() => Ok(RequireMcr(p)),
            Some(_) => Err(ApiError::forbidden()),
            None => Err(ApiError::unauthorized()),
        }
    }
}

#[axum::async_trait]
impl FromRequestParts<AppState> for RequireUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        match resolve_principal(parts, state).await {
            Some(Principal::User(u)) => Ok(RequireUser(*u)),
            // An allowlisted client is a room, not an account: there is nothing
            // to scope "my jobs" or a password change to.
            Some(Principal::OpenMcr(_)) | None => Err(ApiError::unauthorized()),
        }
    }
}

#[axum::async_trait]
impl FromRequestParts<AppState> for RequireAdmin {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        match resolve_principal(parts, state).await {
            Some(Principal::User(u)) if u.role == UserRole::Admin => Ok(RequireAdmin(*u)),
            Some(Principal::User(_)) | Some(Principal::OpenMcr(_)) => Err(ApiError::forbidden()),
            None => Err(ApiError::unauthorized()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    fn parts_with_peer(peer: &str) -> Parts {
        let mut req = Request::builder().uri("/").body(()).unwrap();
        let addr: SocketAddr = peer.parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        req.into_parts().0
    }

    #[test]
    fn an_unknown_peer_is_not_silently_treated_as_loopback() {
        let parts = Request::builder().uri("/").body(()).unwrap().into_parts().0;
        assert_eq!(client_ip(&parts, &CidrSet::default(), false), None);
    }

    #[test]
    fn forwarded_for_is_ignored_unless_the_peer_is_a_trusted_proxy() {
        let mut parts = parts_with_peer("203.0.113.9:44000");
        parts
            .headers
            .insert("x-forwarded-for", HeaderValue::from_static("10.20.0.5"));

        // Not trusted: the socket peer wins, so a forged header cannot claim
        // an allowlisted address.
        let untrusted = CidrSet::parse(&["10.0.0.0/8".to_string()]).unwrap();
        assert_eq!(
            client_ip(&parts, &untrusted, true).unwrap().to_string(),
            "203.0.113.9"
        );

        // Trusted peer: the header is believed.
        let trusted = CidrSet::parse(&["203.0.113.9/32".to_string()]).unwrap();
        assert_eq!(
            client_ip(&parts, &trusted, true).unwrap().to_string(),
            "10.20.0.5"
        );

        // Trusted peer but the feature is off: still the socket peer.
        assert_eq!(
            client_ip(&parts, &trusted, false).unwrap().to_string(),
            "203.0.113.9"
        );
    }

    #[test]
    fn forwarded_for_takes_the_left_most_entry_and_tolerates_ports() {
        let mut parts = parts_with_peer("127.0.0.1:1");
        let trusted = CidrSet::parse(&["127.0.0.1/32".to_string()]).unwrap();

        for (header, expected) in [
            ("10.1.2.3, 192.168.0.1", "10.1.2.3"),
            ("10.1.2.3:5555", "10.1.2.3"),
            ("[2001:db8::1]:5555", "2001:db8::1"),
            ("2001:db8::1", "2001:db8::1"),
        ] {
            parts
                .headers
                .insert("x-forwarded-for", HeaderValue::from_str(header).unwrap());
            assert_eq!(
                client_ip(&parts, &trusted, true).unwrap().to_string(),
                expected,
                "header {header}"
            );
        }
    }

    #[test]
    fn session_cookie_is_httponly_samesite_strict_and_secure_only_under_tls() {
        let plain = build_session_cookie("abc", 3600, false);
        let plain = plain.to_str().unwrap();
        assert!(plain.contains("HttpOnly"));
        assert!(plain.contains("SameSite=Strict"));
        assert!(!plain.contains("Secure"));

        let tls = build_session_cookie("abc", 3600, true);
        assert!(tls.to_str().unwrap().contains("; Secure"));
    }

    #[test]
    fn an_empty_cookie_value_is_not_a_token() {
        // The logout cookie sets `omni_session=`. Reading that back as a token
        // would send an empty string to the session lookup on every request.
        let mut headers = HeaderMap::new();
        headers.insert(COOKIE, HeaderValue::from_static("omni_session=; other=1"));
        assert_eq!(extract_session_token(&headers), None);
    }
}
