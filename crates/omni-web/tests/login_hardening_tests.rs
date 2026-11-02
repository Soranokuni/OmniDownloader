//! Login path hardening (plan P2.18): bounded audit rows, Argon2 off the runtime,
//! atomic attempt reservation, no timing oracle, a limited change-password.

use anyhow::Result;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
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
    state: AppState,
    repo: Repository,
    _dir: TempDir,
}

fn build(ip_limit: u32, account_limit: u32) -> Result<App> {
    let dir = TempDir::new()?;
    let repo = Repository::new(dir.path().join("omni.db"))?;
    let mut config = AppConfig::default();
    config.security.login_rate_limit_per_5min = ip_limit;
    config.security.login_rate_limit_per_account_hour = account_limit;
    repo.create_user("it@station.gr", PASSWORD, UserRole::Admin, "Administrator", None)?;
    repo.create_user("reporter@station.gr", PASSWORD, UserRole::User, "Reporter", Some("PAPADAKI"))?;
    let state = AppState::new(repo.clone(), config, dir.path().join("config.json"));
    Ok(App {
        router: WebServer::build_router(state.clone()),
        state,
        repo,
        _dir: dir,
    })
}

async fn login(
    router: &axum::Router,
    peer: &str,
    email: &str,
    password: &str,
) -> Result<(StatusCode, Option<String>)> {
    let res = router
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
                .body(Body::from(json!({"email": email, "password": password}).to_string()))?,
        )
        .await?;
    let cookie = res
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    Ok((res.status(), cookie))
}

async fn change_password(
    router: &axum::Router,
    peer: &str,
    token: &str,
    current: &str,
) -> Result<StatusCode> {
    let res = router
        .clone()
        .layer(MockConnectInfo(peer.parse::<SocketAddr>()?))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/auth/password")
                .header("content-type", "application/json")
                .header("x-omni-request", "1")
                .header("cookie", format!("omni_session={token}"))
                .body(Body::from(
                    json!({"current_password": current, "new_password": "a-brand-new-passphrase"})
                        .to_string(),
                ))?,
        )
        .await?;
    Ok(res.status())
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

