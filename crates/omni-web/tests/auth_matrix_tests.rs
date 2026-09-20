//! The Phase 2 acceptance test: every route × every kind of caller.
//!
//! Defects W-01 and W-02 were not subtle bugs — they were routes that nobody
//! had decided a policy for. `POST /api/setup` could repoint the playout
//! watchfolder and `POST /api/system/test-email` was a credential oracle for
//! the station mailbox, both without a session.
//!
//! So this file does two things:
//!
//! 1. Drives every route with each class of caller and asserts the outcome.
//! 2. Reads `src/server.rs` and fails if a route exists that this table does
//!    not mention. A policy hole in a *new* route is the failure mode that
//!    actually recurs; a test that only covers today's routes would not see it.

use anyhow::Result;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use std::net::SocketAddr;
use tempfile::TempDir;
use tower::ServiceExt;

use omni_core::config::AppConfig;
use omni_core::models::UserRole;
use omni_core::repository::Repository;
use omni_web::server::WebServer;
use omni_web::state::AppState;

/// The client address used for "on the allowlist" cases.
const ALLOWLISTED: &str = "10.20.30.40:51000";
/// ...and for "somewhere else on the network".
const OUTSIDER: &str = "192.0.2.77:51000";
/// The machine the daemon runs on.
const LOOPBACK: &str = "127.0.0.1:51000";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Caller {
    /// No session, not on the allowlist.
    Anonymous,
    /// No session, but on `security.mcr_open_networks`.
    AllowlistedIp,
    /// No session, connecting from this machine.
    Loopback,
    /// A logged-in journalist.
    User,
    /// A logged-in MCR operator.
    Mcr,
    /// A logged-in administrator.
    Admin,
}

use Caller::*;

struct Harness {
    router: axum::Router,
    repo: Repository,
    tokens: Tokens,
    _dir: TempDir,
}

#[derive(Default)]
struct Tokens {
    user: String,
    mcr: String,
    admin: String,
}

impl Harness {
    fn new(with_admin: bool) -> Result<Self> {
        let dir = TempDir::new()?;
        let repo = Repository::new(dir.path().join("omni.db"))?;

        let mut config = AppConfig::default();
        // The newsroom subnet, as an operator would configure it.
        config.security.mcr_open_networks = vec!["10.20.0.0/16".to_string()];
        // Big limits: the rate limiter has its own tests, and a matrix run
        // makes far more than five login-shaped requests.
        config.security.login_rate_limit_per_5min = 10_000;
        config.security.login_rate_limit_per_account_hour = 10_000;

        let mut tokens = Tokens::default();

        let user_id = repo.create_user(
            "reporter@station.gr",
            "correct-horse-battery",
            UserRole::User,
            "Reporter",
            Some("PAPADAKI"),
        )?;
        tokens.user = omni_core::auth::generate_session_token();
        repo.create_session(user_id, &tokens.user, 1)?;

        let mcr_id = repo.create_user(
            "desk@station.gr",
            "correct-horse-battery",
            UserRole::OpenMcr,
            "MCR Desk",
            None,
        )?;
        tokens.mcr = omni_core::auth::generate_session_token();
        repo.create_session(mcr_id, &tokens.mcr, 1)?;

        if with_admin {
            let admin_id = repo.create_user(
                "it@station.gr",
                "correct-horse-battery",
                UserRole::Admin,
                "Administrator",
                None,
            )?;
            tokens.admin = omni_core::auth::generate_session_token();
            repo.create_session(admin_id, &tokens.admin, 1)?;
        }

        // Job #1 exists, so "404 because the row is missing" can never be
        // mistaken for "404 because the policy hid the route".
        repo.add_job(
            "https://www.youtube.com/watch?v=matrix",
            "1_PAPADAKI_TEST",
            "PAPADAKI",
            "TEST",
            "1",
            0,
            omni_core::models::JobStatus::Pending,
            Some(user_id),
            None,
            None,
        )?;

        let state = AppState::new(repo.clone(), config, dir.path().join("config.json"));
        let router = WebServer::build_router(state);

        Ok(Self {
            router,
            repo,
            tokens,
            _dir: dir,
        })
    }

