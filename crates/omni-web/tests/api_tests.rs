use anyhow::Result;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tempfile::NamedTempFile;
use tower::ServiceExt;

use omni_core::config::AppConfig;
use omni_core::models::JobStatus;
use omni_core::repository::Repository;
use omni_web::server::WebServer;
use omni_web::state::AppState;

fn setup_test_app() -> Result<(axum::Router, Repository)> {
    let temp_db = NamedTempFile::new()?;
    let temp_cfg = NamedTempFile::new()?;
    let repo = Repository::new(temp_db.path())?;
    let config = AppConfig::default();
    let state = AppState::new(repo.clone(), config, temp_cfg.path().to_path_buf());
    let router = WebServer::build_router(state);
    Ok((router, repo))
}

#[tokio::test]
async fn test_system_status_endpoint() -> Result<()> {
    let (app, _) = setup_test_app()?;

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/system/status")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(response.status(), StatusCode::OK);

    let body = response.into_body().collect().await?.to_bytes();
    let json: Value = serde_json::from_slice(&body)?;

    assert!(json.get("free_disk_gb").is_some());
    assert_eq!(json.get("llm_status").unwrap(), "Ready");
    assert_eq!(json.get("mail_status").unwrap(), "Active");
    Ok(())
}

#[tokio::test]
async fn test_jobs_api_full_lifecycle() -> Result<()> {
    let (app, repo) = setup_test_app()?;

    // 1. Create a job via POST /api/jobs
    let create_payload = json!({
        "url": "https://www.youtube.com/watch?v=sample123",
        "notes": "ΓΙΑ ΠΛΑΝΑ",
        "priority": 10
    });

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/jobs")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&create_payload)?))?,
        )
        .await?;

    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await?.to_bytes();
    let json: Value = serde_json::from_slice(&body)?;
    assert_eq!(json.get("status").unwrap(), "ok");
    let job_id = json.get("job_id").unwrap().as_i64().unwrap();
    assert!(job_id > 0);

    // 2. Fetch jobs via GET /api/jobs
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/api/jobs").body(Body::empty())?)
        .await?;

    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await?.to_bytes();
    let json: Value = serde_json::from_slice(&body)?;
    let jobs = json.get("jobs").unwrap().as_array().unwrap();
    assert!(jobs.iter().any(|j| j.get("id").unwrap().as_i64().unwrap() == job_id));

    // 3. Override job via POST /api/jobs/:id/override
    let override_payload = json!({
        "url": "https://www.youtube.com/watch?v=new_sample"
    });

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/jobs/{}/override", job_id))
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&override_payload)?))?,
        )
        .await?;

    assert_eq!(res.status(), StatusCode::OK);

    let updated = repo.get_job(job_id)?.expect("Job must exist");
    assert_eq!(updated.url, "https://www.youtube.com/watch?v=new_sample");
    assert_eq!(updated.status, JobStatus::Pending);

    // 4. Retry job via POST /api/jobs/:id/retry
    repo.update_job_status(job_id, JobStatus::Failed, Some("Simulated fail"), None, None)?;
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/jobs/{}/retry", job_id))
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(res.status(), StatusCode::OK);
    let retried = repo.get_job(job_id)?.expect("Job must exist");
    assert_eq!(retried.status, JobStatus::Pending);

    // 5. Discard job via POST /api/jobs/:id/discard
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/jobs/{}/discard", job_id))
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(res.status(), StatusCode::OK);
    let discarded = repo.get_job(job_id)?;
    assert!(discarded.is_none());

    Ok(())

}

#[tokio::test]
async fn test_journalists_api_crud() -> Result<()> {
    let (app, repo) = setup_test_app()?;

    // 1. Add journalist via POST /api/journalists
    let payload = json!({
        "surname": "PAPADOPOULOS",
        "full_name": "Nikos Papadopoulos",
        "emails": ["npapadopoulos@station.gr", "nikos.p@station.gr"],
        "priority": 15
    });

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/journalists")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&payload)?))?,
        )
        .await?;

    assert_eq!(res.status(), StatusCode::OK);

    // 2. Lookup journalist in database
    let surname = repo.find_journalist_by_email("npapadopoulos@station.gr")?;
    assert_eq!(surname.as_deref(), Some("PAPADOPOULOS"));

    // 3. Delete journalist via POST /api/journalists/:surname
    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/journalists/PAPADOPOULOS")
                .body(Body::empty())?,
        )
        .await?;

    assert_eq!(res.status(), StatusCode::OK);
    let deleted = repo.find_journalist_by_email("npapadopoulos@station.gr")?;
    assert!(deleted.is_none());

    Ok(())
}

#[tokio::test]
async fn test_html_pages_rendering_and_rbac() -> Result<()> {
    let (app, repo) = setup_test_app()?;

    // 1. Unauthenticated access:
    // /mcr is accessible in OpenMcr mode
    let res = app
        .clone()
        .oneshot(Request::builder().uri("/mcr").body(Body::empty())?)
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await?.to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("MCR Handler Portal"));

    // /login and /setup are publicly accessible
    for uri in ["/login", "/setup"] {
        let res = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty())?)
            .await?;
        assert_eq!(res.status(), StatusCode::OK);
    }

    // /user and /admin redirect unauthenticated visitors to /login
    for uri in ["/user", "/admin"] {
        let res = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty())?)
            .await?;
        assert_eq!(res.status(), StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(res.headers().get("location").unwrap(), "/login");
    }

    // 2. Authenticated access:
    // Create Admin user and session
    let admin_id = repo.create_user(
        "admin@station.gr",
        "hash123",
        omni_core::models::UserRole::Admin,
        "Admin",
        None,
    )?;
    let admin_token = omni_core::auth::generate_session_token();
    repo.create_session(admin_id, &admin_token, 7)?;

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin")
                .header("cookie", format!("omni_session={}", admin_token))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await?.to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("IT Administration"));

    // Create Journalist User and session
    let user_id = repo.create_user(
        "reporter@station.gr",
        "hash123",
        omni_core::models::UserRole::User,
        "Reporter",
        Some("PAPADAKI"),
    )?;
    let user_token = omni_core::auth::generate_session_token();
    repo.create_session(user_id, &user_token, 7)?;

    let res = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/user")
                .header("cookie", format!("omni_session={}", user_token))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(res.status(), StatusCode::OK);
    let body = res.into_body().collect().await?.to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("Journalist Ingest Portal"));


    Ok(())
}
