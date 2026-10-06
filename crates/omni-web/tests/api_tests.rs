//! Functional coverage of the JSON API.
//!
//! These tests were originally written against an API with no authentication
//! at all, so every request here now carries a session and the CSRF marker the
//! panel sends. Who is *allowed* to call what is not this file's job — that is
//! `auth_matrix_tests.rs`. This one checks that the calls do the right thing
//! once they are through the door.

use anyhow::Result;
use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::net::SocketAddr;
use tempfile::TempDir;
use tower::ServiceExt;

use omni_core::config::AppConfig;
use omni_core::models::{JobStatus, UserRole};
use omni_core::repository::Repository;
use omni_web::server::WebServer;
use omni_web::state::AppState;

const PEER: &str = "127.0.0.1:50000";

struct App {
    router: axum::Router,
    repo: Repository,
    mcr_token: String,
    user_token: String,
    admin_token: String,
    _dir: TempDir,
}

impl App {
    fn new() -> Result<Self> {
        let dir = TempDir::new()?;
        let repo = Repository::new(dir.path().join("omni.db"))?;
        let config = AppConfig::default();

        let mcr_id = repo.create_user(
            "desk@station.gr",
            "correct-horse-battery",
            UserRole::OpenMcr,
            "MCR Desk",
            None,
        )?;
        let mcr_token = omni_core::auth::generate_session_token();
        repo.create_session(mcr_id, &mcr_token, 1)?;

        let user_id = repo.create_user(
            "reporter@station.gr",
            "correct-horse-battery",
            UserRole::User,
            "Reporter",
            Some("PAPADAKI"),
        )?;
        let user_token = omni_core::auth::generate_session_token();
        repo.create_session(user_id, &user_token, 1)?;

        let admin_id = repo.create_user(
            "it@station.gr",
            "correct-horse-battery",
            UserRole::Admin,
            "Administrator",
            None,
        )?;
        let admin_token = omni_core::auth::generate_session_token();
        repo.create_session(admin_id, &admin_token, 1)?;

        let state = AppState::new(repo.clone(), config, dir.path().join("config.json"));
        let router = WebServer::build_router(state);

        Ok(Self {
            router,
            repo,
            mcr_token,
            user_token,
            admin_token,
            _dir: dir,
        })
    }

    async fn send(
        &self,
        method: &str,
        uri: &str,
        token: &str,
        body: Option<Value>,
    ) -> Result<(StatusCode, Value)> {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("cookie", format!("omni_session={}", token))
            .header("host", "mcr.local")
            .header("x-omni-request", "1")
            .header("origin", "http://mcr.local");

        let body = match body {
            Some(v) => {
                builder = builder.header("content-type", "application/json");
                Body::from(serde_json::to_vec(&v)?)
            }
            None => Body::empty(),
        };

        let res = self
            .router
            .clone()
            .layer(MockConnectInfo(PEER.parse::<SocketAddr>().unwrap()))
            .oneshot(builder.body(body)?)
            .await?;
        let status = res.status();
        let bytes = res.into_body().collect().await?.to_bytes();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok((status, json))
    }
}

#[tokio::test]
async fn system_status_reports_disk_space() -> Result<()> {
    let app = App::new()?;
    let (status, json) = app
        .send("GET", "/api/system/status", &app.mcr_token, None)
        .await?;

    assert_eq!(status, StatusCode::OK);
    assert!(json.get("free_disk_gb").is_some());
    Ok(())
}

#[tokio::test]
async fn health_is_public_and_says_nothing_about_the_station() -> Result<()> {
    let app = App::new()?;
    let res = app
        .router
        .clone()
        .layer(MockConnectInfo(PEER.parse::<SocketAddr>().unwrap()))
        .oneshot(Request::builder().uri("/api/health").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);

    let bytes = res.into_body().collect().await?.to_bytes();
    let json: Value = serde_json::from_slice(&bytes)?;
    assert_eq!(json.get("status").unwrap(), "ok");
    // Disk, mailbox and queue depth belong behind the MCR gate.
    assert!(json.get("free_disk_gb").is_none());
    assert!(json.get("mail_status").is_none());
    Ok(())
}

