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
        match forwarded_for(&parts.headers, trusted_proxies) {
            Forwarded::Absent => {}
            Forwarded::Client(ip) => return Some(ip),
            // A header the trusted proxy relayed but we cannot read: the peer is
            // the proxy, and giving the proxy's address (often loopback) to an
            // unknown client would hand it that machine's privileges.
            Forwarded::Unresolved => return None,
        }
    }
    Some(peer)
}

/// The socket peer alone, for keys that need *some* address (the login lockout
/// bucket) when [`client_ip`] could not name the client.
pub(crate) fn peer_ip(parts: &Parts) -> Option<IpAddr> {
    peer_addr(parts).map(|addr| unmap_v4(addr.ip()))
}

/// The socket peer, read the same way axum's `ConnectInfo` extractor reads it.
///
/// `axum::serve` inserts `ConnectInfo<SocketAddr>` when the app is served with
/// `into_make_service_with_connect_info`. A test harness instead installs
/// `MockConnectInfo<SocketAddr>`, which only the extractor unwraps — so a
/// direct `extensions.get::<ConnectInfo<_>>()` silently returns `None` under
/// test and every allowlist case looks denied. Both are checked here, so the
/// tests exercise the same code path production does.
pub(crate) fn peer_addr(parts: &Parts) -> Option<SocketAddr> {
    if let Some(ConnectInfo(addr)) = parts.extensions.get::<ConnectInfo<SocketAddr>>() {
        return Some(*addr);
    }
    parts
        .extensions
        .get::<axum::extract::connect_info::MockConnectInfo<SocketAddr>>()
        .map(|m| m.0)
}

/// What `X-Forwarded-For` said, as far as a trusted proxy's word goes.
enum Forwarded {
    /// No such header: the peer is talking for itself.
    Absent,
    /// The client address.
    Client(IpAddr),
    /// Present, but the walk reached something that is not an address (garbage,
    /// or a line that is not plain ASCII).
    Unresolved,
}

/// The real client in `X-Forwarded-For`, read from the right.
///
/// Proxies append the peer they saw to whatever the client sent, so the
/// left-most entries are client-written and anything may be in them. Walking
/// from the right, every entry a trusted proxy added is skipped; the first one
/// that is not a trusted proxy is the client as our own infrastructure saw it.
/// When every entry is a trusted proxy the left-most is used. An entry that does
/// not parse ends the walk as [`Forwarded::Unresolved`]: never fall back to an
/// earlier, client-written entry, and the caller never falls back to the peer.
fn forwarded_for(headers: &HeaderMap, trusted_proxies: &CidrSet) -> Forwarded {
    // Several header lines are one list, in order. Each line is judged alone: a
    // line that is not visible ASCII contributes one unresolvable entry.
    let mut entries: Vec<Option<IpAddr>> = Vec::new();
    for value in headers.get_all("x-forwarded-for") {
        match value.to_str() {
            Ok(line) => entries.extend(line.split(',').map(|e| parse_forwarded_entry(e.trim()))),
            Err(_) => entries.push(None),
        }
    }
    let mut leftmost = None;
    for entry in entries.iter().rev() {
        let Some(ip) = *entry else {
            return Forwarded::Unresolved;
        };
        if !trusted_proxies.contains(ip) {
            return Forwarded::Client(ip);
        }
        leftmost = Some(ip);
    }
    match leftmost {
        Some(ip) => Forwarded::Client(ip),
        None => Forwarded::Absent,
    }
}

