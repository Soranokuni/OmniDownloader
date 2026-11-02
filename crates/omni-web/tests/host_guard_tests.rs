//! The `Host` allowlist (P2.17): a request whose `Host` does not name this
//! server is refused with 421 on every route and method, before auth and CSRF.
//! The allowed set is injected; nothing here depends on the test machine's name.

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::net::SocketAddr;
use tempfile::TempDir;
use tower::ServiceExt;

use omni_core::config::AppConfig;
use omni_core::repository::Repository;
use omni_web::middleware::AllowedHosts;
use omni_web::server::WebServer;
use omni_web::state::AppState;

const PROXY: &str = "10.0.0.1:4000";
const OTHER: &str = "203.0.113.9:4000";

struct Rig {
    router: axum::Router,
    _dir: TempDir,
}

fn rig() -> Rig {
    let dir = TempDir::new().unwrap();
    let repo = Repository::new(dir.path().join("omni.db")).unwrap();
    let mut config = AppConfig::default();
    config.security.trust_proxy_header = true;
    config.security.trusted_proxies = vec!["10.0.0.1/32".to_string()];
    let state = AppState::new(repo, config, dir.path().join("config.json")).with_allowed_hosts(
        AllowedHosts::new(["localhost", "127.0.0.1", "::1", "NEWSROOM-PC", "mcr.station.test:8080"]),
    );
    Rig { router: WebServer::build_router(state), _dir: dir }
}

async fn send(
    rig: &Rig,
    method: &str,
    uri: &str,
    peer: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, String) {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let res = rig
        .router
        .clone()
        .layer(MockConnectInfo(peer.parse::<SocketAddr>().unwrap()))
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn a_foreign_host_gets_421_on_get_and_post_and_the_body_names_the_key() {
    let r = rig();
    let (s, body) = send(&r, "GET", "/api/jobs", OTHER, &[("host", "evil.example")]).await;
    assert_eq!(s, StatusCode::MISDIRECTED_REQUEST);
    assert!(body.contains("security.allowed_hosts"), "body: {body}");
    assert!(!body.contains("evil.example"), "the refused host is not echoed");

    // Even from loopback (the DNS-rebinding case: the peer is the victim's browser).
    let (s, _) = send(&r, "GET", "/api/jobs", "127.0.0.1:5", &[("host", "evil.example:8080")]).await;
    assert_eq!(s, StatusCode::MISDIRECTED_REQUEST);

    // A POST, with the CSRF marker and a matching Origin, so only the Host can be the reason.
    let (s, _) = send(
        &r,
        "POST",
        "/api/auth/login",
        OTHER,
        &[("host", "evil.example"), ("origin", "http://evil.example"), ("x-omni-request", "1")],
    )
    .await;
    assert_eq!(s, StatusCode::MISDIRECTED_REQUEST);

    // Static assets and pages are covered too.
    let (s, _) = send(&r, "GET", "/login", OTHER, &[("host", "evil.example")]).await;
    assert_eq!(s, StatusCode::MISDIRECTED_REQUEST);
}

#[tokio::test]
async fn named_hosts_pass_whatever_the_port_or_letter_case() {
    let r = rig();
    for host in [
        "127.0.0.1:8080",
        "[::1]:8080",
        "localhost",
        "newsroom-pc",
        "NewsRoom-PC:8080",
        "NEWSROOM-PC.",
        "mcr.station.test",
        "MCR.Station.Test:9999",
    ] {
        let (s, _) = send(&r, "GET", "/api/health", OTHER, &[("host", host)]).await;
        assert_eq!(s, StatusCode::OK, "Host {host}");
    }
    // A name that merely contains an allowed one is not allowed.
    let (s, _) = send(&r, "GET", "/api/health", OTHER, &[("host", "newsroom-pc.evil.example")]).await;
    assert_eq!(s, StatusCode::MISDIRECTED_REQUEST);
}

#[tokio::test]
async fn a_missing_host_gets_421() {
    let r = rig();
    let (s, _) = send(&r, "GET", "/api/health", OTHER, &[]).await;
    assert_eq!(s, StatusCode::MISDIRECTED_REQUEST);
}

#[tokio::test]
async fn forwarded_host_can_only_narrow_what_a_trusted_proxy_lets_in() {
    let r = rig();

    // Trusted proxy, Host and every forwarded entry allowed: through.
    let (s, _) = send(
        &r,
        "GET",
        "/api/health",
        PROXY,
        &[("host", "mcr.station.test"), ("x-forwarded-host", "mcr.station.test, newsroom-pc")],
    )
    .await;
    assert_eq!(s, StatusCode::OK);

    // A bad forwarded name is refused even though Host is fine.
    let (s, _) = send(
        &r,
        "GET",
        "/api/health",
        PROXY,
        &[("host", "localhost"), ("x-forwarded-host", "evil.example")],
    )
    .await;
    assert_eq!(s, StatusCode::MISDIRECTED_REQUEST);

    // IIS ARR: Host stays the attacker's name, no X-Forwarded-Host is added. A
    // rebound page forges `X-Forwarded-Host: localhost`; Host still decides.
    let (s, _) = send(&r, "GET", "/api/health", PROXY, &[("host", "evil.example")]).await;
    assert_eq!(s, StatusCode::MISDIRECTED_REQUEST);
    let (s, _) = send(
        &r,
        "GET",
        "/api/health",
        PROXY,
        &[("host", "evil.example"), ("x-forwarded-host", "localhost")],
    )
    .await;
    assert_eq!(s, StatusCode::MISDIRECTED_REQUEST);

    // Apache appends to what the client sent: `localhost, evil.example`.
    for host in ["localhost", "evil.example"] {
        let (s, _) = send(
            &r,
            "GET",
            "/api/health",
            PROXY,
            &[("host", host), ("x-forwarded-host", "localhost, evil.example")],
        )
        .await;
        assert_eq!(s, StatusCode::MISDIRECTED_REQUEST, "Host {host}");
    }
    // Every line counts, not just the first.
    let mut b = Request::builder().uri("/api/health").header("host", "localhost");
    b = b.header("x-forwarded-host", "localhost").header("x-forwarded-host", "evil.example");
    let res = r
        .router
        .clone()
        .layer(MockConnectInfo(PROXY.parse::<SocketAddr>().unwrap()))
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::MISDIRECTED_REQUEST);

    // A non-ASCII forwarded line is refused, not skipped.
    let b = Request::builder()
        .uri("/api/health")
        .header("host", "localhost")
        .header("x-forwarded-host", axum::http::HeaderValue::from_bytes(b"\xff").unwrap());
    let res = r
        .router
        .clone()
        .layer(MockConnectInfo(PROXY.parse::<SocketAddr>().unwrap()))
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::MISDIRECTED_REQUEST);

    // Untrusted peer: the header is ignored, Host decides.
    let (s, _) = send(
        &r,
        "GET",
        "/api/health",
        OTHER,
        &[("host", "localhost"), ("x-forwarded-host", "evil.example")],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = send(
        &r,
        "GET",
        "/api/health",
        OTHER,
        &[("host", "evil.example"), ("x-forwarded-host", "localhost")],
    )
    .await;
    assert_eq!(s, StatusCode::MISDIRECTED_REQUEST);
}