    async fn request(&self, method: &str, uri: &str, caller: Caller) -> StatusCode {
        let peer: SocketAddr = match caller {
            AllowlistedIp => ALLOWLISTED.parse().unwrap(),
            Loopback => LOOPBACK.parse().unwrap(),
            _ => OUTSIDER.parse().unwrap(),
        };

        let mut builder = Request::builder().method(method).uri(uri);

        if let Some(token) = match caller {
            Caller::User => Some(&self.tokens.user),
            Caller::Mcr => Some(&self.tokens.mcr),
            Caller::Admin => Some(&self.tokens.admin),
            _ => None,
        } {
            builder = builder.header("cookie", format!("omni_session={}", token));
        }

        let body = if method == "GET" {
            Body::empty()
        } else {
            // A state-changing request from the panel always carries the CSRF
            // marker and a same-origin `Origin`; the CSRF tests below cover
            // what happens when it does not.
            builder = builder
                .header("content-type", "application/json")
                .header("x-omni-request", "1")
                .header("host", "mcr.local:8080")
                .header("origin", "http://mcr.local:8080");
            Body::from("{}")
        };

        self.router
            .clone()
            .layer(MockConnectInfo(peer))
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap()
            .status()
    }
}

/// What we expect a caller to get.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Expect {
    /// Reached the handler. The handler may still 400 on the empty `{}` body
    /// we send — that is fine and means the policy let it through.
    Reached,
    /// Turned away: 401, 403 or (for pages) a redirect to /login.
    Denied,
    /// Route is not there for this caller at all.
    NotFound,
}

use Expect::*;

fn assert_outcome(route: &str, caller: Caller, expected: Expect, actual: StatusCode) {
    let ok = match expected {
        Reached => !matches!(
            actual,
            StatusCode::UNAUTHORIZED
                | StatusCode::FORBIDDEN
                | StatusCode::NOT_FOUND
                | StatusCode::TEMPORARY_REDIRECT
        ),
        Denied => matches!(
            actual,
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::TEMPORARY_REDIRECT
        ),
        NotFound => actual == StatusCode::NOT_FOUND,
    };
    assert!(
        ok,
        "{route} as {caller:?}: expected {expected:?}, got {actual}"
    );
}

// ==========================================
// The matrix
// ==========================================