/// One `X-Forwarded-For` entry: `ip`, `ip:port`, `[v6]` or `[v6]:port`.
///
/// The whole entry is tried as an address first, so a bare IPv6 such as
/// `::ffff:10.20.1.5` or `2001:db8::a:b` is never cut at its last colon. A
/// `:port` is stripped only from an entry with exactly one colon.
fn parse_forwarded_entry(entry: &str) -> Option<IpAddr> {
    if let Ok(ip) = entry.parse::<IpAddr>() {
        return Some(unmap_v4(ip));
    }
    if let Ok(sa) = entry.parse::<SocketAddr>() {
        return Some(unmap_v4(sa.ip()));
    }
    // `[v6]` without a port.
    if let Some(inner) = entry.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        return inner.parse::<IpAddr>().ok().map(unmap_v4);
    }
    if entry.matches(':').count() == 1 {
        let (host, port) = entry.split_once(':')?;
        if port.parse::<u16>().is_ok() {
            return host.parse::<std::net::Ipv4Addr>().ok().map(|v4| IpAddr::V4(v4));
        }
    }
    None
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
            "Συνδεθείτε για να συνεχίσετε.",
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

    /// The first-run `admin` / `Admin` session may only change its password,
    /// create a user, or sign out (P2.14).
    pub fn password_change_required() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "PASSWORD_CHANGE_REQUIRED",
            "Αλλάξτε πρώτα τον προεπιλεγμένο κωδικό.",
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

/// Like [`RequireUser`], but also admits the first-run default account while
/// its default password is live (P2.14). Only the handlers that account is
/// allowed to reach use it; every other route refuses that session.
pub struct RequireUserAllowDefault(pub User);

/// Like [`RequireAdmin`], but also admits the locked default account (P2.14).
pub struct RequireAdminAllowDefault(pub User);

/// The address the login limiter keys on: the resolved client address, or the
/// socket peer when a trusted proxy's forwarded header is unresolved (P2.18).
pub struct LimiterIp(pub Option<IpAddr>);

#[axum::async_trait]
impl FromRequestParts<AppState> for LimiterIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let (trusted, trust_header) = {
            let cfg = state.config.read().await;
            (
                cfg.security.trusted_proxy_networks(),
                cfg.security.trust_proxy_header,
            )
        };
        Ok(LimiterIp(
            client_ip(parts, &trusted, trust_header).or_else(|| peer_ip(parts)),
        ))
    }
}

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
            Some(Principal::User(u)) if state.default_admin_locks(&u) => {
                Err(ApiError::password_change_required())
            }
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
            Some(Principal::User(u)) if state.default_admin_locks(&u) => {
                Err(ApiError::password_change_required())
            }
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
            Some(Principal::User(u)) if state.default_admin_locks(&u) => {
                Err(ApiError::password_change_required())
            }
            Some(Principal::User(u)) if u.role == UserRole::Admin => Ok(RequireAdmin(*u)),
            Some(Principal::User(_)) | Some(Principal::OpenMcr(_)) => Err(ApiError::forbidden()),
            None => Err(ApiError::unauthorized()),
        }
    }
}

#[axum::async_trait]
impl FromRequestParts<AppState> for RequireUserAllowDefault {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        match resolve_principal(parts, state).await {
            Some(Principal::User(u)) => Ok(RequireUserAllowDefault(*u)),
            Some(Principal::OpenMcr(_)) | None => Err(ApiError::unauthorized()),
        }
    }
}

