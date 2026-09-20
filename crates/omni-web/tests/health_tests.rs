//! The status endpoints report reality (plan P6.2, defect W-09).
//!
//! `mail_status: "Active"` and `llm_status: "Ready"` were string literals. They
//! were true when they were written and never checked again, so the panel
//! showed a healthy mailbox while the mailbox was refusing the password — which
//! is worse than showing nothing, because an operator who trusts it stops
//! looking there. These tests drive the health state to each outcome and assert
//! the endpoints follow it.

use anyhow::Result;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::Value;
use std::net::SocketAddr;
use tempfile::TempDir;
use tower::ServiceExt;

use omni_core::config::AppConfig;
use omni_core::health::{checks, Check, HealthState};
use omni_core::models::{JobStatus, UserRole};
use omni_core::repository::Repository;
use omni_web::server::WebServer;
use omni_web::state::AppState;

const PEER: &str = "127.0.0.1:40000";

struct App {
    router: axum::Router,
    repo: Repository,
    health: HealthState,
    token: String,
    _dir: TempDir,
}

fn build() -> Result<App> {
    let dir = TempDir::new()?;
    let repo = Repository::new(dir.path().join("omni.db"))?;
    let health = HealthState::new();

    let mcr = repo.create_user(
        "desk@station.gr",
        "correct-horse-battery",
        UserRole::OpenMcr,
        "MCR Desk",
        None,
    )?;
    let token = omni_core::auth::generate_session_token();
    repo.create_session(mcr, &token, 1)?;

    let state = AppState::new(repo.clone(), AppConfig::default(), dir.path().join("cfg.json"))
        .with_health(health.clone());

    Ok(App {
        router: WebServer::build_router(state),
        repo,
        health,
        token,
        _dir: dir,
    })
}