/// `(method, uri, [(caller, expectation)])`
type Row = (&'static str, &'static str, &'static [(Caller, Expect)]);

const MATRIX: &[Row] = &[
    // ---- Public ----
    ("GET", "/api/health", &[(Anonymous, Reached), (Admin, Reached)]),
    ("GET", "/login", &[(Anonymous, Reached)]),
    ("GET", "/api/setup/state", &[(Anonymous, Reached)]),
    ("POST", "/api/auth/login", &[(Anonymous, Reached)]),
    ("POST", "/api/auth/logout", &[(Anonymous, Reached)]),
    // `/` always answers, but with a redirect that depends on the caller; it
    // carries no data of its own.
    ("GET", "/", &[(Anonymous, Denied), (Admin, Denied)]),
    // ---- MCR gate ----
    (
        "GET",
        "/mcr",
        &[
            (Anonymous, Denied),
            (AllowlistedIp, Reached),
            (Loopback, Denied),
            (Caller::User, Denied),
            (Mcr, Reached),
            (Admin, Reached),
        ],
    ),
    (
        "GET",
        "/api/jobs",
        &[
            (Anonymous, Denied),
            (AllowlistedIp, Reached),
            (Caller::User, Denied),
            (Mcr, Reached),
            (Admin, Reached),
        ],
    ),
    (
        "POST",
        "/api/jobs",
        &[
            (Anonymous, Denied),
            (AllowlistedIp, Reached),
            (Mcr, Reached),
            (Admin, Reached),
        ],
    ),
    (
        "GET",
        "/api/jobs/1",
        &[(Anonymous, Denied), (AllowlistedIp, Reached), (Admin, Reached)],
    ),
    (
        "POST",
        "/api/jobs/1/override",
        &[
            (Anonymous, Denied),
            (Caller::User, Denied),
            (AllowlistedIp, Reached),
            (Mcr, Reached),
            (Admin, Reached),
        ],
    ),
    (
        "POST",
        "/api/jobs/1/retry",
        &[(Anonymous, Denied), (Caller::User, Denied), (Mcr, Reached)],
    ),
    (
        "POST",
        "/api/jobs/1/discard",
        &[(Anonymous, Denied), (Caller::User, Denied), (Mcr, Reached)],
    ),
    (
        "GET",
        "/api/journalists",
        &[(Anonymous, Denied), (AllowlistedIp, Reached), (Admin, Reached)],
    ),
    (
        "POST",
        "/api/journalists",
        &[(Anonymous, Denied), (Caller::User, Denied), (Mcr, Reached)],
    ),
    (
        "POST",
        "/api/journalists/PAPADAKI",
        &[(Anonymous, Denied), (Caller::User, Denied), (Mcr, Reached)],
    ),
    (
        "GET",
        "/api/system/status",
        &[(Anonymous, Denied), (AllowlistedIp, Reached), (Admin, Reached)],
    ),
    (
        "GET",
        "/api/events",
        &[(Anonymous, Denied), (AllowlistedIp, Reached), (Mcr, Reached)],
    ),
    // ---- Logged in ----
    (
        "GET",
        "/user",
        &[
            (Anonymous, Denied),
            (AllowlistedIp, Denied),
            (Caller::User, Reached),
            (Admin, Reached),
        ],
    ),
    (
        "GET",
        "/api/auth/me",
        &[(Anonymous, Denied), (AllowlistedIp, Denied), (Caller::User, Reached)],
    ),
    (
        "GET",
        "/api/jobs/mine",
        &[(Anonymous, Denied), (AllowlistedIp, Denied), (Caller::User, Reached)],
    ),
    (
        "POST",
        "/api/auth/password",
        &[(Anonymous, Denied), (AllowlistedIp, Denied), (Caller::User, Reached)],
    ),
    // ---- Admin ----
    (
        "GET",
        "/admin",
        &[
            (Anonymous, Denied),
            (AllowlistedIp, Denied),
            (Caller::User, Denied),
            (Mcr, Denied),
            (Admin, Reached),
        ],
    ),
    (
        "GET",
        "/api/admin/users",
        &[
            (Anonymous, Denied),
            (AllowlistedIp, Denied),
            (Caller::User, Denied),
            (Mcr, Denied),
            (Admin, Reached),
        ],
    ),
    (
        "POST",
        "/api/admin/users",
        &[(Anonymous, Denied), (Mcr, Denied), (Admin, Reached)],
    ),
    (
        "POST",
        "/api/admin/users/1/password",
        &[(Anonymous, Denied), (Mcr, Denied), (Admin, Reached)],
    ),
    (
        "POST",
        "/api/admin/purge",
        &[(Anonymous, Denied), (Mcr, Denied), (Admin, Reached)],
    ),
    (
        "POST",
        "/api/admin/vacuum",
        &[(Anonymous, Denied), (Mcr, Denied), (Admin, Reached)],
    ),
    (
        "GET",
        "/api/admin/dependencies",
        &[(Anonymous, Denied), (AllowlistedIp, Denied), (Mcr, Denied)],
    ),
    (
        "POST",
        "/api/admin/update-ytdl",
        &[(Anonymous, Denied), (Caller::User, Denied), (Mcr, Denied)],
    ),
    (
        "POST",
        "/api/admin/rollback-ytdl",
        &[(Anonymous, Denied), (Caller::User, Denied), (Mcr, Denied)],
    ),
    (
        "GET",
        "/api/admin/maintenance",
        &[
            (Anonymous, Denied),
            (AllowlistedIp, Denied),
            (Mcr, Denied),
            (Admin, Reached),
        ],
    ),
    (
        "POST",
        "/api/admin/maintenance/vacuum/run",
        &[(Anonymous, Denied), (Caller::User, Denied), (Mcr, Denied)],
    ),
    (
        "GET",
        "/api/secrets",
        &[
            (Anonymous, Denied),
            (AllowlistedIp, Denied),
            (Caller::User, Denied),
            (Mcr, Denied),
            (Admin, Reached),
        ],
    ),
    (
        "GET",
        "/api/system/logs",
        &[
            (Anonymous, Denied),
            (AllowlistedIp, Denied),
            (Mcr, Denied),
            (Admin, Reached),
        ],
    ),
    (
        "POST",
        "/api/system/test-email",
        &[
            (Anonymous, Denied),
            (AllowlistedIp, Denied),
            (Caller::User, Denied),
            (Mcr, Denied),
            (Admin, Reached),
        ],
    ),
    (
        "POST",
        "/api/system/test-llm",
        &[(Anonymous, Denied), (Mcr, Denied), (Admin, Reached)],
    ),
    // ---- First-run only. With an admin present this is a 404 for everyone
    //      who is not one, including a loopback client.
    (
        "GET",
        "/setup",
        &[
            (Anonymous, NotFound),
            (Loopback, NotFound),
            (AllowlistedIp, NotFound),
            (Caller::User, NotFound),
            (Admin, Reached),
        ],
    ),
    (
        "POST",
        "/api/setup",
        &[
            (Anonymous, NotFound),
            (Loopback, NotFound),
            (AllowlistedIp, NotFound),
            (Caller::User, NotFound),
            (Mcr, NotFound),
        ],
    ),
    // ---- Static assets carry no data ----
    ("GET", "/static/app.css", &[(Anonymous, Reached)]),
    // Last on purpose: a successful call here ends every session for the
    // account, so any row after it would be testing a logged-out caller and
    // failing for the wrong reason.
    (
        "POST",
        "/api/auth/logout-all",
        &[(Anonymous, Denied), (AllowlistedIp, Denied), (Caller::User, Reached)],
    ),
];

#[tokio::test]
async fn every_route_enforces_its_documented_policy() -> Result<()> {
    let h = Harness::new(true)?;

    for (method, uri, cases) in MATRIX {
        for (caller, expected) in cases.iter() {
            let status = h.request(method, uri, *caller).await;
            assert_outcome(&format!("{method} {uri}"), *caller, *expected, status);
        }
    }
    Ok(())
}

/// The defects, named, so a regression reads as itself in the test output.
#[tokio::test]
async fn w01_anonymous_callers_cannot_repoint_the_watchfolder() -> Result<()> {
    let h = Harness::new(true)?;

    let payload = serde_json::json!({
        "email_provider": "Outlook",
        "imap_server": "evil.example",
        "email_address": "attacker@evil.example",
        "email_password": "hunter2",
        "ollama_endpoint": "http://evil.example",
        "ollama_model": "x",
        "watchfolder_path": "\\\\attacker\\share"
    });

    for caller in [Anonymous, AllowlistedIp, Loopback, Caller::User, Mcr] {
        let peer: SocketAddr = match caller {
            AllowlistedIp => ALLOWLISTED.parse().unwrap(),
            Loopback => LOOPBACK.parse().unwrap(),
            _ => OUTSIDER.parse().unwrap(),
        };
        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/setup")
            .header("content-type", "application/json")
            .header("x-omni-request", "1")
            .header("host", "mcr.local")
            .header("origin", "http://mcr.local");
        if let Some(token) = match caller {
            Caller::User => Some(&h.tokens.user),
            Mcr => Some(&h.tokens.mcr),
            _ => None,
        } {
            builder = builder.header("cookie", format!("omni_session={}", token));
        }
        let res = h
            .router
            .clone()
            .layer(MockConnectInfo(peer))
            .oneshot(builder.body(Body::from(payload.to_string()))?)
            .await?;
        assert_eq!(
            res.status(),
            StatusCode::NOT_FOUND,
            "{caller:?} must not reach /api/setup once an admin exists"
        );
    }

    // And the config on disk was never written.
    assert!(!h._dir.path().join("config.json").exists());
    Ok(())
}

#[tokio::test]
async fn the_first_run_window_is_loopback_only_and_closes_once_an_admin_exists() -> Result<()> {
    let h = Harness::new(false)?;
    assert!(!h.repo.has_active_admin()?);

    // A caller on the newsroom subnet cannot claim the first admin account,
    // even though `mcr_open_networks` covers them: an allowlisted IP is a
    // statement about a room, not an authorisation to administer the station.
    assert_eq!(
        h.request("GET", "/setup", AllowlistedIp).await,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        h.request("GET", "/setup", Anonymous).await,
        StatusCode::NOT_FOUND
    );
    // From the machine itself, it is open.
    assert_eq!(h.request("GET", "/setup", Loopback).await, StatusCode::OK);

    // Claim it.
    let payload = serde_json::json!({
        "email_provider": "Outlook",
        "imap_server": "outlook.office365.com",
        "email_address": "ingest@station.gr",
        "email_password": "",
        "ollama_endpoint": "http://localhost:11434/v1",
        "ollama_model": "gemma",
        "watchfolder_path": "C:/watch",
        "admin_email": "it@station.gr",
        "admin_password": "correct-horse-battery",
        "admin_full_name": "Administrator"
    });
    let res = h
        .router
        .clone()
        .layer(MockConnectInfo(LOOPBACK.parse::<SocketAddr>().unwrap()))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/setup")
                .header("content-type", "application/json")
                .header("x-omni-request", "1")
                .header("host", "127.0.0.1:8080")
                .header("origin", "http://127.0.0.1:8080")
                .body(Body::from(payload.to_string()))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    assert!(h.repo.has_active_admin()?);

    // The window is now shut, for loopback too.
    assert_eq!(
        h.request("GET", "/setup", Loopback).await,
        StatusCode::NOT_FOUND
    );
    Ok(())
}

#[tokio::test]
async fn w04_no_default_admin_is_seeded_into_a_fresh_database() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = Repository::new(dir.path().join("omni.db"))?;

    assert!(
        !repo.has_active_admin()?,
        "a fresh database must not come with an administrator"
    );
    assert!(
        repo.get_user_by_email("admin@newsroom.local")?.is_none(),
        "the published default account must not exist"
    );
    assert!(repo.list_users()?.is_empty());
    Ok(())
}