#[tokio::test]
async fn jobs_api_full_lifecycle() -> Result<()> {
    let app = App::new()?;

    // 1. Create
    let (status, json) = app
        .send(
            "POST",
            "/api/jobs",
            &app.mcr_token,
            Some(json!({
                "url": "https://www.youtube.com/watch?v=sample123",
                "notes": "ΓΙΑ ΠΛΑΝΑ",
                "priority": 10
            })),
        )
        .await?;
    assert_eq!(status, StatusCode::OK);
    let job_id = json.get("job_id").unwrap().as_i64().unwrap();
    assert!(job_id > 0);

    // 2. List
    let (status, json) = app.send("GET", "/api/jobs", &app.mcr_token, None).await?;
    assert_eq!(status, StatusCode::OK);
    let jobs = json.get("jobs").unwrap().as_array().unwrap();
    assert!(jobs
        .iter()
        .any(|j| j.get("id").unwrap().as_i64().unwrap() == job_id));

    // 3. Override
    let (status, _) = app
        .send(
            "POST",
            &format!("/api/jobs/{}/override", job_id),
            &app.mcr_token,
            Some(json!({"url": "https://www.youtube.com/watch?v=new_sample"})),
        )
        .await?;
    assert_eq!(status, StatusCode::OK);

    let updated = app.repo.get_job(job_id)?.expect("Job must exist");
    assert_eq!(updated.url, "https://www.youtube.com/watch?v=new_sample");
    assert_eq!(updated.status, JobStatus::Pending);

    // 4. Retry
    app.repo
        .update_job_status(job_id, JobStatus::Failed, Some("Simulated fail"), None, None)?;
    let (status, _) = app
        .send(
            "POST",
            &format!("/api/jobs/{}/retry", job_id),
            &app.mcr_token,
            None,
        )
        .await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        app.repo.get_job(job_id)?.unwrap().status,
        JobStatus::Pending
    );

    // 5. Discard
    let (status, _) = app
        .send(
            "POST",
            &format!("/api/jobs/{}/discard", job_id),
            &app.mcr_token,
            None,
        )
        .await?;
    assert_eq!(status, StatusCode::OK);
    assert!(app.repo.get_job(job_id)?.is_none());

    Ok(())
}

#[tokio::test]
async fn a_job_url_must_be_http_or_https() -> Result<()> {
    let app = App::new()?;

    for bad in [
        "javascript:alert(document.cookie)",
        "data:text/html;base64,PHNjcmlwdD4=",
        "file:///C:/Windows/System32/config",
        "",
    ] {
        let (status, _) = app
            .send("POST", "/api/jobs", &app.mcr_token, Some(json!({"url": bad})))
            .await?;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{bad} must be refused at the API, not left for a renderer to make safe"
        );
    }
    assert!(app.repo.get_all_jobs()?.is_empty());
    Ok(())
}

#[tokio::test]
async fn journalists_api_crud() -> Result<()> {
    let app = App::new()?;

    let (status, _) = app
        .send(
            "POST",
            "/api/journalists",
            &app.mcr_token,
            Some(json!({
                "surname": "PAPADOPOULOS",
                "full_name": "Nikos Papadopoulos",
                "emails": ["npapadopoulos@station.gr", "nikos.p@station.gr"],
                "priority": 15
            })),
        )
        .await?;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(
        app.repo
            .find_journalist_by_email("npapadopoulos@station.gr")?
            .as_deref(),
        Some("PAPADOPOULOS")
    );

    let (status, _) = app
        .send(
            "POST",
            "/api/journalists/PAPADOPOULOS",
            &app.mcr_token,
            None,
        )
        .await?;
    assert_eq!(status, StatusCode::OK);
    assert!(app
        .repo
        .find_journalist_by_email("npapadopoulos@station.gr")?
        .is_none());

    Ok(())
}

#[tokio::test]
async fn the_mcr_fallback_journalist_cannot_be_deleted() -> Result<()> {
    // MCR is where every unresolved job is filed and is a delivery folder
    // name. Deleting it does not fail loudly; it silently breaks routing.
    let app = App::new()?;
    let (status, _) = app
        .send("POST", "/api/journalists/MCR", &app.mcr_token, None)
        .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(app
        .repo
        .list_journalists()?
        .iter()
        .any(|j| j.surname == "MCR"));
    Ok(())
}