#[tokio::test]
async fn an_absolute_form_authority_is_judged_and_must_agree_with_host() {
    let r = rig();
    // HTTP/2 style: authority only, no Host header.
    let (s, _) = send(&r, "GET", "http://mcr.station.test/api/health", OTHER, &[]).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = send(&r, "GET", "http://evil.example/api/health", OTHER, &[]).await;
    assert_eq!(s, StatusCode::MISDIRECTED_REQUEST);
    // Both present and agreeing (port and case aside).
    let (s, _) = send(
        &r,
        "GET",
        "http://MCR.station.test:8080/api/health",
        OTHER,
        &[("host", "mcr.station.test")],
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    // Both present and different: refused although each alone is allowed.
    let (s, _) = send(
        &r,
        "GET",
        "http://localhost/api/health",
        OTHER,
        &[("host", "mcr.station.test")],
    )
    .await;
    assert_eq!(s, StatusCode::MISDIRECTED_REQUEST);
    // A foreign Host behind an allowed authority, and the reverse.
    let (s, _) = send(&r, "GET", "http://localhost/api/health", OTHER, &[("host", "evil.example")]).await;
    assert_eq!(s, StatusCode::MISDIRECTED_REQUEST);
    let (s, _) = send(&r, "GET", "http://evil.example/api/health", OTHER, &[("host", "localhost")]).await;
    assert_eq!(s, StatusCode::MISDIRECTED_REQUEST);
}