impl App {
    async fn get(&self, uri: &str, with_session: bool) -> Result<(StatusCode, Value)> {
        let mut builder = Request::builder().uri(uri);
        if with_session {
            builder = builder.header("cookie", format!("omni_session={}", self.token));
        }
        let res = self
            .router
            .clone()
            .layer(MockConnectInfo(PEER.parse::<SocketAddr>()?))
            .oneshot(builder.body(Body::empty())?)
            .await?;
        let status = res.status();
        let bytes = res.into_body().collect().await?.to_bytes();
        Ok((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
    }
}

#[tokio::test]
async fn system_status_follows_the_real_check_rather_than_a_literal() -> Result<()> {
    let app = build()?;

    // A mailbox that is refusing the password. Previously this was the exact
    // situation in which the panel said "Mail: Active".
    app.health.set(
        checks::MAIL,
        Check::degraded("mailbox rejected the credentials"),
    );
    app.health.set(checks::LLM, Check::ok("gemma3:4b at localhost"));

    let (status, body) = app.get("/api/system/status", true).await?;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(body["status"], "degraded");
    assert_eq!(body["checks"]["mail"]["state"], "degraded");
    assert_eq!(
        body["checks"]["mail"]["detail"],
        "mailbox rejected the credentials"
    );
    assert_eq!(body["checks"]["llm"]["state"], "ok");

    // The legacy two-word fields are derived now, not hardcoded, so a panel
    // cached from before the upgrade still tells the truth.
    assert_eq!(body["mail_status"], "Degraded");
    assert_eq!(body["llm_status"], "Active");

    // And it recovers.
    app.health.set(checks::MAIL, Check::ok("polling"));
    let (_, body) = app.get("/api/system/status", true).await?;
    assert_eq!(body["status"], "ok");
    assert_eq!(body["mail_status"], "Active");
    Ok(())
}

#[tokio::test]
async fn system_status_reports_the_queue_without_loading_the_archive() -> Result<()> {
    let app = build()?;

    app.repo.add_job(
        "https://example.com/a",
        "1_MCR_A",
        "MCR",
        "A",
        "1",
        0,
        JobStatus::Pending,
        None,
        None,
        None,
    )?;
    let reviewed = app.repo.add_job(
        "https://example.com/b",
        "2_MCR_B",
        "MCR",
        "B",
        "2",
        0,
        JobStatus::Pending,
        None,
        None,
        None,
    )?;
    app.repo
        .update_job_status(reviewed, JobStatus::RequiresReview, Some("no stream"), None, None)?;

    let (_, body) = app.get("/api/system/status", true).await?;
    assert_eq!(body["queue"]["pending"], 1);
    assert_eq!(body["queue"]["review"], 1);
    assert_eq!(body["queue"]["total"], 2);

    // The number that actually says "the pipeline has stalled". A plain
    // pending count cannot: twenty pending jobs are normal right after a
    // rundown arrives and alarming an hour later.
    assert!(
        body["queue"]["oldest_pending_age_secs"].is_number(),
        "{body}"
    );
    Ok(())
}

#[tokio::test]
async fn public_health_gives_a_verdict_and_no_detail() -> Result<()> {
    let app = build()?;

    app.health.set(
        checks::TOOLS,
        Check::down("missing or unusable: ffmpeg, ffprobe (expected in D:\\OmniDownloader\\bin)"),
    );
    app.health.set(
        checks::MAIL,
        Check::degraded("IMAP authentication failed for ingest@station.gr"),
    );

    // No session: this is what an external monitor sees.
    let (status, body) = app.get("/api/health", false).await?;

    // 503, because half of monitoring tools only look at the status code. A
    // cheerful 200 with bad news in the body is a health check that never
    // fires.
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["status"], "down");
    assert_eq!(body["checks"]["tools"], "down");
    assert_eq!(body["checks"]["mail"], "degraded");

    let raw = body.to_string();
    assert!(!raw.contains("ffmpeg"), "leaked which tool is missing: {raw}");
    assert!(!raw.contains("OmniDownloader"), "leaked a server path: {raw}");
    assert!(
        !raw.contains("ingest@station.gr"),
        "leaked the station mailbox: {raw}"
    );
    Ok(())
}

#[tokio::test]
async fn public_health_is_200_while_merely_degraded() -> Result<()> {
    // Degraded means the newsroom is still working. Returning 503 here would
    // page someone every time the optional LLM was restarted.
    let app = build()?;
    app.health
        .set(checks::LLM, Check::degraded("endpoint unreachable"));

    let (status, body) = app.get("/api/health", false).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "degraded");
    Ok(())
}

#[tokio::test]
async fn a_fresh_daemon_is_healthy_rather_than_unknown() -> Result<()> {
    // Before the first poll of anything. Reporting "down" here would page
    // someone on every restart.
    let app = build()?;
    let (status, body) = app.get("/api/health", false).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "ok");
    assert!(body["version"].is_string());
    assert!(body["uptime_secs"].is_number());
    Ok(())
}

#[tokio::test]
async fn the_detailed_status_is_not_public() -> Result<()> {
    // It carries paths, the watchfolder location and the mail server name, so
    // it sits behind the MCR gate while `/api/health` does not.
    //
    // The caller has to be *off* the allowlist to show this: the default
    // `mcr_open_networks` includes loopback, so an anonymous request from
    // 127.0.0.1 is a legitimate MCR principal and gets the full status. That is
    // the intended policy, not a hole — but it means a test peered on loopback
    // proves nothing about who is turned away.
    let app = build()?;
    let outsider: SocketAddr = "192.0.2.50:40000".parse()?;

    let res = app
        .router
        .clone()
        .layer(MockConnectInfo(outsider))
        .oneshot(
            Request::builder()
                .uri("/api/system/status")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    // ...while the public verdict still answers that same caller.
    let res = app
        .router
        .clone()
        .layer(MockConnectInfo(outsider))
        .oneshot(Request::builder().uri("/api/health").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    Ok(())
}