#[tokio::test]
async fn html_pages_render_for_the_roles_that_may_see_them() -> Result<()> {
    let app = App::new()?;

    for (uri, token, needle) in [
        ("/mcr", &app.mcr_token, "MCR"),
        ("/user", &app.user_token, "Οι αποστολές μου"),
        ("/admin", &app.admin_token, "Administration"),
    ] {
        let res = app
            .router
            .clone()
            .layer(MockConnectInfo(PEER.parse::<SocketAddr>().unwrap()))
            .oneshot(
                Request::builder()
                    .uri(uri)
                    .header("cookie", format!("omni_session={}", token))
                    .body(Body::empty())?,
            )
            .await?;
        assert_eq!(res.status(), StatusCode::OK, "{uri}");
        let body = res.into_body().collect().await?.to_bytes();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains(needle), "{uri} did not render its own page");
    }

    // /login is public.
    let res = app
        .router
        .clone()
        .layer(MockConnectInfo(PEER.parse::<SocketAddr>().unwrap()))
        .oneshot(Request::builder().uri("/login").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);

    Ok(())
}

#[tokio::test]
async fn a_user_sees_only_their_own_jobs() -> Result<()> {
    let app = App::new()?;
    let reporter = app
        .repo
        .get_user_by_email("reporter@station.gr")?
        .unwrap();

    app.repo.add_job(
        "https://www.youtube.com/watch?v=mine",
        "1_PAPADAKI_MINE",
        "PAPADAKI",
        "MINE",
        "1",
        0,
        JobStatus::Pending,
        Some(reporter.id),
        None,
        None,
    )?;
    app.repo.add_job(
        "https://www.youtube.com/watch?v=theirs",
        "2_GEORGIOU_THEIRS",
        "GEORGIOU",
        "THEIRS",
        "2",
        0,
        JobStatus::Pending,
        None,
        None,
        None,
    )?;

    let (status, json) = app
        .send("GET", "/api/jobs/mine", &app.user_token, None)
        .await?;
    assert_eq!(status, StatusCode::OK);
    let jobs = json.get("jobs").unwrap().as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].get("keyword").unwrap(), "MINE");
    Ok(())
}

/// Policy C: MCR queues a video the sniffer offered from a job's article —
/// only one that is on the offer list, and only once.
#[tokio::test]
async fn an_offered_article_video_can_be_queued_once_and_nothing_else() -> Result<()> {
    let app = App::new()?;
    let mut parent = omni_core::models::NewJob::new("https://www.portal.example/story/1", "1A_MCR_SEISMOS", "MCR");
    parent.keyword = "SEISMOS".into();
    parent.index_str = "1A".into();
    let id = app.repo.enqueue(&parent, 24)?.job_id();
    let raw = "https://cdn.portal.example/video/master.m3u8";
    app.repo.set_candidates(
        id,
        Some(&json!([{ "url": raw, "index_str": "1D", "queued_job_id": null }]).to_string()),
    )?;
    let uri = format!("/api/jobs/{id}/offers/queue");

    // Not on the list: refused, nothing queued.
    let (status, _) = app
        .send("POST", &uri, &app.mcr_token, Some(json!({ "url": "https://evil.example/x.mp4" })))
        .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(app.repo.get_all_jobs()?.len(), 1);

    // On the list: queued with the offered index, journalist and keyword.
    let (status, body) = app.send("POST", &uri, &app.mcr_token, Some(json!({ "url": raw }))).await?;
    assert_eq!(status, StatusCode::OK, "{body}");
    let new_id = body["job_id"].as_i64().unwrap();
    let queued = app.repo.get_job(new_id)?.unwrap();
    assert_eq!((queued.url.as_str(), queued.slug.as_str()), (raw, "1D_MCR_SEISMOS"));
    assert_eq!(queued.status, JobStatus::Pending);

    // Twice: refused, and the offer remembers the job it became.
    let (status, body) = app.send("POST", &uri, &app.mcr_token, Some(json!({ "url": raw }))).await?;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let offers: Value = serde_json::from_str(app.repo.get_job(id)?.unwrap().candidates_json.as_deref().unwrap())?;
    assert_eq!(offers[0]["queued_job_id"], json!(new_id));

    // A reporter cannot use it.
    let (status, _) = app.send("POST", &uri, &app.user_token, Some(json!({ "url": raw }))).await?;
    assert!(status == StatusCode::FORBIDDEN || status == StatusCode::UNAUTHORIZED, "{status}");
    Ok(())
}