#[axum::async_trait]
impl FromRequestParts<AppState> for RequireAdminAllowDefault {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        match resolve_principal(parts, state).await {
            Some(Principal::User(u)) if u.role == UserRole::Admin => Ok(RequireAdminAllowDefault(*u)),
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
    fn forwarded_for_takes_the_right_most_untrusted_entry_and_tolerates_ports() {
        let mut parts = parts_with_peer("127.0.0.1:1");
        let trusted = CidrSet::parse(&["127.0.0.1/32".to_string()]).unwrap();

        for (header, expected) in [
            // Was the left-most ("10.1.2.3") before P2.17; that entry is client-written.
            ("10.1.2.3, 192.168.0.1", "192.168.0.1"),
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

    fn resolve(peer: &str, header: &str, trusted: &[&str]) -> String {
        resolve_opt(peer, header, trusted).expect("a client address")
    }

    fn resolve_opt(peer: &str, header: &str, trusted: &[&str]) -> Option<String> {
        let mut parts = parts_with_peer(peer);
        parts
            .headers
            .insert("x-forwarded-for", HeaderValue::from_str(header).unwrap());
        let trusted = CidrSet::parse(&trusted.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
        client_ip(&parts, &trusted, true).map(|ip| ip.to_string())
    }

    #[test]
    fn forwarded_for_skips_the_proxy_appended_entry() {
        // The proxy 10.0.0.1 appended the peer it saw, 10.0.0.9 here being
        // another untrusted address; the client-written "1.2.3.4" is not the answer.
        assert_eq!(resolve("10.0.0.1:5", "1.2.3.4, 10.0.0.9", &["10.0.0.1/32"]), "10.0.0.9");
    }

    #[test]
    fn a_client_prepended_loopback_never_wins() {
        let got = resolve("10.0.0.1:5", "127.0.0.1, 10.0.0.9", &["10.0.0.1/32"]);
        assert_eq!(got, "10.0.0.9");
        assert_ne!(got, "127.0.0.1");
    }

    #[test]
    fn two_trusted_proxies_in_the_chain_resolve_to_the_real_client() {
        let trusted = ["10.0.0.1/32", "10.0.0.2/32"];
        assert_eq!(resolve("10.0.0.1:5", "203.0.113.7, 10.0.0.2", &trusted), "203.0.113.7");
        // A forged left entry in front of the real one is still not reached.
        assert_eq!(
            resolve("10.0.0.1:5", "127.0.0.1, 203.0.113.7, 10.0.0.2", &trusted),
            "203.0.113.7"
        );
        // Every entry trusted: the left-most.
        assert_eq!(resolve("10.0.0.1:5", "10.0.0.2, 10.0.0.1", &trusted), "10.0.0.2");
    }

    #[test]
    fn garbage_in_the_chain_resolves_to_no_client_never_the_proxy() {
        let trusted = ["10.0.0.1/32", "10.0.0.2/32"];
        // Unparseable entry between a trusted proxy and a believable-looking client.
        assert_eq!(resolve_opt("10.0.0.1:5", "203.0.113.7, garbage, 10.0.0.2", &trusted), None);
        // Garbage on the right: nothing to the left is trusted either.
        assert_eq!(resolve_opt("10.0.0.1:5", "203.0.113.7, garbage", &trusted), None);
        // Garbage left of the first untrusted entry is never reached.
        assert_eq!(resolve("10.0.0.1:5", "garbage, 203.0.113.7", &trusted), "203.0.113.7");
    }

    #[test]
    fn a_non_ascii_forwarded_line_from_a_trusted_proxy_resolves_to_no_client() {
        let mut parts = parts_with_peer("127.0.0.1:1");
        let trusted = CidrSet::parse(&["127.0.0.1/32".to_string()]).unwrap();
        parts
            .headers
            .insert("x-forwarded-for", HeaderValue::from_bytes(b"\xff").unwrap());
        assert_eq!(client_ip(&parts, &trusted, true), None);
        // A readable, untrusted line to its right is reached first and wins.
        parts
            .headers
            .append("x-forwarded-for", HeaderValue::from_static("10.0.0.2"));
        assert_eq!(client_ip(&parts, &trusted, true).unwrap().to_string(), "10.0.0.2");
        // With no header at all the peer speaks for itself.
        parts.headers.remove("x-forwarded-for");
        assert_eq!(client_ip(&parts, &trusted, true).unwrap().to_string(), "127.0.0.1");
    }

    #[test]
    fn bare_ipv6_entries_are_not_cut_at_the_last_colon() {
        let trusted = ["10.0.0.1/32"];
        assert_eq!(resolve("10.0.0.1:5", "::ffff:10.20.1.5", &trusted), "10.20.1.5");
        assert_eq!(resolve("10.0.0.1:5", "2001:db8::a:b", &trusted), "2001:db8::a:b");
        assert_eq!(resolve("10.0.0.1:5", "[2001:db8::a:b]:443", &trusted), "2001:db8::a:b");
        assert_eq!(resolve("10.0.0.1:5", "[2001:db8::1]", &trusted), "2001:db8::1");
        assert_eq!(resolve("10.0.0.1:5", "10.20.1.5:443", &trusted), "10.20.1.5");
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
