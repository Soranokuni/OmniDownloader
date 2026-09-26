//! Login, sessions and lockout (plan P2.2).

use anyhow::Result;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use std::net::SocketAddr;
use tempfile::TempDir;
use tower::ServiceExt;

use omni_core::config::AppConfig;
use omni_core::models::UserRole;
use omni_core::repository::Repository;
use omni_web::server::WebServer;
use omni_web::state::AppState;

const PASSWORD: &str = "correct-horse-battery-staple";

struct App {
    router: axum::Router,
    repo: Repository,
    _dir: TempDir,
}

fn build(configure: impl FnOnce(&mut AppConfig)) -> Result<App> {
    let dir = TempDir::new()?;
    let repo = Repository::new(dir.path().join("omni.db"))?;
    let mut config = AppConfig::default();
    configure(&mut config);

    repo.create_user(
        "it@station.gr",
        PASSWORD,
        UserRole::Admin,
        "Administrator",
        None,
    )?;
    repo.create_user(
        "reporter@station.gr",
        PASSWORD,
        UserRole::User,
        "Reporter",
        Some("PAPADAKI"),
    )?;

    let state = AppState::new(repo.clone(), config, dir.path().join("config.json"));
    Ok(App {
        router: WebServer::build_router(state),
        repo,
        _dir: dir,
    })
}

async fn login(app: &App, peer: &str, email: &str, password: &str) -> Result<(StatusCode, Option<String>)> {
    let res = app
        .router
        .clone()
        .layer(MockConnectInfo(peer.parse::<SocketAddr>()?))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .header("x-omni-request", "1")
                .header("host", "mcr.local")
                .header("origin", "http://mcr.local")
                .body(Body::from(
                    json!({"email": email, "password": password}).to_string(),
                ))?,
        )
        .await?;
    let status = res.status();
    let cookie = res
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    Ok((status, cookie))
}

#[tokio::test]
async fn a_correct_password_issues_a_hardened_cookie() -> Result<()> {
    let app = build(|_| {})?;
    let (status, cookie) = login(&app, "10.0.0.1:1234", "it@station.gr", PASSWORD).await?;
    assert_eq!(status, StatusCode::OK);

    let cookie = cookie.expect("a session cookie must be set");
    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    assert!(cookie.contains("Path=/"), "{cookie}");
    // No TLS configured, so no `Secure`: a Secure cookie over plain HTTP is
    // never sent back, and presents as "login does nothing".
    assert!(!cookie.contains("Secure"), "{cookie}");
    Ok(())
}

#[tokio::test]
async fn an_unknown_account_and_a_wrong_password_are_indistinguishable() -> Result<()> {
    let app = build(|_| {})?;

    let res_unknown = app
        .router
        .clone()
        .layer(MockConnectInfo("10.0.0.1:1".parse::<SocketAddr>()?))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .header("x-omni-request", "1")
                .body(Body::from(
                    json!({"email":"nobody@station.gr","password":"whatever"}).to_string(),
                ))?,
        )
        .await?;
    let status_unknown = res_unknown.status();
    let body_unknown = res_unknown.into_body().collect().await?.to_bytes();

    let res_wrong = app
        .router
        .clone()
        .layer(MockConnectInfo("10.0.0.2:1".parse::<SocketAddr>()?))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .header("x-omni-request", "1")
                .body(Body::from(
                    json!({"email":"it@station.gr","password":"wrong"}).to_string(),
                ))?,
        )
        .await?;
    let status_wrong = res_wrong.status();
    let body_wrong = res_wrong.into_body().collect().await?.to_bytes();

    assert_eq!(status_unknown, StatusCode::UNAUTHORIZED);
    assert_eq!(status_wrong, StatusCode::UNAUTHORIZED);
    // Byte-identical: the login form must not tell an anonymous caller which
    // newsroom addresses have accounts.
    assert_eq!(body_unknown, body_wrong);
    Ok(())
}