#[tokio::test]
async fn a_flood_of_denied_attempts_writes_one_rate_limited_row() -> Result<()> {
    let app = build(3, 100)?;
    for _ in 0..3 {
        let (s, _) = login(&app.router, "10.0.0.1:1", "reporter@station.gr", "wrong").await?;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
    for _ in 0..50 {
        let (s, _) = login(&app.router, "10.0.0.1:1", "Reporter@Station.gr", "wrong").await?;
        assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    }
    let rows = app.repo.recent_login_attempts(1000)?;
    let limited = rows.iter().filter(|r| r.reason.as_deref() == Some("rate_limited")).count();
    assert_eq!(limited, 1, "50 denied attempts must leave exactly one audit row");
    Ok(())
}

#[tokio::test]
async fn a_huge_email_is_stored_truncated() -> Result<()> {
    let app = build(0, 0)?;
    let huge = format!("{}@station.gr", "a".repeat(60 * 1024));
    // Larger than the body cap would be refused, so stay just under it.
    let (s, _) = login(&app.router, "10.0.0.1:1", &huge, "wrong").await?;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let rows = app.repo.recent_login_attempts(10)?;
    assert_eq!(rows[0].email.as_deref().unwrap().chars().count(), 254);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_wrong_logins_complete_and_the_runtime_keeps_answering() -> Result<()> {
    let app = build(0, 0)?;
    let mut tasks = Vec::new();
    for i in 0..20 {
        let router = app.router.clone();
        tasks.push(tokio::spawn(async move {
            login(&router, &format!("10.0.1.{}:1", i + 1), "reporter@station.gr", "wrong").await
        }));
    }
    // While the Argon2 work drains, the health endpoint must keep answering. It is
    // spawned, so it needs a free runtime worker: with Argon2 run inline both of
    // the two workers would be stuck in a hash and it would wait behind them.
    let mut slowest = Duration::ZERO;
    let mut samples = 0;
    std::thread::sleep(Duration::from_millis(200)); // let the logins get going
    while tasks.iter().any(|t| !t.is_finished()) {
        let router = app.router.clone();
        let t0 = Instant::now();
        let health = tokio::spawn(async move {
            let res = router
                .layer(MockConnectInfo("127.0.0.1:9".parse::<SocketAddr>().unwrap()))
                .oneshot(Request::builder().uri("/api/health").body(Body::empty()).unwrap())
                .await
                .unwrap();
            res.status()
        });
        // Timed from the spawn: the wait for a free worker is the point.
        let status = health.await?;
        let took = t0.elapsed();
        assert!(status.is_success() || status.is_client_error());
        slowest = slowest.max(took);
        samples += 1;
        // Pace on the test thread, not the runtime: a stalled runtime must not
        // be able to hide itself by delaying the pacing timer too.
        std::thread::sleep(Duration::from_millis(50));
    }
    for t in tasks {
        assert_eq!(t.await??.0, StatusCode::UNAUTHORIZED);
    }
    println!("slowest health answer: {slowest:?} over {samples} samples");
    assert!(slowest < Duration::from_secs(2), "health took {slowest:?} during the burst");
    assert_eq!(app.state.password_checks.load(Ordering::SeqCst), 20);
    Ok(())
}

#[tokio::test]
async fn a_dropped_request_keeps_its_argon2_permit_until_the_job_finishes() -> Result<()> {
    let app = build(0, 0)?;
    let permits = app.state.argon_permits.clone();
    let total = permits.available_permits();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let state = app.state.clone();
    let call = tokio::spawn(async move {
        state
            .argon(move |_| {
                let _ = started_tx.send(());
                let _ = release_rx.recv();
            })
            .await
    });
    started_rx.await?;
    assert_eq!(permits.available_permits(), total - 1);

    // The client disconnects: hyper drops the request future.
    call.abort();
    assert!(call.await.unwrap_err().is_cancelled());
    assert_eq!(
        permits.available_permits(),
        total - 1,
        "the permit must stay taken while the blocking job still runs"
    );

    release_tx.send(())?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while permits.available_permits() != total {
        assert!(Instant::now() < deadline, "the permit was never returned");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parallel_burst_reaches_password_verification_only_up_to_the_account_limit() -> Result<()> {
    let app = build(0, 5)?;
    let mut tasks = Vec::new();
    for i in 0..50 {
        let router = app.router.clone();
        tasks.push(tokio::spawn(async move {
            login(&router, &format!("10.0.2.{}:1", i + 1), "reporter@station.gr", "wrong").await
        }));
    }
    let mut limited = 0;
    for t in tasks {
        if t.await??.0 == StatusCode::TOO_MANY_REQUESTS {
            limited += 1;
        }
    }
    assert_eq!(app.state.password_checks.load(Ordering::SeqCst), 5);
    assert_eq!(limited, 45);
    Ok(())
}

#[tokio::test]
async fn an_unknown_or_deactivated_account_still_pays_for_a_verification() -> Result<()> {
    let app = build(0, 0)?;
    let (s, _) = login(&app.router, "10.0.0.1:1", "nobody@station.gr", "wrong").await?;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(app.state.password_checks.load(Ordering::SeqCst), 1);

    // The lookup does not return a deactivated account, so it takes the same
    // "unknown" path, dummy verification included.
    let id = app.repo.get_user_by_email("reporter@station.gr")?.unwrap().id;
    app.repo.set_user_active_status(id, false)?;
    let (s, _) = login(&app.router, "10.0.0.1:1", "reporter@station.gr", PASSWORD).await?;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert_eq!(app.state.password_checks.load(Ordering::SeqCst), 2);
    Ok(())
}

#[tokio::test]
async fn a_flood_with_a_different_email_each_time_still_writes_one_row() -> Result<()> {
    let app = build(3, 100)?;
    for i in 0..3 {
        let (s, _) = login(&app.router, "10.0.0.9:1", &format!("a{i}@station.gr"), "wrong").await?;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
    for i in 0..50 {
        let (s, _) = login(&app.router, "10.0.0.9:1", &format!("b{i}@station.gr"), "wrong").await?;
        assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    }
    let rows = app.repo.recent_login_attempts(1000)?;
    let limited = rows.iter().filter(|r| r.reason.as_deref() == Some("rate_limited")).count();
    assert_eq!(limited, 1, "an IP lockout is one row whatever e-mails the flood uses");
    Ok(())
}

/// `legacy-pass-for-tests` hashed with Argon2::default() (m=19456, t=2, p=1).
const LEGACY_HASH: &str =
    "$argon2id$v=19$m=19456,t=2,p=1$c29tZXNhbHR2YWx1ZQ$CRnEH1tdkkEDDhjIwG+KlWGuM9f0qsDCauHpCmOTNd4";

#[tokio::test]
async fn a_legacy_hash_is_upgraded_on_a_successful_login_only() -> Result<()> {
    let app = build(0, 0)?;
    let user = app.repo.get_user_by_email("reporter@station.gr")?.unwrap();
    assert!(app.repo.upgrade_password_hash(user.id, &user.password_hash, LEGACY_HASH)?);
    assert!(omni_core::auth::needs_rehash(LEGACY_HASH));

    // A wrong password leaves the legacy hash alone.
    let (s, _) = login(&app.router, "10.0.0.1:1", "reporter@station.gr", "wrong").await?;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let stored = app.repo.get_user_by_email("reporter@station.gr")?.unwrap().password_hash;
    assert_eq!(stored, LEGACY_HASH);

    let (s, _) = login(&app.router, "10.0.0.1:1", "reporter@station.gr", "legacy-pass-for-tests").await?;
    assert_eq!(s, StatusCode::OK);
    let stored = app.repo.get_user_by_email("reporter@station.gr")?.unwrap().password_hash;
    assert_ne!(stored, LEGACY_HASH);
    assert!(!omni_core::auth::needs_rehash(&stored), "got {stored}");
    assert!(omni_core::auth::verify_password("legacy-pass-for-tests", &stored));
    Ok(())
}

#[tokio::test]
async fn change_password_guesses_share_the_account_limit() -> Result<()> {
    let app = build(0, 3)?;
    let (s, cookie) = login(&app.router, "10.0.0.1:1", "reporter@station.gr", PASSWORD).await?;
    assert_eq!(s, StatusCode::OK);
    let token = session_token(cookie.unwrap());

    // Three wrong guesses are answered; the correct password afterwards is not
    // even looked at.
    for _ in 0..3 {
        let s = change_password(&app.router, "10.0.0.1:1", &token, "not-it").await?;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }
    let s = change_password(&app.router, "10.0.0.1:1", &token, PASSWORD).await?;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);

    // Another account is unaffected.
    let (s, _) = login(&app.router, "10.0.0.2:1", "it@station.gr", PASSWORD).await?;
    assert_eq!(s, StatusCode::OK);
    // And the password was not changed.
    let (s, _) = login(&app.router, "10.0.0.3:1", "reporter@station.gr", "a-brand-new-passphrase").await?;
    assert_ne!(s, StatusCode::OK);
    Ok(())
}