// ==========================================
// CSRF
// ==========================================

async fn post_with(
    h: &Harness,
    headers: &[(&str, &str)],
    caller_token: &str,
) -> StatusCode {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/jobs/1/retry")
        .header("cookie", format!("omni_session={}", caller_token));
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    h.router
        .clone()
        .layer(MockConnectInfo(OUTSIDER.parse::<SocketAddr>().unwrap()))
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn w07_state_changing_requests_need_the_panel_marker_and_a_same_origin() -> Result<()> {
    let h = Harness::new(true)?;
    let token = h.tokens.mcr.clone();

    // A cross-site form post: the browser sends the session cookie, but cannot
    // set a custom header.
    assert_eq!(
        post_with(
            &h,
            &[("host", "mcr.local"), ("origin", "https://evil.example")],
            &token
        )
        .await,
        StatusCode::FORBIDDEN
    );

    // Marker present but the origin is someone else's page.
    assert_eq!(
        post_with(
            &h,
            &[
                ("x-omni-request", "1"),
                ("host", "mcr.local"),
                ("origin", "https://evil.example")
            ],
            &token
        )
        .await,
        StatusCode::FORBIDDEN
    );

    // The panel's own request.
    assert_ne!(
        post_with(
            &h,
            &[
                ("x-omni-request", "1"),
                ("host", "mcr.local"),
                ("origin", "http://mcr.local")
            ],
            &token
        )
        .await,
        StatusCode::FORBIDDEN
    );

    // A stripped Origin/Referer (some corporate proxies) is accepted when the
    // marker is there — see the note on `csrf_guard`.
    assert_ne!(
        post_with(&h, &[("x-omni-request", "1"), ("host", "mcr.local")], &token).await,
        StatusCode::FORBIDDEN
    );

    // GET is never blocked by the guard.
    let status = h.request("GET", "/api/jobs", Mcr).await;
    assert_eq!(status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn responses_carry_the_security_headers() -> Result<()> {
    let h = Harness::new(true)?;
    let res = h
        .router
        .clone()
        .layer(MockConnectInfo(OUTSIDER.parse::<SocketAddr>().unwrap()))
        .oneshot(Request::builder().uri("/login").body(Body::empty())?)
        .await?;

    let headers = res.headers();
    let csp = headers
        .get("content-security-policy")
        .unwrap()
        .to_str()?
        .to_string();
    assert!(csp.contains("default-src 'self'"));
    assert!(csp.contains("script-src 'self'"));
    assert!(csp.contains("frame-ancestors 'none'"));
    assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
    assert_eq!(headers.get("x-frame-options").unwrap(), "DENY");
    assert_eq!(headers.get("referrer-policy").unwrap(), "no-referrer");
    // No CORS layer: the previous `allow_origin(Any)` made every read API
    // scriptable from any page an operator had open.
    assert!(headers.get("access-control-allow-origin").is_none());

    // API responses are additionally uncacheable.
    let api = h
        .router
        .clone()
        .layer(MockConnectInfo(OUTSIDER.parse::<SocketAddr>().unwrap()))
        .oneshot(Request::builder().uri("/api/health").body(Body::empty())?)
        .await?;
    assert_eq!(api.headers().get("cache-control").unwrap(), "no-store");
    Ok(())
}

// ==========================================
// Coverage guard
// ==========================================

/// Fail if `server.rs` declares a route this file never exercises.
///
/// This is the test that would have caught W-01 and W-02 when they were
/// written: both were routes added to the table with no policy decision
/// attached, and no amount of testing the *existing* routes would have noticed.
#[test]
fn no_route_escapes_the_matrix() {
    let source = include_str!("../src/server.rs");

    let mut declared: Vec<String> = Vec::new();
    for line in source.lines() {
        let Some(rest) = line.trim().strip_prefix(".route(\"") else {
            continue;
        };
        let Some(path) = rest.split('"').next() else {
            continue;
        };
        declared.push(path.to_string());
    }
    assert!(
        declared.len() > 20,
        "route extraction broke: found only {} routes",
        declared.len()
    );

    let covered: Vec<&str> = MATRIX.iter().map(|(_, uri, _)| *uri).collect();

    let missing: Vec<&String> = declared
        .iter()
        .filter(|declared_path| {
            !covered
                .iter()
                .any(|tested| paths_match(declared_path, tested))
        })
        .collect();

    assert!(
        missing.is_empty(),
        "these routes have no entry in MATRIX, so nothing asserts who may reach them: {missing:?}"
    );
}

/// Does a concrete tested URI match an axum route pattern (`:id`, `*file`)?
fn paths_match(pattern: &str, actual: &str) -> bool {
    let p: Vec<&str> = pattern.trim_matches('/').split('/').collect();
    let a: Vec<&str> = actual.trim_matches('/').split('/').collect();
    if pattern == "/" {
        return actual == "/";
    }
    let mut ai = a.iter();
    for seg in &p {
        if seg.starts_with('*') {
            return true; // wildcard swallows the rest
        }
        let Some(actual_seg) = ai.next() else {
            return false;
        };
        if seg.starts_with(':') {
            continue;
        }
        if seg != actual_seg {
            return false;
        }
    }
    ai.next().is_none()
}