#[tokio::test]
async fn repeated_failures_from_one_host_are_locked_out_with_retry_after() -> Result<()> {
    let app = build(|c| {
        c.security.login_rate_limit_per_5min = 3;
        c.security.login_rate_limit_per_account_hour = 1000;
    })?;

    for _ in 0..3 {
        let (status, _) = login(&app, "10.0.0.9:1", "it@station.gr", "wrong").await?;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // Fourth attempt is refused before the password is even compared, so the
    // correct password does not get in either.
    let res = app
        .router
        .clone()
        .layer(MockConnectInfo("10.0.0.9:1".parse::<SocketAddr>()?))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .header("x-omni-request", "1")
                .body(Body::from(
                    json!({"email":"it@station.gr","password": PASSWORD}).to_string(),
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(res.headers().get("retry-after").is_some());

    // A different workstation is unaffected.
    let (status, _) = login(&app, "10.0.0.10:1", "it@station.gr", PASSWORD).await?;
    assert_eq!(status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn every_login_attempt_is_recorded() -> Result<()> {
    let app = build(|_| {})?;
    login(&app, "10.0.0.5:1", "it@station.gr", "wrong").await?;
    login(&app, "10.0.0.5:1", "it@station.gr", PASSWORD).await?;

    // Written straight to the table the audit trail reads from. The password
    // itself is never part of the row.
    let conn_check = app.repo.recent_login_attempts(10)?;
    assert_eq!(conn_check.len(), 2);
    assert!(conn_check.iter().any(|a| !a.successful));
    assert!(conn_check.iter().any(|a| a.successful));
    assert!(conn_check.iter().all(|a| a.ip.as_deref() == Some("10.0.0.5")));
    assert!(
        !format!("{conn_check:?}").contains(PASSWORD),
        "a password must never reach the attempt log"
    );
    Ok(())
}

#[tokio::test]
async fn session_lifetime_follows_the_role() -> Result<()> {
    // Admin sessions are the shortest: an admin session can repoint the
    // watchfolder. The MCR desk gets the longest, because a login prompt
    // mid-bulletin is its own kind of outage.
    let app = build(|c| {
        c.security.session_hours_admin = 8;
        c.security.session_hours_user = 12;
    })?;

    let (_, admin_cookie) = login(&app, "10.0.0.1:1", "it@station.gr", PASSWORD).await?;
    let (_, user_cookie) = login(&app, "10.0.0.2:1", "reporter@station.gr", PASSWORD).await?;

    let max_age = |c: Option<String>| -> i64 {
        let c = c.unwrap();
        c.split(';')
            .find_map(|p| p.trim().strip_prefix("Max-Age=").map(str::to_string))
            .unwrap()
            .parse()
            .unwrap()
    };

    assert_eq!(max_age(admin_cookie), 8 * 3600);
    assert_eq!(max_age(user_cookie), 12 * 3600);
    Ok(())
}

#[tokio::test]
async fn an_idle_session_stops_working_even_before_its_absolute_expiry() -> Result<()> {
    let app = build(|c| {
        c.security.session_idle_hours = 1;
    })?;

    let user = app.repo.get_user_by_email("it@station.gr")?.unwrap();
    let token = omni_core::auth::generate_session_token();
    // A 30-day absolute lifetime, so only the idle clock can end this.
    app.repo
        .create_session_with_meta(user.id, &token, chrono::Duration::days(30), None, None)?;

    // Works now.
    let res = app
        .router
        .clone()
        .layer(MockConnectInfo("10.0.0.1:1".parse::<SocketAddr>()?))
        .oneshot(
            Request::builder()
                .uri("/api/auth/me")
                .header("cookie", format!("omni_session={token}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);

    // Backdate last use past the idle window, as an unattended browser would.
    app.repo
        .backdate_session_last_seen(&token, chrono::Duration::hours(3))?;

    let res = app
        .router
        .clone()
        .layer(MockConnectInfo("10.0.0.1:1".parse::<SocketAddr>()?))
        .oneshot(
            Request::builder()
                .uri("/api/auth/me")
                .header("cookie", format!("omni_session={token}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // ...and the row is gone, not merely refused.
    assert!(app.repo.get_user_by_session_token(&token)?.is_none());
    Ok(())
}

#[tokio::test]
async fn changing_a_password_ends_every_session_for_that_account() -> Result<()> {
    let app = build(|_| {})?;

    let (_, cookie_a) = login(&app, "10.0.0.1:1", "reporter@station.gr", PASSWORD).await?;
    let (_, cookie_b) = login(&app, "10.0.0.2:1", "reporter@station.gr", PASSWORD).await?;
    let token_a = session_token(cookie_a.unwrap());
    let token_b = session_token(cookie_b.unwrap());

    let res = app
        .router
        .clone()
        .layer(MockConnectInfo("10.0.0.1:1".parse::<SocketAddr>()?))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/password")
                .header("content-type", "application/json")
                .header("x-omni-request", "1")
                .header("cookie", format!("omni_session={token_a}"))
                .body(Body::from(
                    json!({
                        "current_password": PASSWORD,
                        "new_password": "a-brand-new-passphrase"
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);

    // The *other* device is logged out too. A password change that leaves old
    // sessions alive has revoked nothing.
    for token in [&token_a, &token_b] {
        let res = app
            .router
            .clone()
            .layer(MockConnectInfo("10.0.0.1:1".parse::<SocketAddr>()?))
            .oneshot(
                Request::builder()
                    .uri("/api/auth/me")
                    .header("cookie", format!("omni_session={token}"))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
    }

    // And the new password works.
    let (status, _) = login(
        &app,
        "10.0.0.3:1",
        "reporter@station.gr",
        "a-brand-new-passphrase",
    )
    .await?;
    assert_eq!(status, StatusCode::OK);
    Ok(())
}

#[tokio::test]
async fn a_password_change_needs_the_current_password() -> Result<()> {
    let app = build(|_| {})?;
    let (_, cookie) = login(&app, "10.0.0.1:1", "reporter@station.gr", PASSWORD).await?;
    let token = session_token(cookie.unwrap());

    let res = app
        .router
        .clone()
        .layer(MockConnectInfo("10.0.0.1:1".parse::<SocketAddr>()?))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/password")
                .header("content-type", "application/json")
                .header("x-omni-request", "1")
                .header("cookie", format!("omni_session={token}"))
                .body(Body::from(
                    json!({
                        "current_password": "not-it",
                        "new_password": "a-brand-new-passphrase"
                    })
                    .to_string(),
                ))?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    // The old password still works, so nothing was changed.
    let (status, _) = login(&app, "10.0.0.4:1", "reporter@station.gr", PASSWORD).await?;
    assert_eq!(status, StatusCode::OK);
    Ok(())
}

fn session_token(cookie: String) -> String {
    cookie
        .split(';')
        .next()
        .unwrap()
        .trim()
        .strip_prefix("omni_session=")
        .unwrap()
        .to_string()
}

/// Plan P2.6: the panel can say whether a secret is set, and replace it, but
/// there is no route that returns one.
#[tokio::test]
async fn the_secrets_api_never_returns_a_value() -> Result<()> {
    let dir = TempDir::new()?;
    let repo = Repository::new(dir.path().join("omni.db"))?;
    let admin = repo.create_user("it@station.gr", PASSWORD, UserRole::Admin, "Admin", None)?;
    let token = omni_core::auth::generate_session_token();
    repo.create_session(admin, &token, 1)?;

    let store = omni_core::secrets::SecretStore::new(dir.path().join("secrets.bin"));
    let state = AppState::new(repo, AppConfig::default(), dir.path().join("config.json"))
        .with_secret_store(store.clone());
    let router = WebServer::build_router(state);

    let send = |method: &'static str, body: Option<serde_json::Value>, token: String| {
        let router = router.clone();
        async move {
            let mut builder = Request::builder()
                .method(method)
                .uri("/api/secrets")
                .header("x-omni-request", "1")
                .header("cookie", format!("omni_session={token}"));
            let body = match body {
                Some(v) => {
                    builder = builder.header("content-type", "application/json");
                    Body::from(v.to_string())
                }
                None => Body::empty(),
            };
            let res = router
                .layer(MockConnectInfo("127.0.0.1:1".parse::<SocketAddr>().unwrap()))
                .oneshot(builder.body(body).unwrap())
                .await
                .unwrap();
            let status = res.status();
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            (status, String::from_utf8_lossy(&bytes).into_owned())
        }
    };

    // Set it through the API.
    let (status, _) = send(
        "POST",
        Some(json!({"key": "graph.client_secret", "value": "the-actual-mailbox-secret"})),
        token.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        store.get("graph.client_secret")?.as_deref(),
        Some("the-actual-mailbox-secret")
    );

    // Read it back: only `is_set`.
    let (status, body) = send("GET", None, token.clone()).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("\"graph.client_secret\":true"), "{body}");
    assert!(
        !body.contains("the-actual-mailbox-secret"),
        "the API handed back the secret itself: {body}"
    );

    // An unknown key is refused rather than silently written somewhere that
    // nothing will ever read.
    let (status, _) = send(
        "POST",
        Some(json!({"key": "made.up", "value": "x"})),
        token.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The audit row names the key and not the value.
    let logs = state_logs(&dir)?;
    assert!(logs.iter().any(|m| m.contains("graph.client_secret")), "{logs:?}");
    assert!(
        !logs.iter().any(|m| m.contains("the-actual-mailbox-secret")),
        "the secret leaked into the audit log: {logs:?}"
    );
    Ok(())
}

fn state_logs(dir: &TempDir) -> Result<Vec<String>> {
    let repo = Repository::new(dir.path().join("omni.db"))?;
    Ok(repo
        .get_recent_audit_logs(50)?
        .into_iter()
        .map(|l| l.message)
        .collect())
}
